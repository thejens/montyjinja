//! `py()` expressions and `py_block` call-blocks.

use minijinja::{AutoEscape, Environment, context};
use montyjinja::MontyJinja;

fn env() -> Environment<'static> {
    let mut env = Environment::new();
    MontyJinja::default().install(&mut env);
    env
}

#[test]
fn py_reads_template_context_implicitly() {
    let out = env()
        .render_str(
            r#"{{ py("sum(o['qty'] * o['price'] for o in orders)") }}"#,
            context! { orders => vec![context! { qty => 2, price => 5 }, context! { qty => 1, price => 7 }] },
        )
        .unwrap();
    assert_eq!(out, "17");
}

#[test]
fn py_kwargs_shadow_context_and_loop_variables_resolve() {
    let out = env()
        .render_str(
            r#"{{ py("x * 10", x=3) }}{% for n in [1, 2] %} {{ py("n ** 2 + x") }}{% endfor %}"#,
            context! { x => 100 },
        )
        .unwrap();
    assert_eq!(out, "30 101 104");
}

#[test]
fn values_round_trip_structurally() {
    let out = env()
        .render_str(
            r#"{% set r = py("{'b': (1, 2.5, None), 'a': [True, 'x'], 'big': 2 ** 70}") %}
{{- r|list|join(',') }} {{ r.b[1] }} {{ r.b[2] is none }} {{ r.a[0] }} {{ r.big }}"#,
            (),
        )
        .unwrap();
    // Dict order is Python's insertion order, not sorted.
    assert_eq!(out, "b,a,big 2.5 True True 1180591620717411303424");
}

#[test]
fn py_block_emits_printed_output() {
    let tpl = r#"{% call py_block(limit=2) %}
    top = sorted(people, key=lambda p: -p['score'])[:limit]
    for i, p in enumerate(top, 1):
        print(f"{i}. {p['name']} ({p['score']})")
{% endcall %}"#;
    let people = vec![
        context! { name => "ann", score => 3 },
        context! { name => "bob", score => 9 },
        context! { name => "cy", score => 5 },
    ];
    let out = env().render_str(tpl, context! { people }).unwrap();
    assert_eq!(out, "1. bob (9)\n2. cy (5)\n");
}

#[test]
fn py_block_output_is_escaped_unless_marked_safe() {
    let mut env = env();
    env.set_auto_escape_callback(|_| AutoEscape::Html);
    env.add_template(
        "a.html",
        "{% call py_block() %}print('<i>' + name + '</i>'){% endcall %}",
    )
    .unwrap();
    env.add_template(
        "b.html",
        "{% call py_block(safe=true) %}print('<i>hi</i>'){% endcall %}",
    )
    .unwrap();
    let a = env
        .get_template("a.html")
        .unwrap()
        .render(context! { name => "<x>" })
        .unwrap();
    let b = env.get_template("b.html").unwrap().render(()).unwrap();
    assert_eq!(a, "&lt;i&gt;&lt;x&gt;&lt;/i&gt;\n");
    assert_eq!(b, "<i>hi</i>\n");
}

#[test]
fn python_errors_surface_with_tracebacks() {
    let err = env().render_str(r#"{{ py("[1, 2][5]") }}"#, ()).unwrap_err();
    assert!(err.to_string().contains("IndexError"), "{err}");
    let err = env()
        .render_str(r#"{{ py("undefined_thing + 1") }}"#, ())
        .unwrap_err();
    assert!(err.to_string().contains("NameError"), "{err}");
    let err = env().render_str(r#"{{ py("def (") }}"#, ()).unwrap_err();
    assert!(err.to_string().contains("SyntaxError"), "{err}");
}

#[test]
fn snippets_can_render_partials() {
    let mut env = env();
    env.add_template("row.txt", "[{{ k }}={{ v }}]").unwrap();
    let out = env
        .render_str(
            r#"{% call py_block() %}
for k, v in sorted(cfg.items()):
    print(render('row.txt', k=k, v=v), end='')
{% endcall %}"#,
            context! { cfg => context! { b => 2, a => 1 } },
        )
        .unwrap();
    assert_eq!(out, "[a=1][b=2]");
}

#[test]
fn numbers_keep_their_python_type() {
    let out = env()
        .render_str(
            r#"{{ py("[isinstance(v, float) for v in (a, b, c, d)]", a=1.0, b=1, c=2.5, d=big) }} {{ py("big - 1") }}"#,
            context! { big => u128::MAX },
        )
        .unwrap();
    assert_eq!(
        out,
        "[True, False, True, False] 340282366920938463463374607431768211454"
    );
}
