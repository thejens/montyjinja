//! Rough cost of Python in templates versus native Rust filters.
//! Run with `make bench` (release mode; debug numbers are meaningless).

use std::time::{Duration, Instant};

use minijinja::{Environment, Value, context};
use montyjinja::{Config, Isolation, ModuleStrategy, MontyJinja};

const ITEMS: usize = 1_000;
const ROUNDS: u32 = 10;

/// Only `def`s: the top level is nearly free to re-run.
const LIGHT: &str = r#"
@template_filter
def slugify(text):
    return '-'.join(text.lower().split())

@template_filter
def total(rows):
    return sum(r['qty'] * r['price'] for r in rows)

@template_filter(pure=True)
def total_pure(rows):
    return sum(r['qty'] * r['price'] for r in rows)
"#;

/// The top level builds a lookup table: expensive to re-run, and a larger
/// heap to snapshot.
const HEAVY: &str = r#"
TABLE = {i: f'item-{i * i}' for i in range(2000)}

@template_filter
def lookup(i):
    return TABLE[i % 2000]
"#;

fn env_for(strategy: ModuleStrategy, isolation: Isolation) -> Environment<'static> {
    let mut monty = MontyJinja::new(Config {
        module_strategy: strategy,
        isolation,
        ..Config::default()
    });
    monty.add_module("light.py", LIGHT).expect("light module loads");
    monty.add_module("heavy.py", HEAVY).expect("heavy module loads");
    let mut env = Environment::new();
    monty.install(&mut env);
    env.add_filter("native_slugify", |text: String| {
        text.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
    });
    env
}

fn main() {
    let titles: Vec<String> = (0..ITEMS).map(|i| format!("Hello Big World {i}")).collect();
    let rows: Vec<_> = (0..20).map(|i| context! { qty => i, price => 2.5 }).collect();
    let ctx = context! { titles, rows };

    let configs = [
        ("Snapshot", ModuleStrategy::Snapshot, Isolation::PerCall),
        ("Reexecute", ModuleStrategy::Reexecute, Isolation::PerCall),
        ("PerRender", ModuleStrategy::Snapshot, Isolation::PerRender),
    ]
    .map(|(label, strategy, isolation)| (label, env_for(strategy, isolation)));
    let snapshot = &configs[0].1;
    let module_cases = [
        (
            "filter, light module",
            "{% for t in titles %}{{ t|slugify }}{% endfor %}",
        ),
        (
            "filter, light module, 20-row arg",
            "{% for t in titles %}{{ rows|total }}{% endfor %}",
        ),
        (
            "filter, heavy module",
            "{% for t in titles %}{{ loop.index|lookup }}{% endfor %}",
        ),
        (
            "pure filter, same 20-row arg",
            "{% for t in titles %}{{ rows|total_pure }}{% endfor %}",
        ),
    ];

    println!("per call, {ITEMS} calls x {ROUNDS} rounds\n");
    println!("baseline");
    let native = "{% for t in titles %}{{ t|native_slugify }}{% endfor %}";
    println!(
        "{:<36} {:>12}",
        "native rust filter",
        time(snapshot, native, &ctx)
    );
    let inline = "{% for t in titles %}{{ py(\"'-'.join(t.lower().split())\") }}{% endfor %}";
    println!(
        "{:<36} {:>12}",
        "inline py() expression",
        time(snapshot, inline, &ctx)
    );

    print!("\n{:<36}", "python module");
    for (label, _) in &configs {
        print!(" {label:>12}");
    }
    println!();
    for (name, source) in module_cases {
        print!("{name:<36}");
        for (_, env) in &configs {
            print!(" {:>12}", time(env, source, &ctx));
        }
        println!();
    }
}

fn time(env: &Environment<'_>, source: &str, ctx: &Value) -> String {
    let tpl = env.template_from_str(source).expect("template compiles");
    tpl.render(ctx).expect("warmup renders");
    let start = Instant::now();
    for _ in 0..ROUNDS {
        tpl.render(ctx).expect("renders");
    }
    format_duration(start.elapsed() / (ROUNDS * ITEMS as u32))
}

fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos >= 1_000 {
        format!("{:.1} µs", nanos as f64 / 1_000.0)
    } else {
        format!("{nanos} ns")
    }
}
