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

**Status (2026-09-02): the fast-argument cap was raised 32 → 64 (the
8→16→32 pattern), delivering the measured win.** Isolated A/B (200K
calls, same loop shape): the 32-arg fast form runs ~29x faster than the
33-arg vector form in the JIT (1ms vs 29ms) and ~1.9x interpreted (22ms
vs 42ms) — the vector form's `ArgsBase`/`ArgsPush` per-arg protocol
(~35 FFI calls per call in the JIT) and the `split_off` rebuild were the
cost. With the cap at 64, the 33-arg row takes the one-step fast form:
**`vector leaf call` (renamed `wide leaf call`) interp 42→21ms (2.1x),
jit 29→3.2ms (9x)**, no other row moved. The two `[Value;
FAST_CALL_MAX_ARGS]` buffers (`do_call_apply`, `run_inline_leaf`) grow to
512B each. Tests updated: `wide_fast_form_calls_stay_spec_exact` gained
33-arg (fast) and 65-arg (vector) cases, and the two vector e2e tests
(self tail call, tail-call chain) moved to 65 args so they still exercise
the vector machinery above the cap. Clippy clean, workspace tests green.
The remaining slow shape is a 65+-arg plain call (still the vector
form) — the in-stack-vector follow-up if it matters.

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

**Status (2026-09-02): the premise is superseded by measurement — the
direct-call path is already near its protocol floor, and the plan's
row targets were based on harness-inflated numbers.** 1M-iteration
steady-state A/B (the exact `function calls` row shape measured 7ns/call,
not the harness's 19ns — the `--jit-bench` single-timed-eval methodology
re-creates the callee function per eval, re-probing the per-site leaf
cache): member-callee 7ns/call, direct global 6ns, slot/param 5ns. The
Cut 39/68 per-site leaf-cache gate already skips the probe on warm
repeat visits; what remains per call is the gate itself (~6 loads + 5
compares) + the in-frame leaf run — a ~1-2ns ceiling for Level 1's
"skip the gate for constant callees" on shapes that are already fast.
The real remaining call-row cost is the **apply/call machinery**: the
`apply leaf call` row decomposes to ~90ns/call (vs 10ns for a direct
9-arg call) — the `.apply`/`.call` member read (the prototype chain) +
the builtin round-trip + `create_list_from_array_like`'s per-call copy
(the plan's M10 lever, not M4). Re-scope M4 to the apply/call machinery
or defer to body inlining (the long-term item).

**Harness fix (2026-09-02): `bench_once` now measures steady state.** The
old single-timed-eval methodology re-parsed the snippet per eval, so the
timed window included a fresh ~1ms Cranelift compile of the re-created
bodies (and the interp column's timed eval ran under the GC pressure of
the warmup eval's garbage on allocation-heavy rows). The harness now
evacuates the definition once, binds `bench` + the ARGS to globals once
(function-literal arguments stay the SAME object across calls), warms
with 2 calls, and reports the min of 3 timed calls. The compile
inflation and the GC skew are gone: `function calls` jit 1.9→0.71ms
(the measured ~7ns/call steady), `wide leaf call` 3.2→1.6ms, arithmetic
3.3→2.6ms, and `string concat` interp 10.6→3.6ms (the old number was
GC-polluted — the isolated steady probe confirms 4-5ms). The 2026-09-02
table's small-row JIT gaps were therefore 15-30% pessimistic and the
call rows ~2.7x so.

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

**Decomposition (2026-09-02, 800K stores, the row's own shape):** the
row is `bench(new Uint8Array(800000))` with `ta.length` re-read per
iteration. Three temporary `--jit-bench` rows isolated the per-iteration
costs (jit, min-of-3):

| probe | jit | per-iter |
|---|---|---|
| `ta[k] = k & 255` reading `ta.length` in the test (the row) | ~30ms | 37.5ns |
| same with the length hoisted (`var n = ta.length`) | ~26ms | 32.8ns |
| hoisted length, no store (`s += k`) | ~2.0ms | 2.5ns |

So: the certified counter loop + `k & 255` + `s += k` floor is ~2.5ns/iter;
per-iteration `ta.length` (the compiled `typed_array_length` probe — an
FFI per iteration, M6 LICM territory) is ~5ns/iter; and the STORE is
~30ns/iter — the machine dense-append gate (fails fast for a typed
array) + the `fast_array_element_write` FFI round trip + inside
`typed_array_element_set`: the immutable-buffer check, the
`encode_element_into` element-type dispatch + Number→bytes conversion,
`typed_array_valid_index`, and the `SharedBuffer` write. The row is
~99x vs node's 0.29ms, and the plan's ~8ms target needs the store at
~10ns/iter — a real machine-code inline (M5c), not a cheaper helper
restructure (the FFI + checks floor is ~20ns/iter). M5c is the
UB-sensitive inline the M1 C note warned about (reading the
`TypedArraySlots`/`SharedBuffer` internals — resizable buffers realloc
their storage, so the data pointer must be re-validated against the live
buffer per store) and is its own session.

**M5c status (2026-09-02):** the machine-code inline landed
(`emit_typed_array_store_inline`, shared by the step `AssignMemberComputed`
and the register `StoreMemberComputed` emissions): gate the receiver to a
fixed-length `Uint8Array` over a live, writable, non-resizable buffer (the
`JsObject.typed_array` mirror → `TypedArraySlots`, the shared per-buffer
`BlockState` box for the byte base + the detached/immutable/resizable
flags), the key to a canonical in-range index, the value to an integral
[0, 255] Number; then write the byte straight into the block. Any gate
failure falls back to the existing helper (nothing observable ran — the
write is a pure byte store and the accepted value is a Number). The gate
re-reads the live geometry per store (a helper that detaches/freezes/
resizes between stores is picked up); the data base is mirrored in
`BlockState` (`SharedBuffer::state` — an offset-visible raw box address,
since the `Rc`/`Arc` box layout is not `offset_of!`-expressible across
crates) and updated on resize; the whole probe is cfg-collapsed to the
legacy jump under the `workers` feature (`crux::typed_array::WORKERS` —
the plain machine write would need atomics there). Measured (jit,
min-of-3, 800K stores): the row 26.0→**16.8ms** (~57x vs node, ratio
0.37→0.23); the hoisted-length variant 13.3→**12.4ms**. The per-store
probe is ~13ns/iter — the remaining cost is the per-store re-derivation
of the slots/buffer geometry + the per-iteration `ta.length` probe, both
loop-invariant work for M6 (LICM), not the FFI/encode/checks the inline
replaced (the fallback still measures ~21.5ns/iter for the same shape).
Validation: clippy clean, `cargo test --workspace` green, JIT language
(23721/0/0/0) + JIT built-ins (23657/0/0/0) + jitless built-ins
(23657/0/0/0) sweeps match baseline.

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

**M6 slice 1 status (2026-09-02):** the machine typed-array length read
landed (`emit_typed_array_length_inline`, in `emit_member_cell_read` —
every compiled `GetMemberName` whose name is `length` now probes the
receiver's `typed_array` mirror + fixed-view state first and serves
`slots.array_length` straight from the box, ~2ns, instead of the ~5ns
FFI `typed_array_length` round trip; the FFI probe still covers the
auto/detached/resizable/own-length-shadow misses). Two real bugs were
found and fixed along the way: (1) the FFI probe ignored an own
`length` data property shadowing the %TypedArray%.prototype accessor —
a JIT-compiled `ta.length` read on a defineProperty'd typed array
returned the slots length (e.g. 8) where the interpreter returned the
own value (3); the probe now gates on `has_own_property_atom` like the
interpreter's shortcut, and defining an own `length` clears the
`typed_array` mirror (`typed_array_define_own_property`) so the machine
read/store gates miss to the exact helpers thereafter. (2) both the M5c
store gate and the new read gate AND-ed the block flags together
(`(detached & immutable & resizable) == 0` — only missed when ALL were
set), so detached/auto/immutable views slipped through to the machine
paths; both gates now miss when ANY flag is set. Measured (jit, min-of-3,
800K): typed-array length 10.2→**6.6ms** (ratio 0.18→0.12); typed-array
write 16.8→**15.5ms** (its per-iteration guard read is the same probe).
The residual ~8ns/iter (two ~2ns reads + the general-loop test/dispatch
overhead) is the actual hoisting work — the reads are loop-invariant and
still re-execute per iteration; that needs the pre-head temp + certified-
loop rewrite (the "high effort" slice above), not more read-side
cheapening. Validation: clippy clean (incl. the workers cfg), `cargo test
--workspace` green, JIT built-ins (23657/0/0/0) + jitless built-ins
(23657/0/0/0) sweeps match baseline; differential scripts (shadowed/
detached/auto/resizable/byte-offset/subarray/SAB length reads and the
store edge cases) agree between JIT and the interpreter.

**M6 slice 2 design + decomposition (2026-09-02, measured ceilings):**
the remaining ~8ns/iter is the two loop-invariant `ta.length` reads
still re-executing per iteration. The full hoist's prize, measured by
source-hoisting the length into a var (which turns the general loop
into the fused canonical loop with a `RelLimit::Slot` limit):

| probe (jit, min-of-3) | row | per-iter |
|---|---|---|
| `for k < ta.length: s += ta.length` (the row) | 6.6ms | 8.2ns |
| guard hoisted (`n = ta.length`), body reads `n` | 2.0ms | 2.5ns |
| guard hoisted, body reads `ta.length` | 3.2ms | 4.0ns |
| write row (guard only; body = `ta[k] = k & 255`) | 15.5ms | — |
| write row, guard hoisted | 12.4ms | — |

So the ceiling: length row 6.6→2.0ms (full hoist) or →3.2ms (guard
only); write row 15.5→12.4ms (guard only — its body has no length
reads). Design for the slice: a runtime-guarded once-per-loop hoist of
the loop TEST only (guard-only; body reads stay per-iteration — exact),
for certified `for (var K = INIT; K <op> RECV.length; K++)` loops where
RECV is a frame-slot binding never assigned in the loop and the
body/update are "length-pure" (no explicit calls — nothing can detach/
resize/define-`length` — and no member access except `RECV.length`
reads and `RECV[expr] = v` element stores, which cannot change the
accessor-served length; other receivers could alias RECV through a
global, so they are excluded).

Emission shape (per hoisted loop): a guard evaluates `RECV.length` ONCE
via the probe semantics (IntegerIndexed + no own `length` — the exact
fixed FFI probe / slice-1 machine gate) into a NEW lazily-allocated
hidden frame slot (`Compiler.scope` is OWNED — `frame_size`/`tdz_store`
can grow mid-compile before any call runs, so no pre-scan is needed; a
synthetic `\0`-prefixed AtomId maps to the slot so the fast loop's
synthetic `K <op> HIDDEN` test resolves `RelLimit::Slot` and takes the
existing fused canonical loop). On a probe MISS (any other receiver,
an own-`length` shadow, auto/detached views) the loop re-runs as the
general per-iteration loop (unchanged semantics). The fast + general
loops are separate emissions (their bottoms differ); bodies compile
identically in both (body member reads re-resolve exactly). A new
label-fixup step (`TypedArrayLengthHoist { target }`, the
`JumpIfRelLimit` pattern: Step variant + Fixup + interpreter arm +
JIT emit arm via the existing `TypedArrayLength` FFI + sentinel) pops
the receiver and pushes the length on a probe hit.

**M6 slice 2 status (2026-09-02):** the guard-only hoist landed for
certified `for (var K = INIT; K <op> RECV.length; K++)` loops (the
emission shape above: `Step::TypedArrayLengthHoist` guard + hidden
hoist slot via `alloc_hoist_slot`, the synthetic `\0hoist<N>` binding
resolving `RelLimit::Slot`, and two `compile_for` copies; the step's
interpreter arm mirrors the FFI probe, its JIT arm lowers through the
`TypedArrayLength` FFI + sentinel). One soundness bug was found and
fixed during differential testing: the fast copy probes RECV BEFORE
the head init runs, so a head initializer that plainly assigns RECV
(`for (var k = (ta = other, 0); k < ta.length; …)` with ta/other
frame-slot bindings) left the guard's hoisted length stale — the
general loop's first test reads the post-init length, the hoisted copy
never re-reads it; `hoistable_length_loop` now scans the head
declarator initializers with `collect_assigned_expr` alongside the
body and update. Measured (jit, min-of-3, 800K): typed-array length
6.6→**3.19ms** (interp 56.3→25.3 — the step-level transform is
shared) and typed-array write 15.6→**12.47ms** (interp 74.1→42.6) —
both rows at the guard-only ceilings in the table above. Validation:
clippy clean, `cargo test --workspace` green, JIT + jitless language
(23721/0/0/0) and built-ins (23657/0/0/0) sweeps match baseline; the
16-case differential (canonical length/write rows, own-`length`
shadow, impure head init, head/update RECV reassigns, break/continue
bodies, member-left/`<=`/`>=` forms, alias reads, nested loops,
Float64, zero-length) agrees between JIT and the interpreter with
hand-traced results.

Both follow-up bugs were investigated and FIXED (2026-09-02): (1) the
JIT leaf-inlined run of a certified body whose fast/general loop
contains a `break`/`continue` out of the loop corrupted the CALLER (the
call after the leaf returned blank output / hit wrong functions — the
machine-state symptom the slice's body purity had avoided, reached by
the plain fused loop) — `steps_are_leaf` now excludes `Break`/
`Continue`, so such bodies run the general path with their own
frame/buffer, which is exact; (2) the acc path syncs its counter to the
binding with a single `FastLoopStore` at the loop's END step, and a
labeled `break`/`continue` to a label OUTSIDE the body jumps past that
step — the binding then keeps its pre-loop value, observable after the
transfer (`outer: for (var k = 0; k < n; k++) { ... break outer; }`
leaves k = 0) — the acc decision now rejects bodies whose labeled
transfers leave the loop (`acc_body_label_transfers_inside`), falling
back to the slot path, whose head writes the binding every iteration.
The doc's original "Bug 2" — element-read counter loops with a
non-canonical test shape (`k >= 0`, `ta.length > k`) returning 0 in
both modes — was a differential ARTIFACT, not a defect: those
differential cases summed a ZERO-FILLED `new Uint8Array(1000)`, so 0
was correct; with a filled array both shapes compute correctly in JIT
and jitless.

Body-read hoisting (length row 3.2→2.0ms, the last 1.2ms) is M6 slice
3: compile member reads of `RECV.length` in the fast body as
`LoadLocal(HIDDEN)` — a compile-time hook in the member path gated on
the loop's guard having passed. Validation for each slice: clippy +
workspace tests + JIT/jitless built-ins sweeps + differential scripts.

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

**M10 decomposition (2026-09-02, 200K jit):** direct leaf call 5ns/call;
recognized `.call` (fixed args) 70ns; recognized `.apply` with a dense
9-element array 85ns (the element handling adds only ~1.7ns/element via
the dense fast path); an UNRECOGNIZED apply shape (`g = f.apply` hoisted,
then `g.call(f, null, arr)`) 840ns — so the compiled `CallApply`
recognition already buys ~10x, and the residual ~70-80ns is the FIXED
machinery: the member read of `.apply`/`.call` (a Function.prototype
chain read), the `call_slow(CallApply)` round trip, the argument-region
copy + `[thisArg, f, rest...]` layout rebuild, and `do_call_fast`. The
interp apply steady-state (the harness-fix numbers) is ~102ns/call vs
the direct ~50ns.

**Next-slice design (the JIT inlines the recognized shape):** for a
compiled `CallApply` whose member read resolved the realm's intrinsic
(compare `resolved` against the intrinsic's cached identity — the
compiler already knows the pattern; the fallback is the current slow
path), rebuild the fast layout in machine code (drop the resolved
`apply`/`call`, move `thisArg` before `f`) and route into the
`emit_call` leaf-inline machinery — skipping the `call_slow` round trip
and the helper's re-checks. The dense-`arr` apply then extends the
leaf-inline probe to read the array's buffer directly. This is the
~70ns → ~15-20ns slice; it is a compiler+runtime slice of its own
session.

**Status (2026-09-02): slice 1 LANDED — the compiled intrinsic fast
path.** The `Step::CallApply` arm now compares the member-read result
against the realm's intrinsic bits (`JitCallContext` snapshots
`apply_builtin`/`call_builtin` per run, gated on a new
`CompiledBody::has_call_apply` flag — leaf/resume ctxs included, since a
leaf body can contain the step) plus a Function-tag receiver gate, then
rebuilds the direct-call layout in machine code: `.call` (fixed args) is
a pure region shift, `.apply` routes a nullish argArray to a zero-arg
call and copies a dense Array's elements to the buffer top via a new
`apply_args_fill` helper (the one heap read the machine code cannot do;
rejects — nothing written — on non-dense/too-long/no-room shapes), and
both route into `emit_call` (which now takes a runtime `argc` value).
The fallback (shadowed `apply`/`call`, non-Function receiver, non-dense
argArray) is the unchanged `call_apply` slow path — `do_call_apply`'s
exact TypeError for a non-callable receiver is preserved by the
Function gate. Measured: **apply leaf call jit 16.5 → ~7.0ms** (4 runs;
interp unchanged ~20.8). Validation: 5 new e2e tests (counting wrappers
prove the dense fill runs per iteration with zero `call_apply` calls,
`.call` needs no helpers at all, a shadowed `apply` runs the slow path
per iteration, the non-Function receiver keeps `do_call_apply`'s
message, and the nullish/empty/array-like shapes stay correct), clippy
clean, workspace tests green. The residual ~35ns/call on the row is the
member read of `.apply` (a separate step) plus the per-iteration fill
copy — the next slice is inlining the member read / skipping the fill on
a generation-validated repeat.

**Known pre-existing JIT bug (reproduced at HEAD, FIXED 2026-09-02):**
a compiled body that throws a call error (a non-function callee, or a
callee body that throws) into its OWN catch inside a loop, many
iterations (≈200+), panicked "a pending JIT error is present" — the
covered-error dispatch lost the ctx error. Root cause: a helper error
inside a try leaves the erroring step's operands on the working stack
(the interpreter's covered error keeps them on its growing Vec — an
invisible leak, confirmed by instrumentation); the JIT's FIXED buffer
mirrored the leak, so the machine sp drifted +operands per iteration
until the writes overran the buffer and corrupted the ctx (measured: sp
+16 bytes per iteration; the ctx sat ~1880 bytes above buf_end, hit
after ~125 iterations). Fix (Cut 70, JIT): the compiled `EnterTry` saves
the working-sp per handler, and each catch/finally entry step resets the
sp to it — the catch/finally regions never read try-body values, so
resuming at the try-entry depth is unobservable and bounds the buffer.
Gated off suspension bodies (a resume restores the working region into a
fresh buffer, so a pre-suspension sp is stale — the async-rejection e2e
caught that). Validation: 100K-iteration catch/finally loops pass (two
new e2e tests), the 201-fixture language/statements+expressions try
cluster passes, and the full language (23721/0/0/0) and built-ins
(23657 pass / 0 fail / 0 crash / 0 hang) sweeps are at their baselines.
The interpreter's equivalent Vec-growth leak is documented but
unchanged (bounded per call; a hot catch loop grows the stack until the
call returns).

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
