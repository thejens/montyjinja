//! Per-render memoization of `pure=True` functions.
//!
//! `tick` below deliberately lies about being pure: it counts its own
//! calls, which makes cache hits observable. `PerRender` isolation lets the
//! count accumulate across calls in one render.

use minijinja::Environment;
use montyjinja::{Config, Isolation, LoadError, MontyJinja};

const MODULE: &str = r#"
calls = []

@template_global(pure=True)
def tick(x, **kw):
    calls.append(x)
    return len(calls)

@template_filter('tock', pure=True)
def _tock(x):
    calls.append(x)
    return len(calls)

@template_global
def impure(x):
    calls.append(x)
    return len(calls)

@template_global(pure=True)
def fragile(x):
    calls.append(x)
    if len(calls) == 1:
        raise ValueError('first call fails')
    return len(calls)
"#;

fn env(memo_capacity: usize) -> Environment<'static> {
    let mut monty = MontyJinja::new(Config {
        isolation: Isolation::PerRender,
        memo_capacity,
        ..Config::default()
    });
    monty.add_module("memo.py", MODULE).unwrap();
    let mut env = Environment::new();
    monty.install(&mut env);
    env
}

#[test]
fn pure_flag_is_reported_per_export() {
    let mut monty = MontyJinja::default();
    let module = monty.add_module("memo.py", MODULE).unwrap();
    let pure: Vec<_> = module
        .exports()
        .iter()
        .map(|e| (e.name.as_str(), e.pure))
        .collect();
    assert_eq!(
        pure,
        [
            ("tock", true),
            ("tick", true),
            ("impure", false),
            ("fragile", true)
        ]
    );
}

#[test]
fn repeated_pure_calls_reuse_the_result_within_a_render() {
    let env = env(1_024);
    let tpl = "{{ tick(1) }} {{ tick(1) }} {{ tick(2) }} {{ tick(1) }} {{ 'a'|tock }} {{ 'a'|tock }}";
    assert_eq!(env.render_str(tpl, ()).unwrap(), "1 1 2 1 3 3");
    // A new render starts with an empty cache.
    assert_eq!(env.render_str(tpl, ()).unwrap(), "1 1 2 1 3 3");
    let tpl = "{{ impure(1) }} {{ impure(1) }}";
    assert_eq!(env.render_str(tpl, ()).unwrap(), "1 2");
}

#[test]
fn keys_distinguish_python_types_and_keyword_arguments() {
    let env = env(1_024);
    // 1, 1.0 and True are equal in Python but not interchangeable.
    let tpl = "{{ tick(1) }} {{ tick(1.0) }} {{ tick(true) }} {{ tick(1, a=1) }} {{ tick(1, a=2) }} {{ tick(1, a=1) }}";
    assert_eq!(env.render_str(tpl, ()).unwrap(), "1 2 3 4 5 4");
}

#[test]
fn exceptions_are_not_cached() {
    let env = env(1_024);
    assert!(env.render_str("{{ fragile(1) }}", ()).is_err());
    // The failed call discarded its session, so `calls` restarts; the
    // retry runs instead of replaying the error.
    let tpl = "{% set _ = tick(0) %}{{ fragile(1) }} {{ fragile(1) }}";
    assert_eq!(env.render_str(tpl, ()).unwrap(), "2 2");
}

#[test]
fn capacity_bounds_the_cache() {
    let tpl = "{{ tick(1) }} {{ tick(2) }} {{ tick(1) }} {{ tick(2) }}";
    // Only the first result fits; the second is recomputed every time.
    assert_eq!(env(1).render_str(tpl, ()).unwrap(), "1 2 1 3");
    assert_eq!(env(0).render_str(tpl, ()).unwrap(), "1 2 3 4");
}

#[test]
fn pure_must_be_a_bool() {
    let err = MontyJinja::default()
        .add_module(
            "bad.py",
            "@template_filter(pure='yes')\ndef f(x):\n    return x\n",
        )
        .unwrap_err();
    assert!(matches!(err, LoadError::Invalid(_)), "{err}");
}
