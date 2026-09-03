# Performance plan: interpreter + JIT (2026-09-03)

> Supersedes `gap-close-plan.md` (closed 2026-09-03). That plan was
> organized around benchmark rows and committed expected numbers before
> measuring them; this one is organized around the mechanisms that cost
> time, treats the two engines (interpreter, JIT) as one shared-machinery
> system, and uses the vendored V8 checkout (`v8/`, $V8 below) as the
> design reference — we borrow architecture, never code.

## 0. What the previous analysis got wrong

- **It decomposed rows, not mechanisms.** `compound assign`, `property
  read`, and `buildString` all trace to the same machinery (property
  access and allocation) but were planned as separate milestones with
  separate (mostly wrong) estimates. Row gaps are symptoms; the plan
  should target the shared cause.
- **It committed expected numbers before measuring.** Several
  `Expected:` values were contradicted by the outcomes and had to be
  deleted (M4's call-row targets were based on a harness artifact; M5's
  ~8ms target landed at 16.8; M6's ~1ms property-read target never
  existed). A forward plan must open every item with its probe and never
  state a target it has not measured toward.
- **It ignored the reference implementation.** V8's speed on the exact
  rows we studied comes from a small set of architectural mechanisms
  (shapes + inline-property offsets, per-site feedback, full-code
  baseline compilation, nursery allocation). The old plan never mapped
  its machinery against them.
- **The harness itself was broken for part of the record.** `bench_once`
  re-compiled per timed eval and ran the interp column under warmup
  garbage, inflating the JIT gaps 15-30% and the call rows ~2.7x. The
  corrected numbers moved the goalposts mid-plan.
- **One disposition in the closed plan is itself wrong** — the M9 note
  claims a warm-store fast path "must handle chain invalidation across
  objects." Spec `OrdinarySetWithOwnDescriptor` (7.3.3) consults the
  chain only when the OWN property is absent: an own writable data
  property shadows every chain accessor. A generation-validated store
  cell therefore never needs chain tracking. That correction is the seed
  of L1 below.

## 1. Ground rules

1. **Compliance is the constraint.** The engine passes ~99% of the
   runnable test262 corpus (language 23721/23724, built-ins 23657/23812
   incl. 155 skips, annexB 1086/1086). No performance landing proceeds
   without the three release sweeps at baseline, clippy clean, and the
   workspace tests green. A perf change that costs a fixture is reverted.
2. **A lever opens with its probe.** Before implementation, quantify the
   current cost of the mechanism being replaced with a dated measurement
   (recorded in `docs/perf.md`). No expected numbers on a milestone; a
   milestone records what its probe showed and what its landing measured.
3. **One experiment at a time.** A landing names the next experiment.
4. **Both engines move together.** The interpreter and the JIT lower the
   same `Step`/register-op streams and share the object machinery; a
   mechanism change must state its effect on each and be measured on
   both (`--jit-bench` runs each row in both modes).
5. **$V8 is the reference.** Each lever cites the V8 mechanism it
   mirrors and why that mechanism is fast.

## 2. The two engines as they stand (measured 2026-09-02/03)

- **Interpreter**: a step-dispatch VM over compiled `Step` streams.
  Certified bodies (scope analysis) get frame slots; straight-line
  loop/leaf bodies lower to register runs (`RunRegBody`, one dispatch,
  accumulator + dedicated f64 counter field). Member reads are served by
  generation-validated direct-mapped cells; arrays by dense `ArraySlots`
  mirrors. Measured per-operation floors: register-run loop body
  ~4ns/iter after the M7 slices; warm member read ~4-12ns; warm member
  write ~146ns (compound-assign decomposition, 2026-09-03). Step fusion
  beyond the register runs measured ~0 (slices 19-20) — dispatch count
  is not the remaining lever.
- **JIT**: Cranelift compiles only CERTIFIED bodies reached by ordinary
  calls (`scope` = Some). Bodies on the general path — anything with env
  machinery (try/catch, `with`, `eval`, closures that capture with
  `this`, uncertified writes) — never reach `run_compiled_body` and run
  interpreted forever. Within the certified subset the JIT is strong
  (fast loops, leaf-inline, caller-slot args, machine typed-array
  stores).

The wide remaining gaps, after the harness corrections and M7, are the
rows whose cost is property-write machinery (`compound assign` interp
~12x vs node jitless, of which ~146ns/iter of ~186 is the write), JIT
coverage (bodies that never JIT), and allocation churn.

## 3. Why V8 is fast — the mechanisms, mapped to this engine

| V8 mechanism | $V8 source | What it buys | Slag's current analog |
|---|---|---|---|
| Maps (shapes) with descriptor offsets + in-object fields | `src/objects/map.h` | property load/store = shape identity check + direct field access; no name resolution per access | generation-validated cells (reads) + full `[[Set]]` re-resolution (writes) |
| Per-site inline caches / feedback vectors | `src/ic/ic.cc`, `src/objects/feedback-vector.h` | monomorphic fast path validated by one shape compare; exact fallback | global direct-mapped caches (thrash with many keys, Cut 35 slice 5) |
| Accumulator bytecode + specialized handlers | `src/interpreter/bytecodes.h` | low per-op interpreter cost | register runs (already match/beat this on certified straight-line bodies) |
| Baseline compilation of ALL code (Sparkplug) | `src/baseline/baseline-compiler.cc` | every body runs compiled, then hot bodies tier up | certified-subset-only JIT; the general path never compiles |
| Nursery (bump) allocation | `src/heap` | cheap per-object allocation | per-object `Gc::new`/Rc boxes on the hot paths |

## 4. Levers

### L1 — Property machinery (the dominant lever; both engines)

Property access is where JS programs spend their time and where the
remaining measured gaps concentrate (reads ~4-12ns vs V8's ~1-2; writes
~146ns vs V8's few). Three phases, each independently valuable:

**L1a — Warm-store fast path (next experiment).** A store-side cell
mirroring the read cells: keyed by (object id, name, generation), it
records "own writable data property." On a hit the write skips
`put_value`'s re-derivation (namespace checks, receiver boxing,
`find_ecma_accessor`'s own-property lookup) and the second
property-vector lookup inside `set_with_receiver_key`, storing directly
(with the generation bump). Sound because an own writable data property
shadows the entire chain (7.3.3 step 3), so no setter tracking is
needed; the existing slice-11 discipline (every own-property mutation
bumps, including in-place `set_key`) invalidates on redefinition/delete/
accessor-conversion. Applies only when receiver == base on an
Object/Function. **Probe first**: prototype the cell, measure the
`compound assign` row against the 146ns decomposition; gate on the row
moving with the sweeps at baseline. Interp impact direct; JIT impact via
its `call_slow` fallbacks (the compiled fast paths are separate).
**L1b — Fused compound store** (`o.x += v` as one register op, riding on
L1a's cell + the read cell): merges the read-modify-write under one
validation. Interp and register-op paths.
**L1c — Shapes/maps with inline-property offsets (structural, long
pole).** Give ordinary objects a stable shape with offset-addressed
properties so hot reads AND writes are a shape compare + field access in
both engines, replacing the generation/id/name probes, killing the
direct-mapped thrash, and making the JIT emit the same check inline.
Shape transitions on structural change; exotic receivers/accessors/index
keys fall back to the existing exact machinery. This is the V8
`map.h`/descriptor model, phased: read path first (interp shared helper,
then JIT inline), then the write path, then transitions. The compliance
gate is the sweeps plus targeted differential probes (shadowing,
accessor conversion, prototype mutation, delete/redefine during loops).

Timing note: this decision gets CHEAPER the earlier it is made. The
generation-validated cell model is the accretion point — every new
property path written on top of it (L1a's store cell, the register and
JIT member ops) either becomes a legacy layer to carry or must later be
re-expressed on shapes. The engine is three weeks old; building the
read/write/JIT paths on the final representation once beats retrofitting
them after more machinery depends on the cells. V8's ~18 years set the
ceiling, not the schedule — the cost of adopting shapes only grows as
the property machinery grows, so L1c should follow L1a/L1b promptly
rather than drift.

### L2 — Per-site feedback (after L1)

Per-call-site IC entries (shape/offset pairs on the L1c model) shared by
the interpreter (validate + direct access) and the JIT (monomorphic fast
path with exact slow path), replacing the global direct-mapped tables
that thrash at scale. V8's `ic.cc`/feedback-vector model. Defer until L1
establishes the shape representation; a per-site cache on the current
hash-based cells buys little.

### L3 — JIT coverage: compile the general path

Today the JIT only compiles certified bodies. Every uncertified hot body
(try/catch, `with`, captured closures, most methods on complex objects)
runs the interpreter forever. Mirror Sparkplug (`baseline/`): compile
ANY body by emitting each step's work in machine code, calling into the
shared general machinery for the env/handler steps instead of
dispatching them — the dispatch disappears, the semantics stay exact
(the handler table, covered-error paths, and suspension state are
already the interpreter's; the compiled code must reuse them, not
reimplement). **Probe first**: measure (a) what fraction of a realistic
hot corpus never reaches the JIT today (the scope gate), and (b) the
dispatch share of a general-path hot loop (e.g. a try/catch loop, a
`with`-free but uncertified closure) by comparing interp time against a
hand-compiled-equivalent. Gate the first slice on the narrowest
uncertified shape (env reads/writes only, no try) with the sweeps at
baseline; widen only per measured gain.

### L4 — Allocation (bump arena)

The `buildString`/construct-churn rows allocate a box per element/object
(the rope append and per-object `Gc::new`). V8's nursery is a bump
allocator with a copying collector; Slag's per-object allocation is the
measured interp floor on those rows. A bump arena for the hot shapes
(ropes, fresh ordinary objects) with the existing collector sweeping the
arena is the M8 idea, resized to measured need: probe the allocation
share of the construct/buildString rows first (count boxes per
iteration), then build the smallest arena that covers them.

### L5 — Call/construct breadth

The leaf-inline + certified-call machinery is strong within the
certified subset. After L1-L3 land, revisit the remaining call rows with
measurement: method-call inline paths (`o.m()` where `m` is a stable
slot/global), and the residual apply/call member-read cost. No design
work until L3 changes what is even reachable by the JIT.

## 5. Sequencing

The single next experiment is **L1a (the warm-store fast path)**: it
targets the largest measured per-operation cost in the engine (~146ns of
the compound row's ~186ns/iter), is tractable on the current object
model, and its probe is one small cell plus a row re-measure. Landing
order after that follows what L1a/L3's probes show, with one standing
instruction: L1c is the primary architectural investment and is not to
drift — per its timing note it only gets more expensive as the property
machinery accretes on the cell model, so it should start as soon as
L1a/L1b land, ahead of L2 unless the probes change the calculus.

Landing gates (every item): clippy clean, `cargo test --workspace`
green, language + built-ins + annexB release sweeps at baseline, the row
A/B in both modes recorded in `docs/perf.md`, and the measurement the
item's probe promised.

## 6. Measurement discipline

- Rows live in `docs/perf.md` with their dates and harness; the machine
  swings ±15%, so deltas are judged on multi-run interleaved A/Bs, never
  single runs.
- The A/B harness (`bench_once`) measures steady state: definition
  evacuated once, args bound once, warmup before timing — no per-run
  recompile, no warmup-garbage skew.
- Mechanism probes (per-op costs) use isolated loops shaped like the
  real row, alternated in order; a probe that changes the goalposts is
  recorded as such, not silently re-baselined.
