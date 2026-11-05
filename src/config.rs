//! Sandbox configuration shared by modules and inline snippets.

use std::time::Duration;

use monty_types::{OsPolicy, ResourceLimits};

/// Sandbox limits and execution strategy applied to every Python invocation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Memory, time and recursion limits per invocation.
    pub limits: ResourceLimits,
    /// Clock, timezone, randomness and sleep behaviour inside the sandbox.
    pub os_policy: OsPolicy,
    /// Maximum bytes a single invocation may `print()`.
    pub max_output_bytes: usize,
    /// Maximum host round-trips (callbacks + name lookups) per invocation.
    /// Monty stores but does not enforce `max_suspensions` for in-process
    /// hosts, so this crate counts them itself.
    pub max_host_calls: usize,
    /// How a module's top level is made available to each call.
    pub module_strategy: ModuleStrategy,
    /// How long module state lives: one call, or one render.
    pub isolation: Isolation,
    /// Maximum results of `pure=True` functions memoized per render, across
    /// all modules. Once reached, further results are computed but not
    /// stored. `0` disables memoization.
    pub memo_capacity: usize,
    /// Make templates written for Python Jinja2 render the same: Python
    /// methods on values, Python-style printing and HTML escaping, and the
    /// Jinja2 built-ins MiniJinja lacks. Replaces the environment's
    /// formatter and unknown-method callback.
    pub jinja2_compat: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limits: ResourceLimits {
                max_memory: Some(32 * 1024 * 1024),
                max_feed_duration: Some(Duration::from_millis(250)),
                max_recursion_depth: 200,
                ..ResourceLimits::default()
            },
            os_policy: OsPolicy::default(),
            max_output_bytes: 1024 * 1024,
            max_host_calls: 1_000,
            module_strategy: ModuleStrategy::default(),
            isolation: Isolation::default(),
            memo_capacity: 1_024,
            jinja2_compat: true,
        }
    }
}

/// Every call starts from a pristine module state; this picks how that
/// state is produced. Both are fully isolated — they differ only in cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleStrategy {
    /// Run the module's top level once at load, snapshot the interpreter,
    /// and restore the snapshot for each call. Cost scales with the size of
    /// the module's heap (roughly 8µs empty, 40µs for a 2000-entry dict).
    /// Best when the top level computes tables, compiles regexes, etc.
    ///
    /// A module whose interpreter state cannot be snapshotted (Monty's
    /// serialization does not cover everything, e.g. some imported modules)
    /// falls back to `Reexecute` automatically.
    ///
    /// The default: its worst case is about 2x slower than `Reexecute`, while
    /// `Reexecute`'s worst case is unbounded.
    #[default]
    Snapshot,
    /// Re-run the module's top level on every call. Cost scales with the
    /// work the top level does (roughly 4µs for a module of plain `def`s).
    /// Best for modules that only define functions.
    Reexecute,
}

/// How long a module's Python state lives once a call has started from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// Every call starts from the pristine module state. A function can
    /// never observe what an earlier call did.
    #[default]
    PerCall,
    /// The first call in a render starts from the pristine state; later
    /// calls in the same render continue from where the previous one left
    /// off, and the state is dropped when the render ends. Nothing crosses
    /// renders, but module globals mutated by one call are visible to the
    /// next, and memory use accumulates against `max_memory` for the whole
    /// render. A call that raises discards the state, so the next call
    /// starts pristine again.
    ///
    /// Avoids restoring the module per call, which dominates the cost of
    /// small functions.
    PerRender,
}
