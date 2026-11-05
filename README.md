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
{% python target=150 %}
for region in regions:
    hits = [m for m, v in zip(months, region['monthly']) if v >= target]
    print(f"- {region['name']}: {', '.join(hits) or 'none'}")
{% endpython %}
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

## Usage

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

### Ideas beyond the POC

- Type-check modules at load time with `monty-type-checking`, so template authors get errors before rendering.
- Support `import` between Python modules through `OsFunctionCall::Import` → `ModuleFile`.
- Memoize `py()` snippets. They read template variables implicitly, so the key would have to include every
  name the snippet looked up.
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
