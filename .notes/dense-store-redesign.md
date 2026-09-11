# Dense-array store redesign: elements kinds + a prototype protector

**Status: Stage 1 LANDED** (prototype registry + elements protector).
Measured: `buildString shape` 15.5 → ~13.4 ms (~1.8x node), `buildString
full` ~17.2 → ~15.7 ms. Validation: `cargo test --workspace` green, clippy
clean; `scratch/stage1_proto.js` (9 warmed invalidation fixtures) and
`scratch/battery.js` / `battery_gc.js` byte-identical under
jit / `--jitless` / `--gc-stress` and vs node; the language / built-ins /
annexB sweeps match the `scratch/sl|sa|sb.json` baseline exactly (0 fail /
0 crash). Stages 2-4 remain PLAN. Supersedes the
"residual / O1 / O2 / O3" scoping in `.notes/perf.md` (the `buildString
shape` follow-up section). Supersedes nothing in
`.notes/array-store-plan.md`, which is the (resolved) Phase C record.

## Goal, non-goal, and the honest target

Goal: cut the per-store cost of `a[i] = v` on a dense array toward V8's
shape (a kind compare plus a bounds branch), by removing the two things the
current compiled gate pays that V8 does not: the **two-link prototype-chain
revalidation** and the **bundle of separate presence checks**.

Non-goal: **parity.** Our `Value` is NaN-boxed with no Smi, so a computed
index key must be validated with an `f64 -> u32 -> f64` round trip; V8's key
is already a Smi and its check is one compare. Removing that is a
`Value`-representation change (integer values as Smis) that touches every
consumer in the engine — out of scope here.

Realistic target, from the measured budget below: `buildString shape`
**15.5 ms -> ~11-13 ms (1.5-1.8x node)**, not the ~2.5 ns/iter parity line.
If that target is not worth the risk, stop before Stage 1 — stages are
ordered by payoff so the first one can be kept alone.

## Measured budget (per `buildString shape` iteration; ~5.2 ns, node ~2.5)

Each row is isolated by forcing the check's predicate true so Cranelift DCEs
its loads (see the perf.md follow-up):

| piece | cost | removable by |
|---|---|---|
| two-link chain revalidation | **~0.73-0.87 ns** | Stage 1 |
| tag + `array_dense` + length + `[[Extensible]]` + capacity checks | ~0.5 ns | Stage 2 |
| key round-trip + `< 2^32` bound | ~0.25 ns | Stage 3 (partially) |
| element + `elem_len` + `length` + `generation` stores | ~0.5 ns | Stage 4 (partially) |
| loop + `l === 10000` + counter (same order as node's) | ~1.0 ns | — |

## Stage 1 — prototype registry + elements protector — LANDED

Measured (`buildString shape`): 15.5 → ~13.4 ms; `full` ~17.2 → ~15.7 ms.
The epoch compare replaced the four chain blocks in
`emit_dense_array_append_inline`; the `is_prototype` check folded into the
`[[Extensible]]` branch predicate.

**Idea (V8's `NoElementsProtector`, done the way V8 avoids self-invalidation).**
The compiled append only needs the chain check because an own index property
on a prototype link intercepts element stores. If a realm-wide epoch
guarantees no *prototype* gained an own index property since the verdict was
recorded, the chain check collapses to one load + compare.

**Why O1 failed and this doesn't.** O1 bumped the epoch on every index-prop
creation, which does not funnel through one function, and the target loop's
own appends would bump it (self-invalidation). The registry fixes both: the
epoch bumps **only when an element/index property is added to an object that
is registered as a prototype**, and the loop's array is not one.

**Design**

1. `JsObject` gains `pub is_prototype: Cell<bool>` (false in every
   constructor). `ChainVerdict` is deleted.
2. A helper `mark_prototype(proto: Option<&Handle<JsObject>>)` sets the flag
   (conditional store: load, store only if unset) and is called from every
   `[[Prototype]]` install: the constructors that take a `prototype`
   argument (`basic_object_create_with_map`, `with_kind`, `array_create`/
   `init_ordinary`, `string_create`, `proxy_object_create`,
   `integer_indexed_object_create`, `mapped_/unmapped_arguments_object_create`,
   `module_namespace_object_create`, `is_htmldda_object_create`) and
   `JsObject::set_prototype_of`.
3. `crux` gains `pub static PROTOTYPE_EPOCH: AtomicU64` (starts at 1; 0 is
   reserved for "no verdict"). A `pub fn bump_epoch()` does a relaxed add.
4. Bump the epoch in the paths that add an own index property **when
   `self.is_prototype.get()`**: the index-define funnels
   (`create_data_property_key`'s `fresh_data_define` path, `define_property_key`
   / `ordinary_define_own_property`) and the dense element paths
   (`array_element_write_dense`, `dense_index_define`). Also bump in
   `set_prototype_of` (a link may be replaced by an object that already had
   index props).
5. `store_chain_clean: Cell<u64>` becomes the epoch at which
   `store_chain_walk` succeeded (0 = none). `store_chain_clean_hit` becomes
   an epoch compare; `store_chain_walk` still performs the exact walk (it is
   the reference and the recorder).
6. JIT (`emit_dense_array_append_inline`): replace the four chain blocks with
   one load of `PROTOTYPE_EPOCH` + compare against the array's recorded
   epoch; add a `!self.is_prototype` check so a compiled append to a
   registered prototype falls to the helper (which does the bump). Fold the
   `is_prototype` check into the existing `[[Extensible]]` branch predicate
   (measured: folding into a short predicate is fine; only long predicates
   hurt).

**Soundness argument.** A verdict exists only if the walk found every link
Ordinary/Array with no own index-keyed property. If the epoch is unchanged
since: (a) no registered prototype gained an index property (step 4), (b) no
prototype link was reassigned (step 4's `set_prototype_of` bump), so the
array's chain is still the clean one recorded, and (c) an object is
registered before it can be relied on (step 2 runs at the child's creation,
before its first store). An object that gains an index property *before* it
is registered cannot poison a verdict: the only way it becomes a link is
`set_prototype_of` (which bumps) or being a creation-time prototype (whose
child then has no verdict until a walk re-verifies it).

**JIT address-of-static note.** `crux::PROTOTYPE_EPOCH` is a plain
(non-TLS) `static`, so the JIT can materialise `addr_of!(...)` with an
`iconst` and emit an ordinary load — same mechanism as the embedded helper
pointers. A per-agent epoch would need the Agent pointer inside crux, which
crux does not have; a process-global epoch over-invalidates across agents,
which is sound.

**Risk:** moderate-high — a missed bump is a silent wrong result (a
bypassed prototype setter). Mitigated by the enumerated bump sites, the
exact `store_chain_walk` remaining as the recorder, and the fixtures below.

## Stage 2 — elements-kind byte (~0.2 ns)

Fold `array_dense` (dense-ness), `dense`, `[[Extensible]]`, and
`is_prototype` into one `kind: Cell<u8>` on `ArraySlots` (PACKED / HOLEY /
SPILLED, plus flags), keeping `array_dense` for the box pointer. The compiled
gate then does one kind compare instead of ~3 loads/branches. Touches every
dense-mode transition; landed only if Stage 1's fixtures stay green.

**Probed and shelved (2026-09-11).** `[Extensible]` and `is_prototype` are
per-object, not per-`ArraySlots`, so the realistic form is one combined
`JsObject` byte, not an `ArraySlots` kind. Forcing both guard predicates
true so Cranelift DCEs their loads moved `buildString shape` only 13.43 →
13.23 ms — the entire ceiling is ~1.5%, and a combined-byte variant captures
roughly half of that. Not worth duplicating per-object `[[Extensible]]` /
`is_prototype` state, whose desync silently bypasses a prototype setter.
Revisit only alongside the V8-shaped elements-kind redesign (Stage 3 / O3).

## Stage 3 — integer-index key path (~0.1-0.25 ns)

The gate's `f64 -> u32 -> f64` round trip is inherent to the NaN-boxed
`Value`. Shave what is left: drop the separate `< 2^32` compare by folding it
into the round-trip result, and skip the second conversion where the index is
already integer. **Do not** attempt more — the real fix is Smi integers
(non-goal).

## Stage 4 — cursor/length stores (~0.2 ns)

Two of the four fast-path stores are bookkeeping the design added:
`elem_len` (the compiled-append cursor for the `Vec`) and `generation` (the
read-cell invalidation). `generation` is load-bearing (see the
`slag-dense-arrays` skill) and measured ~free. `elem_len` could be dropped by
making the `Vec` header authoritative — but the toolchain's `Vec` layout must
not be read from compiled code, so this means hand-rolling the element buffer
(`#[repr(C)] { ptr, len, cap }`) and teaching `Trace`/the accessors to it.
Largest risk for the smallest gain; land last, or never.

## Cross-cutting risks and rollback

- **Silent wrong results** are the whole danger: a bypassed prototype setter,
  a collected element (GC), or a stale length. Mitigation: each stage keeps
  the interpreter/slow path exact and unchanged, and the reference walk
  (`store_chain_walk`) intact.
- **Sweep-wide regressions**: Stages 1-2 touch crux object construction,
  `set_prototype_of`, and the index-define funnels — all shared. Every stage
  must run all three sweeps, not just the array clusters.
- **Rollback**: each stage is self-contained behind the epoch/kind; reverting
  it restores the previous check. Keep each stage a separate commit.

## Validation protocol (per stage)

1. `cargo test --workspace` + `cargo clippy --workspace --all-targets -D warnings`.
2. The `scratch/battery.js` differential under jit / `--jitless` /
   `--gc-stress`, byte-identical, and vs node.
3. New fixtures for the invalidation surface (Stage 1): an own index data
   property on `Array.prototype` and on `Object.prototype`; the same via
   `Object.defineProperty` (accessor and data); `Array.prototype.push`;
   `Array.prototype.length = 0`; `Object.setPrototypeOf(a, {0: setter})`
   between stores; a Proxy or String-exotic installed as a proto; a proto
   with a hole then filled. Run each with the loop compiled (warm it).
4. All three sweeps (`language`, `built-ins`, `annexB`) on a freshly built
   `sweep.exe`, diffing the fail+crash union against the parent.
5. Report the row (`buildString shape` / `full`) after each stage; stop when
   the payoff flattens.

## What parity would actually require (out of scope)

Representing integer values as Smis (so an element key needs no `f64` round
trip), an `f64`-free length field, and elements kinds with a dictionary
fallback — i.e. adopting V8's object model for arrays. That is a
Value-representation change touching every consumer, not an array-store
change. Do not start it for one `--jit-bench` row. Scoped as its own project
in `.notes/v8-shaped-arrays.md` (WS1-WS5, migration surface, increment order,
risks).
