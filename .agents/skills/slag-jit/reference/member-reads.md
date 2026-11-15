# slag-jit — member reads

The value and chain cells, the map-keyed arm, the helper census.

Part of the `slag-jit` skill; [`../SKILL.md`](../SKILL.md) is the index and
carries the gates. Open the file a trap points at, not all of them.

## 19. The read-side value cells and the store discipline

`member_value_cells` is the read-side data-property value cache (16 slots,
`#[repr(C)]`, indexed `(object_id ^ atom) & (MEMBER_CELLS - 1)`). Any probe that
validates it (`emit_member_cell_probe`, and G8's computed read) must agree with
how the STORE paths keep it current, and the store discipline has two regimes:

- A STRUCTURAL change (define, delete, accessor conversion, map transition)
  bumps `JsObject::generation`, which invalidates every generation-validated
  cell.
- An IN-PLACE VALUE write does NOT bump the generation. `set_key`'s fast path
  bumps, but `JsObject::write_data_property_slot` (the interpreter's L1a warm
  store) and `JsObject::write_data_property` (the JIT's compiled store, via the
  `SetMemberSlot` helper) deliberately leave the generation put and REFRESH
  `member_value_cells[member_cell_index(id, atom)]` with the new value instead.

So a cell keyed by anything other than the atom cannot be kept current by the
store paths — they know the atom, not the key box. That is the trap G8 hit: a
computed-read cell keyed by the key Value's identity and holding the VALUE
validated the receiver generation alone, so `o["a"] = 2` after a cached
`o["a"]` served the stale value (the corpus caught it: 59 `harness` fixtures —
`verifyProperty`'s read/write/restore of a computed key). The landed shape is a
**box → atom map** (`computed_read_cells`, keyed by the key's own bits): both
sides immutable (strings are immutable, the interner is append-only), so it
never needs invalidation, and the compiled probe takes the VALUE from
`member_value_cells` — exactly the interpreter's `member_cell_get`. The probe is
three branchy blocks in `emit_element_read` (Object tag → `is_string(key)` →
box compare → `(live_id ^ atom) & (MEMBER_CELLS-1)` validated on
id/name/generation), so a String-keyed `o[k]` on a plain object is served with
no `intern` and no helper (`str_key.js` 52.7 → 10.6 ms, `control/for_in.js`
34.9 → 10.54 ms).

Corollaries for any new cell:

- If it caches a VALUE, either key it by the atom (so the store paths can
  refresh it) or make it generation-validated AND ensure no in-place write can
  change what it holds. A value cache keyed by a key box is not maintainable.
- Never assume a write bumps the generation. Check the store path first
  (`warm_store_*`, `write_data_property*`, `SetMemberSlot` — all refresh,
  none bump for an in-place value write).
- `the_emitter_and_runtime_computed_read_slots_agree` pins the slot arithmetic
  and `installed_jit_computed_read_after_inplace_store_matches_the_interpreter`
  pins the store-visibility contract; mutation-check both when the probe
  changes.

## 30. The helper-traffic instrument (both tiers)

`JIT_HELPER_STATS=1` emits, at compile time, one inline increment per helper
call indexed by `Helper as usize` into `runtime::jit::JIT_HELPER_COUNTS`; the
CLI's `run_corpus` prints the histogram at the end (`helper <idx> <count>`), and
one workload per process attributes it to that row. It is off by default, so a
default build is byte-for-byte unchanged (which is why a sweep reproduces its
number exactly).

- The per-step funnel is `compiler.rs::emit_raw_call`; the opt tier's is
  `opt_lower::call_helper` (added 2026-10-10 — before that the opt tier had no
  helper census, so a call-shaped row could not be attributed). One helper
  (`Helper::TdzError`, thrown from a TDZ site) calls `call_indirect` directly
  and is not counted; it never returns normally.
- Enumerate the `Helper` enum in `crates/jit/src/helpers.rs` to map an index
  (0-based variant order); the CLI prints the raw index.
- It is a **tool, not a fix**: enabling it slows the run (four instructions per
  call site), so measure with it unset. Read it as traffic, never as time.

`JIT_LEAF_TRACE=1` is its companion for the leaf-call lane: the first few
`leaf_call_probe` rejection reasons are printed (cached gate, so it costs one
load on the already-cold rejection path).

## 35. The prototype-chain cell: one table, one key, and who warms it

The chain cache (`MemberChainCell`, `Agent::member_chain_cells`) has TWO tables
and the compiled probes read only one of them:
`JitCallContext::member_chain_cells` points at the IDENTITY array
(`Vm::member_chain_index` = `(receiver id ^ atom) & (MEMBER_CELLS - 1)`) and
`member_chain_map_cells` at the map-keyed array
(`Vm::member_map_cell_index` = `(map id ^ atom) & …`). Every compiled chain
probe must use the identity array with the identity arithmetic — the objective
is the per-step `compiler.rs::emit_chain_cell_probe`.

- **A map-keyed key against the identity table never hits.** `opt_lower.rs`'s
  `emit_chain_probe` did exactly that (plus an entry gate that admitted only
  dense Arrays, the one receiver the per-step probe declines), so every LIFTED
  body's prototype-method read paid the helper: the same loop ran 15.4 ms with
  `SLAG_OPT=0` and 23.0 ms with the tier on. Nothing warns you — the code is
  well-formed, the cell loads are in the right order, and only the slot
  arithmetic and the table pointer disagree. When a probe "never hits", check
  the two against `runtime::ir`'s index function before touching the gates.
- **The probe is only as warm as the interpreter's recording.** The identity
  cell is written by `resolve_chain_cell`, which runs only on the FULL-Get slow
  path. `member_chain_get`'s map-keyed arm serves a receiver whose identity cell
  is cold and returns early, so `resolve_chain_cell` never runs — a receiver
  created after the map cell warmed (the second `bench()` call, the second
  iteration of `new Foo()` in a loop) never gets an identity cell and its reads
  fall to the helper FOREVER. The repair is one record in the map arm: it has
  just validated the links against this receiver's live chain (via
  `chain_cell_read`) and re-checked own-name absence, so the identity cell it
  writes is exact. Gate it — see the next bullet.
- **An unread record is a pure cost.** The write is 64 bytes on a path that is
  already the hot one, so gate it on (a) `agent.jit_hook.is_some()` — a
  `--jitless` run has no machine reader (`set_churn` +10%, `push_pop` +9%
  ungated), (b) `named_version == 0` (below), and (c) not
  `Array`/`IntegerIndexed`, which every compiled probe declines up front
  (`push_pop` +37%, `array_push` +43% ungated).
- **`named_version == 0` is the machine-readable shadow proof.** The probes'
  receiver-generation check exists to prove "no own `name` appeared since the
  recorded resolution", but an internal-slot mutation (`Set.add`, `Map.set`)
  bumps the generation without touching a named property. `named_version`
  advances on every string-keyed own define/delete (a symbol key does not), so
  `generation == cell.generation || named_version == 0` is the same proof with
  no dependence on the generation — `chain_shadow_free_without_generation`
  generalized. `crux`'s
  `object::tests::a_named_own_property_always_advances_the_named_version` pins
  the invariant it rests on; a new own-property path that forgets to bump it
  silently makes the escape unsound, which is why the escape is written only
  where a test can see the counter.
- **The seam does not cover the opt tier.** `SLAG_CHAIN_PROBE` gates
  `emit_member_read_chained` (per-step) only; `opt_lower`'s probe has no switch,
  so a same-binary A/B of the two settings proves nothing for a lifted body. Use
  the helper census (`JIT_HELPER_STATS`) or `SLAG_OPT=0` to separate the lanes,
  and remember helper counts scale with the corpus runner's `reps` batching
  (`slag-perf-measurement`).

### 35.1 The map-keyed arm and its two gates

The map-keyed chain cell (`member_chain_map_cells`) serves a receiver the
identity cell cannot: one resolution shared by every instance of a shape. Both
probes try it after the identity cell declines, and both land on the same
link/value tail (the cell pointer is a block param in Cranelift; the tail reads
`link_count`/`links` from it).

Its soundness is a *structural* proof that the receiver owns no `name`, because a
compiled hit cannot re-run `has_own_property_atom`:

1. `resolve_chain_cell` records a map cell only for a name the receiver's map does
   **not** describe (`map.find(&key).is_none()`). Without that guard a
   described-but-unwritten hole — absent on the recording instance, so it resolved
   up the chain — gets recorded, and an instance that *did* write the field then
   reads the prototype's value over its own.
2. The receiver admits only two shapes, either of which rules out a vector-only
   own key:
   - `props_deferred` — vector empty, every own key is a map descriptor. This also
     **implies Ordinary** (`define_fresh` refuses every other kind), which is why
     no kind mirror is needed: a Proxy cannot reach this gate.
   - `array_dense != 0 && PROPS_LEN_OFFSET <= 1 && named_version == 0` — a dense
     Array holds only the creation's `length` vector entry, and no string-keyed
     define or delete has happened since. **Arrays measure `named_version == 0`,
     not 1**: the creation's `length` define does not advance it and an
     array-length write is in place. Guessing `1` declines every array and looks
     exactly like an inert arm — check the helper census, not the source.

`PROPS_LEN_OFFSET` is `crux::object::PROPS_LEN_OFFSET`, the offset of the `Props`
length mirror (see `slag-object-layout`); the per-step lane must also route
dense/typed receivers *to* the map gate rather than its old straight-to-`miss`
decline, or arrays never reach it in that tier.

Two measurement notes: the array rows are 1-3 ms, so judge them isolated and
interleaved (min-of-9) or not at all; and an `opt bail`-free body means the opt
lane is in play — `SLAG_OPT=0` is how you tell whether the per-step lane has the
same arm.

### 35.2 Function receivers: the object part, and why the dispatch must branch

`f.apply`/`f.call` and every "read a property off a function" site is a chain read
whose receiver is not an `Object` value: `is_plain_object` rejects `TAG_FUNCTION`,
so the probe used to decline before touching a cell and the read went to the
helper (7.0 M calls on `calls/apply_call` alone).

A Function value's payload is a `Function` box, and its own properties and
`[[Prototype]]` live on `Function::object` — a SEPARATE box. That object part is
the identity the interpreter records (`Vm::chain_object` maps a function value to
`&function.object`), so the probe must key on it:

```text
ptr_of(obj)  = (value & PAYLOAD_MASK) << 4 + GCBOX_DATA_OFFSET
ptr_of(fn)   = load(ptr_of(obj) + offset_of!(Function, object)) + GCBOX_DATA_OFFSET
```

Both probes gate on the tag and dispatch. The shape of that dispatch matters more
than it looks:

- **Do not `select` the two pointers.** Computing both and selecting cost
  `objects/proto_read` **30%**: the function-side load is speculative on every
  ordinary-object read and touches another cache line. Branch instead, and pass
  the pointer through the branch as a Cranelift block parameter
  (`brif(plain, body, &[obj_ptr.into()], fn_gate, &[])`), so the object path's
  instructions are unchanged — re-measured at +1.0% (noise).
- **The block holding the join must not reuse the body's own block names.** The
  body already has a block called `gated` (the dense/typed gate); a second
  `gated` for the pointer join shadowed it and Cranelift panicked with
  `assertion failed: !self.is_sealed(block)` from
  `ssa::declare_block_predecessor` — a jump into an already-sealed block.
- **Both tiers need it.** `calls/apply_call`'s loop lives in the *per-step* lane
  (its `bench` body bails while the callee lifts), and `method_call`/`hof_methods`
  move with it; an opt-only widening measured apply_call -3.5% instead of -37%.
- **Correctness net:** the cell is validated by the object part's generation, so
  `g.apply = function () { return 9; }` (an own property on that same part) bumps
  it and the read returns the own function —
  `installed_jit_function_receiver_chain_read_honors_a_shadowing_apply`.

## 36. The primitive receiver's read: measured, and not a lever

The primitive-receiver reads look like the obvious next cut — a compiled
`s.length` and `n.valueOf()` decline the probe's tag gate and take the
`GetMemberName` helper every time (`opcost/prim_prop` 7.2 M calls,
`opcost/num_valueof` 1.6 M). Both are worth ~12% of their own row, because the
helper is already short and the *cell-served* alternative is not much shorter:

- `s.length` is a virtual own property of the string, so the
  `%String.prototype%` cell can never serve it (the lane excludes the atom) and
  a box-keyed length cell is not maintainable (the store paths know the atom,
  not the box — §19). `JsString` is an enum with no fixed-offset length, so the
  only cheap forms left are a dedicated helper or a header word.
- `get_member_name` short-circuits `length` on a string at its first check, so
  what remains behind the call is ~2 atom compares plus `JsString::len()`.
- `%Number.prototype%` *is* `ObjectKind::Ordinary` (created by
  `ordinary_object_create` with `boxed = Number(0.0)`), so
  `primitive_proto_data_get` does warm `(number_proto_id, valueOf)` — a compiled
  number-prototype arm would need a `number_prototype_bits` field on
  `JitCallContext` and a second lane in both probes.

Measured probe shapes, identical but for the read (`scratch/probe/`, 100k
iterations, min-of-11): own data field from `member_value_cells` **1.05 ms**,
primitive-string `.length` through the helper **1.20 ms**, `%String.prototype%`
method read through the cell **1.41 ms**. So the whole primitive-read lane is a
0.15 ms question on a 1.2 ms row. Do not re-derive it; the row's cost is the
element read and the loop, not the read (`slag-perf-measurement` has the
node-folds-`opcost` trap that made it look like an 18x gap).
