//! # montyjinja
//!
//! Python-powered templating for [MiniJinja](https://docs.rs/minijinja),
//! executed by [Monty](https://github.com/pydantic/monty) — a sandboxed
//! Python interpreter written in Rust. No CPython, no subprocess, no FFI.
//!
//! Three ways to bring Python into templates:
//!
//! 1. **Modules** — decorate functions with `@template_filter`,
//!    `@template_test` or `@template_global` and they become MiniJinja
//!    filters, tests and global functions.
//! 2. **`py()`** — evaluate an inline snippet:
//!    `{{ py("sum(o['total'] for o in orders)") }}`.
//! 3. **`py_block`** — run a block and emit what it prints:
//!    `{% call py_block() %}for o in orders: print(o['id']){% endcall %}`.
//!
//! Python code can call back into the template engine with `render(name,
//! **ctx)`, `render_string(source, **ctx)` and `apply_filter(name, value,
//! *args, **kwargs)`.
//!
//! ```
//! use minijinja::{Environment, context};
//! use montyjinja::MontyJinja;
//!
//! let mut monty = MontyJinja::default();
//! monty.add_module("filters.py", r#"
//! @template_filter
//! def initials(name):
//!     return ''.join(part[0].upper() for part in name.split())
//! "#).unwrap();
//!
//! let mut env = Environment::new();
//! monty.install(&mut env);
//! let out = env.render_str("{{ who|initials }}", context! { who => "ada lovelace" }).unwrap();
//! assert_eq!(out, "AL");
//! ```

mod convert;
mod error;
mod inline;
mod module;
mod runtime;

use std::sync::Arc;

use minijinja::Environment;

pub use convert::{from_monty, to_monty};
pub use error::{LoadError, PythonError};
pub use module::{Export, ExportKind, PythonModule};
pub use monty_types::{OsPolicy, ResourceLimits};
pub use runtime::Config;

/// Collects Python modules and installs them, plus `py()` and `py_block`,
/// into MiniJinja environments.
#[derive(Default)]
pub struct MontyJinja {
    config: Config,
    modules: Vec<PythonModule>,
}

impl MontyJinja {
    /// Uses `config` for every module added afterwards and for inline code.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            modules: Vec::new(),
        }
    }

    /// Loads a module of decorated functions. `script` names it in
    /// tracebacks. The module's top level runs once now, to validate it and
    /// collect its exports, and again (sandboxed, from scratch) on each call.
    pub fn add_module(&mut self, script: &str, source: &str) -> Result<&PythonModule, LoadError> {
        let module = PythonModule::load(script, source, self.config.clone())?;
        self.modules.push(module);
        Ok(self.modules.last().expect("just pushed"))
    }

    /// The loaded modules, in load order.
    pub fn modules(&self) -> &[PythonModule] {
        &self.modules
    }

    /// Registers all module exports plus the `py` and `py_block` globals.
    /// Later modules override earlier ones on name clashes, as MiniJinja's
    /// own registration does.
    pub fn install(&self, env: &mut Environment<'_>) {
        for module in &self.modules {
            module.install(env);
        }
        Arc::new(inline::Inline::new(self.config.clone())).install(env);
    }
}
