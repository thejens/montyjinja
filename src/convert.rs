//! Value bridging between MiniJinja's [`Value`] and Monty's [`MontyObject`].
//!
//! Both sides are copied, never shared: Monty values live in a sandbox heap
//! the host cannot reference, so every crossing is a deep conversion. That is
//! the main per-call cost besides interpretation itself, so callers should
//! pass the data a function needs rather than whole contexts.

use std::fmt;
use std::sync::Arc;

use minijinja::value::{Enumerator, Object, ObjectRepr, Tuple, ValueKind};
use minijinja::{Error, ErrorKind, Value};
use monty_types::{MontyObject, ObjectRef};

/// Nesting cap for both directions. MiniJinja values can be self-referential
/// through dynamic objects, and Monty graphs can share sub-objects; the cap
/// turns a cycle into an error instead of a stack overflow.
pub(crate) const MAX_DEPTH: usize = 64;

/// Converts a template value into a Python value.
///
/// Mapping: undefined/none → `None`, numbers → `int`/`float`, strings → `str`
/// (the "safe" marker is dropped — Python sees plain text), sequences and
/// iterables → `list`, maps → `dict`, anything else → its `str()` form.
pub fn to_monty(value: &Value) -> Result<MontyObject, Error> {
    to_monty_at(value, 0)
}

fn to_monty_at(value: &Value, depth: usize) -> Result<MontyObject, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "value nested too deeply to pass into python",
        ));
    }
    Ok(match value.kind() {
        ValueKind::Undefined | ValueKind::None => MontyObject::none(),
        ValueKind::Bool => MontyObject::bool(value.is_true()),
        ValueKind::Number => number_to_monty(value),
        ValueKind::String => MontyObject::string(value.as_str().unwrap_or_default()),
        ValueKind::Bytes => MontyObject::bytes(value.as_bytes().unwrap_or_default()),
        // Jinja tuples (`(1, 2)`, loop unpacking) stay tuples, as in Jinja2.
        ValueKind::Seq if value.is_tuple() => MontyObject::tuple(
            value
                .try_iter()?
                .map(|item| to_monty_at(&item, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        ValueKind::Seq | ValueKind::Iterable => MontyObject::list(
            value
                .try_iter()?
                .map(|item| to_monty_at(&item, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        ValueKind::Map => {
            let mut pairs = Vec::new();
            for key in value.try_iter()? {
                let item = value.get_item(&key)?;
                pairs.push((to_monty_at(&key, depth + 1)?, to_monty_at(&item, depth + 1)?));
            }
            MontyObject::dict(pairs)
        }
        ValueKind::Plain => MontyObject::string(value.to_string()),
        // Invalid values carry a deferred error; surface it rather than
        // smuggling a placeholder into Python.
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("cannot pass {} into python", value.kind()),
            ));
        }
    })
}

fn number_to_monty(value: &Value) -> MontyObject {
    // Checked first because `as_i64` also accepts whole floats: `1.0` must
    // reach Python as a float, not the int `1`.
    if !value.is_integer() {
        return f64::try_from(value.clone()).map_or_else(|_| MontyObject::none(), MontyObject::float);
    }
    if let Some(int) = value.as_i64() {
        return MontyObject::int(int);
    }
    if let Ok(big) = i128::try_from(value.clone()) {
        return MontyObject::bigint(big.into());
    }
    // Only u128 values above i128::MAX are left.
    u128::try_from(value.clone()).map_or_else(|_| MontyObject::none(), |big| MontyObject::bigint(big.into()))
}

/// Python call arguments converted from a MiniJinja call.
pub(crate) struct PyArgs {
    pub(crate) positional: Vec<MontyObject>,
    pub(crate) keyword: Vec<(MontyObject, MontyObject)>,
}

/// Converts MiniJinja call arguments into Python positional and keyword
/// arguments. MiniJinja passes keyword arguments as a trailing map value.
pub(crate) fn split_kwargs(args: &[Value]) -> Result<PyArgs, Error> {
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

/// Defines `__mj_view__(x)`, which pairs a result with how it must reach
/// the template: `(x, None)` for values that convert directly, and
/// `(list(x), repr(x))` for other iterables.
///
/// Monty hands dict views, ranges, iterators and generators to the host as
/// opaque text, so a template could neither loop over nor index them. Jinja2
/// can, and filters returning generators are common, so their items are
/// materialised in Python and the repr is kept for printing. Every line is
/// part of the line count of any prelude that embeds it.
pub(crate) const PY_VIEW_HELPER: &str = "def __mj_view__(x):\n    \
if x is None or isinstance(x, (str, bytes, list, tuple, dict, set, frozenset, int, float, bool)):\n        \
return (x, None)\n    \
try:\n        \
text = repr(x)\n        \
return (list(x), text)\n    \
except TypeError:\n        \
return (x, None)\n";
/// Lines in [`PY_VIEW_HELPER`].
pub(crate) const PY_VIEW_HELPER_LINES: u32 = 8;

/// Converts a `__mj_view__` pair back into a template value.
pub(crate) fn from_monty_result(pair: ObjectRef<'_>) -> Result<Value, Error> {
    let parts = pair.items().unwrap_or_default();
    match parts.as_slice() {
        [items, repr] => match repr.as_str() {
            Some(repr) => {
                let items = from_monty(*items)?.try_iter()?.collect();
                Ok(Value::from_object(PyView {
                    items,
                    repr: repr.to_owned(),
                }))
            }
            None => from_monty(*items),
        },
        _ => Err(Error::new(
            ErrorKind::InvalidOperation,
            "python result is not a (value, view) pair",
        )),
    }
}

/// A Python iterable that is not a list (a dict view, `range`, a
/// generator): iterates and indexes like a sequence, prints as Python would.
#[derive(Debug)]
pub(crate) struct PyView {
    items: Vec<Value>,
    pub(crate) repr: String,
}

impl Object for PyView {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        self.items.get(key.as_usize()?).cloned()
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.items.len())
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.repr)
    }
}

/// Converts a Python value back into a template value.
///
/// Containers map structurally (tuples and sets become sequences; dicts,
/// dataclass instances and namedtuples become maps). Values with no template
/// equivalent — dates, decimals, functions — become their Python `repr()`;
/// Python code that cares about formatting should return a `str`.
pub fn from_monty(value: ObjectRef<'_>) -> Result<Value, Error> {
    from_monty_at(value, 0)
}

fn from_monty_at(value: ObjectRef<'_>, depth: usize) -> Result<Value, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "python value nested too deeply to return to the template",
        ));
    }
    // Bool is checked before int: Monty stores them as distinct nodes, but a
    // future change folding bool into int must not turn `True` into `1`.
    if let Some(flag) = value.as_bool() {
        return Ok(Value::from(flag));
    }
    if let Some(int) = value.as_int() {
        return Ok(Value::from(int));
    }
    if let Some(text) = value.as_str() {
        return Ok(Value::from(text));
    }
    match value.type_name() {
        "NoneType" => return Ok(Value::from(())),
        "float" => return Ok(Value::from(value.as_float().unwrap_or(f64::NAN))),
        // Ints beyond i64 round-trip through their decimal text.
        "int" => {
            let repr = value.py_repr();
            return Ok(repr
                .parse::<i128>()
                .map_or_else(|_| Value::from(repr), Value::from));
        }
        "decimal.Decimal" | "Decimal" => {
            return Ok(Value::from(value.as_decimal().unwrap_or_default()));
        }
        _ => {}
    }
    if let Some(items) = value.items() {
        let items = items
            .into_iter()
            .map(|item| from_monty_at(item, depth + 1))
            .collect::<Result<Vec<_>, _>>()?;
        // Tuples keep their identity so they render as `(1, 2)`, as Jinja2
        // prints them; lists, sets and the like become plain sequences.
        return Ok(if value.type_name() == "tuple" {
            Value::from_object(Tuple::new(items))
        } else {
            Value::from(items)
        });
    }
    if let Some(pairs) = value.pairs() {
        // Insertion order survives: the crate enables MiniJinja's
        // `preserve_order`, matching Python's ordered dicts.
        return pairs
            .into_iter()
            .map(|(key, item)| Ok((from_monty_at(key, depth + 1)?, from_monty_at(item, depth + 1)?)))
            .collect::<Result<Vec<_>, Error>>()
            .map(Value::from_pairs);
    }
    Ok(Value::from(repr_text(value)))
}

/// The Python `repr()` of a value.
///
/// Objects that only exist inside the sandbox (functions, class objects)
/// reach the host as an opaque `repr` node, which `py_repr()` renders as a
/// Rust debug wrapper — `Repr("<function 'f' at 0x..>")`. Unwrap it so
/// templates and name discovery see what Python would print.
pub(crate) fn repr_text(value: ObjectRef<'_>) -> String {
    let repr = value.py_repr();
    if value.type_name() != "repr" {
        return repr;
    }
    repr.strip_prefix("Repr(\"")
        .and_then(|rest| rest.strip_suffix("\")"))
        .map(|inner| inner.replace("\\\"", "\"").replace("\\\\", "\\"))
        .unwrap_or(repr)
}
