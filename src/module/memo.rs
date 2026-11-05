//! Per-render memoization of functions registered with `pure=True`.
//!
//! A pure function's result depends only on its arguments, so within one
//! render a repeated call can reuse the earlier result instead of converting
//! the arguments and running Python again. The cache is a MiniJinja
//! render-local extension, so it is dropped with the render.

use std::collections::HashMap;

use minijinja::value::ValueKind;
use minijinja::{State, Value};

use super::{Export, ExportKind};
use crate::convert::MAX_DEPTH;

/// Identifies a call: the callee plus a fingerprint of its arguments.
#[derive(PartialEq, Eq, Hash)]
pub(super) struct MemoKey {
    module: usize,
    kind: ExportKind,
    index: usize,
    args: Vec<u8>,
}

impl MemoKey {
    /// `None` if an argument cannot be fingerprinted (an unsupported kind,
    /// or nested too deeply); the call then simply runs uncached.
    pub(super) fn new(module: usize, export: &Export, args: &[Value]) -> Option<Self> {
        let mut fingerprint = Vec::new();
        for arg in args {
            write_value(&mut fingerprint, arg, 0)?;
        }
        Some(Self {
            module,
            kind: export.kind,
            index: export.index,
            args: fingerprint,
        })
    }
}

#[derive(Default)]
struct Memo(HashMap<MemoKey, Value>);

pub(super) fn lookup(state: &State<'_, '_>, key: &MemoKey) -> Option<Value> {
    state.get_extension::<Memo>()?.0.get(key).cloned()
}

/// Stores `value` unless the render already holds `capacity` results. Once
/// full, the cache keeps its earliest entries rather than evicting: a render
/// is short-lived, and the first calls in a loop are as likely to repeat as
/// the last.
pub(super) fn store(state: &mut State<'_, '_>, key: MemoKey, value: Value, capacity: usize) {
    if capacity == 0 {
        return;
    }
    let memo = state.get_or_insert_extension_with(Memo::default);
    if memo.0.len() < capacity {
        memo.0.insert(key, value);
    }
}

// Fingerprint tags. Every encoded value starts with one, and `END` closes a
// container; since no value starts with `END`, the encoding is unambiguous.
const NONE: u8 = 0;
const BOOL: u8 = 1;
const INT: u8 = 2;
const UINT: u8 = 3;
const FLOAT: u8 = 4;
const STR: u8 = 5;
const BYTES: u8 = 6;
const SEQ: u8 = 7;
const MAP: u8 = 8;
const KWARGS: u8 = 9;
const END: u8 = 0xff;

/// Appends a fingerprint of `value` that is injective over everything
/// `to_monty` distinguishes, so equal fingerprints guarantee Python receives
/// identical input.
///
/// Built from template values rather than from the converted Python
/// objects so a cache hit skips conversion entirely. It deliberately does
/// not use `Value`'s own equality, under which `1 == 1.0`: a pure function
/// may still treat those differently (`str(x)`), so every number carries its
/// type. Distinctions `to_monty` erases (undefined vs none, safe vs plain
/// strings, sequence vs iterable) are erased here too.
fn write_value(out: &mut Vec<u8>, value: &Value, depth: usize) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value.kind() {
        ValueKind::Undefined | ValueKind::None => out.push(NONE),
        ValueKind::Bool => out.extend([BOOL, u8::from(value.is_true())]),
        ValueKind::Number if value.is_integer() => match i128::try_from(value.clone()) {
            Ok(int) => {
                out.push(INT);
                out.extend(int.to_le_bytes());
            }
            Err(_) => {
                out.push(UINT);
                out.extend(u128::try_from(value.clone()).ok()?.to_le_bytes());
            }
        },
        ValueKind::Number => {
            out.push(FLOAT);
            out.extend(f64::try_from(value.clone()).ok()?.to_bits().to_le_bytes());
        }
        ValueKind::String => write_text(out, STR, value.as_str()?.as_bytes()),
        ValueKind::Bytes => write_text(out, BYTES, value.as_bytes()?),
        // `to_monty` passes these as their display string.
        ValueKind::Plain => write_text(out, STR, value.to_string().as_bytes()),
        ValueKind::Seq | ValueKind::Iterable => {
            out.push(SEQ);
            for item in value.try_iter().ok()? {
                write_value(out, &item, depth + 1)?;
            }
            out.push(END);
        }
        ValueKind::Map => {
            out.push(if value.is_kwargs() { KWARGS } else { MAP });
            for key in value.try_iter().ok()? {
                let item = value.get_item(&key).ok()?;
                write_value(out, &key, depth + 1)?;
                write_value(out, &item, depth + 1)?;
            }
            out.push(END);
        }
        _ => return None,
    }
    Some(())
}

fn write_text(out: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    out.push(tag);
    out.extend(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    out.extend(bytes);
}

#[cfg(test)]
mod tests {
    use minijinja::Value;
    use minijinja::value::Kwargs;

    use super::write_value;

    fn fingerprint(value: Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_value(&mut out, &value, 0).unwrap();
        out
    }

    #[test]
    fn fingerprints_distinguish_python_types() {
        let values = [
            Value::from(1),
            Value::from(1.0),
            Value::from(true),
            Value::from("1"),
            Value::from(vec![Value::from(1)]),
            Value::from(vec![Value::from(1), Value::from(1)]),
            Value::from(vec![vec![Value::from(1)], vec![]]),
            Value::from(vec![vec![], vec![Value::from(1)]]),
            Value::from_iter([("a", 1)]),
            Value::from(Kwargs::from_iter([("a", Value::from(1))])),
        ];
        for (i, a) in values.iter().enumerate() {
            for b in &values[i + 1..] {
                assert_ne!(fingerprint(a.clone()), fingerprint(b.clone()), "{a:?} vs {b:?}");
            }
        }
        assert_eq!(
            fingerprint(Value::from_safe_string("x".into())),
            fingerprint(Value::from("x"))
        );
    }
}
