//! The registration side of a module: the decorator prelude prepended to
//! user source, and parsing of the registry it fills.

use monty_types::ObjectRef;

use crate::convert::{from_monty, repr_text};
use crate::error::LoadError;

/// Defines the registration API, in two styles:
///
/// * decorators: `@template_filter`, `@template_test`, `@template_global`;
/// * Python Jinja2's own: `env.filters['name'] = fn`, likewise `env.tests`
///   and `env.globals`, so modules written for a `jinja2.Environment` run
///   unchanged. The dispatch code folds these dicts into the registry after
///   the module's top level; it reads them through `__mj_env__`, so a module
///   that rebinds `env` for its own purposes is unaffected.
///
/// It is prepended to user source, and tracebacks are corrected by exactly
/// `PRELUDE_LINES`, so its line count must match. Monty has neither `callable()` nor `fn.__name__`, so a
/// bare `@template_filter` is told apart from `@template_filter('name')` by
/// argument type, and bare names are recovered from the function's repr
/// after the module runs (see `function_name_from_repr`). Each registry
/// entry is `(explicit name or None, function, pure)`.
/// `PY_VIEW_HELPER` is not spliced in here, because `concat!` takes only
/// literals; `module::load` prepends it, and `PRELUDE_LINES` counts both.
pub(super) const PRELUDE: &str = "class __MjEnv__:\n    \
def __init__(self):\n        \
self.filters = {}; self.tests = {}; self.globals = {}\n\
__mj_env__ = env = __MjEnv__(); \
__mj_reg__ = {'filter': [], 'test': [], 'global': []}; \
__mj_dec__ = lambda k, a, p: (lambda fn: (__mj_reg__[k].append((a, fn, p)), fn)[1]) if a is None or isinstance(a, str) else (__mj_reg__[k].append((None, a, p)), a)[1]; \
template_filter = lambda a=None, pure=False: __mj_dec__('filter', a, pure); \
template_test = lambda a=None, pure=False: __mj_dec__('test', a, pure); \
template_global = lambda a=None, pure=False: __mj_dec__('global', a, pure)\n";
pub(super) const PRELUDE_LINES: u32 = 4 + crate::convert::PY_VIEW_HELPER_LINES;

/// What a Python function is registered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportKind {
    Filter,
    Test,
    /// A global function.
    Global,
    /// A global value that is not a function, e.g.
    /// `env.globals['SITE'] = 'example.com'`. Converted once, at load.
    Constant,
}

impl ExportKind {
    const ALL: [Self; 3] = [Self::Filter, Self::Test, Self::Global];

    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Test => "test",
            Self::Global | Self::Constant => "global",
        }
    }
}

/// One exported Python function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub kind: ExportKind,
    pub name: String,
    /// Registered with `pure=True`: results are memoized per render.
    pub pure: bool,
    pub(super) index: usize,
    /// The value of an `ExportKind::Constant`.
    pub(super) constant: Option<minijinja::Value>,
}

pub(super) fn parse_registry(registry: ObjectRef<'_>) -> Result<Vec<Export>, LoadError> {
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
            let [explicit, function, pure] = parts.as_slice() else {
                return Err(invalid("malformed registry entry"));
            };
            let pure = pure.as_bool().ok_or_else(|| {
                LoadError::Invalid(format!(
                    "pure= must be True or False for {}",
                    repr_text(*function)
                ))
            })?;
            if kind == ExportKind::Global && !is_function(*function) {
                let name = explicit
                    .as_str()
                    .ok_or_else(|| invalid("global values need a name"))?
                    .to_owned();
                let value = from_monty(*function)
                    .map_err(|err| LoadError::Invalid(format!("global '{name}': {err}")))?;
                exports.push(Export {
                    kind: ExportKind::Constant,
                    name,
                    pure: true,
                    index,
                    constant: Some(value),
                });
                continue;
            }
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
            exports.push(Export {
                kind,
                name,
                pure,
                index,
                constant: None,
            });
        }
    }
    Ok(exports)
}

/// Whether a registered value is callable.
///
/// Monty has no `callable()`, and functions, builtins and classes reach the
/// host only as opaque repr nodes, so this decides by the repr's prefix.
fn is_function(value: ObjectRef<'_>) -> bool {
    value.type_name() == "repr"
        && ["<function ", "<built-in ", "<class ", "<bound method "]
            .iter()
            .any(|prefix| repr_text(value).starts_with(prefix))
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
    use super::{PRELUDE, PRELUDE_LINES, function_name_from_repr};
    use crate::convert::PY_VIEW_HELPER;

    #[test]
    fn prelude_line_count_matches_its_constant() {
        // Traceback line numbers are shifted by exactly PRELUDE_LINES.
        let lines = PY_VIEW_HELPER.lines().count() + PRELUDE.lines().count();
        assert_eq!(lines, PRELUDE_LINES as usize);
    }

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
