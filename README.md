# montyjinja: run Python Jinja2 templates on MiniJinja, with filters in Python

[![CI](https://github.com/thejens/montyjinja/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/montyjinja/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)

**Render templates written for Python's Jinja2 from Rust, and write their filters, tests and globals in
Python.** montyjinja extends [MiniJinja](https://github.com/mitsuhiko/minijinja) with Python semantics:
`{{ name.title() }}`, `{{ "{:,.2f}".format(total) }}` and `{% for k, v in d.items() %}` work as in Jinja2, and a
`filters.py` that registers `env.filters['money'] = money` loads unchanged. The Python runs in-process in
[Monty](https://github.com/pydantic/monty), Pydantic's sandboxed Python interpreter written in Rust: no CPython,
pyo3, subprocesses or WebAssembly, and every call runs under memory, time and recursion limits.

> **Status:** a working proof of concept with tests, benchmarks and CI. It is not published to crates.io.

```jinja
{# plain Jinja2, as written for Python: methods on values behave like Python's #}
{{ user.name.title() }} spent {{ "{:,.2f}".format(total) }}
{% for key, value in settings.items() %}{{ key }}={{ value }}{{ ", " if not loop.last }}{% endfor %}

{# filters, tests and globals written in Python #}
{{ amount|money('USD') }}  {{ series|sparkline }}  {% if total is outlier(totals) %}⚠{% endif %}

{# inline expressions that read the template context directly #}
{{ py("sum(o['qty'] * o['price'] for o in orders)") }}

{# blocks of Python whose print() output is emitted #}
{% python target=150 %}
for region in regions:
    hits = [m for m, v in zip(months, region['monthly']) if v >= target]
    print(f"- {region['name']}: {', '.join(hits) or 'none'}")
{% endpython %}
```

## Contents

- [Why montyjinja?](#why-montyjinja)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Jinja2 compatibility](#jinja2-compatibility)
- [Features](#features)
- [Performance](#performance)
- [How it works](#how-it-works)
- [Value mapping](#value-mapping)
- [Limitations](#limitations)
- [FAQ](#faq)
- [Development](#development)

## Why montyjinja?

MiniJinja keeps its expression language small on purpose: anything beyond it is written as Rust filters and
functions compiled into the host. That causes problems montyjinja addresses:

- **Templates can't be extended at runtime.** Custom filters need a Rust change and a redeploy. With
  montyjinja, a `.py` file loaded at runtime registers its filters, tests and globals. Users, operators or a CMS
  can extend templates without a Rust toolchain.
- **Macros can only return strings.** Python functions return real values (dicts, lists, numbers) that the
  template can index, loop over and pass on.
- **Jinja expressions are limited.** Comprehensions, `sorted(key=...)`, `try`/`except`, `while` and functions
  with local variables are ordinary Python in `py()` and `{% python %}` blocks. No more `namespace()`
  workarounds.
- **Embedding CPython is heavy and unsandboxed.** Monty is pure Rust and runs in-process with no filesystem,
  environment or network access, plus per-call resource limits.
- **Jinja2 templates don't port as-is.** MiniJinja doesn't run Python methods (`.upper()`, `.format()`,
  `.items()`), prints and escapes values differently, and lacks some built-in filters. montyjinja closes those
  gaps, and a Jinja2 `filters.py` that registers into `env.filters` loads unchanged, as long as it stays within
  Monty's Python subset.

## Installation

The crate is not on crates.io; depend on it from Git:

```toml
[dependencies]
minijinja = "3"
montyjinja = { git = "https://github.com/thejens/montyjinja" }
```

## Quick start

```rust
use minijinja::{Environment, context};
use montyjinja::{Config, MontyJinja};

let mut monty = MontyJinja::new(Config::default());
monty.add_module("filters.py", r#"
@template_filter
def initials(name):
    return ''.join(part[0].upper() for part in name.split())

@template_test('prime', pure=True)  # results reused within a render
def is_prime(n):
    return n > 1 and all(n % d for d in range(2, n))
"#)?;

let mut env = Environment::new();
monty.install(&mut env); // module exports + py() + py_block
env.render_str("{{ who|initials }} {{ 7 is prime }}", context! { who => "ada lovelace" })?;
// → "AL True"
```

`{% python %}` blocks need the source expanded before MiniJinja parses it:

```rust
use montyjinja::{expand_python_tags, python_tag_loader};

env.set_loader(python_tag_loader(minijinja::path_loader("templates")));      // loaded templates
env.add_template_owned("page.html", expand_python_tags(source)?.into_owned())?; // added templates
env.render_str(&expand_python_tags("{% python %}print(1 + 1){% endpython %}")?, ())?;
```

Module state can live for a whole render instead of a single call:

```rust
let monty = MontyJinja::new(Config { isolation: Isolation::PerRender, ..Config::default() });
```

See [`examples/report.rs`](examples/report.rs), [`examples/python/report.py`](examples/python/report.py) and
[`examples/templates/report.md`](examples/templates/report.md) for a fuller demo (`make example`).

## Jinja2 compatibility

With the default `Config::jinja2_compat`, templates written for Python's Jinja2 render the same under MiniJinja.
A conformance suite checks this: [16 templates](tests/jinja2/templates) are rendered by real Jinja2 3.1 into
[golden files](tests/jinja2/expected) (`make golden`), and montyjinja must reproduce them byte for byte.
They share one [`filters.py`](tests/jinja2/filters.py) that registers its filters the Jinja2 way and runs under both
engines.

| Jinja2 behaviour                                      | How montyjinja provides it                                                   |
| ----------------------------------------------------- | ---------------------------------------------------------------------------- |
| Python methods on values: `s.upper()`, `"{:>8}".format(x)`, `d.items()`, `s.split(',', 1)` | MiniJinja's unknown-method callback runs the method in Monty on a copy of the value |
| Dict views, `range` and generators: loop over them, print them as `dict_items([...])` and `range(0, 3)` | Python iterables become sequences that keep their Python `repr` |
| `env.filters['x'] = f`, `env.tests[...]`, `env.globals[...]` (including plain values) | An `env` object in every module's prelude |
| `\|format` with printf semantics (`"%(a)s-%(b)05d"\|format(a='x', b=42)`) | Python's own `%` operator |
| Output as Python's `str()`: `None`, `1e+20`, `('a', 1)`, `{'k': [1.5]}` | A formatter implementing Python's `repr`, including CPython's float rules |
| HTML escaping as MarkupSafe does it (`&#39;`, `&#34;`, `/` left alone) | The formatter, plus `escape`, `e`, `forceescape` and `xmlattr` filters |
| `truncate`, `wordcount`, `center`, `filesizeformat`, `striptags`, `random` | A bundled Python module ([`builtins.py`](src/jinja2/builtins.py)) |
| `cycler`, `joiner`, `{% break %}`, `{% continue %}`, `{% do %}` | Native objects and MiniJinja's `loop_controls` feature |

What still differs, because MiniJinja has no extension point for it:

- **`%` on strings.** `{{ "%.2f" % x }}` fails; MiniJinja's `%` is numeric only. Use `{{ "%.2f"|format(x) }}`.
- **In-place mutation.** `{% do items.append(x) %}` raises an error instead of silently doing nothing, because
  MiniJinja values are immutable. Build a new value (`items + [x]`) or use `namespace()`.
- **Methods on host objects.** Only plain data (strings, numbers, lists, dicts) has Python methods. Rust objects
  in the context keep MiniJinja's behaviour, and Python class instances returned by filters arrive as dicts of
  their attributes, without their methods.
- **Jinja2's Python API.** `pass_context`/`pass_environment` decorators, `Markup` objects in Python code, and
  extensions such as i18n `{% trans %}` have no equivalent.
- **Not yet implemented:** `wordwrap`, `urlize`, `lipsum` and the `callable` test. `striptags` decodes numeric and
  basic named entities only, since Monty has no `html.unescape`.

Turning compatibility on replaces the environment's formatter and unknown-method callback. Set
`Config { jinja2_compat: false, .. }` to keep MiniJinja's own behaviour.

## Features

**Yes, it works, and it is cheap enough to use for real.** Python adds expressiveness to MiniJinja that Jinja's
expression language lacks, and template authors get it without the host shipping a Python runtime or handing
them unsandboxed code execution.

| What you get                                                        | How                                                           |
| ------------------------------------------------------------------- | ------------------------------------------------------------- |
| Jinja2 templates render as in Python                                | See [Jinja2 compatibility](#jinja2-compatibility)             |
| Python filters / tests / globals                                    | `@template_filter`, `@template_test`, `@template_global`, or Jinja2's `env.filters[...]` |
| Inline Python expressions                                           | `{{ py("...") }}`                                             |
| Inline Python blocks                                                | `{% python %}...{% endpython %}`, expanded to a `py_block` call before parsing |
| Template variables visible in inline Python without passing them   | Monty's `NameLookup` suspension resolves names from `State`   |
| Python calling back into the template engine                        | `render(name, **ctx)`, `render_string(src, **ctx)`, `apply_filter(name, value, *args, **kw)` |
| Isolation: no filesystem, env, or network; no state shared between calls | Monty sandbox; a fresh interpreter state for every call    |
| Runaway code stopped                                                | `ResourceLimits` (default: 32 MiB, 250 ms, recursion depth 200) |
| Interpreter panics contained                                        | Every Monty step runs under `catch_unwind` and fails as a Python `RuntimeError` |
| Module state per call or per render                                 | `Isolation::PerCall` (default) or `Isolation::PerRender`      |
| Repeated calls served from a per-render cache                      | `@template_filter(pure=True)` (also on tests and globals)     |
| Real Python tracebacks with the right line numbers                  | `PythonError` attached as the MiniJinja error's `source()`    |
| Thread-safe rendering                                               | Programs are forked per call; there is no global interpreter lock |

## Performance

Measured with `make bench` on an Apple Silicon laptop, release build. Each figure is the cost per filter call
inside a 1,000-iteration `{% for %}` loop. Absolute numbers vary by about 2× between runs on a busy machine;
the ratios are stable.

| Case                                          | Snapshot (default) | Reexecute | PerRender |
| --------------------------------------------- | -----------------: | --------: | --------: |
| native Rust filter (baseline)                 |          ~0.5 µs   |           |           |
| inline `py("'-'.join(t.lower().split())")`    |            ~5 µs   |           |           |
| the same as Jinja2 methods, `'-'.join(t.lower().split())` (3 calls) | ~20 µs |    |           |
| Python module filter, module with only `def`s |           ~16 µs   |    ~12 µs |     ~4 µs |
| … same, with a 20-row list of dicts argument  |           ~47 µs   |    ~36 µs |    ~28 µs |
| Python module filter, module builds a 2000-entry table at top level |  ~165 µs   |   ~590 µs |   ~3.5 µs |
| `pure=True` filter, same 20-row argument every call (cache hit) |  ~4.5 µs   |   ~4.5 µs |   ~4.5 µs |

`Snapshot` and `Reexecute` are `ModuleStrategy` values with the default `Isolation::PerCall`; `PerRender` is
`Isolation::PerRender` on top of `Snapshot`.

Where the time goes:

1. **Isolation.** Under `PerCall`, each call starts from a pristine module state, so no call can see what an
   earlier one did. There are two ways to produce that state ([`ModuleStrategy`](src/config.rs)):
   - `Snapshot`: run the top level once, serialize the suspended interpreter, and deserialize it per call.
     Cost scales with the size of the module's heap.
   - `Reexecute`: re-run the top level for every call. Cost scales with the work the top level does.

   `Isolation::PerRender` pays that cost once per render instead: the first call restores the module, and later
   calls in the same render resume the same suspended interpreter. State never crosses renders, but module
   globals mutated by one call are visible to the next call in that render. A call that raises discards the
   state.
2. **Value conversion.** MiniJinja values are deep-copied into the sandbox heap and back. This is what is left
   under `PerRender` with a 20-row argument. Pass the slice of data a function needs, not the whole context.
3. **Memoization.** A function registered with `pure=True` caches its results per render, keyed by its
   arguments, so a repeated call skips both conversion and Python. What remains on a hit is fingerprinting the
   arguments.

## How it works

```
                 load time                                   every call
 ┌──────────────────────────────────────────┐   ┌───────────────────────────────────────────┐
 │ prelude (1 line: decorators)             │   │ render's session ─or─ restore snapshot    │
 │ + user module                            │   │                  ─or─ re-run top level    │
 │ + out = reg                              │   │            │                              │
 │ + while True:                            │──▶│ resume checkpoint with (kind, idx, args)  │
 │     k, i, a, kw = __mj_ready__(out)      │   │            │                              │
 │     out = reg[k][i](*a, **kw)            │   │ serve suspensions until the checkpoint:   │
 │                                          │   │   FunctionCall → render / apply_filter …  │
 │ run to the first checkpoint:             │   │   NameLookup   → State::lookup (inline)   │
 │   registry → exports (names, kinds)      │   │   OsCall       → NotImplementedError      │
 │   suspended state → snapshot bytes       │   │ result = the checkpoint's argument;       │
 │                                          │   │ PerRender keeps the paused interpreter    │
 └──────────────────────────────────────────┘   └───────────────────────────────────────────┘
```

- **Registration.** A one-line prelude defines `template_filter`, `template_test` and `template_global`. Monty
  has neither `callable()` nor `fn.__name__`, so the decorators tell `@deco` and `@deco('name')` apart by
  argument type. Bare names are recovered from the function's `repr` (`<function 'slugify' at 0x..>`) once it
  reaches the host.
- **Checkpoint loop.** After its top level, the program loops on the host function `__mj_ready__`. The first
  call carries the registry, from which the host reads the exports; every later call carries the previous
  call's result. Monty suspends at each one, and the suspended `FunctionCall` is `serde`-serializable, which is
  what makes `Snapshot` possible. Because a finished call leaves the interpreter paused at the same place,
  `PerRender` can keep it in a MiniJinja render-local extension and resume it for the next call.
- **`{% python %}`.** MiniJinja has no custom-tag API, so [`expand_python_tags`](src/tags.rs) rewrites
  `{% python args %}body{% endpython %}` to `{% call py_block(args) %}{% raw %}body{% endraw %}{% endcall %}`
  in the source, on the same lines. `python_tag_loader` applies it to every loaded template.
- **Memoization.** A `pure=True` call is keyed by the callee plus a type-tagged fingerprint of the template
  values it receives. MiniJinja's own equality treats `1`, `1.0` and `true` as equal, but a Python function may
  not, so the key keeps them apart. Results live in a render-local extension, capped by `Config::memo_capacity`
  (default 1,024 per render). Exceptions are never cached.
- **Panic containment.** Each interpreter step runs inside `catch_unwind`. A panic in Monty fails that call
  with `RuntimeError: internal interpreter error: …`; host callbacks run outside the boundary.
- **Inline code.** `py()` and `py_block` compile the snippet once per (source, keyword names) and cache it.
  Undefined names make Monty suspend with `NameLookup`, and the host answers from the template context. That is
  why `{{ py("n ** 2") }}` works inside `{% for n in ... %}` without passing `n`.
- **Callbacks.** `render`, `render_string` and `apply_filter` are host functions. A Python call to one suspends
  the VM, and the host runs MiniJinja with the live `State` before resuming. A template error becomes a
  catchable Python `RuntimeError`.

## Value mapping

| MiniJinja                        | Python                     | Back to MiniJinja                       |
| -------------------------------- | -------------------------- | --------------------------------------- |
| none / undefined                 | `None`                     | none                                    |
| bool, int, float                 | `bool`, `int`, `float`     | same (ints beyond i64 via i128 or string) |
| string (safe marker dropped)     | `str`                      | string (not safe; it gets autoescaped)  |
| seq / iterable (tuples stay `tuple`) | `list`                 | seq; tuples stay tuples; sets, dict views, ranges and generators become sequences that print as Python does |
| map                              | `dict` (order kept)        | map, order kept (`preserve_order`)      |
| other objects                    | `str(value)`               | class/dataclass instances → map; decimals → string; else `repr()` |

## Limitations

Ordered by how much they would matter for production use.

- **In-process only.** Monty's sandbox stops Python-level escapes, and Rust panics inside the interpreter are
  contained. A stack overflow or allocator abort still takes the host process down, as does any panic in a
  binary built with `panic = "abort"`. For untrusted templates, the Monty docs recommend `monty-pool`, which
  runs workers as subprocesses. That would be a natural next step: the suspension-driven design here maps
  directly onto the pool's turn events.
- **`{% python %}` is a source rewrite.** MiniJinja has no custom-tag API. The rewrite assumes the default
  delimiters, cannot contain `{% endraw %}`, and ends at the first literal `{% endpython %}`, even inside a
  Python string. `python_tag_loader` drops `path_loader`'s auto-reload check, which MiniJinja keeps private.
  Plain `{% call py_block() %}` still works; its body is rendered as a template first, so `{{` inside it is
  template syntax.
- **`pure=True` is a promise, not a check.** A function that marks itself pure but reads mutable module state
  (possible under `PerRender`) returns stale results from the cache.
- **`PerRender` trades isolation for speed.** Calls in one render share module globals, and memory accumulates
  against `max_memory` for the whole render. A filter that re-enters its own module through `apply_filter`
  gets a pristine state for the nested call.
- **`py_block` output is not marked safe.** It is escaped under HTML autoescape unless you pass `safe=true`. The
  same applies to filter return values; there is no `Markup` equivalent yet.
- **Monty's Python subset.** It has no `callable()` and no `fn.__name__`. Function objects return to the host
  only as an opaque `repr`. Class and dataclass instances come back as maps of their attributes, decimals as
  their string, and dates as their `repr()` (`datetime.date(2026, 1, 2)`), so return `.isoformat()` when
  formatting matters. Templates get no `async`, no filesystem, and no environment.
- **`MontyRun` is `Send` but not `Sync`.** Its interner uses `RefCell`, so a shared program sits behind a
  `Mutex` that is held only while cloning.
- **The docs are ahead of the crate.** Some APIs in Monty's quickstart (`OsFunctionCall::no_handler_result`)
  are not in the published 1.4.0.
- **Line offsets.** The prelude must stay on one line, and tracebacks are shifted back by one line. Synthetic
  dispatch frames are hidden.

## Roadmap ideas

- Type-check modules at load time with `monty-type-checking`, so template authors get errors before rendering.
- Support `import` between Python modules through `OsFunctionCall::Import` → `ModuleFile`.
- Memoize `py()` snippets. They read template variables implicitly, so the key would have to include every
  name the snippet looked up.
- Expose `monty-pool` as an alternative backend for multi-tenant or untrusted templates.

## FAQ

### How do I write MiniJinja filters in Python?

Decorate functions with `@template_filter`, `@template_test` or `@template_global`, load the file with
`MontyJinja::add_module`, and call `install` on your `Environment`. See [Quick start](#quick-start).

### Is it safe to render user-supplied templates that contain Python?

Monty blocks filesystem, environment and network access, and montyjinja enforces memory, time, recursion,
output and host-call limits on every call. Rust panics inside the interpreter are contained. A stack overflow
or allocator abort can still take down the process, so for hostile multi-tenant input, run rendering in a
separate process. See [Limitations](#limitations).

### How is this different from embedding Python with pyo3?

pyo3 embeds CPython: the full language and its packages, but with no sandbox, a native dependency, and
CPython's global interpreter lock. Monty implements a subset of Python in pure Rust, with no third-party packages,
but it is sandboxed by design, starts in microseconds and renders on many threads at once.

### Can I render my existing Jinja2 templates from Rust?

Usually, yes. Python methods, `|format`, value printing, HTML escaping and the missing built-in filters behave as
in Jinja2, and a conformance suite checks this against real Jinja2. The exceptions are `"..." % x` on strings,
in-place mutation such as `items.append(x)`, and Jinja2's Python API. See
[Jinja2 compatibility](#jinja2-compatibility).

### Can I reuse Python Jinja2 filters?

Often, yes. A module that registers its filters with `env.filters['name'] = fn` (or `env.tests`,
`env.globals`) loads unchanged, as long as its functions avoid third-party imports. Filters that rely on Jinja2's
Python API (`Markup`, `pass_context`) need adapting.

### Is it fast enough?

A Python filter call costs about 4–16 µs, against about 0.5 µs for a native Rust filter. Repeated calls to
`pure=True` functions are served from a per-render cache. A page with hundreds of Python calls still renders
in milliseconds. See [Performance](#performance).

## Development

```sh
make help      # list targets
make check     # fmt-check + clippy -D warnings + tests
make example   # render the demo report
make golden    # regenerate Jinja2 conformance outputs (needs uv)
make bench     # performance table above
```

Requires Rust 1.96+ (Monty's MSRV).

## Acknowledgements

Built on [MiniJinja](https://github.com/mitsuhiko/minijinja) by Armin Ronacher and
[Monty](https://github.com/pydantic/monty) by Pydantic.

## License

[MIT](LICENSE)
