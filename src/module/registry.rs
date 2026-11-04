//! The registration side of a module: the decorator prelude prepended to
//! user source, and parsing of the registry it fills.

use monty_types::ObjectRef;

use crate::convert::repr_text;
use crate::error::LoadError;

/// Defines the registration decorators. It must stay on ONE line: it is
/// prepended to user source, and tracebacks are corrected by exactly
/// `PRELUDE_LINES`. Monty has neither `callable()` nor `fn.__name__`, so a
/// bare `@template_filter` is told apart from `@template_filter('name')` by
/// argument type, and bare names are recovered from the function's repr
/// after the module runs (see `function_name_from_repr`).
pub(super) const PRELUDE: &str = "__mj_reg__ = {'filter': [], 'test': [], 'global': []}; \
__mj_dec__ = lambda k, a: (lambda fn: (__mj_reg__[k].append((a, fn)), fn)[1]) if a is None or isinstance(a, str) else (__mj_reg__[k].append((None, a)), a)[1]; \
template_filter = lambda a=None: __mj_dec__('filter', a); \
template_test = lambda a=None: __mj_dec__('test', a); \
template_global = lambda a=None: __mj_dec__('global', a)\n";
pub(super) const PRELUDE_LINES: u32 = 1;

/// What a Python function is registered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Filter,
    Test,
    Global,
}

impl ExportKind {
    const ALL: [Self; 3] = [Self::Filter, Self::Test, Self::Global];

    pub(super) fn key(self) -> &'static str {
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
    pub(super) index: usize,
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
