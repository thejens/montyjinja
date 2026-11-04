# montyjinja

**Proof of concept:** Python-powered templating for [MiniJinja](https://github.com/mitsuhiko/minijinja),
executed by [Monty](https://github.com/pydantic/monty), Pydantic's sandboxed Python interpreter written in Rust.

No CPython, no subprocess, no FFI. It is one Rust binary, and every Python call runs under memory, time and
recursion limits.

```jinja
{# filters, tests and globals written in Python #}
{{ amount|money('USD') }}  {{ series|sparkline }}  {% if total is outlier(totals) %}⚠{% endif %}

{# inline expressions that read the template context directly #}
{{ py("sum(o['qty'] * o['price'] for o in orders)") }}

{# blocks of Python whose print() output is emitted #}
{% call py_block(target=150) %}
for region in regions:
    hits = [m for m, v in zip(months, region['monthly']) if v >= target]
    print(f"- {region['name']}: {', '.join(hits) or 'none'}")
{% endcall %}
```

## Verdict

**Yes, it works, and it is cheap enough to use for real.** Python adds expressiveness to MiniJinja that Jinja's
expression language lacks: comprehensions, `sorted(key=...)`, `zip`, f-strings with format specs, `divmod`, real
functions with local state, and exceptions. Template authors get that without the host shipping a Python runtime
or handing them unsandboxed code execution.

| What you get                                                        | How                                                           |
| ------------------------------------------------------------------- | ------------------------------------------------------------- |
| Python filters / tests / globals                                    | `@template_filter`, `@template_test`, `@template_global`      |
| Inline Python expressions                                           | `{{ py("...") }}`                                             |
| Inline Python blocks                                                | `{% call py_block(...) %}...{% endcall %}`                    |
| Template variables visible in inline Python without passing them   | Monty's `NameLookup` suspension resolves names from `State`   |
| Python calling back into the template engine                        | `render(name, **ctx)`, `render_string(src, **ctx)`, `apply_filter(name, value, *args, **kw)` |
| Isolation: no filesystem, env, or network; no state shared between calls | Monty sandbox; a fresh interpreter state for every call    |
| Runaway code stopped                                                | `ResourceLimits` (default: 32 MiB, 250 ms, recursion depth 200) |
| Real Python tracebacks with the right line numbers                  | `PythonError` attached as the MiniJinja error's `source()`    |
| Thread-safe rendering                                               | Programs are forked per call; there is no global interpreter lock |

## Performance

Measured with `make bench` on an Apple Silicon laptop, release build. Each figure is the cost per filter call
inside a 1,000-iteration `{% for %}` loop.

| Case                                          | Snapshot (default) | Reexecute |
| --------------------------------------------- | -----------------: | --------: |
| native Rust filter (baseline)                 |          ~0.9 µs   |           |
| inline `py("'-'.join(t.lower().split())")`    |          ~5.5 µs   |           |
| Python module filter, module with only `def`s |           ~15 µs   |    ~11 µs |
| … same, with a 20-row list of dicts argument  |           ~37 µs   |    ~34 µs |
| Python module filter, module builds a 2000-entry table at top level |  ~160 µs   |   ~530 µs |

So a Python filter is roughly **10–40× a native Rust filter**, and a few microseconds in absolute terms. A page
with a few hundred Python calls still renders in single-digit milliseconds. The main costs are:

1. **Isolation.** Each call starts from a pristine module state, so a template cannot leak data from one render
   into the next. There are two ways to produce that state ([`ModuleStrategy`](src/config.rs)):
   - `Snapshot`: run the top level once, serialize the suspended interpreter, and deserialize it per call.
     Cost scales with the size of the module's heap.
   - `Reexecute`: re-run the top level for every call. Cost scales with the work the top level does.

   `Snapshot` is the default. When it is the wrong choice it costs about 1.4× (light modules); when `Reexecute`
   is the wrong choice it costs about 3.3× or more (modules that build data).
2. **Value conversion.** MiniJinja values are deep-copied into the sandbox heap and back. Pass the slice of data
   a function needs, not the whole context.

Persistent interpreter state would be faster (about 6 µs per call in a probe), but it leaks state between
calls. The `MontyRepl` source history also grows by one entry per call, so it does not suit a long-running
template engine.

## How it works

```
                 load time                                   every call
 ┌──────────────────────────────────────────┐   ┌───────────────────────────────────────────┐
 │ prelude (1 line: decorators)             │   │ restore snapshot  ─or─  re-run top level  │
 │ + user module                            │   │            │                              │
 │ + kind, idx, args, kw = __mj_ready__(reg)│──▶│ resume checkpoint with (kind, idx, args)  │
 │ + reg[kind][idx](*args, **kw)            │   │            │                              │
 │                                          │   │ serve suspensions until complete:         │
 │ run to the __mj_ready__ checkpoint:      │   │   FunctionCall → render / apply_filter …  │
 │   registry → exports (names, kinds)      │   │   NameLookup   → State::lookup (inline)   │
 │   suspended state → snapshot bytes       │   │   OsCall       → NotImplementedError      │
 └──────────────────────────────────────────┘   └───────────────────────────────────────────┘
```

- **Registration.** A one-line prelude defines `template_filter`, `template_test` and `template_global`. Monty
  has neither `callable()` nor `fn.__name__`, so the decorators tell `@deco` and `@deco('name')` apart by
  argument type. Bare names are recovered from the function's `repr` (`<function 'slugify' at 0x..>`) once it
  reaches the host.
- **Checkpoint.** After its top level, the program calls the host function `__mj_ready__(registry)`. Monty
  suspends there, and the host reads the exports from the call's arguments. The suspended `FunctionCall` is
  `serde`-serializable, which is what makes `Snapshot` possible. Each template call resumes that checkpoint with
  the dispatch tuple.
- **Inline code.** `py()` and `py_block` compile the snippet once per (source, keyword names) and cache it.
  Undefined names make Monty suspend with `NameLookup`, and the host answers from the template context. That is
  why `{{ py("n ** 2") }}` works inside `{% for n in ... %}` without passing `n`.
- **Callbacks.** `render`, `render_string` and `apply_filter` are host functions. A Python call to one suspends
  the VM, and the host runs MiniJinja with the live `State` before resuming. A template error becomes a
  catchable Python `RuntimeError`.

## Usage

```rust
use minijinja::{Environment, context};
use montyjinja::{Config, MontyJinja};

let mut monty = MontyJinja::new(Config::default());
monty.add_module("filters.py", r#"
@template_filter
def initials(name):
    return ''.join(part[0].upper() for part in name.split())

@template_test('prime')
def is_prime(n):
    return n > 1 and all(n % d for d in range(2, n))
"#)?;

let mut env = Environment::new();
monty.install(&mut env); // module exports + py() + py_block
env.render_str("{{ who|initials }} {{ 7 is prime }}", context! { who => "ada lovelace" })?;
// → "AL True"
```

See [`examples/report.rs`](examples/report.rs), [`examples/python/report.py`](examples/python/report.py) and
[`examples/templates/report.md`](examples/templates/report.md) for a fuller demo (`make example`).

### Value mapping

| MiniJinja                        | Python                     | Back to MiniJinja                       |
| -------------------------------- | -------------------------- | --------------------------------------- |
| none / undefined                 | `None`                     | none                                    |
| bool, int, float                 | `bool`, `int`, `float`     | same (ints beyond i64 via i128 or string) |
| string (safe marker dropped)     | `str`                      | string (not safe; it gets autoescaped)  |
| seq / iterable                   | `list`                     | seq (tuples, sets and namedtuples too)  |
| map                              | `dict` (order kept)        | map, order kept (`preserve_order`)      |
| other objects                    | `str(value)`               | class/dataclass instances → map; decimals → string; else `repr()` |

## Findings and limitations

These are things the POC ran into, ordered by how much they would matter for a real implementation.

- **In-process only.** Monty's sandbox stops Python-level escapes, but a crash inside the interpreter (stack
  overflow, allocator abort) takes the host process down with it. For untrusted templates, the Monty docs
  recommend `monty-pool`, which runs workers as subprocesses. That would be a natural next step: the
  suspension-driven design here maps directly onto the pool's turn events.
- **No custom tags in MiniJinja.** A real `{% python %}` tag is not possible, so `py_block` uses
  `{% call %}`. Its body is rendered as a template first, so `{{` inside the Python is template syntax. Wrap
  such code in `{% raw %}`.
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

### Ideas beyond the POC

- Type-check modules at load time with `monty-type-checking`, so template authors get errors before rendering.
- Support `import` between Python modules through `OsFunctionCall::Import` → `ModuleFile`.
- Memoize pure `py()` snippets per render.
- Expose `monty-pool` as an alternative backend for multi-tenant or untrusted templates.

## Development

```sh
make help      # list targets
make check     # fmt-check + clippy -D warnings + tests
make example   # render the demo report
make bench     # performance table above
```

Requires Rust 1.96+ (Monty's MSRV).

## License

MIT
