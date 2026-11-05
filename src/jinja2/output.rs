//! How values are printed and escaped, matching Python Jinja2.
//!
//! Jinja2 prints `str(value)` and, under HTML autoescaping, escapes it with
//! MarkupSafe. MiniJinja's defaults differ in two visible ways: its HTML
//! escaping also escapes `/` and uses other entities for quotes, and its
//! float formatting is Rust's (`1e20` prints as `100000000000000000000.0`,
//! Python prints `1e+20`). The formatter and escape filters here follow
//! Python instead.

use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, Error, ErrorKind, Output, State, Value};

use monty_types::MontyObject;

use super::objects::PyRange;
use crate::convert::{MAX_DEPTH, PyView};

pub(super) fn install(env: &mut Environment<'_>) {
    env.set_formatter(format_value);
    env.add_filter("escape", escape);
    env.add_filter("e", escape);
    env.add_filter("forceescape", |value: Value| {
        Value::from_safe_string(escape_html(&python_str(&value)))
    });
    env.add_filter("xmlattr", xmlattr);
}

/// MarkupSafe's escaping: exactly `& < > " '`, with numeric entities for
/// the quotes.
pub(super) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Python's `str(value)`: strings as they are, everything else as
/// [`python_repr`].
fn python_str(value: &Value) -> String {
    match value.as_str() {
        Some(text) if value.kind() == ValueKind::String => text.to_owned(),
        _ => python_repr(value, 0),
    }
}

/// Python's `repr(value)`.
///
/// Built here rather than by converting to Python and asking Monty, for two
/// reasons: host objects (loops, namespaces, views, ranges) render
/// themselves, and Monty's float repr differs from CPython's for small
/// exponents (`1e-7` versus `1e-07`). Strings use Monty's string repr,
/// which follows CPython's quoting and escaping rules.
fn python_repr(value: &Value, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return "...".to_owned();
    }
    let join = |items: Vec<String>| items.join(", ");
    match value.kind() {
        ValueKind::None => "None".to_owned(),
        ValueKind::Bool => if value.is_true() { "True" } else { "False" }.to_owned(),
        ValueKind::Number if !value.is_integer() => {
            f64::try_from(value.clone()).map_or_else(|_| value.to_string(), python_float_repr)
        }
        ValueKind::String => MontyObject::string(value.as_str().unwrap_or_default()).py_repr(),
        ValueKind::Bytes => MontyObject::bytes(value.as_bytes().unwrap_or_default()).py_repr(),
        ValueKind::Seq if !renders_itself(value) => {
            let items: Vec<String> = value
                .try_iter()
                .map(|iter| iter.map(|item| python_repr(&item, depth + 1)).collect())
                .unwrap_or_default();
            match (value.is_tuple(), items.len()) {
                (true, 1) => format!("({},)", items[0]),
                (true, _) => format!("({})", join(items)),
                (false, _) => format!("[{}]", join(items)),
            }
        }
        ValueKind::Map => {
            let items: Vec<String> = value
                .try_iter()
                .map(|keys| {
                    keys.map(|key| {
                        let item = value.get_item(&key).unwrap_or_default();
                        format!(
                            "{}: {}",
                            python_repr(&key, depth + 1),
                            python_repr(&item, depth + 1)
                        )
                    })
                    .collect()
                })
                .unwrap_or_default();
            format!("{{{}}}", join(items))
        }
        // Integers, host objects and anything else render themselves.
        _ => value.to_string(),
    }
}

/// Sequences that already print the way Python does: views of Python
/// iterables and `range`. MiniJinja's own lists are objects too, so this
/// names the exceptions rather than testing for objects in general.
fn renders_itself(value: &Value) -> bool {
    value.downcast_object_ref::<PyView>().is_some() || value.downcast_object_ref::<PyRange>().is_some()
}

/// CPython's `repr(float)`: the shortest digits that round-trip, in fixed
/// notation for decimal exponents from -4 to 15 and scientific notation
/// (signed exponent of at least two digits) otherwise.
fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    // Rust's `{:e}` without a precision yields the shortest round-trip
    // digits, e.g. "-1.2345e3".
    let sci = format!("{value:e}");
    let (mantissa, exponent) = sci.split_once('e').expect("{:e} always has an exponent");
    let exponent: i32 = exponent.parse().expect("{:e} exponent is an integer");
    let (sign, mantissa) = mantissa.strip_prefix('-').map_or(("", mantissa), |m| ("-", m));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{first}{fraction}e{exp_sign}{:02}", exponent.unsigned_abs())
    }
}

fn format_value(out: &mut Output<'_>, state: &mut State<'_, '_>, value: &Value) -> Result<(), Error> {
    let escape = match state.auto_escape() {
        AutoEscape::None => false,
        AutoEscape::Html => !value.is_safe(),
        // JSON and custom escaping keep MiniJinja's behaviour.
        _ => return minijinja::escape_formatter(out, state, value),
    };
    // Undefined values print as nothing, or fail under strict undefined
    // behaviour; MiniJinja's formatter implements both.
    if value.is_undefined() {
        return minijinja::escape_formatter(out, state, value);
    }
    let text = python_str(value);
    let written = if escape {
        out.write_str(&escape_html(&text))
    } else {
        out.write_str(&text)
    };
    written.map_err(|_| Error::new(ErrorKind::WriteFailure, "failed to write output"))
}

/// Jinja2's `escape`: escapes unless the value is already safe.
fn escape(value: Value) -> Value {
    if value.is_safe() {
        value
    } else {
        Value::from_safe_string(escape_html(&python_str(&value)))
    }
}

/// Jinja2's `xmlattr`: renders a mapping as escaped XML/HTML attributes,
/// skipping `None` and undefined values. Keys containing whitespace, `/`,
/// `>` or `=` are rejected, as Jinja2 does, since they could break out of
/// the attribute.
fn xmlattr(state: &State<'_, '_>, map: Value, autospace: Option<bool>) -> Result<Value, Error> {
    if map.kind() != ValueKind::Map {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "xmlattr expects a mapping",
        ));
    }
    let mut parts = Vec::new();
    for key in map.try_iter()? {
        let value = map.get_item(&key)?;
        if value.is_none() || value.is_undefined() {
            continue;
        }
        let key = python_str(&key);
        if key.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '/' | '>' | '=')) {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("invalid character in attribute name: {key:?}"),
            ));
        }
        parts.push(format!(
            "{}=\"{}\"",
            escape_html(&key),
            escape_html(&python_str(&value))
        ));
    }
    let mut rv = parts.join(" ");
    if autospace.unwrap_or(true) && !rv.is_empty() {
        rv.insert(0, ' ');
    }
    Ok(if matches!(state.auto_escape(), AutoEscape::None) {
        Value::from(rv)
    } else {
        Value::from_safe_string(rv)
    })
}

#[cfg(test)]
mod tests {
    use minijinja::Value;

    use super::{escape_html, python_float_repr, python_str};

    #[test]
    fn escapes_like_markupsafe() {
        assert_eq!(
            escape_html(r#"<a href="/x">'&'</a>"#),
            "&lt;a href=&#34;/x&#34;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn prints_floats_and_data_like_python() {
        assert_eq!(python_str(&Value::from(1e20)), "1e+20");
        assert_eq!(python_str(&Value::from(3.0)), "3.0");
        assert_eq!(
            python_str(&Value::from(vec![Value::from(1e-7), Value::from(())])),
            "[1e-07, None]"
        );
        assert_eq!(python_str(&Value::from("it's")), "it's");
    }

    #[test]
    fn float_repr_matches_cpython() {
        for (value, expected) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (1234.5, "1234.5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (-1.25e-10, "-1.25e-10"),
            (1e100, "1e+100"),
            (0.1 + 0.2, "0.30000000000000004"),
            (f64::NAN, "nan"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(python_float_repr(value), expected, "{value:e}");
        }
    }
}
