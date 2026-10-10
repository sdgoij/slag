# The structural gap: the object model, not codegen

**Status: open (probe, 2026-10-10).** The 23x JIT gap to V8 is not Cranelift and
not arithmetic — it is that Slag's compiled code emits a **runtime helper call
per object-model operation** (property read, element store, object literal)
with no inline cache and no inline allocation, while V8's TurboFan inlines
those. Arithmetic is compiled inline.

## 1. Method

The corpus's `opcost` rows cannot measure this: V8 *folds* them (a
loop-invariant `o.x` is hoisted, `({a:1}).a` is escape-analyzed to a constant),
so their "gap" is V8 doing zero work. Built a **fold-resistant** set instead —
values retained/escaped so neither engine can eliminate the work —
`scratch/struct/*.js`, run through the corpus protocol in all four modes
(`slag --corpus`, `slag --jitless --corpus`, `node run_node.js`,
`node --jitless run_node.js`).

Per-call ms (fold-resistant; all rows compile except `concat`):

| workload | iters | Slag jit | Slag jl | V8 jit | V8 jl | Slag-jit / V8-jit | Slag-jit / Slag-jl | V8-jit / V8-jl |
|---|---|---|---|---|---|---|---|---|
| `arith` | 20M | 165.9 | 1566.1 | 59.7 | 264.2 | **2.78x** | 9.4x | 4.4x |
| `alloc_obj` | 2M | 385.3 | 413.8 | 9.45 | 129.2 | **40.8x** | **1.07x** | 13.7x |
| `prop_read` | 20M | 371.1 | 1304.8 | 13.48 | 275.6 | **27.5x** | 3.5x | 20.4x |
| `elem_read` | 20M | 187.4 | 1076.3 | 8.17 | 240.3 | **22.9x** | 5.7x | 29.4x |

(`call_leaf` and `concat` are excluded: `concat` bails to the per-step lane
(`opt bail: Step("FusedLoop")`), and `call_leaf`'s mutable global makes V8's
call anomalously slow, so neither is a clean comparison.)

## 2. What it says

- **Codegen is fine.** Real arithmetic work is **2.78x** V8. The paper's claim
  (Cranelift ≈ TurboFan) is directionally right; the 23x is not here.
- **Allocation is 40.8x and the JIT does nothing for it** (Slag-jit/Slag-jl =
  **1.07x**). Slag allocates at ~193 ns/object; V8's JIT at ~4.7 ns. V8's JIT
  is 13.7x faster than its own interpreter here; Slag's is 1.07x — allocation
  is a runtime call, never inlined.
- **Property read 27.5x; element read 22.9x.** V8's JIT is 20–29x faster than
  its interpreter on these; Slag's is only 3.5x / 5.7x.
- Against V8's *interpreter*, Slag is only ~4x off on these rows. **The gap is
  specifically against V8's JIT's object-model inline fast paths.**

## 3. The mechanism (helper census, `JIT_HELPER_STATS=1`)

Counts are per `bench()` call (the harness makes ~7 calls):

- `prop_read`: `get_member_name` **20M** → **one helper call per property
  read**; `get_member_map_slot` (the shape-slot fast path) **never fires**.
  So a read on a varying receiver has no inline cache at all.
- `alloc_obj`: `object_fast` 2M + `get_member_name` 2M + `set_member_computed`
  2M → three helper calls per iteration (literal create, field read, element
  store). No inlined allocation; the object literal is a helper.
- `elem_read`: **no per-read helper** — the dense element read is inlined, yet
  it is still 22.9x V8 (9.4 ns/read vs 0.4). So the inline element path itself
  is slow (guard/bounds/conversion chain), a second, separate lever.
- (Contrast: foldable `opcost/obj_prop` shows `get_member_name` × 1 — Slag's
  LICM hoists the invariant read out. LICM works; the per-op IC does not.)

## 3b. Allocation split out (it is GC, not the alloc instruction)

`alloc_obj` bundles three helpers, so it does not size allocation alone. Built a
ladder over the same 2M dense loop (`scratch/struct/`, `scratch/ladder*/`): a
bare floor, a store of one *fixed* object (no allocation), a fresh-literal
allocation, and alloc+read. All compile (no bail). Per 2M iterations:

| row | Slag jit | V8 jit |
|---|---|---|
| `bare2m` | 1.24 | 0.38 |
| `store_only` (fixed object) | 33.7 | 3.9 |
| `alloc_only` (fresh literal) | 370.8 | 16.0 |
| `alloc_read` | 378.0 | 9.7 |

Subtracting: **element store ≈ 16 ns**, **property read ≈ 3.6 ns**, **allocation
≈ 337 ms / 2M ≈ 168 ns/object**. Allocation is ~88% of the row.

But 168 ns/object is not the allocation instruction — it is GC. `--gc-trace` on
the same row:

```
minor  pause_us=151970  live=2003177->2003177  swept=0    young=1985665
major  pause_us=104111  live=2003177->3174    swept=2000003
```

One minor (152 ms) + one major (104 ms) ≈ **256 ms of the 371 ms (≈70%)**, i.e.
~128 ns/object of GC. The minor pause scales **linearly with the live young
set** at ~70 ns/object (235,665 young → 16.6 ms; 1,985,665 → 152 ms), and the
major sweep at ~52 ns/object. However nothing is promoted, so the nursery grows
to the full live set and every minor is O(live); a healthy copying scan is
~1–2 ns/object. The residual (371 − 256 = 115 ms → ~41 ns/object for the alloc
fast path itself, after the 16 ns store) is ~7x V8's ~6 ns.

So the allocation axis splits:

1. **Nursery/GC (≈70% of the row, the dominant cost)** — the young set is not
   bounded or drained, so a minor scans the entire live young set at ~70 ns per
   object, and the sweep is ~52 ns per object. V8 does the identical 2M retained
   allocations *including* its GC in ~8 ns/object all-in.
2. **The allocation fast path (~41 ns/object, ~7x V8)** — the `object_fast`
   helper call, not inlined.

## 4. The lever

Not codegen, not arithmetic. Two structural arcs:

1. **Inline caches for the object model in compiled code** — a monomorphic
   shape-slot fast path in the emitted code for named reads/writes (so
   `get_member_name`/`set_member_computed` are the slow miss, not the norm),
   and an **inline allocation fast path** (bump pointer + the young-gen check
   in the compiled code, not a `object_fast` call).
2. **The inline dense-element path** (guard/bounds/conversion cost), since it
   is already helper-free yet 23x off.

Both are large but well-defined; measure each with the `scratch/struct/` rows
(now the fold-resistant reference set) plus the differential gate.

## 5. Churn baseline (record to track progress/regression)

Real corpus churn rows — objects allocated and *dropped* immediately — run
isolated, min-of-5, at HEAD `d612ce0c`:

```
bash scratch/churn-baseline.sh ./target/release/slag.exe
```

| row | jit min-of-5 | collections | minors | majors | swept | major pause avg/max (µs) | GC total (≈) |
|---|---|---|---|---|---|---|---|
| `objects/destructure` | 166.8 ms | 489 | **0** | 489 | 2,000,001 | 120 / 209 | ~59 ms (~35%) |
| `calls/construct_churn` | 91.0 ms | 123 | **0** | 123 | 500,001 | 131 / 263 | ~16 ms (~18%) |
| `objects/spread_assign` | 143.1 ms | 98 | **0** | 98 | 399,357 | 234 / 364 | ~23 ms (~16%) |
| `arrays/hof_methods` | 23.3 ms | 1 | **0** | 1 | 1,954 | 445 / 445 | ~0.4 ms |

Observations:

- **Real churn fires only *major* collections, and often.** `destructure` does a
  full collection every ~4k allocations (489 for 2M objects). The adaptive
  nursery that grew to 2M in the retained synthetic (`alloc_only`) does not
  appear when the garbage dies — there is no effective young generation here,
  so young garbage is not collected cheaply.
- GC is **~35%** of the heaviest churn row (`destructure`), ~16–18% of the
  others; per-object GC ~30–57 ns.
- So for real churn the object model (alloc + property ops) is still the larger
  half, with GC a consistent ~third — versus the retained synthetic where GC
  was ~70%.
- **Track the *isolated* number.** The isolated `destructure` (167 ms) differs
  from the full-corpus one (`bench.js`, ~337 ms) by 2x — corpus pollution.

## 5b. The GC trigger policy (why churn majors — probed)

Policy (`crates/runtime/src/agent.rs::maybe_collect`, `crates/crux/src/heap.rs`):

- **Minor**: `young_count() >= nursery_threshold` (default **8192**) and not
  `MINOR_DISABLED`.
- **Major**: `live_count() > 2 * max(1024, last_collected_live)` and not
  `MAJOR_DISABLED`.
- `live_count()` is **old live + the uncollected young cohort** (trace:
  `live=7275 = 3179 old + 4096 young`), while `last_collected_live` is total
  live *after* a major (young is 0 then, so it is the old live).
- Back-off: a collection that swept 0 sets its level's `*_DISABLED` flag until
  the next script boundary (`note_collection` / `note_minor_collection`).

Consequences:

- **Churn** (garbage dies): old live is small/stable (~3179), so the major
  threshold is ~6358, which the accumulating *young* crosses at ~4096 — before
  `young` reaches 8192. So a **full major fires every ~4k allocations and the
  minor never runs** — the trace's `major live=7275->3179 swept=4096 young=4096`
  on repeat, 489 times.
- **Retained** (nothing dies): the first collections sweep 0 → both levels back
  off → the loop runs collection-free and the nursery grows to 2M, then one big
  collection at the end. (This is why the retained synthetic was GC-heavy in
  total but did only ~2 collections.)

So the major's growth trigger measures *total* live, but the code's own comment
says it should track the **old generation's** growth. The young cohort awaiting
its minor inflates `live_count` and prematurely triggers majors.

**Confirmation** (no code change, plain mode so the knob actually applies):

| `--nursery-threshold` | collections | minors | majors | pause |
|---|---|---|---|---|
| 8192 (default) | 489 | **0** | 489 | 128 µs |
| 4096 | 490 | **488** | 2 | 65 µs |
| 2048 | 977 | 976 | 1 | 33 µs |

Wall time (plain, min-of-5, includes startup/compile): **209 → 182 ms**
(threshold 8192 → 4096), ~13%; so the premature-major policy is ~27 ms of
`destructure`'s ~167 ms/call (~16%).

**Fix (applied):** `maybe_collect` now

1. fires a **minor** when the heap has grown past the major threshold on the
   young cohort alone (`young_dominated = live > threshold && young > 0`), so a
   young-dominated heap is reclaimed by the cheap collection instead of a full
   major; and
2. paces the **major** by the old generation (`old_gen_live = live - young`),
   so the young cohort awaiting its minor cannot trigger it.

A naive old-gen-only major (subtract `young`, nothing else) is **wrong**: it
breaks `v8::array_buffer`'s *a_buffer_over_host_memory_releases_it_when_the_isolate_goes*.
The teardown fires one `maybe_collect` with `live=3320 young=3320 old=0`, which
the total-live major reclaimed; with old-gen-only it collects nothing, because
the young garbage is below the 8192 nursery threshold and never reaches it. The
`young_dominated` arm keeps that collection (as a minor).

**Results** (isolated min-of-5; `scratch/churn-baseline.sh`):

| row | before | after |
|---|---|---|
| `objects/destructure` | 166.8 ms | **140.4 ms** (−16%) |
| `objects/spread_assign` | 143.1 ms | **122.1 ms** (−15%) |
| `calls/construct_churn` | 91.0 ms | 86.3 ms (−5%) |
| `arrays/hof_methods` | 23.3 ms | 23.6 ms (flat) |

`destructure`'s collections: **489 majors → 489 minors** (0 majors), GC pauses
~62.6 ms → ~29.8 ms.

Corpus (`node tools/corpus/bench.js`, three runs): `mean-jitGap` **23.4 →
~20.2** (20.66 / 20.65 / 19.35), `mean-jlGap` ~5.3 → ~4.6, `mismatches 0`. The
win is broad (most rows allocate), not just the churn rows.

**Gates:** `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green; test262 `language` 23726/0/0/0 and `built-ins`
23820/0/1/0.

**Measurement trap:** `run_corpus` never calls `apply_nursery_options`, so
`--nursery-*` and `--gc-stress` are **silently ignored in `--corpus` mode** — a
corpus run always uses the default policy. GC knobs must be measured in plain
(`run_file_inner`) / `--bench` mode.

## 6. Caveats

- These are synthetic (tight loops); real workloads mix shapes. The corpus is
  not a substitute because V8 folds most of its rows.
- `alloc_obj`'s three-helper bundle is now split (§3b): element store ~16 ns,
  property read ~3.6 ns, allocation ~168 ns/object (≈70% GC, ~41 ns the alloc
  fast path).
