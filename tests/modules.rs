//! Python modules registered as filters, tests and globals.

use minijinja::{AutoEscape, Environment, context};
use montyjinja::{Config, ExportKind, Isolation, LoadError, ModuleStrategy, MontyJinja, ResourceLimits};

const FILTERS: &str = r#"
@template_filter
def slugify(text):
    return '-'.join(text.lower().split())

@template_filter('shout')
def _shout(text, times=1):
    return text.upper() + '!' * times

@template_test
def prime(n):
    return n > 1 and all(n % d for d in range(2, n))

@template_global()
def stats(values):
    total = sum(values)
    return {'n': len(values), 'total': total, 'mean': total / len(values)}

@template_filter
def explode(x):
    return 1 / x
"#;

fn env_with(source: &str) -> Environment<'static> {
    env_with_strategy(source, ModuleStrategy::default())
}

fn env_with_strategy(source: &str, module_strategy: ModuleStrategy) -> Environment<'static> {
    env_with_config(
        source,
        Config {
            module_strategy,
            ..Config::default()
        },
    )
}

fn env_with_config(source: &str, config: Config) -> Environment<'static> {
    let mut monty = MontyJinja::new(config);
    monty.add_module("filters.py", source).unwrap();
    let mut env = Environment::new();
    monty.install(&mut env);
    env
}

#[test]
fn discovers_exports_by_kind_and_name() {
    let mut monty = MontyJinja::default();
    let module = monty.add_module("filters.py", FILTERS).unwrap();
    let names: Vec<_> = module
        .exports()
        .iter()
        .map(|e| (e.kind, e.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            (ExportKind::Filter, "slugify"),
            (ExportKind::Filter, "shout"),
            (ExportKind::Filter, "explode"),
            (ExportKind::Test, "prime"),
            (ExportKind::Global, "stats"),
        ]
    );
}

#[test]
fn filters_tests_and_globals_render() {
    let env = env_with(FILTERS);
    let tpl = r#"{{ title|slugify }} {{ "hi"|shout(times=3) }} {{ 7 is prime }} {{ 8 is prime }}
{%- set s = stats([2, 4, 6]) %} {{ s.n }}/{{ s.total }}/{{ s.mean }}
{%- for n in range(10) if n is prime %} {{ n }}{% endfor %}"#;
    let out = env
        .render_str(tpl, context! { title => "Hello Big World" })
        .unwrap();
    assert_eq!(out, "hello-big-world HI!!! True False 3/12/4.0 2 3 5 7");
}

#[test]
fn python_tracebacks_point_at_user_lines() {
    let env = env_with(FILTERS);
    let err = env.render_str("{{ 0|explode }}", ()).unwrap_err();
    assert!(err.to_string().contains("ZeroDivisionError"), "{err}");
    let source = std::error::Error::source(&err)
        .expect("python error attached")
        .to_string();
    // `return 1 / x` is line 21 of FILTERS (which starts with a newline).
    assert!(
        source.contains("File \"filters.py\", line 21, in explode"),
        "{source}"
    );
    assert!(!source.contains("__mj_"), "internal frames leak: {source}");
}

#[test]
fn calls_do_not_share_state() {
    let env = env_with(
        r#"
seen = []

@template_global
def remember(x):
    seen.append(x)
    return len(seen)
"#,
    );
    let out = env
        .render_str("{{ remember(1) }}{{ remember(2) }}{{ remember(3) }}", ())
        .unwrap();
    assert_eq!(out, "111");
}

#[test]
fn python_can_call_back_into_the_template_engine() {
    let mut env = env_with(
        r#"
@template_global
def card(user):
    return render('card.html', name=apply_filter('title', user['name']), role=user['role'])

@template_global
def inline(n):
    return render_string('{% for i in range(n) %}*{% endfor %}', n=n)

@template_filter
def chain(text):
    return apply_filter('shout', text, times=2)

@template_filter
def shout(text, times=1):
    return text.upper() + '!' * times
"#,
    );
    env.add_template("card.html", "<b>{{ name }}</b> ({{ role }})")
        .unwrap();
    let out = env
        .render_str(
            "{{ card(u) }} {{ inline(3) }} {{ 'hey'|chain }}",
            context! { u => context! { name => "ada lovelace", role => "admin" } },
        )
        .unwrap();
    assert_eq!(out, "<b>Ada Lovelace</b> (admin) *** HEY!!");
}

#[test]
fn host_errors_are_catchable_python_exceptions() {
    let env = env_with(
        r#"
@template_global
def safe_render(name):
    try:
        return render(name)
    except RuntimeError as e:
        return 'missing'
"#,
    );
    assert_eq!(
        env.render_str("{{ safe_render('nope.html') }}", ()).unwrap(),
        "missing"
    );
}

#[test]
fn modules_do_not_see_template_variables() {
    let env = env_with("@template_global\ndef leak():\n    return secret\n");
    let err = env
        .render_str("{{ leak() }}", context! { secret => 42 })
        .unwrap_err();
    assert!(err.to_string().contains("NameError"), "{err}");
}

#[test]
fn runaway_code_is_stopped_by_limits() {
    let config = Config {
        limits: ResourceLimits {
            max_feed_duration: Some(std::time::Duration::from_millis(50)),
            max_memory: Some(4 * 1024 * 1024),
            ..Config::default().limits
        },
        ..Config::default()
    };
    let mut monty = MontyJinja::new(config);
    monty
        .add_module(
            "evil.py",
            "@template_global\ndef spin():\n    while True:\n        pass\n\n\
             @template_global\ndef hog():\n    return 'x' * (64 * 1024 * 1024)\n",
        )
        .unwrap();
    let mut env = Environment::new();
    monty.install(&mut env);

    let start = std::time::Instant::now();
    let err = env.render_str("{{ spin() }}", ()).unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(err.to_string().contains("time limit"), "{err}");

    let err = env.render_str("{{ hog() }}", ()).unwrap_err();
    assert!(err.to_string().contains("MemoryError"), "{err}");
}

#[test]
fn sandbox_has_no_filesystem_or_environment() {
    let env = env_with(
        "import os\n\n@template_global\ndef home():\n    return os.getenv('HOME')\n\n\
         @template_global\ndef passwd():\n    return open('/etc/passwd').read()\n",
    );
    for call in ["{{ home() }}", "{{ passwd() }}"] {
        let err = env.render_str(call, ()).unwrap_err();
        assert!(err.to_string().contains("python global"), "{call}: {err}");
    }
}

#[test]
fn load_errors_are_reported() {
    let mut monty = MontyJinja::default();
    let err = monty
        .add_module("bad.py", "def broken(:\n    pass\n")
        .unwrap_err();
    assert!(matches!(err, LoadError::Python(_)), "{err}");

    let err = monty
        .add_module("anon.py", "anon = template_filter(lambda x: x)\n")
        .unwrap_err();
    assert!(matches!(err, LoadError::Invalid(_)), "{err}");

    let err = monty
        .add_module("raises.py", "\n\nraise ValueError('nope')\n")
        .unwrap_err();
    let LoadError::Python(py) = err else {
        panic!("{err}")
    };
    assert!(py.traceback().contains("line 3"), "{}", py.traceback());
}

#[test]
fn results_follow_autoescaping() {
    let mut env = env_with("@template_filter\ndef bold(x):\n    return '<b>' + x + '</b>'\n");
    env.set_auto_escape_callback(|_| AutoEscape::Html);
    env.add_template("t.html", "{{ 'a'|bold }}").unwrap();
    let out = env.get_template("t.html").unwrap().render(()).unwrap();
    assert_eq!(out, "&lt;b&gt;a&lt;/b&gt;");
}

#[test]
fn renders_concurrently() {
    let env = std::sync::Arc::new(env_with(FILTERS));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let env = std::sync::Arc::clone(&env);
            std::thread::spawn(move || {
                (0..50)
                    .map(|_| {
                        env.render_str("{{ t|slugify }}", context! { t => format!("Thread {i}") })
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for (i, handle) in handles.into_iter().enumerate() {
        let outputs = handle.join().unwrap();
        assert!(
            outputs.iter().all(|out| *out == format!("thread-{i}")),
            "{outputs:?}"
        );
    }
}

#[test]
fn strategies_behave_identically() {
    let stateful = r#"
TABLE = {i: i * i for i in range(100)}
seen = []

@template_global
def square(i):
    seen.append(i)
    return (TABLE[i], len(seen))
"#;
    for strategy in [ModuleStrategy::Snapshot, ModuleStrategy::Reexecute] {
        let env = env_with_strategy(FILTERS, strategy);
        let out = env
            .render_str("{{ 'A b'|slugify }} {{ 7 is prime }} {{ 0|explode }}", ())
            .unwrap_err();
        assert!(
            out.to_string().contains("ZeroDivisionError"),
            "{strategy:?}: {out}"
        );

        let env = env_with_strategy(stateful, strategy);
        let out = env.render_str("{{ square(3) }} {{ square(9) }}", ()).unwrap();
        assert_eq!(out, "(9, 1) (81, 1)", "{strategy:?}");
    }
}

const COUNTER: &str = r#"
seen = []

@template_global
def remember(x):
    seen.append(x)
    return len(seen)

@template_filter
def explode(x):
    seen.append('poisoned')
    raise ValueError(x)

@template_filter
def nested(x):
    seen.append(x)
    seen.append(x)
    return apply_filter('nested_inner', x)

@template_filter
def nested_inner(x):
    seen.append(x)
    return len(seen)
"#;

/// A second module, so a call can fail inside `COUNTER` while the render
/// carries on. `apply_filter` runs in the caller's render, unlike
/// `render_string`, which starts a nested one.
const CATCHER: &str = r#"
@template_global
def swallow(name):
    try:
        return apply_filter(name, 'boom')
    except RuntimeError:
        return 'caught'
"#;

fn per_render(strategy: ModuleStrategy) -> Environment<'static> {
    let mut monty = MontyJinja::new(Config {
        module_strategy: strategy,
        isolation: Isolation::PerRender,
        ..Config::default()
    });
    monty.add_module("counter.py", COUNTER).unwrap();
    monty.add_module("catcher.py", CATCHER).unwrap();
    let mut env = Environment::new();
    monty.install(&mut env);
    env.add_template("inner.html", "{{ remember(9) }}").unwrap();
    env
}

#[test]
fn per_render_state_lives_for_one_render() {
    for strategy in [ModuleStrategy::Snapshot, ModuleStrategy::Reexecute] {
        let env = per_render(strategy);
        let tpl = "{{ remember(1) }}{{ remember(2) }}{% include 'inner.html' %}";
        assert_eq!(env.render_str(tpl, ()).unwrap(), "123", "{strategy:?}");
        assert_eq!(
            env.render_str(tpl, ()).unwrap(),
            "123",
            "{strategy:?}: leaked across renders"
        );
    }
}

#[test]
fn per_render_state_is_discarded_when_a_call_raises() {
    let env = per_render(ModuleStrategy::Snapshot);
    // `explode` takes the session built by the first two calls, mutates it
    // and raises. The next call must start pristine, not from the session
    // `explode` left half-modified.
    let tpl = "{{ remember(1) }}{{ remember(2) }}{{ swallow('explode') }}{{ remember(3) }}";
    assert_eq!(env.render_str(tpl, ()).unwrap(), "12caught1");
}

#[test]
fn per_render_reentry_keeps_the_outer_session() {
    let env = per_render(ModuleStrategy::Snapshot);
    // `nested` holds the session (two appends) while `nested_inner` runs on
    // a pristine one (one append). The outer session is stored last, so the
    // next call sees three entries, not two.
    let out = env
        .render_str("{{ 'a'|nested }} {{ remember('b') }}", ())
        .unwrap();
    assert_eq!(out, "1 3");
}

#[test]
fn modules_that_cannot_be_snapshotted_fall_back_to_reexecution() {
    // Monty's serialized state for an imported `re` does not read back, so
    // this module is re-run per call even under the default `Snapshot`.
    let env =
        env_with("import re\n\n@template_filter\ndef words(s):\n    return len(re.findall(r'\\w+', s))\n");
    assert_eq!(env.render_str("{{ 'a b c'|words }}", ()).unwrap(), "3");
}
