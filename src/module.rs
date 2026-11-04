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

use std::sync::Arc;

use minijinja::value::{Rest, ValueOrKwargs};
use minijinja::{Environment, Error, State, Value};
use monty::{FunctionCall, MontyRun};
use monty_types::{CompileOptions, ExcType, MontyException, MontyObject, ObjectRef};

use crate::config::{Config, ModuleStrategy};
use crate::convert::{from_monty, repr_text, to_monty};
use crate::error::{LoadError, PythonError};
use crate::runtime::{Execution, HOST_FUNCTIONS, NameResolution, Script, SharedProgram, Stop};

/// Defines the registration decorators. It must stay on ONE line: it is
/// prepended to user source, and tracebacks are corrected by exactly
/// `PRELUDE_LINES`. Monty has neither `callable()` nor `fn.__name__`, so a
/// bare `@template_filter` is told apart from `@template_filter('name')` by
/// argument type, and bare names are recovered from the function's repr
/// after the module runs (see `function_name_from_repr`).
const PRELUDE: &str = "__mj_reg__ = {'filter': [], 'test': [], 'global': []}; \
__mj_dec__ = lambda k, a: (lambda fn: (__mj_reg__[k].append((a, fn)), fn)[1]) if a is None or isinstance(a, str) else (__mj_reg__[k].append((None, a)), a)[1]; \
template_filter = lambda a=None: __mj_dec__('filter', a); \
template_test = lambda a=None: __mj_dec__('test', a); \
template_global = lambda a=None: __mj_dec__('global', a)\n";
const PRELUDE_LINES: u32 = 1;

/// After the module's top level, the program calls the `CHECKPOINT` host
/// function with the registry and suspends. The host reads the exports from
/// that call at load time; for each template call it resumes the checkpoint
/// with `(kind, index, args, kwargs)` and the program dispatches. Pausing
/// there is what lets a fully initialised module be snapshotted once and
/// restored per call (see `ModuleStrategy`).
const DISPATCH: &str = "\n__mj_kind__, __mj_index__, __mj_args__, __mj_kwargs__ = __mj_ready__(__mj_reg__)\n\
__mj_reg__[__mj_kind__][__mj_index__][1](*__mj_args__, **__mj_kwargs__)\n";
const CHECKPOINT: &str = "__mj_ready__";

/// What a Python function is registered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Filter,
    Test,
    Global,
}

impl ExportKind {
    const ALL: [Self; 3] = [Self::Filter, Self::Test, Self::Global];

    fn key(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Test => "test",
            Self::Global => "global",
        }
    }
}

/// One exported Python function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub kind: ExportKind,
    pub name: String,
    index: usize,
}

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
        match execution.drive(progress, None, Some(CHECKPOINT))? {
            Stop::Checkpoint(call) => Ok(*call),
            Stop::Complete(_) => Err(execution.fail(MontyException::new(
                ExcType::RuntimeError,
                Some("module finished without reaching its dispatch point".to_owned()),
            ))),
        }
    }

    fn call(&self, state: &mut State<'_, '_>, export: &Export, args: &[Value]) -> Result<Value, Error> {
        let args = split_kwargs(args)?;
        let result = self.invoke(state, export, args).map_err(|err| {
            err.into_template_error(&format!("python {} '{}'", export.kind.key(), export.name))
        })?;
        from_monty(result.as_ref())
    }

    fn invoke(
        &self,
        state: &mut State<'_, '_>,
        export: &Export,
        args: PyArgs,
    ) -> Result<MontyObject, PythonError> {
        let mut execution = self.execution();
        let checkpoint = match &self.snapshot {
            // The bytes were produced by this process from a value of the
            // same type, so decoding can only fail on a Monty bug.
            Some(bytes) => postcard::from_bytes::<FunctionCall>(bytes)
                .map_err(|err| execution.fail(MontyException::runtime_error(err)))?,
            None => self.run_to_checkpoint(&mut execution)?,
        };
        // Each call gets the full time budget, regardless of how long the
        // module's top level took when the snapshot was made.
        checkpoint.tracker().on_feed_start();
        let dispatch = MontyObject::tuple([
            MontyObject::string(export.kind.key()),
            MontyObject::int(i64::try_from(export.index).unwrap_or(i64::MAX)),
            MontyObject::list(args.positional),
            MontyObject::dict(args.keyword),
        ]);
        let progress = execution.resume(checkpoint, dispatch)?;
        match execution.drive(progress, Some(state), None)? {
            Stop::Complete(value) => Ok(value),
            Stop::Checkpoint(_) => unreachable!("no checkpoint was requested"),
        }
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

fn parse_registry(registry: ObjectRef<'_>) -> Result<Vec<Export>, LoadError> {
    let invalid = |msg: &str| LoadError::Invalid(msg.to_owned());
    let pairs = registry
        .pairs()
        .ok_or_else(|| invalid("registry is not a dict"))?;
    let mut exports = Vec::new();
    for kind in ExportKind::ALL {
        let entries = pairs
            .iter()
            .find(|(key, _)| key.as_str() == Some(kind.key()))
            .and_then(|(_, list)| list.items())
            .ok_or_else(|| invalid("registry is missing a kind"))?;
        for (index, entry) in entries.into_iter().enumerate() {
            let parts = entry.items().ok_or_else(|| invalid("malformed registry entry"))?;
            let [explicit, function] = parts.as_slice() else {
                return Err(invalid("malformed registry entry"));
            };
            let name = match explicit.as_str() {
                Some(name) => name.to_owned(),
                None => function_name_from_repr(&repr_text(*function)).ok_or_else(|| {
                    LoadError::Invalid(format!(
                        "cannot infer a {} name for {}; pass one explicitly, e.g. @template_{}('name')",
                        kind.key(),
                        repr_text(*function),
                        kind.key()
                    ))
                })?,
            };
            exports.push(Export { kind, name, index });
        }
    }
    Ok(exports)
}

/// Extracts `name` from Monty's function repr, `<function 'name' at 0x..>`.
///
/// Load-bearing workaround: Monty does not expose `__name__`, and returns
/// function objects to the host only as their repr. Lambdas (`<lambda>`)
/// are rejected because they have no usable name.
fn function_name_from_repr(repr: &str) -> Option<String> {
    let rest = repr.strip_prefix("<function '")?;
    let name = &rest[..rest.find('\'')?];
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    valid.then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::function_name_from_repr;

    #[test]
    fn parses_function_repr() {
        assert_eq!(
            function_name_from_repr("<function 'slugify' at 0x6c>").as_deref(),
            Some("slugify")
        );
        assert_eq!(function_name_from_repr("<function '<lambda>' at 0x6c>"), None);
        assert_eq!(function_name_from_repr("'slugify'"), None);
    }
}
