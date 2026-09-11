# V8-shaped arrays: Smi integers + elements kinds

**Status: SCOPE (not started).** This is the large-risky-change option, scoped
as a project of its own. It is the successor to `.notes/dense-store-redesign.md`
(Stage 1 — the prototype-elements protector — landed; Stages 2-4 were shelved).
It is *not* a shortcut for the `buildString shape` row, and it does not inherit
that row's validation-only risk profile: it changes the representation of every
JS value.

## Why this is a project, not a tweak

The row after Stage 1: `buildString shape` ~13.4 ms / 3M iterations = **~4.5
ns/iter**; node ~7.6 ms = **~2.5 ns/iter**. The gap is ~1.9 ns/iter.

The store itself is now within ~1.5x of V8's shape. The remaining gap is split
between (a) the store's residual gate and stores, and (b) the *loop
scaffolding*, which is f64 in Slag and Smi in V8:

- `i++`, `l++`, `l === 10000`, and the `i < 3000000` limit are all `f64`
  add/compare in Slag (`emit_update`/`inc_counter`/`emit_fast_loop_head` are
  `fadd` on bitcast doubles; the interpreter's inline arithmetic is
  `Value::Number(num + imm)`).
- The array key `l++` therefore arrives as a Number and must be validated with
  an `f64 -> u64 -> f64` round trip (`fcvt_to_uint_sat` + `fcvt_from_uint` +
  `fcmp`), because Slag has no integer representation.

Stage 2 (a one-byte kind that folds the `[[Extensible]]`/`is_prototype` guard)
was probed at a **~1.5% ceiling** and shelved — see the redesign note. So the
remaining levers are the ones below, and they are large because they change
`Value`.

## Honest target

**~8-11 ms (`buildString shape`), i.e. ~1.1-1.4x node** — every identified
piece is addressable, but parity is *not* guaranteed: the loop scaffolding, the
NaN-boxed-double fallback for non-integers, and cache/branch overhead outside
the redesign's reach leave a floor. Treat "match node" as a stretch goal that
this project makes *possible*, not as its acceptance criterion.

## Workstreams

### WS1 — Smi integers (the Value-representation change)

Add a tag (tag 11 in the reserved range, currently free) holding a signed
44-bit integer in the payload, with values outside `[-2^43, 2^43)` staying as
doubles. Tag 11 is already safe for the conservative stack scan:
`Value::encoded_box_address` accepts only tags `TAG_BIGINT..=TAG_FUNCTION`
(4..=8), so a Smi payload is never chased as a pointer.

The work is not the encoding — it is making integer *producers* emit Smis and
every integer consumer handle them:

- Arithmetic fast paths, interpreter and JIT: `Vm::binary_inline`, the
  `BinaryOp` + immediate arms in `run_inner_inner` / `run_leaf_ops`, and the
  JIT's `emit_binary` / `emit_binary_known` / `emit_arith` /
  `emit_num_slot_rmw` / `emit_update` / `emit_fast_loop_head` /
  `emit_leaf_op` / `inc_counter`. Each `fadd`-family site gains a Smi path
  with overflow promotion to double.
- The fast-loop machinery (`FastLoopVar`, `RelLimit`, `NumRhs::{Acc,Imm,
  Counter,CounterImm,Slot}`) is f64-typed throughout; the loop-limit analogue
  of `NumRhs::Slot` for Smi counters is the piece that removes the f64 loop
  compare.
- `Value::kind()` / `is_number` / `as_number` / `to_number` / `ValueKind`.

### WS2 — Smi array keys

With WS1, the compiled append's key gate becomes a tag check + payload read
instead of the f64 round trip (`emit_dense_array_append_inline`'s
`key_ok_check` block). The interpreter's `array_element_write` /
`dense_index_define` accept an integer key without re-parsing. This is where
the ~0.25 ns/iter key cost goes.

### WS3 — Elements kinds (the one-byte gate)

Give `ArraySlots` a `kind: Cell<u8>` (PACKED / HOLEY x SMI / DOUBLE / ELEMENTS,
plus DICTIONARY and SPILLED) replacing the `dense: Cell<bool>` flag and
carrying the `[[Extensible]]` / `is_prototype` fast-path bits, so the compiled
gate is one kind compare plus a bounds branch instead of the `array_dense`
pointer test, the `[[Extensible]]` load, the `is_prototype` load, and the
capacity pair. Note the Stage 2 probe: the *guard* folds alone cap at ~1.5%;
the kind's value is folding the whole gate, which only pays once the key path
(WS2) is also cheap.

The trap is transitions: every dense-mode transition (spill, hole creation,
`length` grow/shrink, non-w/e/c define, freeze) must maintain the kind, and a
miss is a silent wrong element. `spill_dense_array`, `array_element_write(_dense)`,
`dense_index_define`, `array_set_length`, `array_pop_dense`, `delete_key`.

### WS4 — Remove the per-store generation bump

The compiled append writes `generation += 1` to invalidate the generation-keyed
read caches (`array_element_value_cells`, `array_length_cells`, the
`member_value_cells` entry for `length`, the for-of verdicts). It measures
~0.06 ns/iter — small on its own, but it is the reason the length read cannot
be cached. A realm/epic epoch invalidation (the Stage 1 protector is a working
precedent) could remove both the store and the cache-miss on `length`.

### WS5 — (optional) loop-guard hoisting / versioning

The f64 loop scaffolding is the other half of the gap. Smi counters (WS1)
shrink it; hoisting or versioning the loop guard for call-free bodies would
shrink it further, without a global invariant. Scoped separately because the
`readonly` route was rejected (the compiled body's own helper fallback writes
the cells it would read — see the skill).

## Measured migration surface

Sized with `grep` over `crates/` at the Stage 1 tree:

| surface | count |
|---|---|
| `Value::Number(` construction sites | 1,461 |
| `.as_number()` call sites | 239 |
| `ValueKind::Number` matches | 107 |
| `.kind()` call sites (813 runtime, 57 crux) | 901 |
| NaN-box tag-constant references | 153 |
| `f64` mentions / `as f64` casts | 1,307 / 487 |
| `generation` references (all crates) | 482 |

Not all are on the critical path, but the counts set the review and
regression budget: this is a whole-engine change, and the JIT's inline
offsets/tag compares (`crux::TAG_*`, `offset_of!`) are a frozen ABI that moves
with it.

## Increment order (each lands green and measured)

1. **WS1a — additive Smi, double still authoritative.** Add the tag,
   constructors, `kind()`/`is_number`, and tracing/scanning. No producer emits
   Smis yet; the row does not move. Gate: all three sweeps + the battery.
2. **WS1b — integer literals and `++`/compound counters emit Smis**, with
   overflow promotion. First measurable win on the loop scaffolding. Gate:
   new Smi-range/overflow fixtures + the battery + the sweeps.
3. **WS2 — the array key path takes a Smi** (compiled gate + interpreter).
   Gate: the dense-store fixture suite + the sweeps.
4. **WS3 — elements kinds**, one kind compare for the gate. Gate: the Stage 1
   invalidation fixtures (the guard bits move into the kind) + the sweeps.
5. **WS4 — epoch invalidation of the read caches**, drop the per-store bump.
   Gate: the mirror/length readers, `--gc-stress`, the sweeps.
6. **WS5 — loop-guard hoisting** (separate decision after WS4's number).

Each increment is independently revertible; the benchmark is reported after
each and the project stops when the payoff flattens (the Stage 1-2 precedent).

## Risk register

- **Silent wrong results are the whole danger.** Smi overflow promotion, an
  elements-kind transition that lies, a stale read cache. Mitigation: the
  interpreter/slow path stays exact and unchanged per increment, the existing
  differential battery under jit / `--jitless` / `--gc-stress` and vs node is
  the backstop, and every increment adds targeted fixtures.
- **GC / stack scanning.** A Smi must never be chased as a pointer. The tag
  range in `encoded_box_address` already excludes tag 11; a new tag must keep
  that invariant (and any future precise-value stack map must branch on the
  tag).
- **The JIT's frozen ABI.** Offset/tag constants are shared between `crux` and
  `jit`; the value layout change touches both at once.
- **Sweep breadth.** This touches every value producer and consumer, so all
  three areas plus `intl402` must be swept per increment, not just the array
  clusters.
- **Effort.** Multi-week. Not justified for one benchmark row unless the
  operator wants the engine-level change on its own merits.

## Validation protocol (per increment)

1. `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test
   --workspace`.
2. The differential battery (`scratch/battery.js`, `battery_gc.js`) under jit /
   `--jitless` / `--gc-stress`, byte-identical, and vs node.
3. The Stage 1 invalidation fixtures (`scratch/stage1_proto.js`) plus new
   Smi-range/overflow and elements-kind-transition fixtures.
4. All three sweeps (`language`, `built-ins`, `annexB`) on a freshly built
   `sweep.exe`, diffing the fail+crash union against the pre-increment
   baseline (`scratch/sl|sa|sb.json`).
5. Report the row; stop when the payoff flattens.

## Non-goals

- Parity as a hard requirement (see "Honest target").
- A dictionary/elements-kind fallback redesign beyond what WS3's transitions
  need; the existing spill-to-properties path stays the correctness backstop.
- Changing the NaN-boxing *layout* beyond adding the Smi tag — the frozen-ABI
  constants stay stable.
