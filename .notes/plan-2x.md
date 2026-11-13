# Plan: 2x node on the corpus before v1.0

**Target.** Corpus total (79 rows, `tools/corpus/workloads`, min-of-N, jit) at most
**2.0x node**: <= ~1450 ms against node's 725 ms. Today: **2468-2526 ms (3.4-3.5x)**.
So ~1075 ms has to come out.

**Definition of done.** Corpus total <= 1450 ms; every row's result identical in
jit / jitless / `--gc-stress` / `--nursery-stress` (79/79); test262 `language`,
`built-ins`, `annexB` at 0 failures in both modes under a 15 s deadline;
`clippy --workspace --all-targets -D warnings`; `cargo test --workspace`.

## Where the time is (measured)

| block | ms | node | what it is |
|---|---:|---:|---|
| `head-let` | 621 | 52 | creation: 3 captured closures, 3 fresh bindings, 6 new-key stores, `Object.create`, for-in |
| `evaluation-order` | 313 | 29 | creation + templates: 1 captured closure, 2 template joins |
| `opcost/*` (36 rows) | 359 | ~90 | one builtin/op per loop iteration |
| long tail (41 rows) | ~1180 | ~555 | allocation, member ops, strings, enumeration |

The two hot rows are **creation-bound, not call-bound** (their fixtures have 6 calls
per body, not 112). A call costs 6-15 ns depending on shape; a captured closure
costs ~445 ns; an ordinary object ~60 ns; a new-key store 109 ns; a builtin method
call 240-300 ns. Node's equivalents are 4-25 ns.

## The model

Three regimes for the same operation, and which one you get decides everything:

| operation | folded / straight-line | via helper or VM |
|---|---:|---:|
| object literal | 3.1 ns | 60 ns |
| member store (presize hole) | 5 ns | 109 ns (new key) |
| call | 5.4 ns (slot) | 15 ns (method) |

Everything below is about moving code from the right column to the left one, or
about not paying the right column by accident.

## Workstreams

### W1 — a deopt must not strand a loop  (in flight)

**Symptom.** `scratch/row/h7` (callee in a local slot, read out of an object):
48.68 ms with the typer's read guard, **9.38 ms with `SLAG_TYPER=0`** (interleaved
min-of-7, same binary); control `h6` 5.19 vs 5.20. Census: 2,000,012 VM dispatches
with the guard, 0 without. IR: `v4:num = GuardType(v3, v3) 4` on a value that is a
function.

**Root cause.** `pass/guard.rs` keeps a read guard on *any* `FrameStore`, on the
theory that `narrow` will use the numeric-slot proof. When the slot is later loaded
as a call's callee, the guard buys nothing and its deopt fires; a deopt has no way
back into compiled code, so the rest of the loop runs in the VM lane at ~5x.

**Change.** Keep a store's guard only when the slot it writes is loaded into a
type-sensitive op (`Add/Sub/Mul/Div/Mod/Lt/Le/Gt/Ge`). Direct type-sensitive
consumers, edge arguments and slot-less stores unchanged.

**Accept.** `h7` 48.7 -> <=11 ms; `h6`/`h1`/`h4` flat; no row worse than +2%;
corpus total not up.

**Risk / fallback.** Narrowing what `narrow` may infer could un-prove arithmetic
slots. Fallback rule if a loop row regresses: drop the store guard only when the
slot's load feeds `Op::Call`.

### W2 — deopt re-entry (OSR)

**Symptom.** One deopt per call costs the rest of the call 4-9x (W1 is one
instance of it; any legitimate guard failure has the same shape).

**Change.** After a deopt at a loop head, re-enter compiled code there (OSR), or
recompile with widened types and re-enter. Start by measuring how many corpus rows
deopt at all (`SLAG_JIT_DEOPT_TRACE=1` per row) — that number sets the size.

**Accept.** A deopt at iteration k costs O(1), not O(n-k). Measured on a probe with
a legitimately failing guard.

### W3 — creation: closure + capture context

**Symptom.** ~445 ns per captured closure (row measurements: preamble 35, Function
box + `JsObject` + env edge 104, compiled-body cache 26, record insert 40-48,
boilerplate 29, rest 23, capture-context creation ~188). Node: ~25 ns.

**Change, in order of measured size.** (a) the two arena boxes -> one allocation
with the function's slots inline; (b) the `ecma_functions` insert + the compiled-body
cache lookup (previously bounded at 2% of the row *alone*; worth it only as part of
this); (c) the capture context's construction.

**Accept.** Captured closure <= 150 ns; `head-let` and `evaluation-order` each
>= 20% down; `construct_churn`, `spread_assign`, `json_roundtrip` each >= 15% down.

### W4 — new-key store (transition cell)

**Symptom.** `p.w = i` on a literal costs +109 ns over the same literal with `w: 0`
(+5 ns), and calls the full `SetMemberName` helper on every store. Every
`this.x = x` in a constructor pays it.

**Change.** A transition cell: `(map id, atom) -> next map + field`, probed inline
by the compiled store; on a hit adopt the next map and write the field.

**Accept.** +109 -> <=30 ns; `construct_churn` and `spread_assign` each >= 15% down.

### W5 — builtin entry

**Symptom.** 240-300 ns per builtin method call (`slice` 242, `regexp.test` 292,
`forEach` 302, `Map.forEach` 277) against node's 12-21, on one shared path.

**Change.** Emit the fast lane for the hot builtins instead of marshalling into a
Rust handler per operation; per-element callbacks should not re-enter the generic
`call` funnel.

**Accept.** `array_slice`, `array_for_each`, `regexp_test`, `map_for_each`,
`typed_array_for_each` each >= 30% down (~209 ms available).

### W6 — async state

**Symptom.** 7.6 us per async call (capability 680, instantiation 970, body run
3229, `attach_await` 1826; resume 806) and ~2 KB of live objects per pending await
(50k awaits does not finish). Two callables are created *per await*.

**Change, in order.** (a) resume handlers once per `AsyncFunctionState`; (b) the
two resolving functions of `new_promise_capability`; (c) the per-await promise.
Also resolve the unexplained gap: the harness measures 11.5 ms for a call the
script itself times at 3.5 ms.

**Accept.** async call <= 3 us; `async_churn` and `promise_churn` >= 50% down.

## Order and rationale

1. **W1 now** — smallest, verified 5x on a common shape, no new machinery.
2. **W2** — sets the floor under every other guard fix; measure the deopt census
   first so it is sized.
3. **W3 + W4** — the 933 ms block and the constructors; this is where the 1075 ms
   mostly is.
4. **W5** — ~209 ms across one shared path.
5. **W6** — unbounded memory curve and the worst ratio, but the measurement
   discrepancy has to be understood first.

## Not doing

- **The inliner.** Measured: corpus 2436 vs 2444 ms (wash). It needs post-inline
  specialization to pay, which is a compiler project.
- **ns-level slices** on rows not in the tables above.
- **PGO / function alignment.** Already measured and rejected in `.notes/perf.md`.

## Measurement protocol for every item

One row per process, isolated dir, interleaved A/B against the same-session parent
binary, min-of-7; a runtime-gated same-binary A/B where the change allows it.
Require >= 10% before believing a move, except where the item is a deopt/bug fix
whose mechanism is proven by the census (W1: 2,000,012 -> 0 dispatches).
