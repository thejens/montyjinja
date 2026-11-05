"""Renders tests/jinja2/templates with real Python Jinja2 to produce the
expected outputs that montyjinja must reproduce byte for byte.

Run with `make golden`, which pins the Jinja2 version via uv.
"""

import json
import pathlib

import jinja2

ROOT = pathlib.Path(__file__).resolve().parent.parent / "tests" / "jinja2"


def main() -> None:
    env = jinja2.Environment(
        loader=jinja2.FileSystemLoader(ROOT / "templates"),
        # MiniJinja's defaults: autoescape by extension, loop controls on.
        autoescape=jinja2.select_autoescape(["html", "htm", "xml"]),
        extensions=["jinja2.ext.loopcontrols", "jinja2.ext.do"],
    )
    exec((ROOT / "filters.py").read_text(), {"env": env})
    context = json.loads((ROOT / "context.json").read_text())
    out_dir = ROOT / "expected"
    out_dir.mkdir(exist_ok=True)
    for template in sorted((ROOT / "templates").iterdir()):
        rendered = env.get_template(template.name).render(context)
        (out_dir / template.name).write_text(rendered)
        print(f"{template.name}: {len(rendered)} bytes")


if __name__ == "__main__":
    main()
