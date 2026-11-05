//! Python methods on template values, and Python's `%` for `|format`.
//!
//! MiniJinja implements a few methods natively; every other call reaches its
//! unknown-method callback, answered here by running the method in Monty on
//! a copy of the value. Results therefore follow Python's semantics exactly,
//! including format specs, `str.split` edge cases and dict views.

use std::sync::Arc;

use minijinja::value::{Rest, ValueKind, ValueOrKwargs};
use minijinja::{Environment, Error, ErrorKind, State, Value};
use monty_types::{ExcType, MontyObject};

use crate::convert::{PY_VIEW_HELPER, from_monty, from_monty_result, split_kwargs, to_monty};
use crate::inline::Inline;
use crate::runtime::NameResolution;

pub(super) fn install(env: &mut Environment<'_>, inline: &Arc<Inline>) {
    let methods = Arc::clone(inline);
    env.set_unknown_method_callback(move |state, value, method, args| {
        call_method(&methods, state, value, method, args)
    });
    let format = Arc::clone(inline);
    env.add_filter(
        "format",
        move |state: &mut State, value: Value, args: Rest<ValueOrKwargs>| {
            format_filter(&format, state, &value, &args.into_values())
        },
    );
}

fn call_method(
    inline: &Inline,
    state: &mut State<'_, '_>,
    value: &Value,
    method: &str,
    args: &[Value],
) -> Result<Value, Error> {
    let unknown = |detail: String| Error::new(ErrorKind::UnknownMethod, detail);
    // The parser only produces identifiers, but the name is spliced into
    // source code, so this is checked rather than assumed.
    let is_identifier = method.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !method.starts_with(|c: char| c.is_ascii_digit());
    // Values with no Python equivalent (macros, host objects, undefined)
    // keep MiniJinja's own error.
    let receiver = match value.kind() {
        ValueKind::Undefined | ValueKind::Plain | ValueKind::Invalid => None,
        _ if is_identifier => to_monty(value).ok(),
        _ => None,
    };
    let Some(receiver) = receiver else {
        return Err(unknown(format!("{} has no method named {method}", value.kind())));
    };
    let args = split_kwargs(args)?;
    // Lists and dicts can be changed in place by methods like `append`. A
    // copy is all Python sees, so such a change would be silently lost;
    // comparing reprs before and after turns that into an error instead.
    let mutable = matches!(value.kind(), ValueKind::Seq | ValueKind::Map) && !value.is_tuple();
    let call = format!("__mj_recv__.{method}(*__mj_args__, **__mj_kwargs__)");
    // The result goes through `__mj_view__`, so `d.items()` or `s.split()`
    // results that Monty would return as opaque views stay iterable.
    let source = if mutable {
        format!(
            "{PY_VIEW_HELPER}__mj_before__ = repr(__mj_recv__)\n__mj_out__ = {call}\n\
             (__mj_view__(__mj_out__), repr(__mj_recv__) != __mj_before__)"
        )
    } else {
        format!("{PY_VIEW_HELPER}(__mj_view__({call}), False)")
    };
    let inputs = vec![
        ("__mj_recv__".to_owned(), receiver),
        ("__mj_args__".to_owned(), MontyObject::list(args.positional)),
        ("__mj_kwargs__".to_owned(), MontyObject::dict(args.keyword)),
    ];
    let outcome = match inline.evaluate(state, &source, inputs, NameResolution::Strict) {
        Ok(outcome) => outcome,
        Err(err) if err.exception().exc_type() == ExcType::AttributeError => {
            return Err(unknown(err.summary()));
        }
        Err(err) => return Err(err.into_template_error(&format!("python method .{method}()"))),
    };
    let parts = outcome.value.as_ref().items().unwrap_or_default();
    let [result, mutated] = parts.as_slice() else {
        unreachable!("the method snippet always evaluates to a pair");
    };
    if mutated.as_bool() == Some(true) {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                ".{method}() changes the {} in place, but template values are immutable and \
                 the change would be lost; build a new value instead (e.g. `items + [x]`)",
                value.kind()
            ),
        ));
    }
    from_monty_result(*result)
}

/// Jinja2's `format` filter: `value % (kwargs or args)`, with Python's
/// printf-style semantics rather than MiniJinja's approximation.
fn format_filter(
    inline: &Inline,
    state: &mut State<'_, '_>,
    value: &Value,
    args: &[Value],
) -> Result<Value, Error> {
    let args = split_kwargs(args)?;
    if !args.positional.is_empty() && !args.keyword.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "format can't handle positional and keyword arguments at the same time",
        ));
    }
    let inputs = vec![
        ("__mj_fmt__".to_owned(), MontyObject::string(value.to_string())),
        ("__mj_args__".to_owned(), MontyObject::tuple(args.positional)),
        ("__mj_kwargs__".to_owned(), MontyObject::dict(args.keyword)),
    ];
    let source = "__mj_fmt__ % (__mj_kwargs__ or __mj_args__)";
    let outcome = inline
        .evaluate(state, source, inputs, NameResolution::Strict)
        .map_err(|err| err.into_template_error("format filter"))?;
    from_monty(outcome.value.as_ref())
}
