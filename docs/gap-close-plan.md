# Plan: closing the Slag–Node gap

## 1. Measured baseline (2026-09-02)

The full `--jit-bench` suite (12 rows) re-measured against node v24.12.0,
both JIT (default) and interpreter-only (`--jitless`, V8's Ignition), on
the same machine and session. Slag columns are best-of-3 `--jit-bench`
process runs; Node columns are the best (steady-state) round of
`tools/jit_bench/node_bench.js`. All four modes agree on every row's
completion value. Recorded in `docs/perf.md` (measured 2026-09-02).

| Benchmark | slag interp | slag jit | node jitless | node jit | interp gap | jit gap |
|---|---|---|---|---|---|---|
| arithmetic | 26.2 | 3.3 | 10.2 | 0.58 | 2.6x | 5.7x |
| property read | 54.5 | 6.9 | 12.4 | 0.32 | 4.4x | 21.8x |
| string concat | 10.6 | 2.7 | 1.5 | 0.53 | 7.1x | 5.0x |
| function calls | 6.3 | 1.9 | 1.9 | 0.06 | 3.3x | 32x |
| global read | 23.2 | 3.8 | 7.8 | 0.32 | 3.0x | 12.1x |
| compound assign | 19.7 | 2.5 | 1.6 | 0.06 | 12.6x | 41.5x |
| buildString shape | 180.1 | 54.2 | 53.1 | 8.2 | 3.4x | 6.6x |
| buildString full | 86.9 | 32.3 | 26.2 | 10.2 | 3.3x | 3.2x |
| typed-array write | 75.0 | 30.2 | 13.6 | 0.29 | 5.5x | 103x |
| typed-array length | 59.3 | 11.4 | 16.9 | 0.47 | 3.5x | 24.3x |
| vector leaf call | 45.0 | 29.7 | 9.5 | 0.12 | 4.8x | 258x |
| apply leaf call | 20.4 | 18.3 | 6.1 | 2.16 | 3.4x | 8.5x |

Gaps are slag ÷ node (ms).

## 2. Goal

Interpreter median gap **3.4x → ~2x**; JIT median gap **17x → ~5x**;
focus on the wide rows (compound assign, typed-array write, vector leaf
call). "Closed" means the row's gap halves or better, measured per the
A/B protocol in §6 — not a single run.

## 3. The gap, decomposed

The interpreter gaps are per-op machinery cost. The JIT gaps are three
things V8 does that Slag's straight-line lowering does not — **callee
inlining**, **loop-invariant code motion (LICM)**, and **bounds-check
elision + register quality** — plus **FFI-helper calls per element** on
the shapes the JIT lowers through the shared machinery.

| row | interp bottleneck | jit bottleneck | lever |
|---|---|---|---|
| arithmetic | loop head | codegen quality | unroll (M7) |
| property read | `member_cell_get` double probe | V8 hoists invariant `o.a`/`o.b` | single probe (M3); LICM (M6) |
| string concat | rope node alloc + `Rc` bumps | concat helper round-trip | arena alloc (M8) |
| function calls | — (fine) | probe-per-call vs V8 inlining the leaf | known-leaf inline (M4) |
| global read | — (fine) | LICM hoists `g` | LICM (M6) |
| compound assign | read-modify-write machinery | unroll + keep `o` in a register | fused cell op (M3/M9); unroll (M7) |
| buildString shape | dense-append machinery (shared with JIT) | same shared machinery | dense elements (M1) |
| buildString full | concat + array machinery | concat helper | M1/M8 |
| typed-array write | per-write view checks | FFI helper per element + re-checked bounds | fused write (M3); inline store (M5) |
| typed-array length | — (fine) | LICM hoists `ta.length` | LICM (M6) |
| vector leaf call | `do_call`'s `split_off` + fast-layout rebuild | rebuild + probe + no inlining | emit fast layout (M2); M2/M4 |
| apply leaf call | arg-list copy + member read | member read + leaf frame setup | M10, M4 |

## 4. Milestones

### M0 — Profiling pass (before any slice)

Wrap the hot helpers in counting/instrumented fns (the `leaf_call_probe`
count precedent) and decompose each row's per-op cost: `member_cell_get`
(map probe vs value-cell probe vs dispatch), `typed_array_element_set`,
`do_call`'s rebuild, the leaf-call core, rope `concat`. Several slices
have two candidate targets (e.g. M3's map probe vs value-cell probe) —
measure before picking. Add a `bare loop` row to `--jit-bench` so the
loop-head floor stays visible next to the machinery rows.

**Status (2026-09-02):** the `bare loop` row landed (function-wrapped
`--bench` shape; certifies, ratio 0.13): interp ~22.4ms, jit ~2.9ms.
The M3 A/B below resolved the member-probe decomposition; the other
rows' decompositions remain when their slices start.

### M1 — Dense array elements (both modes) — existing plan, highest absolute ROI

`docs/array-store-plan.md` Item 2 (`ArraySlots`: keyless
`Vec<Option<Value>>` + `Cell<f64>` length, spill-on-miss). The
`buildString shape` row is the suite's biggest absolute time (interp
180ms); the append path (`array_element_write`: key + clone +
`SmallProps::push` + index-map insert + length write + generation bump +
RefCell borrows) is ~60ns/iter, and the JIT store helper calls the same
shared machinery — why the JIT row cannot go below it today. Phase A
(representation + the three hot paths: write/get/length) and Phase B
(exotic ops over the buffer) are the bulk; Phase C (buffer-direct ICs +
`offset_of!` inline store) is where the JIT row moves.

- **Expected:** buildString shape interp 180→~90ms, jit 54→~25ms; every
  array-store shape (the property-escape cluster) lifts too.
- **Risk:** Phase B (Array exotic semantics) — the plan's "spill on any
  shape the buffer cannot represent exactly" keeps it a fast path, not a
  second implementation. `IntegerIndexed` is the blueprint.
- **Validation:** the plan's per-phase gate (workspace tests + clippy +
  JIT/jitless sweep at zero regressions); track the buildString rows.

**Status (2026-09-02): dense elements are LANDED — the plan doc's baseline
predates the current tree.** `ArraySlots` (`elements`/`length`/`dense`),
the dense write/get/length paths, the exotic ops (own-keys, get-own-
property, delete, define, set-length), the spill fallback, the Slice 1b
chain-clean verdict, the runtime IC fronts (`array_element_value_cells`/
`array_length_cells`), and the JIT `fast_array_element_write` helper are
all in: buildString shape measures 176–180ms interp / 51–54ms jit (vs
the plan's ~740ms baseline). Remaining per-op decomposition (A/B'd,
2026-09-02):

- **The length mirror is load-bearing — cannot drop it.** A no-op'd
  `write_length_mirror` corrupts `length` once a string prop lands
  (probe: `a[2]=3 len=0`) and collapses `buildString full` to 302µs with
  a false result-ok. Reverted.
- **An `is_empty()` guard on the mirror is noise** (181–183ms vs
  176–180ms — the shared borrow costs the skipped mut borrow). Reverted.
- The append path is a broad sum of small costs (elements borrow+push,
  mirror borrow, generation bump, chain-clean hit, the caller's
  nullish/kind/key checks, the register store's discarded result push,
  the PostInc `update_value`) — no single 5ns+ hog remains on the interp
  side. The remaining interp gap to node jitless (3.4x) is this per-op
  floor.
- **The JIT row's remaining lever is the Phase C inline store** —
  `fast_array_element_write` is still an FFI call per element. Inlining
  the dense append in machine code (offset_of! into `ArraySlots` +
  RefCell + Vec-push discipline, call_slow on the rare path) is the next
  M1 slice; the RefCell/Vec/realloc surface makes it a UB-sensitive
  slice worth its own session.

**Status update (2026-09-02): M1 C slice 1 landed — the inline dense-
append gate.** A new pub `JsObject::array_dense` cell (the ArraySlots box
base, set at `array_create`, cleared on spill — the JIT reads it via
`offset_of!`, sidestepping the `ObjectKind` enum-layout problem) gates a
compiled computed-store fast path shared by the step `AssignMemberComputed`
and the register `StoreMemberComputed` emissions: the machine code checks
the object tag, `array_dense`, the canonical-index key (a separate
is-double gate block — `fcvt_to_uint` on a NaN-boxed heap key's bits traps
in the lowering; found by the string-key crash bisect), and `index ==
slots.length`, then runs the stateful append (extensibility, chain-clean,
push, length + mirror, generation) through a new narrow `dense_array_append`
helper (the four-file mirror). Typed arrays / updates / hole-fills /
spills / non-canonical keys fall through to the existing
`fast_array_element_write` / `assign_member_computed`. Measured:
**buildString shape jit 52.9 → 42.9ms (~19%, 3-run stable)** — the
register-path store previously went straight to `call_slow(SetMemberComputed)`;
no other row moved. Validation: the `installed_jit_dense_array_element_write_fast_path`
e2e now counts `dense_array_append` (was `fast_array_element_write`), a new
`installed_jit_register_store_member_computed_takes_the_inline_append` e2e
covers the register gate, clippy clean, workspace tests green (incl. the
fallback-in-a-compiled-loop crash regression), and the new paths are clean
under `--gc-stress` (the `array_dense` handle needs no trace — it is always
reachable via `kind`). The remaining M1 C work is the deeper inline
(RefCell/Vec push in machine code + the chain-clean/mirror inline, the
`Option<Value>` 16-byte stride, and the free NaN tag for a hole sentinel).

### M2 — Vector-form calls without the rebuild (both modes)

`Step::Call`'s handler does `args.split_off(base)` (a `Vec` alloc) and
rebuilds the `[this, callee, args]` fast layout per call; the JIT's
`call_vector` mirrors it. The compiler knows the arg count at compile
time — emit the vector form's args **directly in fast layout** for plain
(non-spread) vector calls, so `do_call`/`call_vector` read them in place.
The `vector leaf call` row is the 33-arg case (the fast cap is 32, so it
always takes this path).

- **Expected:** vector leaf call interp 45→~25ms, jit 30→~15ms.
- **Risk:** low-medium — the interpreter already builds this exact
  layout; the change is where it is built.

### M3 — Single fused member-read probe (interp)

`member_cell_get` probes the map fast path (`member_cell_get_map`), then
`value_cell` — so a warm loop's read is a map read + in-fields access + a
per-read value-cell write. Fold them into one check (or reorder per M0's
measurement) and shave the per-read dispatch
on the register path (`GetMemberNameLocal`). Feeds property read
(54.5→~30ms), compound assign, and every member-heavy loop.

- **Expected:** property read 4.4x→~2.4x, compound assign 19.7→~15ms.
- **Risk:** low — invalidation is already generation-validated; do not
  drop a bump (Cut 35 slice 11 rule: every own-property mutation path
  must bump).

**Status (2026-09-02): slice 1 landed — the value cell is probed first.**
The warm read path is now a pure (id, name, generation) compare (no map
read, no in-fields access, no per-read write); the map probe runs only
on a value-cell miss and still warms the cell. A/B (alternating
builds): `property read` interp 54.3→50.8ms median (~6.5%, non-
overlapping across 6 runs each) and the 5M-iteration probe 271→252ms
(~7%); JIT row unchanged; global read and compound assign unchanged.
Behavior-preserving (both caches serve the same own-data property, both
revalidated); clippy clean, workspace tests green. The remaining
`GetMemberNameLocal` dispatch cost and compound-assign write side are
the next slices.

### M4 — JIT: statically-known leaf calls jump in place

The single biggest JIT lever. Every call site runs the leaf-call
protocol per iteration (probe → frame → completion round-trip) even when
the callee is a stable frame-slot/global certified leaf —
`jit-report.md` §7 item 4 flags "skipping the probe for the
statically-known case" as future work. Level 1 (no body copying): the
machine code re-validates the cached callee's identity against the
slot/global cell (a `Value` bits compare, the `TailCallSelfCheck`
pattern), then runs the leaf's compiled body on the same Vm, skipping
the probe + fresh-frame + completion. The register-leaf `CallerSlots`
alias (Cut 35 slice 23) is the frame-discipline blueprint.

- **Expected:** function calls 1.9→~0.5ms, apply 18.3→~8ms, vector leaf
  call (with M2) 15→~8ms.
- **Risk:** medium — the leaf-eligibility gate (re-validation must mirror
  `can_inline_leaf`) and the frame discipline. Body inlining with
  dead-arg elimination (the last factor to node's 0.12ms) is the
  explicitly long-term follow-up.

### M5 — JIT: inline typed-array store + bounds elision

The `typed-array write` row (103x) is the canonical
`for (k = 0; k < ta.length; k++) ta[k] = k & 255` shape: the loop guard
**is** the bounds check, yet every store calls the
`fast_array_element_write` → `typed_array_element_set` FFI helper which
re-checks the view + bounds + encodes. Three steps: (a) recognize the
guard-shaped store (store index == loop counter, guard on the same
length, call-free body so the view cannot detach/mutate mid-loop); (b)
elide the per-store re-check; (c) inline the store as machine code
(`offset_of!` into `TypedArraySlots` — the dense-elements Phase C
pattern; the encode is already allocation-free).

- **Expected:** 30.2→~8ms (~28x gap, without SIMD).
- **Risk:** medium — the soundness argument is guard-identity + call-free
  body; the `encode_element_into` primitive-only gate already exists.

### M6 — JIT: loop-invariant code motion

The mechanism behind three wide rows (property read 21.8x, global read
12.1x, typed-array length 24.3x): V8 hoists `o.a`/`g`/`ta.length` out of
the loop because the body never writes them. Start with the safe subset:
hoist a `GetMemberName*`/`LoadGlobal`/`typed_array_length` read to a
pre-head temp when its operands are loop-invariant, the receiver is
never written in the loop, and the body contains no calls (no
alias/escape). Compose with the register-op machinery (a hoisted temp is
a frame-slot or machine-local).

- **Expected:** property read 6.9→~1ms, global read 3.8→~0.5ms,
  typed-array length 11.4→~1ms.
- **Risk:** high effort (the first real dataflow optimization in the
  JIT); the soundness rule is "no write to the receiver and no call in
  the body".

### M7 — Loop unrolling + register quality

`compound assign`'s 41.5x is largely V8 keeping `o` in a register across
an unrolled body (the value does change, so no LICM). Check what
Cranelift's `opt_level`/settings give on the certified loop head first
(cheap), then manual 2-4x unroll of the fused head if needed. Helps
arithmetic (5.7x), compound assign, buildString.

### M8 — Arena allocation (interp)

`docs/gc-plan.md`'s remaining lever: `Gc::new` heavier than `Rc::new`;
recovers the string-concat/construct-churn ~2x regressions. The rope
append allocates a box per append (100K for the string-concat row); a
bump arena + the small-string path (Cut 67) cuts the alloc + `Rc` bump.

- **Expected:** string concat 10.6→~6ms (7.1x→~4x).

### Longer tail

- **M9 — fused compound member op** (`o.x += 1` as one register op:
  generation-validated cell read + add + store back): compound assign
  interp 19.7→~10ms.
- **M10 — apply arg-list copy**: `create_list_from_array_like`'s dense
  path still copies; inline the known `.apply` into a vector call (the
  M4 + M2 combination): apply interp 20.4→~12ms.

## 5. Sequencing

1. **Week 1:** M0 profile → M1 Phase A (dense-elements hot paths) → M2
   (vector layout — small, both modes).
2. **Then:** M3 (interp probe), M4 (JIT call inline — the call rows), M5
   (typed-array store).
3. **Then:** M6 LICM, M7 unroll, M8 arena.

Priorities weight "both modes" (M1, M2) and the widest rows first; a
re-cut toward the interpreter median (or the JIT) is a stated option.

## 6. Tracking & methodology

- **Gate:** the §1 table in `docs/perf.md` (measured 2026-09-02). After
  each milestone, re-run `--jit-bench` and `tools/jit_bench/node_bench.js`
  in both modes and append a dated row.
- **A/B protocol (the machine swings ±15%; judge only multi-run
  deltas):** alternate base/new order per pair, min-of-3+ runs, prefer
  the isolated 5M-iteration probe over the full bench to amplify the
  signal above load noise (the slice-19 order-bias lesson).
- **Validation per milestone:** `cargo clippy --workspace --all-targets --
  -D warnings` clean, `cargo test --workspace` green, then the JIT and
  `--jitless` sweeps at zero regressions; new e2e/unit tests for the
  soundness edges (spill triggers, guard identity, LICM write/alias
  rules, leaf eligibility).
