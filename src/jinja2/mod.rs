//! Compatibility with Python Jinja2, so templates written for it render the
//! same under MiniJinja.
//!
//! MiniJinja implements the Jinja2 language but not Python's object model:
//! values are not Python objects, and output and escaping follow MiniJinja's
//! own conventions. This layer closes the gaps that MiniJinja's extension
//! points allow:
//!
//! * Python methods on any value (`name.upper()`, `"{:,}".format(n)`,
//!   `d.items()`) run in Monty, with Python's exact semantics ([`methods`]).
//! * `|format` uses Python's `%` operator, as Jinja2's does.
//! * Output prints values as Python's `str()` does and escapes HTML the
//!   way MarkupSafe does ([`output`]).
//! * Jinja2 built-ins MiniJinja lacks: filters written in Python
//!   ([`BUILTINS`]), plus `forceescape`, `xmlattr`, `cycler` and `joiner`.
//!
//! What no extension point reaches is documented in the README: chiefly the
//! `%` operator on strings and in-place mutation of values.

mod methods;
mod objects;
mod output;

use std::sync::Arc;

use minijinja::Environment;

use crate::inline::Inline;

/// Jinja2 built-in filters that MiniJinja lacks and that are plain text
/// processing, implemented in Python and registered Jinja2-style.
pub(crate) const BUILTINS: &str = include_str!("builtins.py");
/// Script name of [`BUILTINS`] in tracebacks.
pub(crate) const BUILTINS_SCRIPT: &str = "<jinja2 builtins>";

/// Installs everything except [`BUILTINS`], which is a regular module the
/// caller installs alongside the user's own.
pub(crate) fn install(env: &mut Environment<'_>, inline: &Arc<Inline>) {
    methods::install(env, inline);
    output::install(env);
    objects::install(env);
}
