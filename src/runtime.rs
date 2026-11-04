//! Drives one sandboxed execution to completion, answering every suspension
//! (host function calls, undefined-name lookups, OS calls) from the host.
//!
//! Each execution clones a pre-compiled [`MontyRun`], so calls share no
//! Python state: a filter cannot leak data from one render into another, and
//! any number of threads can render concurrently without locking.

use std::sync::{Mutex, PoisonError};

use minijinja::value::Kwargs;
use minijinja::{Error, State, Value};
use monty::{MontyRun, RunProgress};
use monty_types::{
    CallArgs, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult, OsPolicy,
    PrintWriter, ResourceLimits, ResourceTracker,
};

use crate::convert::{from_monty, to_monty};
use crate::error::PythonError;

/// Host functions every sandboxed snippet can call. They are compiled in as
/// external-function inputs, so a call suspends the VM and the host answers
/// it with full access to the MiniJinja environment.
pub const HOST_FUNCTIONS: [&str; 3] = ["render", "render_string", "apply_filter"];

/// A compiled program shared across threads.
///
/// `MontyRun` is `Send` but not `Sync` (its interner uses `RefCell`), so it
/// cannot sit behind a bare `Arc`. The lock is held only while cloning; the
/// clone then executes without any shared state.
pub struct SharedProgram(Mutex<MontyRun>);

impl SharedProgram {
    pub fn new(program: MontyRun) -> Self {
        Self(Mutex::new(program))
    }

    fn fork(&self) -> MontyRun {
        // A panic while cloning cannot leave the program half-written, so a
        // poisoned lock still guards a valid value.
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// Sandbox limits applied to every Python invocation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Memory, time and recursion limits per invocation.
    pub limits: ResourceLimits,
    /// Clock, timezone, randomness and sleep behaviour inside the sandbox.
    pub os_policy: OsPolicy,
    /// Maximum bytes a single invocation may `print()`.
    pub max_output_bytes: usize,
    /// Maximum host round-trips (callbacks + name lookups) per invocation.
    /// Monty stores but does not enforce `max_suspensions` for in-process
    /// hosts, so this crate counts them itself.
    pub max_host_calls: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limits: ResourceLimits {
                max_memory: Some(32 * 1024 * 1024),
                max_feed_duration: Some(std::time::Duration::from_millis(250)),
                max_recursion_depth: 200,
                ..ResourceLimits::default()
            },
            os_policy: OsPolicy::default(),
            max_output_bytes: 1024 * 1024,
            max_host_calls: 1_000,
        }
    }
}

/// How undefined Python names are answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameResolution {
    /// Raise `NameError`, as normal Python would. Used for modules, whose
    /// behaviour must not depend on which template happens to call them.
    Strict,
    /// Look the name up in the calling template's context. Used for inline
    /// snippets, which read like part of the template.
    TemplateContext,
}

/// Result of a completed invocation.
pub struct Outcome {
    pub value: MontyObject,
    pub printed: String,
}

/// Where the code came from, for traceback rendering.
#[derive(Clone, Copy)]
pub struct Script<'a> {
    pub name: &'a str,
    pub line_offset: u32,
    /// Last line of user code; later lines are generated and hidden.
    pub last_user_line: Option<u32>,
}

/// Runs `program` with `inputs` until it completes, serving suspensions.
///
/// `state` is `None` only while discovering a module's exports at load
/// time, when there is no template to call back into.
pub fn execute(
    program: &SharedProgram,
    inputs: Vec<MontyObject>,
    config: &Config,
    mut state: Option<&mut State<'_, '_>>,
    names: NameResolution,
    script: Script<'_>,
) -> Result<Outcome, PythonError> {
    let fail = |exc| {
        let err = PythonError::new(exc, script.name, script.line_offset);
        match script.last_user_line {
            Some(line) => err.hide_frames_after(line),
            None => err,
        }
    };
    let mut printed = String::new();
    let cap = Some(config.max_output_bytes);
    let tracker = ResourceTracker::new(config.limits.clone());

    let mut progress = program
        .fork()
        .start(inputs, tracker, PrintWriter::CollectString(&mut printed, cap))
        .map_err(fail)?;
    let mut host_calls = 0usize;

    loop {
        if !matches!(progress, RunProgress::Complete(_)) {
            host_calls += 1;
        }
        let over_budget = host_calls > config.max_host_calls;
        let budget_exc = || {
            MontyException::new(
                ExcType::RuntimeError,
                Some(format!("exceeded {} host calls", config.max_host_calls)),
            )
        };
        let writer = PrintWriter::CollectString(&mut printed, cap);

        progress = match progress {
            RunProgress::Complete(value) => return Ok(Outcome { value, printed }),
            RunProgress::FunctionCall(call) if over_budget => call.abort(budget_exc(), writer),
            RunProgress::FunctionCall(call) => {
                let result = match state.as_deref_mut() {
                    Some(state) => call_host(state, &call.function_name, &call.args),
                    None => ExtFunctionResult::Error(MontyException::new(
                        ExcType::RuntimeError,
                        Some(format!(
                            "{}() is only available while a template is rendering",
                            call.function_name
                        )),
                    )),
                };
                call.resume(result, writer)
            }
            RunProgress::NameLookup(lookup) if over_budget => lookup.abort(budget_exc(), writer),
            RunProgress::NameLookup(lookup) => {
                let result = match (names, state.as_deref()) {
                    (NameResolution::TemplateContext, Some(state)) => {
                        lookup_template_name(state, &lookup.name)
                    }
                    _ => NameLookupResult::Undefined,
                };
                lookup.resume(result, writer)
            }
            // The sandbox has no filesystem or environment; refuse rather
            // than silently answering with fabricated data.
            RunProgress::OsCall(call) => {
                let exc = MontyException::new(
                    ExcType::NotImplementedError,
                    Some(format!("{:?} is not available in templates", call.function_call)),
                );
                call.resume(ExtFunctionResult::Error(exc), writer)
            }
            // Host functions answer synchronously, so the only way to reach
            // this is `await` on something we never handed out.
            RunProgress::ResolveFutures(futures) => futures.abort(
                MontyException::new(
                    ExcType::RuntimeError,
                    Some("async code is not supported in templates".to_owned()),
                ),
                writer,
            ),
        }
        .map_err(fail)?;
    }
}

fn lookup_template_name(state: &State<'_, '_>, name: &str) -> NameLookupResult {
    match state.lookup(name) {
        Some(value) if !value.is_undefined() => match to_monty(&value) {
            Ok(obj) => NameLookupResult::Value(obj),
            Err(err) => NameLookupResult::Error(host_exception(&err)),
        },
        _ => NameLookupResult::Undefined,
    }
}

fn call_host(state: &mut State<'_, '_>, name: &str, args: &CallArgs) -> ExtFunctionResult {
    match dispatch_host(state, name, args).and_then(|value| to_monty(&value)) {
        Ok(obj) => ExtFunctionResult::Return(obj),
        Err(err) => ExtFunctionResult::Error(host_exception(&err)),
    }
}

fn dispatch_host(state: &mut State<'_, '_>, name: &str, args: &CallArgs) -> Result<Value, Error> {
    let positional = args.args().map(from_monty).collect::<Result<Vec<_>, _>>()?;
    let kwargs = args
        .kwargs()
        .map(|(key, value)| {
            let key = key.as_str().unwrap_or_default().to_owned();
            from_monty(value).map(|value| (key, value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let text_arg = |idx: usize| {
        positional
            .get(idx)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::new(
                    minijinja::ErrorKind::MissingArgument,
                    format!("{name}() argument {} must be a string", idx + 1),
                )
            })
    };
    match name {
        "render" => {
            let template = state.env().get_template(&text_arg(0)?)?;
            template.render(Value::from_pairs(kwargs)).map(Value::from)
        }
        "render_string" => state
            .env()
            .render_str(&text_arg(0)?, Value::from_pairs(kwargs))
            .map(Value::from),
        "apply_filter" => {
            let filter = text_arg(0)?;
            let mut filter_args: Vec<Value> = positional.into_iter().skip(1).collect();
            if !kwargs.is_empty() {
                filter_args.push(Value::from(Kwargs::from_iter(kwargs)));
            }
            state.apply_filter(&filter, &filter_args)
        }
        other => Err(Error::new(
            minijinja::ErrorKind::UnknownFunction,
            format!("unknown host function {other}()"),
        )),
    }
}

/// Template errors raised by host callbacks become Python `RuntimeError`s, so
/// sandboxed code can catch them like any other exception.
fn host_exception(err: &Error) -> MontyException {
    MontyException::new(ExcType::RuntimeError, Some(err.to_string()))
}
