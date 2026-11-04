//! Python modules whose decorated functions become MiniJinja filters, tests
//! and global functions.
//!
//! ```python
//! @template_filter
//! def slugify(text):
//!     return '-'.join(text.lower().split())
//!
//! @template_test('prime')
//! def is_prime(n):
//!     return n > 1 and all(n % d for d in range(2, int(n ** 0.5) + 1))
//! ```

mod registry;

use std::collections::HashMap;
use std::sync::Arc;

use minijinja::value::{Rest, ValueOrKwargs};
use minijinja::{Environment, Error, State, Value};
use monty::{FunctionCall, MontyRun};
use monty_types::{CompileOptions, ExcType, MontyException, MontyObject};

use crate::config::{Config, Isolation, ModuleStrategy};
use crate::convert::{from_monty, to_monty};
use crate::error::{LoadError, PythonError};
use crate::runtime::{Execution, HOST_FUNCTIONS, NameResolution, Script, SharedProgram, Stop};
pub use registry::{Export, ExportKind};
use registry::{PRELUDE, PRELUDE_LINES, parse_registry};

/// After the module's top level, the program loops forever: it calls the
/// `CHECKPOINT` host function and suspends, the host resumes it with
/// `(kind, index, args, kwargs)`, it dispatches, and it hands the result to
/// the next checkpoint call.
///
/// The first checkpoint carries the registry, from which the host reads the
/// exports at load time. Every later one carries a call's result. Because
/// the program is always paused at the same place between calls, a fully
/// initialised module can be snapshotted once and restored per call (see
/// `ModuleStrategy`), or kept suspended and resumed again by the next call
/// in the same render (see `Isolation::PerRender`).
const DISPATCH: &str = "\n__mj_out__ = __mj_reg__\n\
while True:\n    \
__mj_kind__, __mj_index__, __mj_args__, __mj_kwargs__ = __mj_ready__(__mj_out__)\n    \
__mj_out__ = __mj_reg__[__mj_kind__][__mj_index__][1](*__mj_args__, **__mj_kwargs__)\n";
const CHECKPOINT: &str = "__mj_ready__";

/// Suspended module programs kept for the rest of a render under
/// `Isolation::PerRender`, keyed by `ModuleInner::id`. Stored as a typed
/// render-local extension, so MiniJinja drops it, and every interpreter in
/// it, when the render's `State` goes away.
#[derive(Default)]
struct Sessions(HashMap<usize, FunctionCall>);

/// A loaded, validated Python module, ready to be installed into any number
/// of environments. Cheap to clone.
#[derive(Clone)]
pub struct PythonModule {
    inner: Arc<ModuleInner>,
}

impl std::fmt::Debug for PythonModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonModule")
            .field("script", &self.inner.script)
            .field("exports", &self.inner.exports)
            .finish_non_exhaustive()
    }
}

struct ModuleInner {
    script: String,
    /// Line count of the user's source, to hide the dispatch frames.
    source_lines: u32,
    program: SharedProgram,
    /// The checkpoint call, serialized, under `ModuleStrategy::Snapshot`.
    snapshot: Option<Vec<u8>>,
    config: Config,
    exports: Vec<Export>,
}

impl PythonModule {
    /// Compiles `source`, runs its top level once to collect registrations,
    /// and validates them.
    pub fn load(script: &str, source: &str, config: Config) -> Result<Self, LoadError> {
        let code = format!("{PRELUDE}{source}{DISPATCH}");
        let inputs = std::iter::once(CHECKPOINT)
            .chain(HOST_FUNCTIONS)
            .map(str::to_owned)
            .collect();
        let program = MontyRun::new(code, script, inputs, CompileOptions::default())
            .map_err(|exc| LoadError::Python(PythonError::new(exc, script, PRELUDE_LINES)))?
            .with_os_policy(config.os_policy.clone());

        let mut module = ModuleInner {
            script: script.to_owned(),
            source_lines: u32::try_from(source.lines().count()).unwrap_or(u32::MAX),
            program: SharedProgram::new(program),
            snapshot: None,
            config,
            exports: Vec::new(),
        };
        let mut execution = module.execution();
        let checkpoint = module
            .run_to_checkpoint(&mut execution)
            .map_err(LoadError::Python)?;
        let registry = checkpoint
            .args
            .arg(0)
            .ok_or_else(|| LoadError::Invalid("checkpoint carried no registry".to_owned()))?;
        module.exports = parse_registry(registry)?;
        if module.config.module_strategy == ModuleStrategy::Snapshot {
            let bytes = postcard::to_allocvec(&checkpoint)
                .map_err(|err| LoadError::Invalid(format!("cannot snapshot module: {err}")))?;
            module.snapshot = Some(bytes);
        }
        Ok(Self {
            inner: Arc::new(module),
        })
    }

    /// Everything the module registered, in definition order per kind.
    pub fn exports(&self) -> &[Export] {
        &self.inner.exports
    }

    /// Registers every export with `env`.
    pub fn install(&self, env: &mut Environment<'_>) {
        for export in &self.inner.exports {
            let module = Arc::clone(&self.inner);
            let export = export.clone();
            let name = export.name.clone();
            match export.kind {
                ExportKind::Filter => {
                    env.add_filter(name, move |state: &mut State, args: Rest<ValueOrKwargs>| {
                        module.call(state, &export, &args.into_values())
                    })
                }
                ExportKind::Global => {
                    env.add_function(name, move |state: &mut State, args: Rest<ValueOrKwargs>| {
                        module.call(state, &export, &args.into_values())
                    })
                }
                ExportKind::Test => env.add_test(
                    name,
                    move |state: &mut State, args: Rest<ValueOrKwargs>| -> Result<bool, Error> {
                        module
                            .call(state, &export, &args.into_values())
                            .map(|v| v.is_true())
                    },
                ),
            }
        }
    }
}

impl ModuleInner {
    fn execution(&self) -> Execution<'_> {
        let script = Script {
            name: &self.script,
            line_offset: PRELUDE_LINES,
            last_user_line: Some(self.source_lines),
        };
        Execution::new(&self.config, NameResolution::Strict, script)
    }

    /// Runs the module's top level, without template access, up to the
    /// dispatch checkpoint.
    fn run_to_checkpoint(&self, execution: &mut Execution<'_>) -> Result<FunctionCall, PythonError> {
        let inputs = std::iter::once(CHECKPOINT)
            .chain(HOST_FUNCTIONS)
            .map(|name| MontyObject::function(name, None))
            .collect();
        let progress = execution.start(&self.program, inputs)?;
        let stop = execution.drive(progress, None, Some(CHECKPOINT))?;
        Self::next_checkpoint(execution, stop)
    }

    /// The checkpoint the dispatch loop suspended at.
    fn next_checkpoint(execution: &mut Execution<'_>, stop: Stop) -> Result<FunctionCall, PythonError> {
        match stop {
            Stop::Checkpoint(call) => Ok(*call),
            // The dispatch loop never exits on its own; reaching the end
            // means user code subverted it (e.g. rebound `__mj_ready__`).
            Stop::Complete(_) => Err(execution.fail(MontyException::new(
                ExcType::RuntimeError,
                Some("module left its dispatch loop".to_owned()),
            ))),
        }
    }

    /// Identifies this module among the sessions of a render. The address is
    /// stable and unique for as long as an installed closure holds the `Arc`.
    fn id(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    fn call(&self, state: &mut State<'_, '_>, export: &Export, args: &[Value]) -> Result<Value, Error> {
        let args = split_kwargs(args)?;
        let checkpoint = self.invoke(state, export, args).map_err(|err| {
            err.into_template_error(&format!("python {} '{}'", export.kind.key(), export.name))
        })?;
        let result = checkpoint.args.arg(0).map(from_monty).transpose()?;
        if self.config.isolation == Isolation::PerRender {
            state
                .get_or_insert_extension_with(Sessions::default)
                .0
                .insert(self.id(), checkpoint);
        }
        Ok(result.unwrap_or_default())
    }

    /// Runs one call and returns the checkpoint the module suspends at
    /// afterwards, which carries the call's result as its argument.
    fn invoke(
        &self,
        state: &mut State<'_, '_>,
        export: &Export,
        args: PyArgs,
    ) -> Result<FunctionCall, PythonError> {
        let mut execution = self.execution();
        // Taken out rather than borrowed: the call may re-enter this module
        // through `apply_filter` with the same `State`. The nested call then
        // finds no session and starts pristine, and whichever call finishes
        // last leaves its session behind.
        let session = match self.config.isolation {
            Isolation::PerRender => state
                .get_extension_mut::<Sessions>()
                .and_then(|sessions| sessions.0.remove(&self.id())),
            Isolation::PerCall => None,
        };
        let checkpoint = match (session, &self.snapshot) {
            (Some(session), _) => session,
            // The bytes were produced by this process from a value of the
            // same type, so decoding can only fail on a Monty bug.
            (None, Some(bytes)) => postcard::from_bytes::<FunctionCall>(bytes)
                .map_err(|err| execution.fail(MontyException::runtime_error(err)))?,
            (None, None) => self.run_to_checkpoint(&mut execution)?,
        };
        // Each call gets the full time budget, regardless of how long the
        // module's top level (or earlier calls in the render) took.
        checkpoint.tracker().on_feed_start();
        let dispatch = MontyObject::tuple([
            MontyObject::string(export.kind.key()),
            MontyObject::int(i64::try_from(export.index).unwrap_or(i64::MAX)),
            MontyObject::list(args.positional),
            MontyObject::dict(args.keyword),
        ]);
        let progress = execution.resume(checkpoint, dispatch)?;
        let stop = execution.drive(progress, Some(state), Some(CHECKPOINT))?;
        Self::next_checkpoint(&mut execution, stop)
    }
}

/// Python call arguments converted from a MiniJinja call.
struct PyArgs {
    positional: Vec<MontyObject>,
    keyword: Vec<(MontyObject, MontyObject)>,
}

/// Converts MiniJinja call arguments into Python positional and keyword
/// arguments. MiniJinja passes keyword arguments as a trailing map value.
fn split_kwargs(args: &[Value]) -> Result<PyArgs, Error> {
    let (positional, kwargs) = match args.split_last() {
        Some((last, rest)) if last.is_kwargs() => (rest, Some(last)),
        _ => (args, None),
    };
    let positional = positional.iter().map(to_monty).collect::<Result<_, _>>()?;
    let mut pairs = Vec::new();
    if let Some(kwargs) = kwargs {
        for key in kwargs.try_iter()? {
            let value = kwargs.get_item(&key)?;
            pairs.push((to_monty(&key)?, to_monty(&value)?));
        }
    }
    Ok(PyArgs {
        positional,
        keyword: pairs,
    })
}
