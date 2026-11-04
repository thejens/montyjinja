//! `{% python %}` blocks, expanded before MiniJinja parses the template.

use minijinja::{Environment, ErrorKind, context};
use montyjinja::{MontyJinja, expand_python_tags, python_tag_loader};

const REPORT: &str = r#"Report
{% python target=150 %}
    # `{{` would be template syntax inside a plain py_block call.
    hits = {r['name']: [m for m, v in zip(months, r['monthly']) if v >= target] for r in regions}
    for name, months_hit in hits.items():
        print(f"{{{name}}}: {', '.join(months_hit) or 'none'}")
{% endpython -%}
done"#;

fn env() -> Environment<'static> {
    let mut env = Environment::new();
    MontyJinja::default().install(&mut env);
    env.set_loader(python_tag_loader(|name| {
        Ok(match name {
            "report.txt" => Some(REPORT.to_owned()),
            "broken.txt" => Some("ok\n\n{{ 1 + }}\n{% python %}print(1){% endpython %}".to_owned()),
            "unclosed.txt" => Some("\n{% python %}print(1)".to_owned()),
            _ => None,
        })
    }));
    env
}

fn ctx() -> minijinja::Value {
    context! {
        months => ["jan", "feb", "mar"],
        regions => vec![
            context! { name => "north", monthly => [100, 160, 200] },
            context! { name => "south", monthly => [90, 80, 70] },
        ],
    }
}

#[test]
fn loaded_templates_run_python_blocks() {
    let out = env().get_template("report.txt").unwrap().render(ctx()).unwrap();
    assert_eq!(out, "Report\n{north}: feb, mar\n{south}: none\ndone");
}

#[test]
fn render_str_needs_explicit_expansion() {
    let source = expand_python_tags("{% python %}print(sum(range(n))){% endpython %}").unwrap();
    let out = env().render_str(&source, context! { n => 5 }).unwrap();
    assert_eq!(out, "10\n");
}

#[test]
fn template_errors_keep_their_original_line() {
    let err = env().get_template("broken.txt").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SyntaxError);
    assert_eq!(err.line(), Some(3), "{err}");
}

#[test]
fn malformed_python_blocks_name_the_template() {
    let err = env().get_template("unclosed.txt").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SyntaxError);
    assert!(
        err.to_string()
            .contains("unclosed.txt: unclosed {% python %} block (line 2)"),
        "{err}"
    );
}
