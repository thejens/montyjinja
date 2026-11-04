//! Renders a small sales report whose filters, tests and globals are
//! written in Python. Run with `make example`.

use std::error::Error;

use minijinja::{Environment, context};
use montyjinja::MontyJinja;

fn main() -> Result<(), Box<dyn Error>> {
    let mut monty = MontyJinja::default();
    let module = monty.add_module("report.py", include_str!("python/report.py"))?;
    for export in module.exports() {
        eprintln!("registered python {:?}: {}", export.kind, export.name);
    }

    let mut env = Environment::new();
    env.add_template("report.md", include_str!("templates/report.md"))?;
    env.add_template("card.md", include_str!("templates/card.md"))?;
    monty.install(&mut env);

    let regions = vec![
        context! { name => "North", monthly => vec![120.0, 135.5, 128.0, 160.25, 171.0, 190.5] },
        context! { name => "South", monthly => vec![98.0, 91.5, 87.0, 85.0, 80.5, 79.0] },
        context! { name => "East", monthly => vec![140.0, 150.0, 149.5, 155.0, 162.0, 158.0] },
        context! { name => "West", monthly => vec![410.0, 395.0, 430.0, 445.5, 460.0, 480.0] },
    ];
    let out = env
        .get_template("report.md")?
        .render(context! { year => 2026, regions, target => 150 })?;
    println!("{out}");
    Ok(())
}
