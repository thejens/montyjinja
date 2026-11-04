//! Drives sandboxed executions, answering every suspension (host function
//! calls, undefined-name lookups, OS calls) from the host.
//!
//! Each execution starts from a private copy of a compiled program or
//! snapshot, so calls share no Python state: a filter cannot leak data from
//! one render into another, and any number of threads can render
//! concurrently.
//!
//! Every step that executes interpreter code goes through [`contained`], so
//! a Rust panic inside Monty fails that one call instead of unwinding
//! through the template engine.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, PoisonError};

use minijinja::value::Kwargs;
use minijinja::{Error, State, Value};
use monty::{FunctionCall, MontyRun, RunProgress};
use monty_types::{
    CallArgs, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult, PrintWriter,
    ResourceTracker,
};

use crate::config::Config;
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

/// Where the code came from, for traceback rendering.
#[derive(Clone, Copy)]
pub struct Script<'a> {
    pub name: &'a str,
    pub line_offset: u32,
    /// Last line of user code; later lines are generated and hidden.
    pub last_user_line: Option<u32>,
}

/// Why [`Execution::drive`] returned.
pub enum Stop {
    /// The program finished with this value.
    Complete(MontyObject),
    /// The program called the checkpoint function; the call is returned
    /// unanswered so the host can resume (or snapshot) it.
    Checkpoint(Box<FunctionCall>),
}

/// One invocation: collects `print()` output and counts host round-trips.
pub struct Execution<'a> {
    config: &'a Config,
    names: NameResolution,
    script: Script<'a>,
    printed: String,
    host_calls: usize,
}

impl<'a> Execution<'a> {
    pub fn new(config: &'a Config, names: NameResolution, script: Script<'a>) -> Self {
        Self {
            config,
            names,
            script,
            printed: String::new(),
            host_calls: 0,
        }
    }

    /// Everything the invocation printed so far.
    pub fn into_printed(self) -> String {
        self.printed
    }

    pub fn fail(&self, exc: MontyException) -> PythonError {
        let err = PythonError::new(exc, self.script.name, self.script.line_offset);
        match self.script.last_user_line {
            Some(line) => err.hide_frames_after(line),
            None => err,
        }
    }

    fn writer(&mut self) -> PrintWriter<'_> {
        PrintWriter::CollectString(&mut self.printed, Some(self.config.max_output_bytes))
    }

    /// Starts a fresh copy of `program`.
    pub fn start(
        &mut self,
        program: &SharedProgram,
        inputs: Vec<MontyObject>,
    ) -> Result<RunProgress, PythonError> {
        let tracker = ResourceTracker::new(self.config.limits.clone());
        let program = program.fork();
        let writer = self.writer();
        contained(|| program.start(inputs, tracker, writer)).map_err(|exc| self.fail(exc))
    }

    /// Answers a suspended call — typically a restored checkpoint.
    pub fn resume(&mut self, call: FunctionCall, value: MontyObject) -> Result<RunProgress, PythonError> {
        let writer = self.writer();
        contained(|| call.resume(value, writer)).map_err(|exc| self.fail(exc))
    }

    /// Serves suspensions until the program completes or calls `checkpoint`.
    ///
    /// `state` is `None` while a module's top level runs, which must not
    /// depend on any particular template.
    pub fn drive(
        &mut self,
        mut progress: RunProgress,
        mut state: Option<&mut State<'_, '_>>,
        checkpoint: Option<&str>,
    ) -> Result<Stop, PythonError> {
        loop {
            let over_budget = !matches!(progress, RunProgress::Complete(_)) && {
                self.host_calls += 1;
                self.host_calls > self.config.max_host_calls
            };
            let budget_exc = MontyException::new(
                ExcType::RuntimeError,
                Some(format!("exceeded {} host calls", self.config.max_host_calls)),
            );
            let names = self.names;
            let writer = PrintWriter::CollectString(&mut self.printed, Some(self.config.max_output_bytes));

            let next = match progress {
                RunProgress::Complete(value) => return Ok(Stop::Complete(value)),
                RunProgress::FunctionCall(call) if Some(call.function_name.as_str()) == checkpoint => {
                    return Ok(Stop::Checkpoint(Box::new(call)));
                }
                RunProgress::FunctionCall(call) if over_budget => {
                    contained(|| call.abort(budget_exc, writer))
                }
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
                    contained(|| call.resume(result, writer))
                }
                RunProgress::NameLookup(lookup) if over_budget => {
                    contained(|| lookup.abort(budget_exc, writer))
                }
                RunProgress::NameLookup(lookup) => {
                    let result = match (names, state.as_deref()) {
                        (NameResolution::TemplateContext, Some(state)) => {
                            lookup_template_name(state, &lookup.name)
                        }
                        _ => NameLookupResult::Undefined,
                    };
                    contained(|| lookup.resume(result, writer))
                }
                // The sandbox has no filesystem or environment; refuse rather
                // than silently answering with fabricated data.
                RunProgress::OsCall(call) => {
                    let exc = MontyException::new(
                        ExcType::NotImplementedError,
                        Some(format!("{:?} is not available in templates", call.function_call)),
                    );
                    contained(|| call.resume(ExtFunctionResult::Error(exc), writer))
                }
                // Host functions answer synchronously, so the only way to
                // reach this is `await` on something we never handed out.
                RunProgress::ResolveFutures(futures) => {
                    let exc = MontyException::new(
                        ExcType::RuntimeError,
                        Some("async code is not supported in templates".to_owned()),
                    );
                    contained(|| futures.abort(exc, writer))
                }
            };
            progress = next.map_err(|exc| self.fail(exc))?;
        }
    }

    /// Starts `program` and runs it to completion.
    pub fn run(
        &mut self,
        program: &SharedProgram,
        inputs: Vec<MontyObject>,
        state: Option<&mut State<'_, '_>>,
    ) -> Result<MontyObject, PythonError> {
        let progress = self.start(program, inputs)?;
        match self.drive(progress, state, None)? {
            Stop::Complete(value) => Ok(value),
            Stop::Checkpoint(_) => unreachable!("no checkpoint was requested"),
        }
    }
}

/// Runs one interpreter step, converting a panic inside Monty into a Python
/// `RuntimeError`.
///
/// `AssertUnwindSafe` is sound here because the step owns the interpreter
/// state it advances: on a panic that state is dropped with the closure and
/// never observed again. The only borrowed data, the print buffer, is a
/// `String` and stays valid whatever was appended. Host callbacks run outside
/// this boundary, so panics in the host's own filters still propagate as
/// they would without this crate. Containment requires `panic = "unwind"`
/// (Rust's default); under `panic = "abort"` the process still terminates.
pub(crate) fn contained<T>(step: impl FnOnce() -> Result<T, MontyException>) -> Result<T, MontyException> {
    catch_unwind(AssertUnwindSafe(step)).unwrap_or_else(|payload| {
        Err(MontyException::new(
            ExcType::RuntimeError,
            Some(format!(
                "internal interpreter error: {}",
                panic_message(&*payload)
            )),
        ))
    })
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic with a non-string payload")
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

#[cfg(test)]
mod tests {
    use super::contained;

    #[test]
    fn panics_become_python_runtime_errors() {
        let result = contained::<()>(|| panic!("heap corrupted"));
        let exc = result.unwrap_err();
        assert_eq!(
            exc.summary(),
            "RuntimeError: internal interpreter error: heap corrupted"
        );
        assert_eq!(contained(|| Ok(7)).unwrap(), 7);
    }
}
