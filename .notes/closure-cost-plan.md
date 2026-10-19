# Closure creation: shrinking `EcmaFunction` (and the registry)

**Status: PLAN.** Baseline, profile, and the rejected alternatives were measured
2026-10-19. Nothing here is implemented. The measured numbers live in
`.notes/perf.md` ("Closure creation, profiled properly" and "Deferred function
boilerplate").

## Why this is the next target

Creating a closure costs **~333 ns**:

| probe (300k iterations) | time |
| --- | --- |
| `var f = function () { return 1; };` only | 97-105 ms |
| …plus one call each | 138-147 ms |

That is the dominant cost of the corpus's largest row
(`language/statements/for-in/head-let-fresh-binding-per-iteration.js`, ~550 ms
of a ~3.1 s corpus total), and it is paid by real code: closures in loops are
ordinary. It is also paid twice — the call path copies the whole
`EcmaFunction` record (`ordinary_call` reads a copy; `function.rs` has a
`.cloned()`), so the record's size shows up at creation and per call.

## Measured budget (per closure, ~333 ns)

Accumulated means over 19,000 instantiations (not single samples — the 100 ns
Windows timer made a one-sample split misleading; see the perf note):

| phase | ns |
| --- | --- |
| `ecma_functions.insert` + `store_construct_patterns` | **97** |
| `Function::new_with_prototype` + `set_environment_edge` | 59 |
| `shared_function_body` | 56 |
| `set_function_properties_batched` (length/name/caller/arguments) | 31 |
| `shared_compiled_body` + `set_compiled` | 30 |
| `capture_source` (the per-closure source slice) | 27 |
| `shared_params` | 17 |
| setup (strict check, realm/contexts, `parse_text`) | 16 |

## The record: 408 bytes

`size_of::<EcmaFunction>()` = 408. Exact contributions:

| group | bytes |
| --- | --- |
| 3 × `Option<JsString>` — `name`, `source`, `parse_text` (48 each) | 144 |
| 6 × `Vec` — `fields`, `private_methods`, `decorator_initializers`, `computed_keys`, `outer_chain`, `per_iteration_chain` (24 each) | 144 |
| 2 × `Option<Value>` — `home_object`, `super_constructor` | 32 |
| `Option<ScriptOrModule>` | 16 |
| `Rc<[BindingElement]>` (params) | 16 |
| 5 × 8-byte handles/Rcs — `body`, `environment`, `private_environment`, `realm`, `ir` | 40 |
| flags/enums + padding | 16 |
| **total** | **408** |

For an ordinary closure (no class state, no capture chain) **~324 of the 408
bytes are empty or trivial**, and the identity-hashed map still copies all of
it on every insert.

## Rejected alternatives (measured — do not redo)

| attempt | result |
| --- | --- |
| Defer `length`/`name`/`caller`/`arguments` like `prototype_pending` | 300k closures 97-100 → **108-113 ms**; with a call 138-141 → **177-178 ms**. The batched adoption is cheaper than the lazy path's record lookup + name clone. Reverted; see the perf note. |
| `HashMap<u64, Box<EcmaFunction>>` | Isolated 248-byte stand-in: 52 → **128 ns/insert** (the allocator costs more than the copy it saves). |
| `Vec` slab indexed by function id | ~77 MB at 300k ids (ids are permanent, entries 408 B). |
| Single-sample phase profiling | Not evidence on a 100 ns timer. Accumulate. |

## Stage A — class-only state behind one box (recommended first)

Move `fields`, `private_methods`, `decorator_initializers`, `computed_keys`,
`super_constructor`, `private_environment` into
`struct ClassParts`, stored as `Option<Box<ClassParts>>`. The
`constructor_kind`/`is_class_constructor`/`default_derived` flags stay on the
record.

- **Record:** 408 → ~288 bytes (−29%).
- **Expected:** the insert's value copy drops ~25-30 ns. The isolated bench
  scales with value size (52 ns for 248 B in steady state), so a 120-byte cut
  is proportional.
- **Cost/risk:** every class/snapshot field access goes through the box; the
  `Trace` impl must walk it; a `Box` is allocated only for class constructors
  (rare). The fields stay `pub`; call sites become `data.class.as_ref()?` or a
  small accessor.
- **Touch points:** `crates/runtime/src/function.rs` (both `EcmaFunction`
  literals), `class.rs`, `snapshot.rs`, `ir.rs` (construct/field-init paths),
  `eval.rs`, and the class-field JIT steps if they read the record directly.
- **Netting:** `calls/construct_churn` and the class fixtures must not regress
  (the box adds one allocation and one indirection per class constructor).

## Stage B — share the source info (also removes 27 ns)

`source` and `parse_text` are 96 bytes inline and are re-created per closure:
`capture_source` walks to the enclosing text and `span_slice` allocates a new
`JsString` every time.

- Cache `SourceInfo { slice: JsString, full: JsString }` **per site**, keyed
  like `shared_function_body` (function node address, realm, span, source
  hash — the body cache already computes that tuple), or store it in that
  cache's entry. The record then holds `Option<Rc<SourceInfo>>` (**8 bytes**,
  96 → 8) and `capture_source`'s 27 ns per closure disappears after the first.
- **Soundness:** source texts are immutable. `Flat`/`Small` are self-contained;
  confirm the texts are leaves (if a `ConsString`/`Rope` ever arrives, it holds
  arena pointers and must not live in a non-GC `Rc` — the existing body cache
  already holds a non-GC `Rc<Block>` of AST nodes, which is the same class of
  decision). `Function.prototype.toString` reads `source`; snapshots must
  reproduce it.
- **Expected:** ~35-45 ns per closure (96 B of copy + the slice allocation).

## Stage C — the call-path copy

After A+B the record is ~290 bytes and `ordinary_call` still copies it per
call. Measure `--jit-bench` `function calls`, `calls/recursive_fib`, and
`calls/method_call` before/after A+B; if the copy still matters, it is removed
by Stage D rather than by an unsafe borrow split.

## Stage D — move the record into the GC arena (biggest, last)

Store a `Gc<EcmaFunction>` on the function's object part (the
`function_self` pattern), so creation is an arena allocation (~5-6 ns per the
GC allocation measurements) instead of a 408-byte map insert, and reads
dereference a pointer instead of copying.

- Requires: tracing the record from the object (move the edge out of
  `Agent::ecma_functions`), an id→function path for the API / stack-trace /
  snapshot / feedback consumers, and keeping `leaf_records` /
  `construct_property_patterns` keyed by function id.
- This is the V8 shape (JSFunction → SharedFunctionInfo/Context). Do not start
  it before A+B are measured; it is a multi-file change and may not be needed.

## Order, targets, decision point

1. **Stage B** (small, also removes a separate cost): expect the 300k probe
   ~100 → ~90 ms.
2. **Stage A** (record shrink): expect ~90 → ~70 ms.
3. Re-measure the corpus row (~550 ms) and the call rows.
4. Only then evaluate **D**.

Stop rule: if B+A together do not take the 300k probe below ~85 ms (from
97-105), do not attempt D.

## Reproduction

Probes (git-ignored, recreate under `scratch/ab/<name>/w.js`, run with
`./target/release/slag.exe --corpus scratch/ab/<name>`):

```js
// e7: 300k anonymous closures, never observed  (~97-105 ms)
function bench() { for (var k = 0; k < 300000; k++) { var f = function () { return 1; }; } return 0; }
bench();
```

```js
// e5: 300k closures, one call each              (~138-147 ms)
function bench() { var t = 0; for (var k = 0; k < 300000; k++) { var f = function () { return 1; }; t += f(); } return t; }
bench();
```

- Corpus row: `./target/release/slag.exe --corpus tools/corpus/workloads/language/statements/for-in`
  (~550 ms; the 733 ms seen once was thermal/load).
- Full corpus: `node tools/corpus/bench.js` (parity must stay 0).
- Profiling method: accumulate per-phase means over ≥1,000 calls and print
  once; never trust a single sample (see `.notes/perf.md`).
- Isolated map bench: `scratch/insert_bench.rs` reproduced in the perf note
  (248-byte record: 52 ns/insert steady, 224 with growth; boxed 128).

## Validation gates (per landed stage)

`cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D
warnings`; `rustfmt --edition 2024` on touched files; `sweep.exe all --timeout
15 --recheck-timeout 15` and `sweep.exe intl402 ...` must stay 48,633 + 3,365 =
51,998 at 100%; corpus parity 0; same-binary A/B for every claimed win; a
`.notes/perf.md` entry per stage.
