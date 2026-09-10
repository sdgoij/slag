# Performance

Current performance state of the slag runtime and the PLAN Phase 18
performance milestones, each behind a benchmark gate rather than a
correctness gate.

This file is the single, self-contained performance record for the slag
runtime (consolidated 2026-09-05 from the earlier plan, task-list, and
scratch documents — everything those files said lives here now). Reading
order: the failed-experiments register and the state/remaining sections
below summarize the whole effort; the `## Benchmark gate` section is the
dated journal of every landing and probe; the milestone sections near the
end record the major rewrites (NaN-boxed values, shapes/IC, ropes, the
bytecode VM, the GC); and the final section archives the two superseded
planning documents verbatim for provenance.

## Failed experiments

Ideas that were designed, implemented, or probed and did not pay. Re-check
a premise here before re-proposing it. Each entry gives the idea, the
measurement that killed or bounded it, and its disposition.

### Reverted code

- **The dense-append length mirror is load-bearing** (2026-09-02, the M1
  decomposition). No-op'ing the `ArraySlots` length mirror corrupted
  `length` once a string property landed (probe `a[2] = 3` left `len 0`)
  and collapsed the buildString-full row to 302µs with a false result-ok;
  an `is_empty()` guard on the mirror measured as noise (181-183 vs
  176-180ms — the shared borrow cost more than the skipped mut borrow).
  Both reverted; the mirror stays.
- **Cut 35 slice 24 — the result-store direct write** (2026-08-24). Wrote
  the leaf call's result straight to the target slot (a `result_target` +
  bool return, so the caller skipped its pop) instead of the `stack.push`
  → handler pop → store. Measured a ~1.3ns/call REGRESSION on both call
  rows (5M probe, consistent in both pair orders): the amortized Vec
  push+pop cost less than the plumbing (the param, the bool, the store's
  TDZ check in the inlined tail). Reverted before commit; the result
  round trip is not a lever.
- **Direct-operand local compounds as one fat op** (2026-09-04, the
  `BinStoreReg` follow-up). Generalizing the store-step fuse to the
  direct-RHS shapes (`s += 1`, `s += t`, and the captured/context forms)
  collapsed the whole RMW into ONE fat op. Interleaved 3-round A/B vs the
  `BinStoreReg` parent measured a consistent ~1ns/iter REGRESSION on
  `arithmetic` and `bare loop` (11.46-11.61 → 12.27-14.06 and 10.34-10.80
  → 10.83-11.94): the executor's per-op match dispatch (~1-2ns) plus the
  small shared combine work beat a single fat arm, whose extra
  discriminant branches and cold operand tail hurt the hot arm's layout.
  Reverted; the direct-right shapes stay three ops, and the register-run
  local-compound arc is closed by measurement (its row levers moved to the
  property write end-state).
- **`MEMBER_CELLS` 16 → 256** (2026-09-04, part of the read-thrash probe).
  Did not move the cycling-object read rows and REGRESSED every warm row
  ~25% — the larger inline tables bloated the Agent hot struct (the
  documented inline-table-bloat trap). Not landed; read cells stay at 16.
- **The JIT chain-probe inline** (2026-09-01). Inlining the chain-read
  validation in machine code measured SLOWER than the helper call; the
  chain read's cost is the fixed `member_chain_get` validation, and no
  machine copy of it paid.
- **A shared per-node rope flatten cache** (2026-09-06, the charCodeAt
  re-flatten fix's first draft). Making the `JsString` rope/ConsString
  flatten cache `Rc<OnceLock<Arc<[u16]>>>` (a clone SHARES the box's
  cache, so a per-call owned clone never re-flattens) measured a ~2x
  REGRESSION on append-heavy rows in an isolated A/B: every `s += 'x'`
  past the flat threshold allocates a ConsString node, and the `Rc` adds
  a second allocation per node. concat_loop (200k appends, isolated)
  jitless 8.4ms -> 16.1ms (jit 11.7-12.4 -> 13.3-26.4, bimodal) vs the
  parent; the charCodeAt gain was identical to the shipped fix. Reverted;
  the shipped fix instead keeps the inline cache and seeds the owned
  clone at the handle->owned conversion sites (`JsString::owned_of`),
  which costs nothing on node construction.

### Premises falsified by probe (never paid)

- **Per-site read ICs — interpreter** (2026-09-04). The premise "the read
  residual is in-suite direct-mapped thrash" was measured with certified
  register-run bodies: a warm member read is ~3.5ns/op, and reading 64
  DISTINCT same-map objects in one body costs ~4.2ns/read — the map-cell
  layer absorbs every value-cell miss at +0.7ns. Interpreter reads are
  near their floor; per-site read feedback is not a lever.
- **Per-site member-IC gate — JIT (Slice 4)** (2026-09-05). The compiled
  own-read gate — a per-call-site, direct-mapped cell recording a site's
  last own-data resolution (receiver map id + slot + name), validated
  against the receiver's LIVE map id after a value-cell miss and before
  the shared map-cell probe — measured FLAT: cycling same-shape reads
  stayed ~19-23ns/op, identical to the map-cell table path it replaced.
  Root cause: the cycling cost is dependent-load latency (map handle →
  map id → slot, plus the value-cell probe), which Cranelift's scheduler
  does not overlap, and the gate's own validation loads are a comparable
  serial chain that adds its own latency. Prototype-chain reads are
  latency-bound the same way, so the designed one-link chain variant was
  dropped with the gate; the whole experiment was reverted to the Slice-3
  state. Soundness side-finding kept on record: any per-site member IC
  MUST validate the property NAME — site ids are per-compiled-body and
  reset each compile, so a bare site index can collide onto another
  property's slot.
- **Warm chain reads are fixed-cost, not walk-cost** (2026-09-04). Clean
  marginal rows with numeric prototype values (both engines): own-data
  ~1-3ns interp / ~0.5ns jit, chain 1-link ~18/~17ns, chain 2-link
  ~19/~17.5ns — FLAT in link depth, so `member_chain_get`'s validation
  dominates, not the walk. The 2026-09-01 JIT inline-probe experiment
  measured slower, and no shape-free slice clears it; the fix is the
  shape-end-state read (a shape compare + slot serves the chain at
  own-data cost).
- **The L3 "compile the general path" (Sparkplug analog)** (2026-09-04).
  The scope-gate probe falsified the premise: try/catch bodies certify
  AND reach the JIT (a per-iteration `try { s += o.x } catch` loop runs
  ~125ms interp / ~72ms jit for 1M, ratio 0.57 — compiled; a try AROUND
  the whole loop equals the certified control ~17-19ms interp / ~3.7ms
  jit). The residual uncertified hot shape — a method whose arrow
  captures `this` (~40x, up to ~75x for per-call arrow creation) — was
  fixed ~33x by certifying this-capturing arrows (row 2.3 below), a far
  smaller change than general-path compilation. Do not start the
  Sparkplug analog without a corpus probe showing an uncertified hot body
  whose cost is dispatch (not the env path certification removes).
- **M4 — statically-known leaf calls skip-the-gate** (2026-09-02). Died
  by measurement: steady-state direct calls were already ~5-7ns/call at
  the leaf-call protocol floor — the plan's row targets were
  harness-inflated (the old single-timed-eval methodology re-created the
  callee per eval, re-probing the per-site leaf cache, and ran the interp
  column under warmup garbage). The real call-row residual was the
  apply/call machinery (M10, which landed), so the level-1 skip-the-gate
  was never built.
- **M7's manual-unroll premise** (2026-09-03). Dropped before work: the
  register-run and raw-f64-counter cuts put the loop counter in a machine
  register across the back edge, so the remaining JIT row gaps are
  codegen quality, not unrollable dispatch.
- **M9's chain-invalidation premise** (2026-09-03, corrected the same
  day). The fused compound store was assumed to need setter/chain
  invalidation machinery (a setter added to a prototype does not bump the
  receiver's generation). Spec 7.3.3 step 3 consults the chain only when
  the OWN property is absent, so an own writable data property shadows the
  entire chain and the store cell needs NO chain tracking. The corrected,
  smaller design became L1a (landed); the fused store landed later
  through the register-run member-compound fusions.
- **The bump-arena build (L4/M8)** (2026-09-04 counting probe). The
  premise "buildString/construct churn allocate a hot box per element"
  measured false: construct churn = exactly 1 arena box per iteration
  (the instance the program constructs — no arena can remove it) and
  buildString full = ~390 boxes TOTAL across the whole row (~1.1M dense
  element writes allocate zero). The proposed bump arena ALREADY exists
  (bump + size-classed free-list; the free-list half measured net-neutral
  and registration ~11ns/alloc). No arena work is indicated.

### Measured-closed candidates (worth re-checking only on new evidence)

- **The field-authoritative (Option 3) storage migration.** Rejected by
  measurement twice: the 2026-09-03 attribution probe put the interpreter
  lever at ~5ns of a ~63ns row (every structural consumer must become
  field-aware, and attr drift — `defineProperty` leaving a mapped key
  non-enumerable/non-writable while the map keeps describing it — needs a
  fork-to-overflow or map-fork mechanism first); the 2026-09-05 Slice-3
  scoping re-derived the JIT ceiling at ~4ns/op (a machine inline read of
  a mirrored overflow box is indirection-bound: object → overflow handle
  → box → values → element). Defer until a real slice needs addressable
  storage past the inline fields.
- **Apply/.call member-read inline (row 4.1)** (2026-09-04). Fresh A/B
  decomposition: the ~40-44ns interp / ~20-37ns jit overhead over a
  direct leaf call is allocation-free and spreads across the per-
  iteration chain member read + intrinsic compare + CallApply dispatch —
  no narrow `.apply`-only target (the read is shared with every `o.m()`;
  a 2026-09-01 inline validation measured slower). Read side defers to
  the shape end-state, dispatch side to the registered-call floor.
- **M3 slice 2, M7 slice 2, M1-C-deep, M2's 65+ args** (closed 2026-09-03
  in the first plan's disposition). Assessed not worth against the
  measurements: a second register accumulator (~¼ of a ~4ns/iter register
  body, at the cost of a new VM register with JIT liveness and GC rooting
  across the back edge), the remaining member-read dispatch (single-digit
  percent on one row), the machine-code RefCell/Vec dense-append inline
  (UB-sensitive, for a row already at 42.9ms jit), and 65+-arg vector
  calls (no bench row exercises them).

## Where the work stands (2026-09-05)

### The suite today

`--jit-bench`, all rows result-ok (re-baselined 2026-09-04 at HEAD
`7312c72`; only the Slice 1-3 compiled-read micro probes landed since,
which touch no `--jit-bench` row):

| row | interp (ms) | jit (ms) |
|---|---|---|
| arithmetic | 13.3 | 2.46 |
| bare loop | 12.6 | 2.32 |
| property read | 26.7 | 5.69 |
| string concat | 3.96 | 1.96 |
| function calls | 5.67 | 0.762 |
| global read | 16.6 | 3.49 |
| compound assign | 3.56 | 1.43 |
| buildString shape | 92.9 | 33.4 |
| buildString full | 74.2 | 24.4 |
| typed-array write | 32.3 | 12.3 |
| typed-array length | 11.9 | 1.86 |
| wide leaf call | 18.7 | 1.71 |
| apply leaf call | 19.2 | 7.09 |

JIT ratios are 0.09-0.49 across rows (compiled bodies run 2-11x under
the interpreter). Since the 2026-09-02 baseline the interpreter closed
most of the row gaps the landings targeted: arithmetic 26.2 → 13.3,
property read 54.5 → 26.7, string concat 10.6 → 3.96, compound assign
19.7 → 3.56, typed-array write 75 → 32.3, buildString shape 180 → 92.9.
The five original gate rows are 100-600x under the corrected
pre-migration baselines (see the dated `Current status` record).

### What the work has established

- **Property stores** went from full `[[Set]]` re-derivation per write
  (~140-180ns) to a layered fast path: the L1a warm-store cell, a separate
  256→4096-entry write table, shape-keyed (map id, name) cells, a direct
  own-data fallback for vector-only keys, and a compiled shape gate with a
  narrow slot write — same-shape stores at any object count sit at the
  ~44-67ns/store warm level in both engines; single-object rows unchanged.
- **Property reads** are served by generation-validated value cells, a
  shape layer, and compiled inline reads for map-pinned fields: interpreter
  reads are at their floor (~3.5ns warm), and the compiled cycling-object
  reads sit ~20-25ns/op (latency-bound — see Slice 4's flat result).
- **Map/Set** went from O(n) scans and ~55-arm dispatch chains to a
  hash-indexed entry store plus O(1) registered handlers (~38x on
  delete+set churn; Map.get/Set.has rows to ~220-245ns/call).
- **Agent-dependent builtins** (String/Number/Boolean/BigInt/Keyed/Object/
  DataView) dispatch O(1) by function id instead of linear identity
  chains (charCodeAt ~3.1x, Object.hasOwn ~8.6x, DataView reads ~2.2-2.5x).
- **Primitive property/method reads** no longer box a wrapper per access
  (strings ~23x on `.length`, Number methods ~8-13x).
- **The interpreter** gained register-run bodies (fused loop heads,
  counter-fed compounds, member/element fuses, do-while loops, elided
  completion resets, hoisted typed-array length guards) — per-op floors of
  ~4ns/iter on register loops and ~3.5ns warm reads.
- **The JIT** gained inline typed-array stores/reads and length hoists,
  the dense-append inline gate, compiled intrinsic apply/call, and
  certified `this`-capturing arrows.

The dated records for every landing and probe are the `###`-headed
entries in the Benchmark gate journal below.

### The plan's levers and their fates

The mechanism plan organized the work into five levers; their status:

- **L1 — property machinery (both engines).** L1a (warm-store fast path)
  LANDED. L1b (fused compound store) was delivered through the
  register-run member-compound fusions rather than as a standalone op.
  L1c (shapes with inline offsets) is PARTIAL: the record discipline
  (warm stores stop bumping the generation), the store-cell capacity and
  shape-keyed cells, the map-describes-every-default-key slice, and the
  compiled shape-compare reads/stores (Slices 1-3) all landed; the
  read-end-state premise measured out on BOTH engines (interpreter
  falsification 2026-09-04, JIT Slice 4 flat 2026-09-05); the Option-3
  field-authoritative migration measured not-justified twice. See the L1c
  decision section below for what that leaves.
- **L2 — per-site feedback.** Read-side per-site ICs closed by probe on
  both engines. The remaining per-site arguments (vector-only-key stores
  past 4096 objects, chain reads at own-data cost, the JIT shape-compare
  end-state) all sit behind the L1c shape/storage end-state.
- **L3 — JIT coverage.** The compile-the-general-path premise was
  falsified by the scope-gate probe; the two certification gaps it
  exposed (nested non-arrow `this`/`arguments`, `this`-capturing arrows)
  both landed. Remaining scope=None shapes are narrow (with/eval/async-
  generator/super-constructors); a corpus probe gates any further work.
- **L4 — allocation.** Closed by the counting probe (the arena already
  exists; no second hot shape).
- **L5 — call/construct breadth.** The O(1) handler-registration arc
  closed (all modules whose methods a probe showed hot are registered);
  the compiled intrinsic apply/call path landed; the apply/call member-
  member-read residual is diffuse and defers to L2.

## What is left to do (2026-09-05)

### The merged task list (status of every tracked item)

Row ids below are the stable ids the journal entries cite. Almost every
row is landed or closed; the genuinely open items are 1.1/1.2 (the L1c
shape end-state), 2.1 (gated on a corpus probe), and the probe-first
interpreter rows (buildString/typed-array machinery).

**P0 — Correctness (JIT)**

| row | item | status |
|---|---|---|
| 0.1 | JIT Float16Array/typed-array miscompile: compiled `makeArrayLike`-style loops read all-`NaN` from some iteration onward then segfault (~200-fixture JIT-only cluster; `--jitless` clean; pre-existing; not GC) | **FIXED** by the Linux debug work (2026-09-05); unblocks the clean Linux JIT built-ins sweep and the `-p jit` release test binary |

**P1 — Structural property machinery (L1c → L2)**

| row | item | status |
|---|---|---|
| 1.1 | L1c read/write end-state on maps/shapes (hot member paths serve via shape-compare + inline-field access; exotic receivers/accessors/index keys fall back) | **PARTIAL**: record discipline (stones 1-3), the store-cell capacity slices, shape-keyed store cells, the direct own-data fallback, the map-describes-every-default-key slice, and the compiled Slices 1-3 all landed; the READ-end-state premise was probed and FALSIFIED on the interpreter (reads ~3.5-4.2ns across 64-object sets) and measured FLAT on the JIT (Slice 4). The remaining write end-state (true defines; JIT shape-compare) is behind the storage decision below |
| 1.2 | L2 per-site feedback (per-call-site shape/offset ICs shared by both engines) | re-scoped/deferred by the probes: the interpreter read path does not need it; the write side beyond 4096 objects and the chain-read slice wait on the L1c shape representation; the JIT shape-compare end-state would use it |
| 1.3 | L1a store cells on a separate, larger table | landed (2026-09-04): 64-distinct-object cycling stores ~10x |
| 1.4 | Primitive-string property reads without boxing a wrapper | landed: ~23x on `.length`/unit rows, boxes 400k → 0 |
| 1.5 | Primitive-string METHOD reads on `%String.prototype%` without boxing | landed: ~1.4-1.7x on charCodeAt/charAt/indexOf |
| 1.6 | Non-string primitives (Number/Boolean/BigInt/Symbol) method reads without boxing | landed: ~8-13x on Number method rows |
| 1.7 | Store-cell capacity 256 → 4096 (boxed, heap-direct) | landed: the >256-object store cliff removed through ~4k objects |
| 1.8 | Shape-keyed store cells + the direct own-data fallback (the >4096-object cliff) | landed: 8192/16384-object rows to the warm level; vector-only keys served by the direct resolve-and-write fallback |

**P2 — JIT coverage (L3)**

| row | item | status |
|---|---|---|
| 2.1 | Compile the general path (Sparkplug analog) | open but gated: the scope-gate probe closed the premise for try/catch and this-arrows; do NOT start without a corpus probe showing an uncertified hot body whose cost is dispatch |
| 2.2 | Certification over-rejection: nested non-arrow functions' own `this`/`arguments` | landed (~6x on the construct-wrapped shape) |
| 2.3 | `this`-capturing arrows certify | landed (~33x on the callback-in-method shape) |

**P3 — Allocation (L4 / M8 arena)**

| row | item | status |
|---|---|---|
| 3.1 | Bump arena for the hot shapes | closed by the counting probe (2026-09-04): the arena already exists; construct = 1 box/iter and buildString full = ~390 boxes total — no second hot shape |

**P4 — Call/apply residual (L5 / M10 slice 2)**

| row | item | status |
|---|---|---|
| 4.1 | Inline the `.apply`/`.call` member read on the compiled intrinsic path | probe-closed (2026-09-04): the ~40-44ns interp / ~20-37ns jit residual is diffuse (chain read + intrinsic compare + CallApply dispatch); read side defers to the shape end-state |
| 4.2 | O(1) per-function-id handlers for agent-dependent builtins | landed for String + Number/Boolean/BigInt (charCodeAt ~3.1x, toFixed ~1.7x, toString ~2-2.8x) |
| 4.3 | Keyed module (Map/Set/WeakMap/WeakSet + iterator nexts) in the O(1) table | landed: Map.get ~7.8x, Set.has ~18x |
| 4.4 | Object + DataView in the O(1) table (named-handler refactor first) | landed: Object.hasOwn ~8.6x, DataView reads ~2.2-2.5x; the registration arc is CLOSED |

**P5 — Small interpreter micro-slices**

| row | item | status |
|---|---|---|
| 5.1 | Drop the per-`if` completion reset in certified loops | landed (~5% on buildString shape) |
| 5.2 | Closed-plan residuals (M7 slice 2, M1-C-deep, M2 65+ args, M3 slice 2, LICM of `o.a`/`g`) | closed as not-worth (see Failed experiments) |
| 5.3 | `BinStoreReg` — statement-position local compound fuses bin+store | landed (arithmetic ~15%, compound assign ~12%) |
| 5.4 | Direct-operand local compounds as one fat op | closed by measurement (REVERTED — a ~1ns/iter regression) |

### The genuinely open items

1. **The L1c shape/storage end-state (1.1/1.2)** — the only remaining
   structural item, and everything below it hangs on the storage decision
   (next section):
   - **True defines** (a genuinely new key on a live object) still run the
     full `[[Set]]` in both engines — no IC can make them faster without
     the storage migration.
   - **Vector-only keys on shared maps** (>4096-object store loops on a
     5th+ field) still take the (id, name) table and cliff — the per-step
     shape key needs the storage model.
   - **Chain-member reads** (~17-18ns both engines, flat in depth) stay
     ~7-18x own-data reads until a shape compare + slot serves them at
     own-data cost.
   - **The compiled read gap** (cycling same-shape ~20-25ns/op vs the
     ~4ns single-object floor) is dependent-load latency (Slice 4
     measured the per-site gate flat); only cutting probe depth — a
     single validated map id serving slot arithmetic inline — reduces it.
2. **The remaining large interpreted rows**, to be attacked probe-first
   outside the property track: the dense-array/string machinery
   (buildString shape ~93ms / full ~74ms) and the typed-array write path
   (~32ms) — their residuals are per-op step dispatch and machinery
   floors, not allocation (3.1) or the read/write cells.
3. **WeakMap/WeakSet stay linear** (GC compaction renumbers slots, which
   a position index must clear at every sweep) — a deliberate gap, not in
   any measured row; a probe showing them hot would reopen it.

### Recommended order

Everything concrete is landed or closed by probe. The standing rules:
no next slice without its probe, and the full gate before every landing.
The next lever should come from a fresh `--jit-bench` row scan (the
2026-09-04 re-baseline above is the reference), with the buildString and
typed-array rows as the leading probe candidates. The property track's
remaining work is the storage decision below — start it only behind a
probe showing a >4-key compiled read/write row (or a chain-read row) is
hot enough to justify the migration.

## Working rules and measurement discipline

- **Compliance is the constraint.** No perf landing proceeds without the
  full gate: `cargo clippy --workspace --all-targets -- -D warnings`
  clean; `cargo test --workspace` green (including the new regression and
  edge e2e tests each landing adds); the three release sweeps at baseline
  (language 23721/23724 with 3 skip, built-ins 23657/23812 with 155 skip,
  annexB 1086/1086 — zero fail/crash/hang); and the targeted edge probes
  passing under the JIT, `--jitless`, and `--gc-stress`. A perf change
  that costs a fixture is reverted.
- **A lever opens with its probe.** Quantify the mechanism being replaced
  with a dated measurement before implementing; never commit expected
  numbers on a milestone — record what the probe showed and what the
  landing measured.
- **One experiment at a time.** A landing names the next experiment.
- **Both engines move together.** The interpreter and the JIT lower the
  same `Step`/register-op streams and share the object machinery; state
  and measure each mechanism on both (`--jit-bench` runs every row in
  both modes).
- **Measurement discipline.** The machine swings ±15%; judge only
  multi-run interleaved A/B deltas (alternate base/new order, min-of-3+
  runs), never single runs; prefer isolated 5M-iteration probes over the
  full bench to amplify the signal above load noise; the A/B harness
  measures steady state (the definition evacuated once, args bound once,
  2-call warmup, min of 3 timed calls — no per-run recompile, no
  warmup-garbage skew); a later measurement that contradicts an earlier
  note deletes or updates the note.
- **Sweep timeouts.** Never run a sweep with a per-fixture timeout above
  15 seconds (`--timeout 15 --recheck-timeout 15`, or the default):
  anything that cannot finish in 15s is too slow by definition, and a
  hang under the deadline is a real result, not something to reclassify
  with a longer timeout.

## Reference model — why V8 is fast and what Slag mirrors

The mechanism plan's design reference is the vendored V8 checkout: we
borrow architecture, never code. The table maps each V8 mechanism to what
it buys and to Slag's (current) analog:

| V8 mechanism | What it buys | Slag's analog |
|---|---|---|
| Maps (shapes) with descriptor offsets + in-object fields (`map.h`) | property load/store = shape-identity check + direct field access; no name resolution per access | generation-validated value cells (reads) plus — originally full `[[Set]]` (writes); now shape-keyed store cells, the direct own-data fallback, and compiled shape gates with narrow slot read/write helpers for map-described keys |
| Per-site inline caches / feedback vectors (`ic.cc`) | monomorphic fast path validated by one shape compare; exact fallback | global direct-mapped caches (thrash with many keys → separate/wider write tables); per-site read feedback measured NOT to pay on either engine — deferred behind the shape end-state |
| Accumulator bytecode + specialized handlers (`bytecodes.h`) | low per-op interpreter cost | register runs (`RunRegBody`: one dispatch per straight-line segment, accumulator + dedicated f64 counter field) — already at/under V8-ignition on certified bodies |
| Baseline compilation of ALL code (Sparkplug, `baseline/`) | every body runs compiled, then hot bodies tier up | certified-subset-only JIT; the general-path compile was probed and de-scoped (try/catch and this-arrows already certify) |
| Nursery (bump) allocation | cheap per-object allocation | per-object arena-box allocation; the counting probe closed the dedicated-arena idea (the arena already exists) |

The two engines as they stand: the interpreter is a `Step`-dispatch VM
over compiled function IR; certified bodies (scope analysis) get frame
slots and their loops lower to register runs; member reads are served by
cells + the shape layer. The JIT (Cranelift) compiles the certified
bodies reached by ordinary calls and shares the object machinery; both
engines measured per-op floors of ~4ns/iter register loops, ~3.5ns warm
member reads, and ~44-67ns warm same-shape stores at any object count.

## The L1c property-shape end-state: the three storage options and the decision

This is the fork the property-shape thread had to settle (analysis
2026-09-05), about what "a mapped key with ordinal ≥ 4" (past the four
inline fields) means for storage. The presize sub-question mostly
dissolves once the options are clear, so it is folded in.

**Option 1 — Vector-slot pinning.** Keep the existing insertion-ordered
property vector as the only store. A map descriptor at ordinal `o ≥ 4`
means "this key lives at vector position `o`," enforced by an
append-alignment rule (transition a new key only when it lands at the
descriptor boundary), with boilerplate presize capped at 4 (pre-described
but unstored fields are only safe as in-fields holes below 4).

- Pros: smallest change — no new per-object allocation, no new trace
  edge, no double write; enumeration/delete/descriptor consumers keep
  working off the vector untouched; the map-field extensions are small;
  immediately makes the runtime read map cells and the shape write cells
  exact for ordinals ≥ 4 (the map cell's slot becomes object-independent
  for every default key).
- Cons: the ordinal==slot coincidence is an invariant that must hold
  forever (enforced at the three define sites; a missed gate silently
  misaddresses); the ≥4 read is still a borrow into the property vector;
  and — the big one — it is NOT machine-addressable: the vector can be
  inline or heap and reallocs, so the JIT can never emit a stable
  shape-compare + offset load for a >4 key. It pins the logical slot, not
  an address.

**Option 2 — Dedicated out-of-line value array (mirror).** The object
adds a per-object heap value array indexed by map ordinal ≥ 4 (V8's
property backing store); the inline fields + this array are the
map-addressed value region. The vector stays authoritative for
keys/attrs/enumeration; the array is a hole-capable mirror, so presize
can describe >4 fields and skipped fields read absent.

- Pros: ordinal addressing is independent of vector order (no alignment
  gate, no scramble hazard); presize can grow past 4; a stable base
  pointer + fixed stride gives a future JIT inline load a real target;
  read/write paths stay uniform (the map field is always an array
  access).
- Cons: every mapped ≥4 value exists twice (vector + mirror) — a second
  store per define and per warm write, a second trace path, and a
  resize-on-transition path; doubles memory for exactly the properties
  that are hottest at scale; and it is a MIDDLE state — if the end-state
  is field-authoritative (Option 3), the mirror period is transitional
  overhead, not a step you would keep.

**Option 3 — Field-authoritative (the full V8 model).** Keys/attrs move
entirely into the map's descriptors for map-described properties; the
object holds only values (inline + an out-of-line array); the vector
shrinks to overflow/dictionary/accessor/attr-drift cases. Enumeration,
own-keys, getOwnPropertyDescriptor, delete, the lazy index — everything
that reads the vector — becomes descriptor/field-aware.

- Pros: the clean end-state — one value copy, true shape semantics,
  natural JIT offsets, no mirror drift.
- Cons: it is the whole migration, not a slice; it was MEASURED as an
  interpreter lever (2026-09-03 attribution: ~5ns of a ~63ns row — "not
  justified" — because every structural consumer must be reworked), and
  attr drift is the killer: `defineProperty` can leave a mapped key
  non-enumerable/non-writable while the map keeps describing it, which
  field-authoritative storage cannot tolerate — that path needs a
  fork-to-overflow or map-fork mechanism first.

**What the evidence says.** No current `--jit-bench` row moves under any
option (a cap probe showed the 5th+ field define costs the same as the
1st-4th, and warm 5th-field stores already equal warm 2nd-field stores
via the direct fallback), so the choice was 100% downstream-unblocking
and risk was the dominant selection criterion. The presize sub-question
dissolves: Options 1 and 2 both lose almost nothing by capping presize at
4. Only Options 2/3 give the JIT an addressable offset for >4 keys;
Option 1 cannot.

**Decision: Option 1 now, Option 3 later — Option 2 skipped as a
permanent state.** Option 1 is the smallest change that establishes
"maps describe every default key with a real, shape-pinned offset," and
its alignment invariant (mapped keys are exactly the vector prefix, in
descriptor order) is precisely the ordering a later field-authoritative
migration needs — it is not throwaway (Option 3 just splits that
prefix's values out into an ordinal-indexed array and re-points
enumeration at the descriptors). Option 1 LANDED as the
map-describes-every-default-key slice (commit `2067ee1`, the immediate
predecessor of the fix commit Slice 1 sits on): with every default key
pinned at a map-described vector slot, the interpreter map cells and
shape write cells served ordinals ≥ 4 — the substrate Slices 1-3 built
on. A later Option 3 slice stays gated on its own probe (a compiled
member-read/write row with >4-key shapes actually being hot) and on the
attr-drift fork mechanism.

## Current architecture

- **Value representation**: a NaN-boxed `u64` (PLAN Phase 18): a quiet-NaN
  tag region (top 16 bits `0x7FF8`) holds a 4-bit tag plus a 44-bit payload
  for the heap variants; every other bit pattern is a double stored
  exactly. The payload holds the allocation pointer shifted right 4 (the
  box base is 16-byte aligned), so a full 48-bit address space round-trips.
  `Value` is `Copy` since the GC milestone (GC-5): the heap is
  GC-managed, so a value is a plain word with no refcount bookkeeping, and
  the collector traces the boxes. `match value.kind()` keeps the old enum
  arm shapes via a `ValueKind` mirror.
- **Interpreter**: a `Step` bytecode VM over the compiled function IR
  (`crates/runtime/src/ir.rs`): every expression and statement compiles to
  a `Step` at creation, and a `Vm` dispatch loop executes the compiled
  body for ordinary calls/constructs, generators, async functions, and
  top-level scripts. The old tree-walker survives only as isolated
  single-expression helpers (computed keys, destructuring defaults, class
  heritage); no statement or control-flow structure is walked anymore.
- **Objects**: ordinary Rust structs with a property vector and a lock-free
  prototype `Cell`; per-object state (promises, generators, buffers, ...)
  lives in agent-side tables keyed by object id. A shape/IC layer
  accelerates property access — map-based shapes, the generation-validated
  member-value cells, and a lazy key→slot hash index for large property
  vectors (all invalidated by structural changes only).
- **Strings**: UTF-16 code units in a flat buffer or a depth-capped rope of
  concatenation nodes (see the string-rope milestone below); `concat` appends
  in O(1) once a string is large, and the flat form is materialized lazily
  and cached. Strings of ≤16 units live inline in the box (Cut 67).
- **Memory**: a GC-managed arena heap — bump allocation + mark-sweep with
  root tracing (incl. a conservative native-stack scan), ephemeron-aware
  WeakMap/WeakSet, and `WeakRef`/`FinalizationRegistry` driven by the
  heap; `--gc-stress` collects per allocation. See `.notes/gc-plan.md`.

## Benchmark gate

The CLI's `--bench` mode runs a fixed micro-benchmark suite and reports
wall time per benchmark:

```
cargo run -p cli --release -- --bench
```

Current benchmarks (12 rows): arithmetic, bare loop, indexed store,
property access, string concatenation, array iteration, function calls,
closure capture, per-iteration, construct churn, and the two buildString
shapes. Each snippet is evaluated once to warm
up (interning, hook installation), then timed. The sources use `var` (not
`let`) declarations: a second evaluation in the same realm re-declaring
`let` bindings is a SyntaxError, so the original `let`-based snippets made
the timed run measure that error path (the pre-migration "57µs arithmetic"
snapshot below was such a measurement, not the real loop). Numbers are only
comparable within a build profile; record an early snapshot (debug and
release) and compare against it after each milestone below.

### Baseline snapshot (2026-08-18)

The original snapshot was recorded with `let`-based snippets whose second
evaluation errored; it is kept here for the record but is not a valid loop
time. The corrected `var`-based methodology measured the real pre-migration
loops in release on the same machine:

| Benchmark | release (original snapshot, error path) | release (corrected, real loop) |
|---|---|---|
| arithmetic | 57µs | 2.52s |
| property access | 20µs | 3.22s |
| string concat | 51µs | 0.88s |
| array iteration | 28µs | 15.42s |
| function calls | 18µs | 5.73s |

(The arithmetic benchmark is a 1M-iteration `n += i * 2` loop; the real
numbers are dominated by the tree-walker's identifier resolution and
environment machinery, not by value representation.)

### Current status (measured 2026-09-01)

All five benchmark-gate rows have closed their ≥5x target by one to two
orders of magnitude — the bytecode-VM, shapes/IC, rope, and NaN-boxing
milestones are all done (see the Deferred milestones table). Interpreter
medians (release, 3-run, `--bench`; the rows below are the original five):

| Benchmark | corrected baseline | today | speedup | 5x target |
|---|---|---|---|---|
| arithmetic | 2.52s | ~15.2ms | ~166x | 0.50s — met |
| property access | 3.22s | ~28.3ms | ~114x | 0.64s — met |
| string concat | 0.88s | ~4.0ms | ~218x | — |
| array iteration | 15.42s | ~25.0ms | ~617x | 3.08s — met |
| function calls | 5.73s | ~27.5ms | ~208x | 1.15s — met |

(The `bytecode-plan.md` gate used a later, already-optimized walker
baseline — arithmetic 1.14s → the plan's "≤0.23s"; the current 15.2ms is
~15x below that target too. The machine-drift caveat from the earlier
sections applies — judge only multi-run deltas.)

The full `--bench` suite (12 rows) and the newer interpreter rows: bare
loop ~14.4ms, indexed store ~48ms, closure capture ~33ms, per-iteration
~9.2ms, construct churn ~17ms, buildString shape ~180ms (all 1M-iteration
loops except buildString's 3M). The per-op floor behind these is ~50x
lower than the 2026-08-31 measurement — see the floor section below. The
remaining levers are listed in the levers tables in the typed-array and
floor sections.

### Node comparison and optimization plan (measured 2026-08-21)

Fresh baseline (release, current tree, 3-run medians of `--bench`) with the
same sources run through node v24.12.0 (warm, 3-run medians):

| Benchmark | slag | node | ratio |
|---|---|---|---|
| arithmetic | 1.058s | 8.43ms | 125x |
| property access | 1.318s | 1.31ms | 1006x |
| string concat | 0.150s | 1.43ms | 105x |
| array iteration | 13.25s | 59.4ms | 223x |
| function calls | 1.829s | 1.33ms | 1375x |

(The ratios are JIT-dominated — V8 inlines/constant-folds these shapes —
so they rank the machinery-heavy paths rather than set a target; the gate
remains the internal ≥5x.)

Probe decomposition (release, interleaved 3-run medians) shows where the
time goes:

- **Array iteration is 75% of the bench suite** (13.25s of ~17.6s). An
  empty for-of body costs 12.98s — ~11s (83%) is the iterator machinery
  (iterator object + 1M `next()` calls + 1M result-object allocations);
  the same workload with an index loop is 2.00s (~6x headroom).
- **Arithmetic is bound by the global path**: top-level `n += i * 2` is
  1.05s vs 0.135s for the same loop in a fast-certified function (frame
  slots); the empty top-level loop is ~0.57s. A global-var cell (cached
  global-object slot per name) is the documented lever.
- **Function calls** (1.83s) pay the full `ExecutionContext` push/pop and
  ~10 Rc refcount bumps per 1M calls.

Ranked plan (each behind the usual zero-regression + clippy validation):

1. **P0 — dense-array for-of fast path** — *landed*: a plain Array with the
   stock `@@iterator` iterates by index (new `expr::for_of_begin` guard +
   `ForOfEntry::Fast` in the Vm), re-reading `length` and each element per
   step so a body that mutates the array is observed exactly as the stock
   iterator would; any shadowed/patched `@@iterator`/`next`/`return`, a
   proxy, or a non-Array receiver falls back to the generic protocol. Array
   iteration measured **13.25s → 2.70s (~4.9x)**; full sweeps at zero
   regressions (language 23,690/0/34 unchanged; built-ins 23,432/0/154 —
   eight marginal RegExp property-escape fixtures stopped timing out).
2. **P1 — global var cells** — *landed*: the `Vm` caches the global
   object's property-vector slot per declared top-level name (`global_cells`
   + `load_global_value`/`store_global_value`), re-validated on every access
   against the stored key — an insert/delete/redefinition shifts or
   replaces slots, and the reference path re-resolves (strict non-writable
   errors, accessors, and missing bindings all fall through). Measured:
   arithmetic 1.06s → **0.19s**, property access 1.32s → **0.35s**, string
   concat 0.15s → **0.08s**, function calls 1.83s → **0.69s** (array
   iteration unchanged at 2.7s); full sweeps at zero regressions
   (language 23,690/0/34 unchanged; built-ins 23,436/0/154 — four more
   marginal RegExp fixtures stopped timing out).
3. **P2 — lightweight call path** — *landed*: the certified fast body's
   pushed `ExecutionContext` drops the `script_or_module`/`source`/
   `private_environment` clones (its certification excludes the only
   readers — eval/import/private/Annex B/function-creation), the record is
   fetched with one scoped borrow per branch (a `.cloned()` would
   deep-copy the whole record per call), the VM call steps call
   `function::call_inner` directly (the agent is already set for the
   duration of `run_inner`, so the two TLS swaps are redundant), and
   `dispose_env_resources` skips its drain via a cheap emptiness read.
   Measured: function calls 1.83s → **0.63s**; full sweeps at zero
   regressions (language 23,690/0/34 unchanged; built-ins 23,426/0/154 —
   the pass/hang delta vs the P1 run is the marginal RegExp property-escape
   timeout boundary, all new hangs in the known slow set).
4. **P3 — property-access IC** — *landed*: a small direct-mapped cache on
   the `Vm` (`member_cells`, 16 entries, `(object id, name atom) → slot`)
   serves `GetMemberName`/`GetMemberComputed` own-data reads — the slot is
   re-validated on every access against the stored key and property kind, so
   a structural change, redefinition, accessor conversion, or hash collision
   falls back to the full Get and re-resolves. Measured: property access
   1.32s → **0.24s**; full sweeps at zero regressions (language
   23,690/0/34 unchanged; built-ins 23,429/0/154, all hangs in the known
   slow set).
5. **Numeric-key array fast path** — *landed with P3*: a second direct-mapped
   cache (`array_element_cells`, `(object id, index) → slot`) serves
   `GetMemberComputed` for canonical Number indices and the dense-Array
   for-of fast path — a Number key converts purely (ToPropertyKey of a
   number runs no user code), so the element reads without the
   number→string→intern round-trip; the for-of fast path also inlines the
   length read (`array_length`, the array's `length` is always the first
   property-vector entry) and compiles a simple `var` head to
   `ForOfBindGlobal`/`ForOfBindLocal` instead of the per-element
   binding-initialization. Measured: array iteration 2.71s → **2.29s**, and
   the variable-index `a[j]` shape ~550ms → ~240ms; full sweeps at zero
   regressions (built-ins 23,438/0/154).

Methodology note: per-process timing is load-sensitive on this machine
(5.5x spurious swings observed), so only interleaved multi-run medians or
the in-process `--bench` harness count.

### Cut 11 — for-of begin hoist, fast-script certification, element writes (measured 2026-08-21)

Fresh 3-run medians of `--bench` (release), against the Node v24.12.0
comparison (same sources, warmed, 2nd run) both with and without the JIT:

| Benchmark | slag | node (jit) | node (--jitless) | slag vs jitless |
|---|---|---|---|---|
| arithmetic | 182ms | 1ms | 11ms | 17x |
| property access | 242ms | 1ms | 13ms | 19x |
| string concat | 78ms | 4ms | 5ms | 16x |
| array iteration | 324ms | 3ms | 26ms | 12x |
| function calls | 642ms | 1ms | 16ms | 40x |

The `--jitless` column (V8's ignition bytecode interpreter, no JIT) is the
realistic interpreter-vs-interpreter picture; the ratios rank the
remaining machinery: function calls are the standout gap (the per-call
`ExecutionContext` push), array iteration is the closest.

The ranked plan's five milestones all landed with zero conformance
regressions; four further wins followed (each with the same validation —
clippy clean, `cargo test --workspace` green, language sweep
23,690/0/34 unchanged, built-ins sweep 0 fail/0 crash):

6. **Hoisted for-of begin detection** — `for_of_begin` verifies the stock
   `%Array.prototype.values%` iterator over a plain Array (intrinsic
   identity of `@@iterator`/`next`, no `return` on the
   `%ArrayIteratorPrototype%` chain) *without allocating the iterator
   object or calling `values()`* — the created stock iterator is empty and
   unobservable, so the checks are observably identical. The begin was
   ~10µs (iterator allocation + `values()` call + array_iter_data
   bookkeeping + chain checks) and dominated the array bench (100k
   begins/bench): array iteration 2.17s → **1.37s**.
7. **Fast-script certification for `for-of` with a simple `var` head** —
   `script_scan_allows` rejected *every* script containing a `for-of`,
   sending the whole script (including its plain loops and the body's
   global reads) to the slow env-chain path (~7x slower loops). A `var`
   ident head binds via `ForOfBindGlobal`/`ForOfBindLocal` with no
   per-iteration environment, so the loop is fast-script-safe; lexical
   heads, destructuring heads, and env-path bodies still bail. Array
   iteration 1.37s → **0.33s** (the whole script finally runs on global
   cells); it also removed a per-process pollution where any prior for-of
   made later index loops 7x slower.
8. **O(1) property-index maintenance** — appends (the generic define and
   the array dense-append paths) now insert the new key into the lazy
   `property_index` HashMap instead of invalidating it, so sequential
   fills are no longer O(n²) (each member lookup on a growing array
   rebuilt the index). `Array.prototype.push` growth: 100k pushes >120s
   (quadratic) → **~1s** (linear); the property-escape fixtures' 10k+
   element builds stopped dominating their run time.
9. **O(1) IC slot resolution** — `resolve_member_cell` and
   `resolve_array_element` replaced linear `props.iter().position()` scans
   with the `property_index` (`JsObject::property_slot`), fixing the
   member-lookup half of the push quadratic and big-array element
   resolution (a 1M-element for-of that hung before completes in ~2s).
10. **Fast array element writes** — the compiled `a[i] = v` and
    `Array.prototype.push` element writes bypass the full `[[Set]]`
    machinery (`JsObject::array_element_write`): an existing own writable
    data element updates in place, and a missing element (hole fill or
    dense append) creates the own property after verifying a fully
    ordinary prototype chain with no own property at the index, a writable
    length, and an extensible array — anything else falls back to the full
    `[[Set]]` (accessors, proxies, frozen/sealed, strict-mode errors).
    Writes 3.1µs → **1.6µs** (dense), the property-escape build 4.3s →
    **2.9s** (and the push builtin from ~11µs — its dispatch-chain linear
    scan is a separate, still-open item).

Current suite total: **17.6s → ~1.47s (~12x)**; array iteration 13.25s →
324ms (~41x). All gates ≥5x vs the corrected baseline are met several
fold. The remaining known-slow conformance set (RegExp property-escapes:
~420-440 fixtures hang at the 30s batch timeout under 8-job load; the
TypedArray copyWithin handful) is build-bound (~6.7s fixture build, ~1µs
per element write + a per-char property-escape table scan per regex
test); a precomputed match table per property is the deferred fix.

### Cut 12 — per-call dispatch: env-constant sync skip, single record fetch, shared IC caches (measured 2026-08-22)

Fresh 3-run medians of `--bench` (release, interleaved with the A/B
builds below):

| Benchmark | Cut 11 | Cut 12 |
|---|---|---|
| arithmetic | 183ms | 178ms |
| property access | 241ms | 237ms |
| string concat | 58ms | 59ms |
| array iteration | 317ms | 312ms |
| function calls | 625ms | 431ms |

Function calls 625ms → 431ms (~1.45x) on the way to the 40x-vs-Node
`--jitless` gap (16ms); suite ~1.47s → ~1.21s.

The function-call path was decomposed per call (Cut 12 stage 1):

1. **Certified-body env constancy** — a certified body's `lexical_env`
   never changes (every binding is a frame slot; no `with`/`try`/
   `switch`/`for-in`/`for-of`/`using`), so the dispatch loop skips its
   per-step `running_context_mut` + `Rc::ptr_eq` sync for it
   (`CompiledBody::env_constant`). `fast_block` was extended from
   empty blocks to any block whose lexical declarations are all frame
   slots, so `let`/`const` blocks in certified bodies stop allocating
   envs.
2. **Single `ecma_functions` fetch** — `call_inner` fetches the record
   once and hands the certified fast path its fields (ir/environment/
   realm/strict) as `FastCallData`, dropping the second HashMap hit per
   EcmaScript call; `is_eval_function` skips the intrinsics lookup for
   non-function and EcmaScript callees (`%eval%` is a builtin);
   `with_agent` is a no-op when the agent is already current (nested
   certified `run_inner` entries), removing the redundant TLS swap.
3. **Agent-shared IC caches** — the global-var cells and the P3
   member/element cells moved off the per-call `Vm` onto the `Agent`.
   They were re-created (and ~900 bytes re-zeroed) by every `Vm::new`,
   so each function call and script evaluation started cold; they are
   re-validated against the current realm's global and each object's
   property vector on every access, so sharing them across Vms — and
   across realms — is exact. A function's member accesses now hit its
   caller's warmed cells: a 1M-call `f(o) { return o.a }` probe
   0.61s → 0.52s (~15%), and a global-member-through-call probe
   1.08s → 0.93s (~14%), at zero cost to pure-call shapes.

   The originally planned stage 2b — running a certified callee on the
   caller's `Vm` (a frame-stack split, saving/restoring the ~35
   body-specific fields around the recursive dispatch run) — was
   implemented and A/B'd: it warmed the member shapes (~10%) but cost
   ~6% on pure-call and recursion shapes (the save/restore of ~35
   fields outweighs the `Vm::new` init it replaces, now that the cell
   zeroing is gone). The agent-shared caches deliver the warmth at zero
   per-call cost, so the split was reverted. The certified context push
   also shares one `EnvRef` between `lexical_environment` and
   `variable_environment` (a certified body never reads the latter).

Conformance after Cut 12: zero regressions across the sweeps (language
23,690/0/34; built-ins 23,272/0/154 — the 386 hangs are the known
RegExp property-escape + TypedArray copyWithin set; annexB 1,086/0/0).

The remaining function-call cost (~330ns for an `empty()` call, 2M
calls) is the certified call machinery: the `ExecutionContext` push
(~4-6 Rc clones), the `Vm::new`/drop of the frame + env-stack Vec (a
16-byte alloc per call), `setup_frame`, and the `ecma_functions`
HashMap hit. The next lever is eliminating the per-call `Vm` (a
grow-down value stack with a frame boundary, keeping the certified
callee's control stacks isolated), or caching the certified record's
fields on the function object.

### Cut 13 — identity hashing, empty-frame skip, inline env stack (measured 2026-08-22)

Three per-call costs from the `empty()` decomposition (~330ns/call after
Cut 12):

1. **Identity hasher for `ecma_functions`** — the keys are already-
   unique u64 function ids, so std's SipHash (default) burned ~20ns per
   `Call` hashing them; a 15-line `IdentityHasher` (wrap in
   `BuildHasherDefault`) turns that into an identity fold. The HashMap
   still probes and compares keys, so collisions are handled exactly as
   before.
2. **Skip `setup_frame` for zero-slot frames** — a certified body with
   no bindings (`frame_size == 0`, e.g. `empty()`) never reads the
   frame; `Vm::new` already left the inline buffer in place, so the
   slot-by-slot setup (with its per-slot TDZ checks) is skipped.
3. **Inline `EnvStack`** — the per-call `Vm` allocated a one-element
   `Vec` for the scope-environment stack in `Vm::new` (a heap
   round-trip every call, and again on every `EnterBlock` push). The
   stack is now an 8-entry inline `[Option<EnvRef>; 8]` with a heap
   fallback for deep nesting, so the base env and shallow block envs
   cost no allocation at all. The two disposable-resource drains were
   rewritten against the new storage (the `CatchBind` drain is
   destructive, so it truncates after collecting).

Measured (release, `empty()` 2M calls — isolates the call path; the
machine was load-shifted ~5-8% for the bench runs): 330ns → ~280ns per
call; the `--bench` function-calls row ~420-440ms under load (was
~431ms baseline). Zero conformance regressions (language 23,690/0/34;
built-ins 23,268/0/154 — 390 hangs, the known RegExp property-escape +
TypedArray copyWithin set; annexB 1,086/0/0).

The remaining ~280ns/call is the `ExecutionContext` push (~4-6 Rc
clones), the `Vm::new`/drop of the remaining fields, and the certified
record's field clones. A grow-down value stack with the caller's Vm
(the frame-stack split) was A/B'd at Cut 12 and regressed; the next
candidates are caching the certified record's hot fields on the
function object (a direct-mapped id → (ir, env, realm, strict) cache
on the Agent) and trimming the context push.

### Cut 14 — global-cell fast path and V8-interpreter study (measured 2026-08-22)

The `--bench` rows are all top-level loops over declared global `var`s
(`n`, `i`, `s`, `o`, `f`), so the global-access path and the loop head
dominated every row. Fresh 3-run medians of `--bench` (release,
interleaved with probes):

| Benchmark | before | after |
|---|---|---|
| arithmetic | ~196ms | 70ms |
| property access | ~247ms | 116ms |
| string concat | ~60ms | 55ms |
| array iteration | ~320ms | 255ms |
| function calls | ~440ms | 280ms |

Per-iteration decomposition (1M iterations): the empty loop head
(`i < 1M` test + `i++` + jump) was ~180ns and is now ~48ns; the
`n += i*2` body adds ~25ns. The wins:

1. **Direct-mapped global cells** — `global_cells` was a `HashMap`
   (even identity-hashed, ~10ns probe); it is now a 32-entry
   `[Option<(AtomId, usize)>]` array indexed by `name & 31`, and the
   `load`/`store`/`update` fast paths probe it with a single compare.
   The per-access context-stack walk + realm-global clone is gone too:
   the `Vm` caches the running context's global object on first access
   (a body's realm cannot change while its own steps run).
2. **Fused global update** (`++`/`--`) — `IncGlobal`/`DecGlobal`/
   `UpdateGlobal` previously composed `load_global_value` +
   `store_global_value` (two probes, two `RefCell` borrows); they now
   read/update/write through one borrow, with a Number fast path that
   skips the `to_numeric` call.
3. **Number loop test** — `jump_if_rel_global`/`jump_if_rel_imm`
   compare a Number counter against the numeric limit directly off the
   cell/slot (Rust's NaN-false semantics match JS), falling back to
   `apply_binary` only for non-number counters.
4. **Env-constant scripts** — `compile_statements` set
   `env_constant: false` unconditionally, so every fast-script step
   paid the running-context sync (a `last_mut` + `Rc::ptr_eq`). The
   `Compiler` now tracks whether any emitted step switches the lexical
   environment (`EnterBlock`/loop/catch/with envs) and sets
   `env_constant` accordingly; the bench scripts are env-constant, so
   the per-step sync is skipped.
5. **Dispatch cleanup + inline arithmetic** — the loop's double
   bounds check became a single `get`, and `BinaryImm` inlines the
   number-number arithmetic path (two tag checks + a direct op) for
   Sub/Mul/Div/Rem, falling back to `apply_binary`.

Conformance: zero regressions (language 23,690/0/34; built-ins
23,259/0/154 — 399 hangs, the known RegExp property-escape +
TypedArray copyWithin set; annexB 1,086/0/0).

**The V8 `--jitless` study** (the vendored checkout in `v8/`): V8's
ignition interpreter runs the same loops at ~11-16ms/1M (~1ns per
bytecode) because (a) the dispatch is a computed-goto jump table over
minimal assembly handlers, (b) global loads are **PropertyCell**
indirections — the feedback vector holds a weak cell pointer, the cell
holds the value, so a read is two loads with no per-access validation
(a redefinition replaces the cell and marks the old one with a hole),
and (c) the interpreter is an accumulator machine — binary ops read
one operand from the accumulator and one from a register, with no
stack push/pop per step.

The path from the current ~10ns/step to V8's ~1ns/step is therefore:
1. **Cell-backed global bindings** — hold the script's declared `var`s
   behind a stable `PropertyCell`-like object (value lives in the
   cell; redefinition replaces it), so a global load/store is a cached
   cell pointer + one field load — no `RefCell` borrow, no key
   re-validation. This is the single biggest remaining lever for the
   global-path benches.
2. **Accumulator/register execution** — drop the per-step value-stack
   push/pop for the common unary/binary shapes: operand and result
   registers encoded in the step, like ignition's accumulator.
3. **Fused loop step** — recognize the canonical
   `for (var i = INIT; i <op> LIMIT; i++) BODY` shape and run the
   test + body + increment with one dispatch per iteration (the
   dispatch-loop extraction this requires is the mechanical part).

**Cell-backed globals were implemented and reverted (measured
regression).** A `GlobalCell { value, writable, valid }` was attached to
`Property`, every ordinary set/define kept it in sync (or invalidated it
on a redefine — V8's hole), and the fast path read/wrote the cell with
no property-vector borrow; two variants were A/B'd: a mirror write
keeping `Data.value` fresh (the fast write then paid TWO validation
passes) and a cell-authoritative variant with the slow reads routed
through the cell (`Property::value`, `to_descriptor`, `ordinary_get`,
`get_property`'s fast path, `member_cell_get`). Both regressed: the
empty loop head 48ns → 50-59ns and arithmetic 70ms → 78-96ms. The
reasons: the `RefCell` borrow this engine replaces is ~2ns on a hot
cache line (V8's heap has no borrow flags), the cell adds an `Rc`
indirection plus `valid`/`writable` checks per access, the 16-byte
`Property` growth widens every property vector, and the mirror variant
doubled the write path. The validated-slot fast path (probe + borrow +
key match) remains the best fit for this object model; the V8 cell
advantage is specific to its GC-managed, borrow-free heap.

### Cut 15/16 — fused loop head, script var slots, inline Value ops (measured 2026-08-22)

Fresh 3-run medians of `--bench` (release):

| Benchmark | Cut 14 | now |
|---|---|---|
| arithmetic | 70ms | 43ms |
| property access | 116ms | 83ms |
| string concat | 55ms | 43ms |
| array iteration | 255ms | 233ms |
| function calls | 280ms | 276ms |
| suite | ~780ms | ~680ms |

1. **Fused canonical loop head (`Step::FastLoopHead`)** — the
   `for (var i = INIT; i <op> LIMIT; i++/i--)` on one fast binding now
   runs increment + re-test + back-jump in a single dispatch (the body
   dispatches inline), replacing `IncGlobal` + `JumpIfLtGlobalImm` +
   `Jump`. Measured: **no gain on its own** — the savings (2 of 3
   dispatches) are inside noise; the per-iteration cost was the global
   load/store work, not the dispatch count. Kept (it removes steps and
   composes with the slot work below).
2. **Certified-script var slots (`ScriptSlots`)** — the closed-world
   script's declared `var`s live in frame slots for the whole run
   (borrow-free; the prologue loads each var's current global value,
   the epilogue writes back the assigned ones). The per-access cost
   drops from the global fast path (direct-mapped probe + `Rc` clone
   of the global handle + `RefCell` borrow + key compare) to a plain
   frame read/write — the V8 context-slot model for non-escaping
   script vars. Qualification is a closed-world scan: no
   function/class decls, no `call`/`new`, no `this`/closures, no
   `globalThis`-family identifiers, no destructuring patterns (a
   declared-but-never-assigned read-only global like `Infinity` gets
   no write-back). Scripts failing the scan keep the Cut 14 global
   path unchanged.
3. **`#[inline]` on the NaN-boxed `Value` hot surface** (crux) —
   `is_double`/`tag`/`is_uninitialized`/`as_number`/`is_number`/
   `Boolean`/`Number` plus `Clone::clone` and `Drop::drop`. With no
   LTO, every cross-crate call was a real call on the hottest path;
   this was the largest single win (~25-30% on the loop rows: empty
   loop 50 → 37ms, arithmetic 97 → 64ms before the slot change
   compounded).

Conformance: zero regressions — language 23,690/0/34, built-ins
23,255/0/154 (403 hangs: the known RegExp property-escape + TypedArray
set, unchanged in kind; the count is the performance signal at the 30s
ceiling, see the RegExp table item in Deferred milestones), annexB
1,086/0/0.

Remaining `--bench` levers, in measured order: (1) the accumulator
loop counter (the head is still ~15 ops/iteration round-tripping the
frame; V8 keeps the counter in a register), (2) array-iteration fast
path (the array row's 233ms is the per-element for-of `next()` call
machinery), (3) the per-call machinery (Vm::new + ExecutionContext
push + TLS re-entry — the Cut 12 frame-stack-split analysis), (4) the
RegExp property-escape match table (the built-ins hang set).

### Cut 17 — accumulator loop counter (measured 2026-08-22)

`Vm` gained an `acc` field; a canonical loop whose counter is a frame
slot and whose body only reads it (or assigns/updates it in statement
position — a body scan certifies this) runs with the counter in `acc`
for the loop's duration: `FastLoopBind` loads it once, the head
(`FastLoopHead { var: Acc }`) increments and re-tests it in place, the
body's redirected reads/writes use `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc`,
and `FastLoopStore` writes it back at the exit (`break` lands on the
store). This is the V8 register-counter model for the loop head.

Measured: **~0 (within noise)** — arithmetic 43→43ms, empty loop
37→36ms. The frame-slot access the accumulator replaces was already
cheap; the per-iteration floor is the ~15-op head machinery (dispatch
+ `Value` inc/test round-trip: clone, `is_uninitialized`, `as_number`,
`Value::Number` construction) at ~2ns/op. Kept anyway: it removes the
frame round-trip, composes with the register-machine work, and is
conformance-clean.

The real head cut requires the counter to live as a raw `f64` (no
`Value` round-trip per iteration) with a specialized step — the
register machine below — or a cheaper dispatch loop.

Remaining `--bench` levers, in measured order: (1) the raw-f64 loop
counter (specialized steps, no `Value` inc/test machinery), (2)
array-iteration fast path (the array row's 233ms is the per-element
for-of `next()` call machinery), (3) the per-call machinery (Vm::new +
ExecutionContext push + TLS re-entry — the Cut 12 frame-stack-split
analysis), (4) the RegExp property-escape match table (the built-ins
hang set).

Three rows were added to `--bench` (2026-08-22) to cover the Cut 3
continuation certification shapes the original five predate: `closure
capture` (1M calls through a closure reading its enclosing body's
captured binding — the context-chain slices, ~320ms), `per-iteration`
(100k calls to closures created over a `for (let i ...)` head — the
per-iteration machinery, ~51ms), and `construct churn` (100k `new C`
on a constructor reading `this` — the this slots + construct fast
path, ~452ms). The 2026-08-18 gate baselines above cover only the
original five rows.

### Cut 18 — for-of/for-in heads certification (2026-08-22)

The last certification gap from `bytecode-plan.md` §8 item 7: a body
containing a `for-in`/`for-of` with an ident head now takes the fast
path instead of bailing to the env path. A `var` head binds the
slot/global/context directly per element/key; an uncaptured lexical
head re-inits its flat slot per iteration; a captured lexical head runs
the per-iteration env machinery (fresh copies the body's closures
observe, mirroring the certified `For`). The head's TDZ environment is
skipped — the slot/context marker reproduces the RHS `ReferenceError`.
Destructuring/expr/`using` heads and async for-of keep the env path.

Measured: the existing `--bench` rows don't move — they are scripts,
and the script path already certified `var`-head for-of (the dense
array fast path binds the head via `ForOfBindGlobal`/`ForOfBindLocal`).
The win is real-world function bodies: `for (const x of arr)` /
`for (let k in obj)` loops inside functions no longer kick the whole
body onto the env path. Conformance at baseline: language 23,690/0/34,
annexB 1,086/0/0, built-ins 23,216/0/154 (the known RegExp
property-escape + TypedArray hang cluster); workspace tests 4311/0
including a new fast-path step-stream test asserting the certified
steps.

### Cut 19 — per-call Vm reuse (measured 2026-08-22)

The per-call machinery's construction cost: every call to a compiled
body built a fresh `Vm` (~30 fields, ~20 empty `Vec`s, the 8-slot
inline frame, the inline env stack) and tore it down. The agent now
keeps a free-list of `Vm`s (`vm_pool`); the ordinary call, construct
fast path, and script/eval paths take one, run, and return it — the
reset clears the Vec stacks in place (capacity kept), re-points the
inline env stack, and leaves the frame stale (the next run's
`setup_frame` overwrites every slot below `frame_size`; a
`frame_size`-0 body never reads it). Suspended generator/async/module
states still own their Vm (never pooled), so no Vm aliases a live
suspension.

Measured (3-run medians, vs the immediately-prior tree):

| Row | before | after |
|---|---|---|
| function calls | ~300ms | **270ms** (-10%) |
| closure capture | ~312ms | **264ms** (-15%) |
| construct churn | ~549ms | **454ms** (-17%) |
| per-iteration | ~52ms | 49ms |

Arithmetic/property/concat/array flat (they don't call). The calls row
is still ~11x node `--jitless` — the per-call `ExecutionContext`
push/pop, the record fetch, and the dispatch-loop entry remain; the
structural fix is the Cut 12 frame-stack-split (one dispatch loop over
a frame stack, no per-call Vm/context). Conformance at baseline:
language 23,690/0/34, annexB 1,086/0/0, built-ins 23,202/0/154 (known
hang cluster); workspace tests 4311/0.

### Cut 20 — certified-construct fast path (measured 2026-08-23)

`ordinary_construct` now mirrors the certified call: a base constructor
with a certified body (empty `context_names`), no instance fields, and
no private methods skips the whole slow path — no `FunctionEnv`, no
`function_declaration_instantiation`, no `initialize_instance_elements`,
and the record is borrowed, not deep-cloned (the params/body AST clones
the slow path pays per construct disappear). `this` comes from
`construct_this_object` (OrdinaryCreateFromConstructor, extracted from
the slow path), the slim `ExecutionContext` matches the certified
call's (`script_or_module`/`source`/`private_environment` are never
consulted by a certified body), and the base-constructor return rule
(object/function return wins, else `this`) is applied directly to the
body completion. Class constructors with instance fields/private
methods and derived constructors keep the slow path — the
`fields`/`private_methods` guards stand in for the skipped
instance-element machinery.

Measured (5-run medians, vs the Cut 19 tree):

| Row | before | after |
|---|---|---|
| construct churn | 454ms | **198ms** (-56%) |

Calls/closure/per-iteration/arithmetic flat (they don't construct). The
construct row is now ~36x node `--jitless` (was ~82x) — the remaining
cost is the `this`-object allocation, the slim context push/pop, and
the dispatch-loop entry, all addressed by the Cut 12 frame-stack-split.
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,201/0/154 (known hang cluster); workspace tests 4311/0.

### Cut 21 — certified-call context trim and fused for-of slot binds (2026-08-23)

Two per-call/per-element trims on the certified path, both strictly
less work with no semantic change (gains sit at the machine's bench
noise floor — ~10-20ns per call/element):

1. **Certified-call context trim** — the `ExecutionContext` pushed per
   certified call cloned the running function (`function` field). The
   only certified-path reader of that field is a sloppy body's mapped
   `arguments` creation (`Step::CreateArguments`, for `callee`); every
   other reader is excluded by certification (SuperCall is
derived-only). The clone is now made only when the body uses
`arguments` in sloppy mode — the bench call/closure/construct shapes
push `None`.
2. **Fused for-of step + slot bind** — a certified for-of whose ident
   head resolves to a frame slot emits `ForOfNextBindLocal` instead of
   `ForOfNext` + `ForOfBindLocal`: the element writes the slot
directly, one less dispatch and no value-stack round-trip per element
(the generic iterator path lands the slot the same way). The `--bench`
array row is a script, whose for-of is a separate pre-existing path,
so the row does not move; the win is function bodies (`for (const v of
arr)` in a certified body).

Measured (5-run medians, release): all `--bench` rows within noise
(function calls ~255ms, closure capture ~256ms, construct churn ~192ms
— the trims are ~1-2% each here; the empty-call probe is flat).
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,216/0/154 (442 hangs, the known load-tied cluster);
workspace tests 4311/0.

### Cut 22 — write-side chain cache: fresh-property stores define directly (measured 2026-08-23)

The construct decomposition showed `this.x = x` at ~470-640ns: a store
on a fresh object runs the full `[[Set]]` — own-scan miss, prototype
chain walk (re-entrant scans + Rc clones per link), then the
descriptor/validate machinery before appending. The walk only matters
when the chain holds an accessor or a non-writable data property for
the key — a writable-data link stops `[[Set]]` at the same "define on
the receiver" outcome, absent links just continue.

A small direct-mapped Agent cache (`member_store_cells`) now records
"the chain from this prototype holds no accessor/non-writable for this
key", re-validated exactly against the chain links' generations: every
own-property mutation (define/delete) and prototype change bumps a
per-object `generation` counter, so a hit re-walks the chain reading
only the links' generations (2-3 clones + compares) instead of the full
property scans. On a verified hit — and only for a plain Ordinary
receiver whose own scan misses — the store appends the writable data
property directly (`fresh_data_define`), skipping the descriptor/
validate machinery too. Any doubt (an exotic receiver or chain link, an
accessor/non-writable anywhere, a non-extensible receiver, an existing
own property) falls back to the full `[[Set]]`.

Measured (5-run medians, release, vs the Cut 20/21 tree):

| Row | before | after |
|---|---|---|
| construct churn | ~192ms | **~152ms** (-21%) |
| `{}`+store 1M (probe) | ~730ms | **~355ms** |

The fresh-object store drops ~470ns → ~95ns; `this.x = x` in the
constructor drops to ~170ns. Calls/closure/array flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins
23,216/0/154 (442 hangs, the known load-tied cluster); workspace tests
4311/0. The remaining construct cost is the `this`-object allocation
(~260ns), the read of `o.x` on a fresh object (~200ns — the GET cache
is object-id-keyed), and the call machinery — the shapes work the
write-side cache is a slice of.

### Cut 23 — proto-keyed read-cell fallback (measured 2026-08-23)

The read of a field on a fresh object (a constructor's new `this`) was
~200ns: the `member_cells` GET cache is object-id-keyed, so every fresh
object missed and fell to the full Get. Fresh instances of the same
constructor share their prototype's shape — `x` sits at the same slot
in every `new C()` — so `resolve_member_cell` now also records the slot
under `(prototype id, name)` (`member_proto_cells`), and
`member_cell_get` falls back to that entry on an object-id miss,
validated per access against the instance's own property vector (a
divergent layout misses and re-resolves, exactly like `member_cells`).

Measured (medians, release, vs Cut 22):

| Row | before | after |
|---|---|---|
| construct churn | ~152ms | **~135ms** |
| `new C(i)`+read probe | ~104ms | **~94ms** |
| `{x:i}`+read 1M probe | ~633ms | **~503ms** |

The fresh-object field read drops ~185ns → ~55ns. Property/array/calls
flat. Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,213/0/154 (445 hangs, known load-tied cluster); workspace
tests 4311/0.

### Cut 24 — for-of fast-verdict cache: skip the iterator-method chain walk (measured 2026-08-23)

An array-iteration probe showed `for_of_begin` at ~1.5µs per call —
160ms of the 237ms array row. Every for-of entry ran `get_method`
(a full `@@iterator` read: two prototype-chain walks + accessor scan)
plus three intrinsics lookups and the iterator-infra checks, even for
a plain Array that had been iterated a million times already.

`for_of_begin` now takes the fast path without `get_method`: a plain
Array with no own `@@iterator` whose prototype is the realm's
%Array.prototype%, plus a gen-validated cached "the Array-iteration
infrastructure is stock" verdict — %Array.prototype%.@@iterator is the
intrinsic, %ArrayIteratorPrototype% has the stock `next`, and no
`return` on the AIP chain. The verdict stores the three shared
objects' generation counters (Cut 22's mechanism): a probe re-reads
them (~3 Rc clones + compares); any mutation — `a[Symbol.iterator]`
patched, `Array.prototype[Symbol.iterator]` replaced, a `return` added
to the chain — bumps one and re-resolves the full check. Custom-proto
arrays and every other doubt fall to the unchanged generic path.

Measured (5-run medians, release, vs Cut 23):

| Row | before | after |
|---|---|---|
| array iteration | ~237ms | **~121ms** (-48%) |
| for-of begin+done 100k (probe) | ~160ms | **~49ms** |

The begin is now ~490ns/outer-iteration (from ~1.5µs), the rest the
step machinery. Construct/property/calls flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins
23,216/0/154 (442 hangs, known load-tied cluster); workspace tests
4311/0. The suite is now ~1.01s (from ~1.15s before this cut).

### Cut 25 — certified leaf calls inline on the caller's Vm (measured 2026-08-23)

A call-family decomposition (Cut 20 follow-up) put ~85-115ns of pure
machinery on every certified call: the execution-context push (~40ns),
`take_vm`/`return_vm` pool round-trip with the 25-field reset (~30-50ns),
the redundant nested `with_agent` TLS pair, the `running_context` env
clone, and the record lookup's triple `Rc` clone. A `f(x) { return x + 1 }`
call paid all of it. Cut 12's naive fix (save/restore all 35 VM fields per
call) lost ~6% — the full-frame copy dominated.

The re-attempt inlines only *certified leaves*: a certified body whose
compiled steps contain no re-entry (no `Call`/`CallFast`/`Construct`/
`SuperCall`/`TaggedTemplate`), no running-context read (no `LoadIdent`,
reference machinery, `this`-value, `new.target`, super, closure creation
— the leaf's closures would capture the CALLER's env), no environment or
iterator machinery, and no sloppy mapped `arguments` (its object reads the
context's `function`). Such a body cannot recurse, so the inline run is a
flat save/restore of the handful of fields a leaf can touch — `ip`, the
frame (swapped, not copied), the value-stack length, `completion`/
`completion_is_empty`, `acc`, `strict`, `body_context`, `chain_short`, the
`list`/`completion`/`var_ref`/`array_index` stack lengths, and `call_args`
(when the leaf observes `arguments`) — plus the pre-existing clean-site
guard: the caller's `try`/`pending`/for-of/for-in/destructure stacks and
env stack must be empty, so the leaf's own `return`/`throw`/`break`/
`continue` resolve against nothing but its own steps (they run through
`run_inner_inner` directly, so a leaf error propagates raw to the caller's
`run_inner`, which applies the caller's handler coverage, iterator close,
and disposal with the caller's `ip` restored — exactly a nested call's
path). A leaf runs with the caller's realm current, so the path is gated
on a single realm. The construct shape (`new C(x)`) inlines the same way:
`construct_this_object` + the base-constructor return rule, gated on the
certified-construct conditions (base kind, no fields/private methods) plus
`is_method`.

Measured (5-run medians, release, vs Cut 24):

| Row | before | after |
|---|---|---|
| function calls | ~256ms | **~174ms** (-32%) |
| closure capture | ~255ms | **~172ms** (-33%) |
| construct churn | ~135ms | **~118ms** (-13%) |

Calls and closure dropped ~82ms each (the per-call machinery is now
~90ns/call instead of ~197ns); construct drops the same pool/context
round-trip. Arithmetic/property/array/per-iteration flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins 23,656/0/154
(2 load-tied hangs — the `Script_-_Balinese`/`Myanmar` property-escape
generates pass individually); workspace tests 4311/0. The suite is now
~0.83s (from ~1.01s). One construct-path gap found and fixed by the
sweep: an object-literal method (`{ method() {} }`) is a certified leaf
whose `new` must throw "not a constructor" — the inline construct path
now checks `is_method`.

### Cut 26 — construct-this prototype cache and function-object member cells (measured 2026-08-23)

A construct-path decomposition (the `new C(i)` bench row) showed the
per-construct cost split three ways: OrdinaryCreateFromConstructor's
`prototype` read ran the full property path (~280ns — own-scan + chain
walk), the fresh object's creation paid the `%Object.prototype%`
intrinsics HashMap lookup per create, and — a surprise — every own-data
read on a FUNCTION value took ~310ns vs ~60ns on a plain object: the
P3 member cells only accepted `ValueKind::Object`, so a function's
`length`/`name`/`prototype`/custom properties fell through to the full
Get on every access.

- **Function member cells**: `member_cell_get`/`resolve_member_cell` now
  serve `ValueKind::Function` values through the function's underlying
  ordinary object (same slot→key→kind re-validation, same proto-keyed
  fallback) — `C.prototype`-style reads cache like any own data read.
- **Construct-this prototype cache**: `construct_this_object` caches the
  constructor's `prototype` read per function id on the agent, re-validated
  against the function object's generation counter (Cut 22's mechanism — a
  redefine/delete bumps it, so a stale entry re-reads). Proxies and other
  exotic newTargets stay on the uncached path (their `prototype` read can
  run traps).
- **`%Object.prototype%` intrinsics cache**: `Intrinsics::object_prototype`
  resolves the realm's `%Object.prototype%` once (the intrinsics table is
  fixed at bootstrap) and serves `ObjectBegin` and the construct fallback
  from a cached handle.

Measured (5-run medians, release, vs Cut 25):

| Row | before | after |
|---|---|---|
| construct churn | ~118ms | **~82ms** (-30%) |
| function calls | ~174ms | ~162ms |
| closure capture | ~172ms | ~165ms |

Function-property probe reads dropped ~310→~90ns; the object-literal
probe ~320→~140ns. Calls/closure moved a few ms (noise band);
arith/property/array/per-iteration flat. Conformance at baseline: language
23,690/0/34, annexB 1,086/0/0, built-ins 23,651/0/154 (7 load-tied
`RegExp/property-escapes/generated/*` hangs, the known slow set — the
15s sweep-timeout rule classifies them as real hangs); workspace tests
4311/0. The suite is now ~0.77s (from ~0.83s).

### Cut 27 — per-array for-of fast verdict (measured 2026-08-23)

An array-row decomposition (the `for (var v of a)` bench row) split the
cost between the for-of BEGIN (100k outer iterations × ~490ns ≈ 49ms —
the Cut 24 fast path still ran the own-`@@iterator` property scan, the
`%Array.prototype%` intrinsics lookup, and the prototype walk per begin)
and the per-element step machinery (~60ms for 1M `ForOfNext`s). The begin
dominated: the same array was re-verified 100k times.

`for_of_begin` now probes a per-array fast-verdict cell — (array id, array
generation, prototype id) — before the Cut 24 checks: a hit skips every
check except the cheap gen-validated stock-iterator probe. The array
generation (Cut 22's mechanism) catches an own `@@iterator` addition and
proto changes; the prototype's own mutations bump ITS generation, which
`for_of_fast_probe` re-validates per access. The cell is populated when
the full check passes; a miss re-runs the checks and re-resolves.

Measured (5-run medians, release, vs Cut 26):

| Row | before | after |
|---|---|---|
| array iteration | ~118ms | **~76ms** (-36%) |
| for-of begin 100k (probe) | ~49ms | **~3ms** |

The begin is now ~30ns/outer-iteration (from ~490ns). All other rows
flat. Conformance: language 23,690/0/34, annexB 1,086/0/0 at baseline;
built-ins 23,211/0/154 with 447 hangs — under the 15s sweep-timeout rule
every one is the >15s slow class (435 RegExp property-escapes generates,
5 CharacterClassEscapes, 4 Temporal argument-string-limits, 3 TypedArray
detached-coercion), all previously passing under the old 120s deadline
(sampled individually: they complete, just over 15s); zero new failures
it. Workspace tests 4311/0. The suite is now ~0.72s (from
~0.77s).

### Cut 28 — static reads for captured per-iteration heads (measured 2026-08-23)

The per-iteration bench row (`fns[j & 15]()` over arrows created in a
certified `for (let i...)` loop) paid a full env-chain walk per call: the
arrow's `i` reference compiled to `LoadIdent` (the per-iteration heads are
deliberately stripped from the closure's outer-chain entries — the capture
context's head slot is stale between iterations), so every call resolved
`i` through the runtime environment. The arrows were also NOT leaf-
eligible, so they paid the full certified-call machinery on top.

A closure created inside a certified per-iteration loop captures the
per-iteration env directly, and that env is its `lexical_env` at run time
— so the read can be static. The closure's metadata now carries a
`per_iteration_chain` (the `(head names, env hop offset)` of the loops open
at its creation site, threaded through `EcmaFunction` →
`CreateArrow`/`CreateFunction` → `compile_body`); `binding` resolves those
heads to the existing `LoadPerIteration`/`StorePerIteration`/
`UpdatePerIteration` steps (depth = the closure's own capture-context hop +
the chain entry's offset), and the per-iteration steps became leaf-eligible
(the inline run sets `lexical_env` to the leaf's env only when the leaf has
such steps — `CompiledBody::leaf_needs_env`). Nested loops, closures-in-
closures, and multi-head loops are covered by the depth bookkeeping; every
uncertain case still falls back to the env walk.

Measured (5-run medians, release, vs Cut 27):

| Row | before | after |
|---|---|---|
| per-iteration | ~47ms | **~23ms** (-51%) |
| function calls | ~162ms | ~172ms |
| closure capture | ~168ms | ~174ms |

One regression found and fixed during the cut: the Cut 27
`for_of_array_cells` (16 × 24-byte inline entries) bloated the Agent
struct's hot-field cache footprint and slowed the leaf-call path ~10ns/call
(isolated by A/B: shrinking the field restored the identity-leaf probe
166→165ms); the cache is now `Box`ed so the Agent holds an 8-byte pointer
instead. The call-family total (calls + closure + per-iteration) still
improved ~5ms net. Conformance at baseline: language 23,690/0/34, annexB
1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow class);
workspace tests 4311/0. The suite is now ~0.69s (from ~0.72s).

### Cut 29 — leaf-inline env clone only on the per-iteration path (measured 2026-08-23)

Cut 28 made the inline leaf run swap `lexical_env` to the leaf's env when
the leaf contains per-iteration steps. To keep the moved `body_env` alive
for that swap it changed `self.body_context.replace(body_env)` to
`body_env.clone()` — a clone executed on EVERY leaf call, even the common
leaf with no per-iteration reads. `EnvRef` is an `Rc<EnvRecord>`, so the
unconditional clone was two refcount atomics per call (fetch-add on clone,
fetch-sub on drop) — ~20-25ms on each call-family row.

The save sequence now clones only on the `leaf_needs_env` path: the false
branch restores the pre-Cut-28 move into `body_context`, the true branch
clones once for the swap (the branch itself is fully predictable and costs
nothing measurable).

Measured (release, vs the pre-fix tree):

| Row | before | after |
|---|---|---|
| function calls | ~190ms | **~166ms** |
| closure capture | ~186ms | **~168ms** |
| per-iteration | ~27ms | ~23ms |
| construct churn | ~84ms | ~80ms |

The call rows are back at the Cut 26 floor while per-iteration keeps its
Cut 28 win — the call family (calls + closure + per-iteration) is ~357ms vs
~403ms pre-fix. Conformance at baseline: language 23,690/0/34, annexB
1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow class);
workspace tests 4311/0. The suite is now ~0.68s.

### Cut 30 — skip the environment entirely for env-free leaves (measured 2026-08-23)

The inline leaf run still built a body context and swapped
`body_context`/`lexical_env` for every leaf, even one whose steps never
read an environment. `steps_are_leaf` guarantees a leaf can only touch
an env through context-slot steps (`LoadContextSlot` etc. resolve
`body_context`) or per-iteration steps (resolve `lexical_env`) — every
other env-reading step (identifiers, closures, env machinery, super,
`this`) is excluded. A new `CompiledBody::leaf_uses_env` flag records
whether the steps contain either family; when false, `run_leaf_body`
skips the body-context creation and both swaps entirely (with a
`debug_assert` on the invariant that an env-free leaf gets no env).

The call sites also cloned the callee's `[[Environment]]` per call
unconditionally (the borrow of the `ecma_functions` map can't coexist
with `&mut agent` in `run_inner_inner`, so the owned handle is
required) — now cloned only for a `leaf_uses_env` leaf, and passed BY
VALUE so the no-capture body moves it straight into `body_context`
(the Cut 29 clone was one of two Rc clones on the closure path).

Measured (release, vs the Cut 29 tree; the machine was load-noisy, so
medians over 6-7 runs):

| Row | before | after |
|---|---|---|
| function calls | ~172ms | **~165ms** |
| closure capture | ~183ms | ~172ms |
| per-iteration | ~23ms | ~22ms |
| construct churn | ~81ms | ~78ms |

Calls dropped ~8-9ms from the env skip plus ~4ms from the lazy call-site
clone; closure lost the second of its two per-call clones (the residual
delta is mostly load noise). The call family is now ~360ms and the suite
~0.67s. Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,211/0/154 (447 >15s hangs, the known slow class); workspace
tests 4311/0.

### Cut 31 — raise the rope flatten cap (measured 2026-08-23)

The string-concat row (`s += 'x'` × 100k, building a 100k-unit rope)
flattened the whole accumulated left side every 64 appends
(`ROPE_MAX_DEPTH`), copying ~156 MB of units over the run. The cap
only bounds drop/flatten recursion, so raising it 64 → 1024 cuts the
flatten copies ~16× (to ~10 MB) while the ≤1024-frame recursion stays
well inside the default 8 MB stack. (Right-append chains like `'x' + s`
were already unbounded — the cap only ever protected left-append
chains — so the hazard surface is unchanged.)

Measured (release, vs Cut 30):

| Row | before | after |
|---|---|---|
| string concat | ~46ms | **~20ms** (-57%) |

All other rows flat; the suite is ~0.64-0.66s. Semantics are unchanged
(the flatten is an internal representation detail — strings are
immutable), but the shared rope machinery warranted the full sweep.
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,211/0/154 (447 >15s hangs, the known slow class); workspace
tests 4311/0.

### Cut 33 — cache the leaf/construct inline verdicts on the function record (measured 2026-08-23)

The leaf-inline eligibility checks in `do_call_fast` and `Step::Construct`
re-walked the callee record per call: the `ir` Option, `ir.leaf`, and the
`is_class_constructor`/`class_field_initializer`/`this_mode`/`is_method`/
`constructor_kind`/`fields`/`private_methods` flags. All of those are
immutable once the ir compiles, so the record now caches the two verdicts
(`leaf_inline`, `construct_inline`) at ir-compile time and the hot paths
read one bool.

One trap: a class constructor's `fields`/`private_methods` are populated
by `build_class` AFTER registration, so the cached construct verdict
computed at compile time was stale (a default class constructor with an
empty leaf body looked inlineable before its fields arrived) — the
verdict is recomputed when `build_class` sets them. The runtime class
tests caught it.

Measured (release, vs Cut 31; the machine was load-noisy, medians over
6 runs):

| Row | before | after |
|---|---|---|
| function calls | ~170ms | ~175ms |
| closure capture | ~169ms | ~173ms |
| per-iteration | ~22ms | ~22ms |
| construct churn | ~80ms | ~78ms |

The delta is inside the noise floor (the change is strictly-less-work:
~4-6 fewer record checks per call), consistent with the Cut 21
noise-floor-win precedent. Conformance at baseline: language 23,690/0/34,
annexB 1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow
class); workspace tests 4312/0.

### Cut 34 — leaf frames on the value stack, a leaf-record cache, and fused statement stores (measured 2026-08-23)

Three changes to the leaf-inline call path and the loop-body statements:

- **The leaf's frame is now a flat segment on the value stack**
  (`Vm::leaf_frame_base`) instead of a swapped `[Value; 8]` inline frame:
  the caller's `frame` is never copied out-and-back, and only the live
  slots (frame_size, not 8) are pushed — the ~256-byte swap and the
  128-byte zero-fill are gone. The frame accessors route through
  `frame_get`/`frame_get_mut`, which branch on the base (fully predictable
  on both paths).
- **A Boxed direct-mapped leaf-record cache** (`Agent::leaf_cache`,
  16 entries keyed by function id — ids are never reused, so no generation
  check): `do_call_fast` reads the compiled ir, strictness, and closure env
  from the cache instead of the `ecma_functions` HashMap on every call.
  Boxed per the Cut 27 lesson (an inline copy bloat the Agent's hot-field
  footprint).
- **`FusedStoreLocal`/`FusedStoreGlobal`**: a statement-position assignment
  to a fast binding stores AND sets the statement completion in one step,
  killing the `Dup` + `StoreLocal` + `SetCompletion` trio (2 fewer steps
  per assignment statement in a loop). The `statement_expr`/`expr_depth`
  compiler fields scope the fusion to the statement's own assignment (a
  nested assignment in an operand still leaves its value).

Measured (release, vs Cut 33; the machine was load-noisy, so the spread
is wide — the calm-period medians):

| Row | before | after |
|---|---|---|
| function calls | ~165-190ms | **~145ms** |
| closure capture | ~167ms | **~152ms** |
| per-iteration | ~22ms | ~21ms |
| construct churn | ~78ms | ~78ms |

Calls and closure both drop ~15-40ms (the frame segment is the bulk; the
cache and fused store are noise-floor). The remaining ~50ns/call is the
leaf dispatch + eligibility floor of the interpreter design — getting
calls/closure under 100ms would need a JIT or a leaner dispatch. The
built-ins sweep showed 10 more >15s hangs (457 vs 447), all in the known
slow RegExp-property-escape/decodeURI classes and all passing when sampled
individually — load-dependent classification wobble, not regressions.
Conformance at baseline otherwise: language 23,690/0/34, annexB 1,086/0/0;
workspace tests 4312/0.

### Cut 35 slice 1 — register-encoded leaf bodies (measured 2026-08-23)

The first slice of the register-bytecode plan (Cut 3): the hot leaf bodies
(`return x + 1`, `(y) => x + y`, `() => i`) lower to a dedicated register
op set (`LeafOp`) and run on a small executor (`run_leaf_regs`/
`run_leaf_ops`) instead of the step dispatch loop:

- **A `LeafOp` set over a single accumulator** (`Vm.acc`) plus the leaf's
  frame segment, capture context, and (for per-iteration reads) lexical
  env: `LoadReg`/`LoadContext`/`LoadPerIter`/`LoadConst`, the binary ops
  (`BinReg`/`BinContext`/`BinPerIter`/`BinImm`/`BinConst`, with the
  `BinaryImm` number-number inline), `StoreReg`, and `ReturnAcc`. The
  lowering (`lower_leaf_ops`) accepts only the left-leaning straight-line
  shapes the hot bodies produce; anything else keeps the step path
  (conservative — a register body is a strict subset).
- **A minimal save/restore**: a register body touches only `acc`, the
  frame, and the env fields — never `ip`, completion, `strict`,
  `chain_short`, the array-index stack, or the arguments slice — so the
  per-call swap shrinks to two fields plus (for captured reads) the two
  env slots.
- **The call path flattens**: `do_call_fast` runs a register leaf directly
  on `run_leaf_regs` (no `run_leaf_call`/`run_leaf_body` indirection, no
  completion round-trip — a register body always completes `Return`); the
  `OrdinaryCallBindThis` logic moved to a shared `bind_this_value` helper.
- **Frame aliasing** (when every frame slot is a present parameter — no
  `this` slot, no var/TDZ slots, all args supplied): the frame overlays
  the caller's argument region on the value stack (`LeafFrame::Alias`), so
  there is no argument copy and no frame push; the caller's truncate
  discards the aliased slots. Missing-arg/this-slot bodies push the frame
  from a copied buffer (`LeafFrame::Pushed`) as before.
- **Context reads mirror `context_chain_env`**: `LoadContext`/`BinContext`
  skip context-transparent envs (a named function expression's
  self-binding scope, a per-iteration copy) before reading the capture
  context — the step path's depth-0 walk, reproduced exactly.

Measured (release, vs Cut 34; calm machine, 5-run medians):

| Row | before | after |
|---|---|---|
| function calls | ~148ms | **~101ms** |
| closure capture | ~157ms | **~105ms** |
| per-iteration | ~20.6ms | ~16.6ms |
| construct churn | ~78ms | ~78ms |

Calls drop 148→~101ms (-32%) and closure 157→~105ms (-33%), landing on
(or just above) the sub-100ms target; the isolated leaf call cost is
~50ns (was ~100ns for the step path, ~74ns after the flatten). Construct
churn is unchanged — `this.x = x` bodies use member machinery, not yet
register-encoded (a later slice). Per-iteration drops too (the
`() => i` body is now two register ops). The 67-case behavior probe
(`scratch/leaf_regs_probe.js`) covers the register path, the aliased and
pushed frames, captured/per-iteration reads, throwing binaries, and
caller-with-try/for-of state — all pass. Conformance: language
23,724/0/34, annexB 1,086/0/0, built-ins 23,812/0/154 with 440 >15s
hangs in the known slow RegExp-property-escape / Temporal-argument-
limits / detached-typed-array classes, all sampled individually PASS
(load-dependent batch classification); workspace tests 4312/0.

### Cut 35 slice 2 — fused global calls + dead-guard skip (measured 2026-08-23)

The second slice attacks the call site itself, two dispatches at a time:

- **`CallFastGlobal`** — a plain call to a declared top-level `var`/function
  global with an `undefined` receiver (`f(x)`, no `?.`, no `with`, no
  `eval`, not inside an optional chain, ≤ 2 plain args) fuses the receiver
  push and the callee load into the call step: the handler reads the
  global cell and passes `undefined` as `this` instead of the stack
  round-trip (`Push(undefined)` + `LoadGlobal` + `CallFast` → one step).
  The read goes through the existing `load_global_value` cache, and the
  leaf-inline path still runs (the frame aliases the argument region the
  same way). The compile-time guard is a certified-script-only shape —
  function bodies (`compile_body`) carry no `script_globals`, so the fuse
  never fires inside them.
- **The `chain_depth == 0` guard skip** — `compile_call_args_guarded` no
  longer emits the `JumpIfChainShort`/`Jump` pair when no optional chain
  is open: `chain_short` is set only inside a chain and cleared by the
  outermost chain node's `ClearChainShort`, so a guard at `chain_depth ==
  0` is provably dead. This also removes the guard from paren'd chain
  callees (`(a?.b)()`, `(a?.(x))(y)`), where the chain ends at the paren
  and the call must run on the chain's value (throwing on `undefined`)
  rather than being skipped.

Measured (release, 6-run medians; the earlier rows' ~98/103ms baselines
were the same guard-skip + register-leaf build):

| Row | before | after |
|---|---|---|
| function calls | ~98ms | **~86ms** |
| closure capture | ~103ms | **~90ms** |
| per-iteration | ~15.8ms | ~15.9ms |
| construct churn | ~77ms | ~74ms |

Both global-call rows drop ~12-13% (every run below the old baseline;
per-iteration/construct call member targets or `new`, so they are
unaffected). Isolated call cost is unchanged (the leaf body already ran at
~50ns); the saving is the two dispatches per call in the loop.

**Known tradeoff**: spec 13.4.3 step 2 (`GetValue` of the callee) runs
before step 4 (the arguments), but the fused handler reads the global cell
only after the args are on the stack — so a declared global redefined as
an accessor with side effects would observe the reversed order. This is
unobservable for the certified-script model the fuse requires (declared
top-level vars are data properties; the direct-mapped `global_cells` cache
already trusts them), and no fixture exercises it — the full sweep stays
clean.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (load-
dependent count); the 20-case fused-call probe
(`scratch/callfast_global_probe.js`) covers zero/one/two-arg leaves,
sloppy/strict `this`, non-leaf and recursive globals, reassigned callees,
and the non-fused fallbacks (3 args, spread, member, local).

### Cut 35 slice 3 — certified functions on the frame-slot path (measured 2026-08-23)

The Cut 16 frame-slot path (declared vars live in frame slots, the loop
counter in the accumulator) rejected any script containing a function
declaration or call — a callable could observe the stale global object
while the slots are authoritative. Slice 3 extends it to scripts whose
callables are provably **global-blind** (`analyze_script_scope` +
`certified_functions` in `ir.rs`):

- **Certified functions** (fixpoint): a top-level function declaration
  certifies when its body never references a declared var (except another
  certified function's stable entry-instantiated global binding), never
  calls an uncertified function, and contains no `this`/`super`/closures
  that could observe the global object, `eval`, `with`, `try`, `switch`,
  `for-in`, `using`, or `globalThis`-family identifiers. A body may read
  params/locals and undeclared names (real, never-stale global
  properties). Recursion never certifies; an assigned function name is
  never a candidate.
- **Certified values**: a certified closure (global-blind function/arrow
  expression), a literal, or a certified-call result (the callee's return
  value is itself certified — fixpoint over the declarations' `return`s).
  A var assigned a certified value (`var f = make(2)`) may be called at
  the top level; multiple assignments AND, compound/`++`/`--` marks the
  var unknown.
- **Certified functions stay global bindings** (never slotted): their
  stable entry-instantiated function objects live on the global object
  (`global_declaration_instantiation` hoists them), so a certified body
  reading another certified function's name is safe, and top-level calls
  to them still fuse (`CallFastGlobal`). The `FunctionDecl` statement
  compiles to nothing on the frame path (the entry instantiation did the
  work).

Measured (release, 6-run medians vs the slice-2 build):

| Row | slice 2 | slice 3 |
|---|---|---|
| function calls | ~86ms | **~65ms** |
| closure capture | ~90ms | **~80ms** |
| arithmetic | ~38ms | ~35ms |
| per-iteration | ~15.9ms | ~15.3ms |
| construct churn | ~74ms | ~74ms |

Both call rows drop further — the calls loop becomes `FastLoopBind`/
`FastLoopHead{Acc}` + `LoadLocal` + `CallFastGlobal` + `FusedStoreLocal`
(one global-cell callee read per iteration), the closure loop runs
entirely on frame/acc (`LoadLocal f` + `CallFast`, zero global access).

**The gate is all-or-nothing per script**: any uncertified function or
call keeps the whole script on the global path (the frame-slot staleness
window is unobservable only when every callable is global-blind). The
15-case probe (`scratch/certified_fns_probe.js`) covers the stale-read
regression (a function reading a declared var must see the live value,
not the stale slot), `globalThis`/closure/`this` bodies, certified-
function-to-certified-function calls, recursion, reassigned names,
certified-value vars (single and re-assigned), and uncertified-value
assignments — all pass.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 4 — fused slot-callee calls (measured 2026-08-23)

`CallFastSlot` extends the fused-call shape to a callee held in a frame
slot (a certified-value var like `var f = make(2)`): the receiver push and
the slot load fuse into the call step, so the closure loop runs entirely
on frame/acc — `FastLoopBind`/`FastLoopHead{Acc}` + `LoadLocal` +
`CallFastSlot` + `FusedStoreLocal`, zero global-object access per
iteration. The closure row converges with the calls row:

| Row | slice 3 | slice 4 |
|---|---|---|
| function calls | ~65ms | ~65ms |
| closure capture | ~80ms | **~67ms** |

**Ordering fix for the global fuse**: the probe for the slot fuse exposed
that `CallFastGlobal` (slice 2) had the same spec-13.4.3 ordering hazard
on the *global-only* path — the fuse fires for any declared global callee
there, so `f(f = g)` called the NEW `f` instead of loading the callee
before the args. The global fuse now requires the callee name to be
**never assigned anywhere in the script** (the assigned prepass walks
function bodies, so an uncertified function's write to the name counts)
and **no call-like node in the arguments** (a builtin like
`Object.defineProperty(globalThis, ...)` could rewrite the global callee).
A slot callee needs only the direct-arg-write check (a certified script's
args can write a declared var only directly; a frame-slot read is
side-effect-free). The 15-case probe (`scratch/callfast_slot_probe.js`)
covers the arg-writes-callee, arg-increments-callee, nested-assignment,
indirect-call-write, and getter-write ordering cases — all pass.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 5 — global-cell cache thrash (measured 2026-08-23)

The construct-churn row stayed ~74ms through every call-path slice, and
isolated measurements of the same loop showed ~36ms. The gap was the
bench itself: the 32-entry direct-mapped `global_cells` cache (Cut 5)
thrashes once a realm accumulates ~30+ globals (the bench runs each row
twice in one realm, and the host globals add more). The construct row's
`C`/`i`/`n`/`o` collided with the earlier rows' names, so every
`LoadGlobal`/`StoreGlobal`/`FastLoopHead` took the reference path
instead of the cached probe — about 2x the loop cost, ~420ns/construct
of pure cache misses. The calls row's names happened not to collide, so
it never showed the effect.

Fixing the diagnosis: `GLOBAL_CELLS` 32 -> 256 removes the thrash
(direct-mapped, so a miss still falls back to the reference path — a
bigger table just makes collisions rare). The construct row drops
74 -> ~36ms with every other row unchanged; real-world global-heavy
scripts (many evals in one realm, host globals, libraries defining many
top-level names) get the same relief.

The construct row is now ~360ns/construct: the body (`this.x = x` =
`LoadLocal` + `AssignMemberName`) was already a certified leaf running
through `run_leaf_construct` (both steps pass `steps_are_leaf` — member
steps are not excluded), so the cost is the loop + object creation +
construct dispatch, not the member write.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 6 — register member stores (measured 2026-08-23)

The register op set grows a `StoreMemberName` (the object in the
accumulator, the value a direct operand) and the lowering accepts plain
member-assign bodies (`this.x = x`, `o.x = v`), so member-store leaves —
including the construct body — run on the register executor instead of
the step path:

- **Lowering**: `Step::AssignMemberName` with a plain `=` pops the value
  and object operands, loads the object into the accumulator, and emits
  `StoreMemberName { name, value }`; `Step::SetCompletion` is skipped (the
  register path's `Empty` maps identically to the step path's `Normal` for
  leaf calls and constructs); a body ending in a store or empty now lowers
  (a fall-off completes `Empty`). Compound assigns and computed-value
  stores stay on the step path.
- **The Acc-clobber trap**: `this.y = a + b` computes the value into the
  accumulator, then the object load would overwrite it — the lowering
  rejects a `RegOperand::Acc` value (the binary ops' right-operand
  restriction). Missed it initially; `Function/S15.3.5_A3_T2`
  (`new Function("arg1,arg2", "...; this.y=arg1+arg2;...")`) wrote the
  object into `y` until the rejection was added.
- **Executor**: the op mirrors `Step::AssignMemberName` — the nullish
  check, then `assign_member` (which can run a setter — same agent-side
  machinery the step path invokes; the pushed result is discarded by the
  frame truncate). A `leaf_operand_value` helper loads the value operand
  with the `LoadReg`/`LoadContext`/`LoadPerIter`/`LoadConst` semantics
  (TDZ and transparent-env walks included).

Measured: the construct row is unchanged (~35ms — the member write was
never the bottleneck; the machinery and dispatch were, and the register
path shaves the dispatch). The win is the machinery itself: member-store
leaves (calls and constructs) now run on the register executor. The
14-case probe (`scratch/store_member_regs_probe.js`) covers plain/const/
computed-value stores, two-store bodies, store-then-return, compound
assigns (step path), captured-object stores, and the bench shape.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 7 — register computed member stores (measured 2026-08-23)

`LeafOp::StoreMemberComputed` extends the register member-store to
computed keys: `o[k] = v` (plain `=` only) lowers with the object in the
accumulator and the key + value as direct operands, and the executor
shares the step path's machinery through an extracted
`assign_computed_plain` helper — the nullish check, the fast array
element write (canonical Number index on a plain Array, skipping the
number→string→intern and [[Set]] chain work), then `to_property_key` +
`assign_member`. The `AssignMemberComputed` step's plain `=` branch now
calls the same helper, so the mirror stays exact.

A computed key or value (`RegOperand::Acc`) keeps the body on the step
path — the object load would clobber the accumulator (the same
restriction as the binary ops and `StoreMemberName`). Compound computed
assigns (`o[k] += v`) stay on the step path.

Measured: construct churn ~35 -> ~33.5ms (the machinery + the step
refactor; mostly noise at this scale). The 14-case probe
(`scratch/store_member_computed_probe.js`) covers param/const keys and
values, the array fast path, computed-key/value step-path fallbacks,
compound assigns, construct `this[K] = x`, and the hot array-fill loop.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 8 — register member reads (measured 2026-08-23)

`LeafOp::GetMemberName` / `LeafOp::GetMemberComputed` extend the register
member ops to reads: `return o.x` / `return o[k]` bodies lower with the
object in the accumulator (and the computed key as a direct operand),
and the executor shares the step path's machinery through extracted
`get_member_name` / `get_member_computed` helpers — the nullish check,
the direct-mapped member-cell cache, the fast array element read (a
canonical Number index on a plain Array), then the property-key
conversion + property machinery. Both `GetMemberName` and
`GetMemberComputed` steps now call the same helpers, so the mirror stays
exact. A computed key (`RegOperand::Acc`) keeps the body on the step
path — the object load would clobber the accumulator.

The read ops compose with the existing register ops: `return o.x + 1`
lowers to `[LoadReg, GetMemberName, BinConst, ReturnAcc]`. The 14-case
probe (`scratch/member_read_regs_probe.js`) covers named/computed/
const-key reads, the array fast path, getters, nested reads, missing
properties, nullish throws, read-then-store bodies, and the
read-compute-read shape.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 9 — register loop bodies (measured 2026-08-23)

The remaining bench rows were already on the frame path — the costs were
script-level step dispatches in the loop bodies. `Step::RunRegBody` runs a
register-lowered loop body against the current frame in one dispatch: the
ops address the frame through `frame_get`/`frame_set` (the register
ops were refactored off the explicit stack base — the leaf path resolves
via `leaf_frame_base`, the script path via the inline `Frame`), the
accumulator is saved and restored around the run (the accumulator-loop
counter), the transient stack use is truncated to the entry length, and
the body's completion is left to the loop machinery (a throwing op
propagates after the restore).

The compiler emits `RunRegBody` in the certified canonical `for` and the
for-of/in loop bodies when `lower_leaf_ops` accepts the body steps (the
value-free `ListBegin`/`ListEnd`/reset/normalize wrappers are now
skipped by the lowering too, so a block-wrapped leaf body lowers as
well). The array-iteration inner body (`n += v`) becomes `[LoadReg(n),
BinReg(Add, v), StoreReg(n)]` in one dispatch; bodies with jumps
(break/continue), `PushAcc` counter reads, or two-member-read shapes
(`o.a + o.b`) stay on the step path.

Measured: array iteration ~71 -> ~60.5ms; the other rows unchanged. The
16-case probe (`scratch/run_reg_body_probe.js`) covers the counter
preservation, non-lowering fallbacks, the throwing-body error path,
nested loops, string-append bodies, and member-store loop bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 10 — fused member reads and register spills (measured 2026-08-23)

The property-access row (`n += o.a + o.b`) had a two-member-read body that
`lower_leaf_ops` rejected: the single accumulator cannot hold the three
live values (`n` must combine after `o.a + o.b`), so it ran as 10 step
dispatches per iteration. Three additions make it a six-op register body
in one `RunRegBody` dispatch:

- **Fused reads** — `LeafOp::GetMemberNameLocal { object_slot, tdz, name }`
  and `GetMemberComputedLocal { object_slot, tdz, key }` load the object
  straight from the frame slot and run the shared `get_member_name` /
  `get_member_computed` in one dispatch (the lowering fuses any `Reg`
  object operand, with the slot's `tdz` bit carried through).
- **Spills** — `LeafOp::PushAcc` pushes the live accumulator value onto
  the value stack when an op needs to overwrite it; `LeafOp::BinAccPop`
  pops it back as the left operand of a binary. The push/pop pairs
  balance inside the body and every caller truncates the stack on
  completion or error, so a spill is just one stack round-trip.
- **The frame-left combine** — `LeafOp::BinLeftReg { op, slot }` computes
  `frame[slot] op acc` for a combine whose left operand is a frame slot
  and whose right is the accumulator's live value. The slot is read at
  the combine, after the accumulator value was computed; that is safe
  only for `tdz=false` slots (the lowering rejects `tdz=true`), because a
  member read's getters cannot reach the body's own frame slots and the
  accepted shapes write no slot between the load and the combine. It also
  preserves the operand order (`n + sum`, never `sum + n`) — the string
  concat probe checks the exact value.

The new shadow-stack entry `RegOperand::Spilled` marks a value pushed by
`PushAcc`; it is consumed only by `BinAccPop` (any load of a spilled
operand rejects the body, keeping it on the step path). The binary ops
now share a `binary_inline` helper that inlines the number-number
arithmetic (`Step::BinaryImm`'s inline) to skip the `apply_binary` call.

The property body `n += o.a + o.b` lowers to `[GetMemberNameLocal(o, a),
PushAcc, GetMemberNameLocal(o, b), BinAccPop(Add), BinLeftReg(Add, n),
StoreReg(n)]` — three dispatches per iteration (body + loop head + loop
store) instead of twelve. The combine rule also admits `y + o.a` and
`n += o.a + y` (a frame-slot left with a computed right).

Measured: property access ~87 -> ~72ms; the other rows unchanged. The
18-case probe (`scratch/slice10_probe.js`) covers the sums, the string
concat order, getter mutation observed by the second read, a throwing
getter, the frame-left and frame-right combines, computed reads, chained
reads, the TDZ rejection (a `tdz=true` left operand stays on the step
path and throws before the read), nullish throws, exotic objects,
undefined operands, valueOf coercion order, accumulator-loop counter
preservation, and nested loops.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 11 — generation-validated member value cache (measured 2026-08-23)

The property row's per-read cost after slice 10 was the member-cell
re-validation: `member_cell_get` re-borrowed the property vector and
re-checked the stored key and property kind on every read (~20ns/read).
Two changes remove most of it:

- **The generation now catches in-place value updates** (`crates/crux`):
  `set_key`'s in-place write, `array_element_write`'s in-place element
  write and dense append, and `array_define_own_property`'s dense append
  (which updates `length` in place) all bump the object generation — the
  only own-property mutations that previously missed it. The write-side
  chain cache and for-of caches only ever re-validate on a mismatch, so
  the extra bumps cost misses, never correctness.
- **`member_value_cells`** — a fronting read cache of (object id, name,
  generation, value): a generation match means no own-property change
  since the read, so the value is returned with no borrow, no key/kind
  re-check. `member_cell_get` fills it on every slot-cache hit and
  `resolve_member_cell` on every resolve; the Cut 23 proto-keyed fallback
  still re-validates against the object's own vector, so fresh instances
  keep working. The `GetMemberNameLocal` op also reads its frame slot by
  reference and tries `member_cell_get` before cloning the value for the
  full fallback, skipping one refcount bump per hit.

The two-member-read property body now costs roughly the loop + two
cached clones instead of two borrow+revalidations: property access
~72 -> ~44ms (2x vs the slice-9 baseline ~87ms). The other rows are
unchanged. The 14-case probe (`scratch/slice11_probe.js`) covers in-place
updates of the cached and sibling properties, delete/defineProperty/
data-to-accessor conversions, mid-loop mutations, two-object and
five-name cache behavior, getter non-caching, array element/length
writes, and prototype-chain shadowing.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 12 — call-site leaf caches (measured 2026-08-23)

After slice 10, the remaining bench rows' cost was the CALL: the pure
loop is ~25ms/1M iterations; `n = f(n)` adds ~50ms. The per-call
machinery was dominated by the callee validation chain — the global
cell load + borrow, `callee.kind()`, the realm check, and the
`leaf_lookup` (~20ns) — plus the leaf run. Two per-call-site leaf
caches skip the chain on a hit:

- **`global_leaf_cells`** — name → the resolved `LeafEntry` for a stable
  global callee, validated by the global object's identity and
  generation (every global-object mutation bumps, slice 11). The
  compiler's never-assigned + no-call-like-args guard makes the callee
  stable for the script's duration, so the cell load, kind check, realm
  check, and lookup are skipped. The leaf-run core is extracted into
  `run_inline_leaf` (shared by `fast_call_core` and both cache-hit
  paths).
- **`slot_leaf_cells`** — frame-slot index → the resolved entry for the
  closure held there, validated by the callee's heap payload
  (`Value::heap_payload`, a new crux accessor — the raw leaked-Rc
  pointer). The cache holds the callee itself, keeping the closure's
  allocation alive, so a payload match can never be a stale address
  reuse — the cached ir + closure env are exactly the callee's. A slot
  reassigned to a different closure (or a non-function) misses on the
  payload and re-resolves.

Measured: function calls ~71 -> ~60ms, closure capture ~72 -> ~66ms;
the other rows unchanged. The 10-case probe (`scratch/slice12_probe.js`)
covers the basic sums, two closures with distinct captures, alternating
callees mid-loop, a `globalThis` reassignment invalidating the global
cache, a builtin callee through a slot, env-free and env-reading leaves,
recursion through a slot call, a non-callable slot, and two-arg leaves.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 13 — generation-validated array element and length caches (measured 2026-08-23)

The for-of fast path re-reads the array's length and each element every
step (the stock iterator semantics), each read doing a `kind()` + a
property-vector borrow + a validation. Slice 11's generation coverage
(in-place element writes, dense appends, and the length updates all
bump) makes both reads cacheable by generation:

- **`array_element_value_cells`** — (array id, index, generation, value)
  fronts `array_element_get`: a generation match returns the element with
  no borrow or key/kind re-check. `resolve_array_element` fills it on
  every resolve, and the computed member-read path (`a[i]`) shares the
  same fast read.
- **`array_length_cells`** — (array id, generation, length) fronts
  `array_length`: a generation match skips the borrow and the number
  conversion the for-of head pays every step.

Measured: array iteration ~58 -> ~44ms; the other rows unchanged. The
10-case probe (`scratch/slice13_probe.js`) covers the sums, in-place
and push mutations observed by the same loop, direct length sets,
holes, two-array cache separation, `a[i]` computed reads, mutations
between passes, truncate-and-grow, and sparse arrays.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 14 — fused leaf binary operands (measured 2026-08-23)

Two more leaf-op fusions cut the leaf bodies' op count:

- **`BinImmLocal { op, slot, tdz, imm }`** — `return x + 1` fuses the
  `LoadReg` + `BinImm` pair into one dispatch (the frame-slot read with
  its `tdz` check, then the number-number inline).
- **`BinCtxReg { op, index, slot, tdz }`** — `(y) => x + y` fuses the
  `LoadContext` + `BinReg` pair (the context-transparent env walk, then
  the frame-slot read — the two reads have no side effects between
  them, so the evaluation order is unchanged). The lowering emits it
  for a captured left with a frame-slot right; the `Binary` arm's
  `else` branch is now a `match (left, right)` with the fused case
  first.

The calls and closure leaf bodies drop from three ops to two
(`BinImmLocal`+`ReturnAcc` / `BinCtxReg`+`ReturnAcc`). Measured mins
(heavy machine load during this slice made the full-suite medians
unreliable; the load inflates the memory-heavy rows): function calls
~60 -> ~59ms, closure capture ~66 -> ~65ms; the other rows unchanged.
The 12-case probe (`scratch/slice14_probe.js`) covers the number and
string operands, valueOf coercion order, the context-transparent walk,
per-iteration + param shapes, non-fused two-param leaves, TDZ left
operands, and the mul/sub/div immediate forms.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 15 — construct-inline lookup through the leaf cache (measured 2026-08-23)

The `Construct` step's certified fast path looked up the callee in the
`ecma_functions` HashMap on every construct — unlike the call path,
which reads a direct-mapped leaf cache. Since a construct-inline body
is a leaf (the `construct_inline` verdict — leaf body, base kind, no
fields/private methods — is cached at ir-compile time), the shared
[`LeafEntry`] now carries that flag, `leaf_lookup` captures it from the
record, and the `Construct` handler reads the leaf cache like
`do_call_fast` — one direct-mapped probe instead of a HashMap lookup
per construct. Non-construct-inline callees (arrows, classes, derived
constructors) fall through to the general machinery unchanged.

Measured (heavy machine load through this slice — the memory-heavy
rows are inflated and the construct win is at the noise floor):
construct churn ~36ms, ~1ms from the lookup removal. The 10-case probe
(`scratch/slice15_probe.js`) covers the bench sum, object-return wins,
primitive-return fallbacks, non-leaf and captured-env constructors,
arrows (TypeError), classes, derived classes with super, new.target,
and prototype-chain instanceof.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 16 — register-encode the accumulator-loop counter read (measured 2026-08-23)

The arithmetic row (`n += i * 2`) was the last step-path bench: its body
reads the accumulator-loop counter (`Step::PushAcc`), which the register
model could not express, so it ran as 5 body steps + the loop head per
iteration. The counter read now lowers:

- **`RunRegBody { ops, push_counter }`** — when the body reads the
  counter, the saved counter is pushed onto the value stack at entry.
- **`LeafOp::LoadCounter`** — acc = pop() (the pushed counter).
- The lowering maps `Step::PushAcc` to a `RegOperand::Counter` shadow
  entry consumed by `load_operand` (at most one per body — the counter
  is pushed once; a second read keeps the step path). A counter as a
  store key/value or a binary right operand is rejected (the operand
  loader cannot express a pop there).

The arithmetic body becomes `[LoadCounter, BinImm, BinLeftReg,
StoreReg]` in one dispatch: arithmetic ~37.5 -> ~33ms. The 12-case probe
(`scratch/slice16_probe.js`) covers the bench sum, the counter as a
store/right-operand (step-path fallbacks), float counters, the
loop-exit counter value, nested loops, two-read bodies, factorial,
string concat, countdowns, and break/continue.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 17 — fused slot-callee call-store (measured 2026-08-23)

The calls/closure rows' loop bodies were `LoadLocal(arg) + CallFastSlot +
FusedStoreLocal` — three dispatches plus the argument and result stack
round-trips per iteration. `emit_statement_store` now recognizes the tail
pattern (the args' `LoadLocal`s followed by a real `CallFastSlot` — whose
guards already passed — and a slot store) and replaces it with
`Step::CallFastSlotStore { callee_slot, arg_slots, store_slot }`: the arg
slots are read in order with their `LoadLocal` TDZ checks, the
slot-callee call runs through the existing machinery (the transient arg
push is truncated by the call core), and the result stores with the
`FusedStoreLocal` TDZ check. The pattern only matches plain slot args, so
member/nested/compound shapes keep the step path unchanged.

The `n = f(n)` body becomes one dispatch: function calls ~59 -> ~57ms,
closure capture ~65 -> ~60ms. The 14-case probe
(`scratch/slice17_probe.js`) covers the sums, 0/1/2-arg shapes, the
arg == store-slot order, member and global callees (no fusion), nested
calls, compound assigns, expression-position assigns, TDZ targets and
args, throwing and non-callable callees.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 18 — do-while for-of/for-in loop shape (measured 2026-08-23)

The certified for-of loop was `[ForOfNextBindLocal; body; Jump(top)]` —
three dispatches per element, the back-jump being one of them. The
protocol steps gained a `back` target and the loop is now a do-while:

```
34: ForOfNextBindLocal { slot: 2, done: 39, back: 35 }   // prologue fetch
35: RunRegBody { ops: [...], push_counter: false }         // body start
36: ForOfNextBindLocal { slot: 2, done: 39, back: 35 }   // loop-bottom fetch
37: ForOfClose
...
39: NormalizeCompletion                                  // done
```

The per-iteration fetch at the loop bottom **is** the back edge: on a
successful fetch `ip = back` (the head bind / body start), so a
straight-line body has no per-element `Jump` dispatch. The prologue
fetch and the loop-bottom fetch are separate steps with the same
`done`/`back`; `continue` targets the per-iteration copy (captured
heads) or the loop-bottom fetch itself, exactly the old back-edge
ordering. The generic (uncertified) paths keep the `Jump` — their
`back` is just the next step (no behavior change) — and the for-of/
for-in `ForOfBoundary` span is unchanged.

The array-iteration row drops ~2-8ms (interleaved same-load baseline,
median ~-5) with every other row flat: the array bench inner loop is
now 2 dispatches per element (fetch + `RunRegBody`). The 30-case probe
(`scratch/slice18_probe.js`) covers dense/hole/sparse arrays, continue,
break, labeled break, per-iteration captures with continue/break,
nested for-of, bodies with calls, generic iterators (Set/string),
for-in with deletion and continue, array mutation during iteration,
zero-iteration loops, and the uncertified paths (destructuring heads,
`with`-forced, lexical-head restore).

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 440 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 20 — fused global-callee call-store (measured 2026-08-23)

The calls row's statement-position call was the one unfused call shape:
`n = f(n)` with `f` a global function declaration compiled to
`ListBegin; LoadLocal; CallFastGlobal; FusedStoreLocal; ListEnd` — the
slice-17 slot-callee fusion skipped global callees. `emit_statement_store`
now recognizes the same tail shape with a `CallFastGlobal` (the slice-2
fused call, `direct_eval` always false) and emits
`CallFastGlobalStore { name, arg_slots, store_slot }` — the arg loads, the
global-callee call (through the slice-12 global leaf cache), and the slot
store in one step, mirroring `CallFastSlotStore` exactly (arg TDZ checks,
the store's `FusedStoreLocal` TDZ check).

Measured: ~0-2ms on the calls row (alternating-order A/B, 5M-iteration
isolation — the engine's dispatch is jump-table cheap, so the 2 saved
dispatches sit below the noise floor). The change is a structural
unification (both callee kinds now fuse) with zero regression. The 16-case
probe (`scratch/slice20_probe.js`) covers 0/1/2-arg global callees,
arg==store-slot order, two-slot shapes, member/compound/expression-position
fallbacks, TDZ args/targets, throwing callees, multiple call sites, and
the slot-callee path (unchanged).

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 21 — dedicated loop-counter field (measured 2026-08-23)

The accumulator-path fast loop kept its counter in `Vm::acc`, so every
register-lowered loop body had to save and restore the accumulator and,
when the body read the counter, push the counter onto the value stack for
`LoadCounter` to pop. The counter now lives in a dedicated
`Vm::loop_counter` field: `FastLoopBind`/`FastLoopHead`/`FastLoopStore`
and the `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc` steps read/write the field
(`FastLoopVar::Acc` is renamed `Counter`), `RunRegBody` no longer
saves/restores the accumulator or pushes the counter (its ops clobber the
accumulator freely), and `LoadCounter` reads the field directly — the
`push_counter` machinery is gone.

**The containment point is `run_leaf_body`**: a leaf-inline body can itself
contain a fast loop (`steps_are_leaf` allows the fast-loop steps), so the
caller's counter is saved/restored across the leaf run exactly like the
accumulator — without it, a leaf's `FastLoopBind` would clobber the
caller's live counter.

Measured: ~0-2ms on the counter-reading rows (alternating-order min-of-3
isolation — the removed save/restore + push/pop sit near the noise floor;
full-bench medians are flat). The win is structural: the counter never
round-trips the value stack, and the field is the prerequisite for a
raw-f64 counter (removing the head's Value ops). The 18-case probe
(`scratch/slice21_probe.js`) covers the bench shape, countdowns,
break/continue, nested loops, leaf-with-loop containment (including a
throwing leaf and two-level nesting), string counters, counter
writes/incs in the body, the for-of inner body reading the outer counter,
and closure bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 440 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 22 — raw-f64 loop counter (measured 2026-08-23)

The `Vm::loop_counter` field is now an `f64` instead of a `Value`. The
loop head's per-iteration work was the Value round-trip — clone the
counter, `as_number()`, compare for the test; clone, `as_number()`,
`Value::Number(x + delta)` for the increment — and the body's
`LoadCounter` wrapped the field back into a Value. With the raw float the
test is a direct f64 compare (NaN semantics match JS: both compare
false), the increment is `self.loop_counter += delta`, and `LoadCounter`/
`PushAcc`/`FastLoopStore` wrap once per read/write instead of per head
dispatch.

**Soundness — the acc-path gates must admit only Numbers.** An f64 cannot
hold a String/BigInt, and the loop protocol lets a non-Number counter
(which the old Value field kept verbatim) flow through `FastLoopBind`
(the init) and `PopAcc` (a statement-position `counter = expr`). Two new
compile gates restrict the acc path: `for_init_counter_number` requires
the loop init to be a provably-Number expression (a numeric literal,
`+expr` — always Number or throws before the store — or `-expr` on a
provably-Number operand, excluding BigInts; a missing/expression/multi-
decl head with a non-Number last counter initializer is rejected), and
`acc_expr_safe`'s statement-position `counter = expr` case now requires a
provably-Number RHS. Everything rejected falls back to the fused slot
path, which is behaviorally identical (it coerces via the general
machinery) — the gates are a pure eligibility restriction, never a
semantics change. The runtime keeps `debug_assert!`s on the gated
bind/store conversions.

The leaf containment point is unchanged: `run_leaf_body` saves/restores
the field across a leaf run (now a plain f64 copy). `Vm::new`/`reset`
seed the field with `0.0`; no code reads it outside an active fast loop.

Measured: a real win — the first structural lever to move since the
step-fusion floor. 5M-iteration alternating-order min-of-3 isolation
(14 runs per binary, base = the slice-21 tree at `89bba88`): empty-head
loop 59→35ms, counter-read body 166→132ms, arith row 165→122ms
(≈5-9ns/iter off the head and counter read; consistent across both
pair orders, far above the ±2-5ms order bias). The 25-case probe
(`scratch/slice22_probe.js`) covers the bench shapes, countdowns,
break/continue, nested loops, leaf containment (including a throwing
leaf), the Number-literal/write gates, and the slot-path fallbacks for
String/BigInt inits and writes (all behaviorally identical).

Validation: clippy `-D warnings` clean, workspace tests green (4313/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (all in the known slow classes;
7 property-escapes more than slice 21's 440, 1 less non-whitespace —
load-classification wobble, no added fail/crash paths).

### Cut 35 slice 23 — caller-frame argument aliasing (measured 2026-08-24)

The fused `CallFastGlobalStore`/`CallFastSlotStore` steps (slices 17/20)
pushed each argument from a caller frame slot onto the value stack, then
the leaf-inline callee either re-read it from that stack region (`Alias`)
or copied it into a pushed frame (`Pushed`). The argument round-trip —
clone + Vec push, stack read, pop — was the fused call core's largest
remaining per-call cost after slice 22.

Slice 23 reads the arguments straight from the caller's frame slots. A
register leaf whose frame is exactly its parameters — `frame_size ==
arity`, no `this`/`arguments`/`var` slots, and no parameter is ever
assigned (the new `ScopeInfo::args_alias` gate, computed in
`analyze_scope` via the assigned-name collection, which walks nested
closures too) — runs with a new `LeafFrame::CallerSlots { base }`: the
caller's `leaf_frame_base` stays active, a new `Vm::leaf_frame_offset`
addresses its slots directly, and nothing is pushed or unwound. The
fused steps keep their TDZ checks (moved ahead of the call, no push) and
pass the args' base; the call core uses the aliasing on a cache hit and
materializes the args on the stack on any fallback (param write, var
slot, `this`, `arguments`, a step-path leaf, a non-leaf — behavior
identical to the pre-slice path).

**The debug-stack trap**: the first cut inlined the CallerSlots run as a
second `run_leaf_regs` call site inside `run_inline_leaf`, and with
`#[inline(always)]` the whole register dispatcher got duplicated into
`fast_call_core`'s per-recursion-level debug frame — a step-path leaf
recursion (the `fast_path_function_declarations` test's `g(5)`) that
passed at the default test-thread stack in the base now overflowed
(needed `RUST_MIN_STACK` 4MB). `run_inline_leaf` was restructured to a
single `run_leaf_regs` call site (the frame source is decided first, one
result-placement tail with a `pre_call` base) — the test passes at the
default stack again and the release win is unchanged.

Measured — the call rows drop ~12-16%: a release Rust-side timing probe
(min-of-5 evals of the exact bench sources, same agent, identical in
both worktrees) shows `n = f(n)` 5M at 254→223ms and the closure-capture
row at 303→253ms (~6.4ns/call and ~9.9ns/call off the fused call core).
The `--bench` harness confirms the direction but swings with load.

The 13-case probe (`scratch/slice23_probe.js`) covers the bench shapes,
two-param leaves, param-write/var-slot/`this`/step-path/non-leaf
fallbacks, the closure-capture shape, extra args, a `let` TDZ arg, and
an arg slot distinct from the target slot. New unit tests assert
`ScopeInfo::args_alias` across the gate (including a closure writing a
captured param) and the fused-site behavior.

Validation: clippy `-D warnings` clean, workspace tests green (4315/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 22).

### Cut 35 slice 24 — result-store direct write (measured 2026-08-24, REVERTED)

The other half of the fused call round trip: `run_inline_leaf` wrote the
leaf's result straight to the target slot (`result_target` + a `bool`
return so the caller skipped its pop) instead of the `stack.push` →
handler `pop()` → store. Measured a ~1.3ns/call REGRESSION on both call
rows (5M-iteration probe, consistent in both pair orders): the
amortized Vec push+pop cost less than the plumbing (the param, the bool,
and the store's TDZ check moved into the inlined tail) needed to remove
them. Reverted before commit — the result round trip is not a lever.

### Cut 35 slice 25 — leaf-call core plumbing (measured 2026-08-24)

Decomposed the ~17ns leaf-call core (5M-iteration probe: `z() { return
1; }` 40.4ns/call minus the 23.7ns no-call loop) into the body ops
(~3ns each) and the machinery, then shaved four pieces:

- **`global_matches`** — the global-leaf-cache validation read the cached
  global handle in place instead of `global_object`'s per-call Rc clone.
- **`bind_this_value` skipped for no-`this`-slot leaves** — the common
  leaf's call now returns `undefined` without the call.
- **`Agent::realm_count`** — the `realms.borrow().len() == 1` check (five
  call sites) is a plain `Cell<usize>`, exact because `realms` is only
  ever pushed (via `initialize_host_defined_realm`) and never popped.
- **The accumulator save/restore removed from `run_leaf_regs` and
  `run_leaf_body`** — `Vm::acc` is read only by the register executor,
  and every leaf-inline call site sits in a step-path body where the
  caller's `acc` is dead (the step path never reads it; the leaf's first
  op loads the accumulator from scratch). The loop-counter save stays
  (a leaf's own fast loop must not clobber the caller's live counter).

Measured: ~-1.6ns/call on both the zero-arg and `n = f(n)` rows
(5M-iteration probe, alternating both pair orders: z 198→190ms, f1
228→220ms) — the first core-plumbing win. The remaining core cost is
`run_leaf_ops`' dispatches (the actual body work) plus the frame/
completion/env saves, which are required.

Validation: clippy `-D warnings` clean, workspace tests green (4316/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 23 — the acc
removal is a semantic claim the full sweep backs).

### Cut 35 slice 26 — inline number-number `Add`/`Exp` (measured 2026-08-24)

The register-op dispatch (`run_leaf_ops`) and the step path's
`BinaryImm` both inline the number-number arithmetic shape for
`Sub`/`Mul`/`Div`/`Rem` — but not `Add` or `Exp`, so the most common
combine (`x + 1`, and every acc-combine `(x + 1) + 1` chain) fell to
the general `apply_binary` call. The `binary_inline` helper (the fused
`BinImmLocal`/`BinCtxReg`/`BinAccPop` shapes) already had `Add` — the
acc-combine arms were inconsistent. All four inline sites now cover the
full arithmetic set (`Add`/`Sub`/`Mul`/`Div`/`Rem`/`Exp`); `apply_binary`
for two numbers is a plain float op, so the inlines are exact, and the
captured/per-iteration binary arms (`BinContext`/`BinPerIter`) now route
through `binary_inline` for the same reason.

Measured with a body-op slope (5M-iteration probe, `return x + 1 + 1 +
…` with increasing `+1` counts): each acc-combine `+1` op dropped from
~7ns to ~3.7ns — the ops2/ops3/ops4 rows fell 249→229, 283→248,
323→266ms. The bench rows do not move (they already use the inlined
fused shapes — `i * 2` is `Mul`, `n + i*2` and `o.a + o.b` combine via
`binary_inline`); the win is the generic `+`/`**` arithmetic bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4315/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 25).

### Cut 35 slice 27 — micro-benchmark vs Node re-measurement (measured 2026-08-24)

Re-ran the Cut 11 comparison against Node v24.12.0 (same sources, same
warmup-then-time-2nd-run methodology, all 8 rows). Node runs each row in
a clean per-row context (`new Function`, warmup call then timed call —
`scratch/bench_node2.js`); that is what reproduces Cut 11's node numbers
(arith 10.2/11ms, array 24.9/26ms, calls 16.1/16ms jitless). Medians of 3
interleaved runs of each binary (order rotated per round to cancel the
machine drift):

| Benchmark | slag | node (jit) | node (--jitless) | slag vs jitless |
|---|---|---|---|---|
| arithmetic | 26.1ms | 0.6ms | 10.4ms | 2.5x |
| property access | 57.8ms | 0.3ms | 14.9ms | 3.9x |
| string concat | 18.4ms | 0.5ms | 1.4ms | 13.3x |
| array iteration | 52.5ms | 0.6ms | 24.6ms | 2.1x |
| function calls | 44.3ms | 1.1ms | 18.6ms | 2.4x |
| closure capture | 52.9ms | 0.9ms | 18.6ms | 2.8x |
| per-iteration | 16.9ms | 0.2ms | 2.5ms | 6.8x |
| construct churn | 38.6ms | 1.7ms | 3.0ms | 12.9x |

All 8 rows report `ok=true` on both engines. Since Cut 11 (measured
2026-08-21, jitless ratios 12-40x) the interpreter core has closed most
of the gap: function calls 2.4x (the old 40x standout, closed by the
caller-frame arg reads of slice 23 and the leaf-call plumbing shave of
slice 25), arithmetic 2.5x, closure capture 2.8x, property access 3.9x,
array iteration 2.1x — the closest row. The remaining gaps are the
non-core machinery: string concat 13.3x and construct churn 12.9x lead
(node's cons-string ropes and fast allocation are hard interpreter
targets), then per-iteration 6.8x (closure call/env reads).

Harness trap: an earlier attempt eval'd each source in a shared global
scope (mirroring how the slag `--bench` rows share one context) and
measured node array iteration at 62ms jit / 88ms jitless — "beating"
node's JIT with slag's 52ms. That under-states V8: the timed 2nd eval is
a fresh compilation TurboFan has not finished tiering, and the
accumulated global `var`s push the global object to dictionary mode (the
same shared-context artifact class as slag's own slice-5 cache thrash).
Cut 11's node numbers reproduce only with the clean per-row harness, so
that is the comparison used here. Note the asymmetry: slag's `--bench`
keeps all 8 rows in one context while node is per-row clean; the
`--jitless` column is the design-relevant interp-vs-interp picture.

### Cut 35 slice 28 — rope appends: iterative drop, Arc-shared flats, higher fold cap (measured 2026-08-24)

The string-concat row builds a 100k-unit rope via `s += 'x'`. Its cost
split into the append machinery (an `Rc<JsString>` wrapper + rope node +
right-operand box per append) and the depth-cap flatten: `concat` folded
the *entire accumulated left side* into a fresh flat every
`ROPE_MAX_DEPTH` (1024) appends, copying ~n²/(2·cap) units — ~10 MB at
100k, ~1 GB at 1M (the 1M probe took ~480 ms, ~3× the machinery floor).
The cap existed only to bound drop recursion.

Three changes to `crates/crux/src/string.rs`:

- **Iterative drop.** `RopeNode` children are now `Option<JsString>` and
  `Drop` unwinds the tree with an explicit worklist
  (`Arc::try_unwrap` on uniquely-owned subtrees, decrementing shared
  ones), so arbitrarily deep chains — including the right-leaning
  prepend chains the cap never protected — free without stack recursion.
- **Arc-shared flat buffers.** `Flat(Arc<[u16]>)` makes `JsString` clones
  O(1): the rope's per-append `right` operand (a 1-unit box every
  iteration) is now an Arc bump, and the depth fold shares the
  materialized flat instead of copying it a second time.
- **Higher fold cap (1024 → 16384).** The cap now exists purely for the
  amortized fold-vs-drop tradeoff: folding every 16384 appends copies
  ~n²/32768 units (~0.6 MB at 100k, ~60 MB at 1M) and bounds the final
  tree to ~cap nodes, so dropping a 100k chain went from ~7 ms (100k
  individual node frees, the uncapped version) to <1 ms.

Measured (release, 3-run interleaved medians vs the slice-26 base
binary):

| Row | before | after |
|---|---|---|
| string concat | 18.6ms | **16.7ms** (-10%) |
| string concat 1M probe | ~480ms | **~230ms** (2.1x) |

All other bench rows flat (property-access +3.4% and per-iteration +2.8%
were within the ±5% drift band — a tight interleaved re-check of
property access showed no regression). Semantics are unchanged (strings
are immutable; the fold/flatten are internal representation), but the
shared rope machinery warranted the full sweep: language 23,724/0/34,
annexB 1,086/0/0, built-ins 23,812/0/154 with 447 >15s hangs — identical
to baseline. Workspace tests 4316/0, clippy `-D warnings` clean.

The remaining gap vs node jitless (1.4ms → ~12x) is the append machinery
itself (~165ns/iter): the per-append `Rc<JsString>` wrapper and node
allocation on top of the VM's leaf-register body. The structural next
lever is a `Value::String` representation that avoids the per-append Rc
box.

### Cut 35 slice 29 — inline the string-string concat in `binary_inline` (measured 2026-08-24)

A leaf-path decomposition of the string row (1M-iteration probes): the
loop/register machinery is ~30ns/iter, the VM's concat path (the
`apply_binary` call + two `as_string` Rc round-trips + the result
`Handle`) added ~28ns, and the rope concat itself is ~57ns base plus up
to ~50ns of fold overhead at the 100k bench scale. Slice 29 removes the
VM-side call and Rc churn:

- `binary_inline` now inlines the (String, String) `Add` case — the rope
  concat — skipping the `apply_binary` call (the number-number inline
  from slices 10/26 was already there; this is the string counterpart).
- New `Value::as_string_ref` borrows the string without `as_string`'s Rc
  reconstruct-and-clone round-trip; `apply_binary`'s string fast path and
  the new inline both use it.
- `LeafOp::BinConst` now routes through `binary_inline` (replacing its
  duplicated number inline), so the bench body's `s += 'x'` (LoadReg,
  BinConst String(x), StoreReg) takes the string fast path.

Measured (1M-iteration leaf probes, interleaved base/new): the
empty-append probe (`s += ''`, the is-empty fast path — no node build)
went 63 → ~56ms (~7ns/iter: the call + Rc round-trips removed), and the
and the full-append probe's min went 191 → 182ms. The bench row moved 16.7 →
~16.5ms (within the ±5% drift band — the isolated probes carry the
signal). Validation: clippy `-D warnings` clean, workspace tests 4316/0,
full sweeps at baseline (language 23,724/0/34, annexB 1,086/0/0,
built-ins 23,812/0/154 + 447 hangs).

### Cut 35 slice 30 — merge the rope node into the string box (measured 2026-08-24)

`JsString` becomes a single enum: `Flat(Arc<[u16]>)` or
`Rope { left/right: Option<Handle<JsString>>, len, depth: u32, flat }`.
The rope node IS the box the value points at, so an append is **one
allocation** (was: an `Rc<JsString>` wrapper + an `Arc<RopeNode>`), and
`concat` now takes `&Handle<JsString>` and returns `Handle<JsString>` —
the operands' own boxes become the node's children (Rc bumps, no
copies). The empty-operand paths return the operand handle with no
allocation at all. Sizes: the box went 16 → 48 bytes (enum + u32 depth),
so each append allocates 64B instead of ~144B across two boxes.

Consequences handled:

- **`!Send`.** Rc children make `JsString` non-`Send`; the only `Send`
  consumer was the well-known-symbol table, which is now `thread_local`
  (per-agent — the spec wants per-realm symbols anyway).
- **Iterative drop.** The `Drop` impl `take`s the children (Rc has a null
  niche, so `Option<Handle>` is still 8 bytes) and unwraps uniquely-
  owned subtrees with a worklist; cloning children instead of taking
  them silently degraded to recursive field-drop deallocation (the first
  cut overflowed the stack at 200k nodes — the take-based version is
  what ships).
- **Flat-cache sharing lost.** Rope clones get fresh `flat` caches (a
  shared rope flattened through two handles flattens twice); the
  `rope_equality_and_clone_share_the_flat_cache` test became
  `rope_equality_and_clone_correctness` (content, not pointer,
  equality).
- **`large_enum_variant` lints.** The 16 → 48B `JsString` grew every AST
  node embedding it, crossing clippy's threshold on `ExportDecl`
  (`crates/syntax/src/ast.rs`) and `StaticElement` (`class.rs`) — both
  got targeted `#[allow]`s with comments (boxing the AST strings is
  deferred; the AST is transient compiler input).

Measured (release, interleaved 3-run medians vs the slice-29 binary):

| Row | before | after |
|---|---|---|
| string concat | 17.6ms | **12.0ms** (-32%) |
| string concat 1M probe | ~210ms | **~132ms** (-37%) |
| empty-append 1M probe | ~56ms | **~38ms** (-32%) |

All other bench rows within ±2.3% (function calls +2.3%, closure
capture +1.6% — a residual code-layout cost of the bigger string code;
per-iteration -6.8% was within noise). One trap surfaced and fixed
during measurement: `binary_inline`'s string-string `Add` inline
**bloated every register-op call site's icache** (the concat body is
large) — the call and closure rows measured +3-6ns/call until the
string path moved to a `#[inline(never)]` `concat_strings` helper. The
slice-29 `Value::as_string_ref` borrow is gone (dead): the Handle-based
`concat` needs the `as_string` handles.

Validation: clippy `-D warnings` clean (the two allows above),
workspace tests 4316/0, full sweeps at baseline (language 23,724/0/34,
annexB 1,086/0/0, built-ins 23,812/0/154 + 447 hangs). The remaining
string-concat gap vs node jitless (1.4ms → ~8.6x) is the loop machinery
plus the allocation itself — the next lever is the arena/GC milestone.

### Interpreter per-op floor (2026-08-31 measurement, resolved 2026-09-01)

The 2026-08-31 profiling of the sweep's slowest cluster (the RegExp
property-escapes / CharacterClassEscapes fixtures, ~380-430 load-dependent
"hangs") traced the cost to the interpreter's per-op speed: the vendored
harness `buildString` (`test262/harness/regExpUtils.js`) fills a JS array
in a loop and calls `String.fromCodePoint.apply`, and that body ran
entirely interpreted:

| primitive | 2026-08-31 | now (2026-09-01) | node (approx.) |
|---|---|---|---|
| bare `for` iteration (1M) | ~0.7 µs/iter | ~14.4 ns/iter (~49x) | 1-10 ns |
| indexed store `a[l++] = v` (1M) | ~2.8 µs/op | ~48 ns/op (~58x) | ~5-20 ns |
| buildString shape (3M appends) | ~1670 ms | ~180 ms (~9x) | ~6 ms |

Both levers the 2026-08-31 section recommended are resolved:

1. **JIT certification for the `buildString` shape is done** (2026-09-01):
   dense element stores and the array-store / member-write / `.apply`
   steps certify — the JIT rows are buildString shape ~43ms / full ~38ms
   under the default CLI JIT (see the CLI-JIT-default section), and the
   apply/call builtins leaf-inline certified-leaf callees (see the
   vector-call/apply section). The hang cluster is gone — current full
   sweeps report 0 hang.
2. **The interpreter's core loop itself is ~50x better on the same
   shapes.** The bare loop is ~14.4ns/iter and an indexed store ~48ns/op
   (the cuts since then: fused loop heads, register bodies, the raw-f64
   loop counter, the generation-validated element caches, and the
   allocation-free element encode). The `--bench` rows `bare loop` and
   `indexed store` keep the floor visible, per the original section's
   suggestion. The residual gap to mainstream on the store shapes is the
   per-op helper/FFI cost and the property-vector writes — a real but
   secondary target, as the original note said.

### CLI JIT default, detached-view length, and the typed-array JIT picture (measured 2026-09-01)

Supersedes the "the JIT changes nothing on these loops" claim in the floor
section above: dense element stores (609ce62) and the `buildString` shape now
certify (b61a0b5), so the JIT is a real escape hatch for that cluster
(`--jit-bench` buildString rows: 191ms → 43ms).

- **The CLI installs the JIT by default** (2026-09-01): `slag file.js` runs
  certified bodies through Cranelift; `--jitless` opts out; `--bench` still
  measures the interpreter floor. Default vs `--jitless` on the Node probes:
  buildString shape 43ms vs 194ms (4.5x), buildString full 29ms vs 113ms
  (3.9x).
- **The numeric-index typed-array `length` fast path missed the
  detached-buffer branch**: `typed_array_effective_length` returned the
  pre-detach length after `transfer()` (the spec's length getter returns 0
  for a detached view, 25.2.3.1). Fixed at the source — the function now
  returns 0 for a detached buffer, which also fixes
  `typed_array_own_property_keys` on detached views.
- **The "JIT ignores typed-array loops" reading was a probe artifact**: a
  `new Uint8Array(...)` inside the body emits `Step::Construct`, which the
  JIT cannot lower, so the whole body bails to the interpreter. With the
  array hoisted (passed in), the same loops compile: `view.length` loop
  56ms → 18ms (3.1x), element-write loop 92ms → 54ms (1.7x). Native `fill`
  stays ~700ms with or without the JIT — the builtin pays a per-element
  `Vec` encode/decode allocation.

Node v24.12.0 comparison (same machine, interleaved 7-rep medians):

| probe | node | slag default (jit) | slag --jitless | gap |
|---|---|---|---|---|
| buildString shape (3M appends) | 6ms | 42ms | 189ms | 7x |
| buildString full (2.16M cps) | 13ms | 28ms | 110ms | 2.2x |
| typed-array length read (800k) | ~1ms | 56ms | 56ms | ~56x |
| typed-array element write (800k) | ~1ms | 90ms | 90ms | ~90x |
| typed-array native `fill` (800k) | <1ms | 680ms | 680ms | ~680x |

### Typed-array fill fast path, the JIT inline typed-array store, and the remaining typed-array picture (measured 2026-09-01)

The typed-array rows above were the largest remaining gaps; two fixes closed
most of them, and the CLI's `--jit-bench` gained permanent rows (typed-array
write / typed-array length) so the floors stay visible. Note the table's
56ms/90ms columns for the length/write rows were the INTERPRETER numbers —
the hoisted-array probes compile (see the probe-artifact bullet above) and
measure 19.5ms / 51.5ms under the default JIT.

- **`fill` encodes once and writes the buffer directly.** The old loop ran
  `set_property(this, &key(k), value)` per element: a decimal-string
  `JsString` allocation, a `canonical_index` parse back to an index, and a
  fresh `encode_element` `Vec` per write. The builtin already coerces the
  value exactly once (spec 25.2.3.9 step 4) and re-validates the view after
  the coercion (steps 12-13), so the new path encodes once and writes the
  same bytes per element — `JsObject::typed_array_fill_encoded`, no
  per-element key, parse, or allocation. Measured in the release unit
  harness (5 fills of 800k, ~1.97ms each): **~680ms → ~2ms (~340x)**, i.e.
  the ~680x row is now ~2x vs node.
- **The JIT's inline store helper now handles typed arrays.**
  `fast_array_element_write` (the compiled `o[i] = v` fast path) previously
  covered only plain Arrays; typed arrays fell through to the general
  `assign_member_computed` helper — a second FFI call plus re-checks per
  element. It now stores through `typed_array_element_set` for
  `IntegerIndexed` receivers, gated on PRIMITIVE values only: an
  Object/Function value's element coercion (ToNumber/ToBigInt) can run
  `toPrimitive` user code, which the helper's "never sets the pending byte"
  contract forbids, so those return 0 and the fallback runs the identical
  coercion on the VM (nothing observable ran on the fast attempt, so
  re-running is safe; the wrong-content-type TypeError is thrown by the
  fallback). A/B on the new bench row (3 runs each, alternating order): the
  JIT write row dropped **~59ms → ~51.5ms (~13%)**, non-overlapping spreads.
- **The own-`length` divergence is fixed** (2026-09-01, `58b6763`): a
  typed array CAN carry an own `length` data property —
  `Object.defineProperty(ta, 'length', {value: 1})` succeeds via
  OrdinaryDefineOwnProperty — and it must shadow the prototype accessor
  (node reads 1), but the interpreter's `get_member_name` typed-array
  shortcut returned the slots length first (slag read 4). The shortcut is
  now gated on the absence of an own `length`
  (`JsObject::has_own_property_atom` — a parse-free vector scan, usually
  empty; measured ~6% on the JIT length row, within noise), falling
  through to OrdinaryGet when one exists; a configurable delete restores
  the accessor. Every read path shares the helper, so the JIT is covered
  transitively. Verified against node v24.12.0; unit tests cover
  data/accessor shadowing, redefine, delete, and unrelated own
  properties; the full sweep is green. The JIT `view.length` helper
  (below) is now safe to build.

Remaining levers (the two typed-array rows below are closed — see the
next section), in order of expected value:

| lever | current gap vs node | where |
|---|---|---|
| GC slot-arena allocation (GC-5's remaining lever) — `Gc::new` heavier than `Rc::new`; recovers the construct-churn / string-concat ~2x regressions | 2x | `.notes/gc-plan.md` |
| interpreter per-op floor — the 0.7µs bare-loop iteration is ~100x off mainstream; the floor section calls the VM core a real but secondary target | ~100x | `runtime/src/ir.rs` |
| the apply floor — **closed** (the compiled `CallApply` step below): interp 208→101ms, jit 204→89ms on the `apply leaf call` row; the residual is the member read + element reads + leaf frame setup | ~5x on a 1-elem apply | `runtime/src/ir.rs` |

### Typed-array element encode and the compiled length probe (measured 2026-09-01)

The two remaining typed-array levers are closed; the typed-array rows are
now the closest to node in the whole suite.

- **The per-element encode no longer allocates.** `encode_element`'s
  fresh `Vec<u8>` per write is replaced by `encode_element_into` — a
  stack `[u8; 8]` buffer (the largest element is 8 bytes) returning the
  used length — used by every per-element write path
  (`typed_array_element_set`, `typed_array_set`, the index define),
  plus Atomics' `element_raw` and `fill`'s encode-once. The JIT store
  helper inherits it through `typed_array_element_set`. A/B on the
  `--jit-bench` rows: the JIT write row **51.5ms → ~30ms (~42%)** and
  the interpreter write row **~92ms → ~73ms (~20%)** (3 runs each,
  stable).
- **The compiled `view.length` read serves the slots directly.** A new
  `typed_array_length` helper (the four-file mirror: a pure
  `(ctx, object)` probe returning the slots length for an IntegerIndexed
  receiver or the canonical-NaN sentinel) fronts the compiled
  `GetMemberName`-with-`length` site before the member-cell probe — a
  hit skips the `get_member_name` FFI round-trip. The probe is exact for
  the same receivers the interpreter's fast path serves (the
  own-`length` shadow is handled on the interpreter side); every other
  receiver falls through to the member-cell probe / helper unchanged
  (`{ length: 5 }` reads, string `.length`, etc.). The helper is
  whitelisted as leaf-eligibility-neutral (pure, `emit_raw_call`).
  Measured: the JIT length row **19.5ms → ~11.3ms (~42%)** (3 runs,
  stable); the interpreter row is unchanged (its own fast path already
  served the slots).

With these, the table's earlier gaps collapse: the JIT length row is
~11x vs node (was ~56x per the older table) and the JIT write row ~30x
(was ~90x); both rows are the closest to node in the suite alongside
buildString.

### Vector-call leaf-inline and the apply/call leaf fast path (measured 2026-09-01)

The last call-step gap from `.notes/jit-report.md` §7 item 7 — "a vector
call to a certified LEAF still runs the general `call_inner`" — is closed,
and `Function.prototype.apply`/`call` gained the same leaf fast path. New
`--jit-bench` rows: `vector leaf call` (9-arg leaf, 200K) and
`apply leaf call`. (The `vector leaf call` row now uses 17 args — see the
fast-argument cap note below.)

- **The interpreter's vector-form call (`Step::Call` — the ≥9-arg or
  spread form) now leaf-inlines.** `do_call` rebuilt the fast-form layout
  on the value stack and routed through `do_call_fast`, the same shared
  core the JIT's `call_vector` helper already used (Cut 50): a
  certified-leaf callee runs inline on the caller's Vm — no
  execution-context push, no fresh-Vm round trip. The JIT side was
  already there; the interpreter's `do_call` was the remaining
  general-`call_inner` gap. Direct eval, the callable check, and the
  general path all come from the shared core unchanged (the old handler's
  special cases deleted). Measured on the new row: **interp 53.2ms →
  21.3ms (2.5x)**, within 1.25x of the JIT's ~17ms.
- **`f.apply(this, arr)` / `f.call(this, …)` to a certified leaf run the
  leaf machinery.** The builtins previously built the arg list and went
  through `crate::function::call` → `ordinary_call` (the general path —
  fresh Vm + execution-context push). New `try_leaf_call` runs a
  certified-leaf callee through `do_call_fast` on a POOLED Vm —
  register-op (or JIT machine-code) body execution with no context push.
  It mirrors `fast_call_core`'s leaf gate: a single realm, an EcmaScript
  function, and a `leaf_lookup` hit — which only ever caches
  `leaf_inline` bodies (`set_compiled` excludes class constructors and
  class-field initializers), so `C.apply(null, [])` still throws the
  "must be called with new" TypeError through the general path. Design
  notes: it deliberately does NOT route through the running Vm (the
  caller's `&mut Vm` and an args slice into its stack are live across the
  native-handler call — mutating it through a raw pointer would be UB),
  and the pooled Vm is registered as an `ACTIVE_RUNS` entry for the whole
  leaf window (`with_leaf_run`) so a budget collection inside the body
  traces its stack exactly like `run_inner` (verified under
  `--gc-stress`). Measured (A/B, 3 runs each, alternating order): **interp
  235ms → 208ms (~11%), jit 223ms → 204ms (~8.5%)** on the new row;
  apply-to-leaf is also ~100ms faster than apply-to-a-non-leaf (315ms),
  i.e. the callee dispatch is the saved part.

**Remaining apply floor (follow up later).** The apply row is still ~1µs
per call (208ms/200K) with the leaf callee itself only ~85ns — the floor
is the builtin round-trip, not the callee: reaching `apply` through the
general call machinery, `create_list_from_array_like`'s per-call `Vec`
build (the dense fast path exists but still copies the elements), and the
pooled-Vm take/return resets. Cutting it needs call-site recognition of
the `.apply` pattern — inlining `f.apply(a, arr)` as a spread/vector call
in the compiler (the V8 approach) — a substantially larger slice than the
leaf-inline here. **CLOSED 2026-09-01** — see the compiled `CallApply` step
section below.

### Call-site `.apply`/`.call` recognition: the compiled `CallApply` step (measured 2026-09-01)

The apply floor is closed by recognizing the `.apply`/`.call` member-call
pattern in the compiler (`Compiler::try_compile_apply_call`): `f.apply(x,
arr)` / `f.call(x, ...)` in a certified body compiles to the normal member
read (a shadowed `apply`/`call` resolves onto the stack and is called
normally) plus the new `Step::CallApply`, whose handler compares the
resolved function against the realm's cached intrinsic
(`Intrinsics::apply_builtin`/`call_builtin`) and, on a match, calls `f`
directly on the caller's Vm — the `this` argument, the
`CreateListFromArrayLike` element reads (dense-array fast path included),
and the leaf-inline `do_call_fast` run with no builtin dispatch, no
`leaf_lookup` HashMap, and no pooled-Vm take/return. Any other resolved
function falls back to the general call of that function exactly as
`CallFast` would, so shadowing, the is-callable-before-argArray order
(the intrinsic's step-1 TypeError runs before any getter), array-like
(non-Array) arg lists, getter elements, holes, and the extra-arg ignore
stay spec-exact. The JIT lowers the step through the `call_apply` slow-path
helper (the four-file mirror), so compiled bodies with `.apply`/`.call`
keep compiling.

Measured on the `apply leaf call` row (200K, release, 3-run medians):
**interp 208ms → ~101ms (~2.05x), jit 204ms → ~89ms (~2.3x)**; the
remaining ~90-100ms is the per-call member read, the `CreateListFromArrayLike`
length/element reads, and the leaf-inline frame setup (the leaf itself is
~85ns). The `call_apply` helper also means a compiled body containing a
`.apply` call no longer bails out of the JIT. (The arg-list build still
allocates a per-call `Vec` for the general path — the dense fast path
avoids the per-element key/Get machinery, not the allocation; inlining
the dense element pushes onto the stack is a listed follow-up.)

**Dense-array argument list** (2026-09-01): the listed follow-up landed.
`do_call_apply`'s `Apply` arm now recognizes a dense Array argArray and
pushes its elements straight onto the value stack — no per-call `Vec`
allocation, no `length` property path / `ToLength` round trip, no
`[[Get]]`-loop element reads. The gate is exact: a hole, a length past
the buffer end, or a non-Array falls back to
`create_list_from_array_like` (whose own fast paths and the spec `[[Get]]`
loop keep those shapes unchanged), and the buffer borrow never spans the
call (a re-entrant callee may mutate the argument array). Measured on the
`apply leaf call` row (200K, release, A/B medians): **interp ~101ms →
~57ms, jit ~98ms → ~50ms (~1.8x / ~1.9x)** — the JIT's `call_apply` helper
inherits it automatically. Spec-exact: the new
`apply_dense_array_fast_path_preserves_spec_semantics` test (holes,
partially-filled arrays, re-entrant mutation, 10k-element arrays,
array-likes, getters) plus the Function/prototype/apply+call fixture
clusters clean under `--gc-stress --jitless`; full sweep (JIT default and
`--jitless`) at 0 fail / 0 crash / 0 hang.

**Fast-argument cap raised to 16** (2026-09-01): `FAST_CALL_MAX_ARGS` 8 →
16 — plain calls with 9-16 arguments now take the fast `CallFast`/`TailCallFast`
form (one step, the leaf-inline probe) instead of the vector form, whose
JIT path ran 11 helper calls per iteration (`ArgsBase` + 9×`ArgsPush` +
`Call`). The two `[Value; FAST_CALL_MAX_ARGS]` stack buffers
(`do_call_apply`, `run_inline_leaf`) grow with the cap; a 17+-arg call or
a spread still takes the vector form. Measured on the `vector leaf call`
row's 9-arg shape (200K, release): **interp ~22.5ms → ~14ms, jit ~17.9ms
→ ~2.4ms (7.4x)** — the 9-arg leaf now runs fully in machine code. The
row was bumped to 17 args so the vector form stays benchmarked (interp
~30ms, jit ~21ms). The vector-form JIT tests that used 10-arg calls
switched to 17 to keep their coverage. Spec-exact: the new
`wide_fast_form_calls_stay_spec_exact` test plus the full sweep (JIT
default and `--jitless`) at 0 fail / 0 crash / 0 hang.

**Fast-argument cap raised to 32** (2026-09-02): `FAST_CALL_MAX_ARGS` 16 →
32 — plain calls with 17-32 arguments now take the fast form too. The
remaining `vector leaf call` row's 17-arg shape (200K, release, A/B):
**interp ~28.8ms → ~17.0ms, jit ~21.4ms → ~2.8ms (7.6x)** — the 17-arg
leaf now runs fully in machine code (the vector form's JIT path ran 18
helper calls per iteration: `ArgsBase` + 16×`ArgsPush` + `Call`). The
row was bumped to 33 args so the vector form stays benchmarked (interp
~44-48ms, jit ~31-32ms); the vector-form JIT tests that used 17-arg
shapes switched to 33 to keep their coverage. Spec-exact: the
`wide_fast_form_calls_stay_spec_exact` test now covers the 33-arg
vector boundary, plus the full sweep (JIT default and `--jitless`) at 0
fail / 0 crash / 0 hang.

**Prototype-chain member-read cache** (2026-09-01): the remaining
member-read cost of the apply path — `f.apply`'s `apply` lives on
`Function.prototype`, so the own-property member cells never serve it and
every read paid the full prototype walk. A new agent-level
`member_chain_cells` cache stores the resolved chain value for
`(receiver, name)`, re-validated by the receiver's generation and each
walked link's (id, generation) (an own property on the receiver, a link's
mutation, or a proto replacement bumps a generation; links below the
found one cannot shadow its own data property, and accessors are never
cached). This serves every method read (`f.apply`, `arr.push`, `o.m()`),
not just the apply path. Measured on the `apply leaf call` row (200K,
A/B): **interp ~57ms → ~20ms, jit ~52ms → ~18ms (~2.8x / ~2.9x)** — the
interpreter's member read and the JIT's `GetMemberName` slow helper both
hit the cache. Spec-exact: the new
`chain_member_cache_stays_spec_exact_under_invalidation` test (own-prop
shadowing, link redefinition, accessors run per read, proto replacement,
`Function.prototype.apply` patching) plus the full sweep (JIT default
and `--jitless`) at 0 fail / 0 crash / 0 hang.

**The JIT inline prototype-chain probe was implemented and reverted
(measured regression, 2026-09-01).** The natural follow-up to the cache
above — the JIT's `emit_member_cell_read` still called the
`GetMemberName` helper for chain reads (`f.apply`), so a compiled probe
was added to its slow path: a Function receiver hops through
`crux::Function.object` to the receiver JsObject, the cell's (id, name,
receiver generation, link_count) is validated, and the cached links'
(id, generation) are walked against the LIVE `prototype` fields — a hit
serves the cached value with no helper call, a miss falls through to
`GetMemberName` exactly. Soundness mirrors the interpreter (receiver
generation covers own-property changes; each link's generation covers
its mutation or a proto replacement; accessors are never cached). A/B
vs the committed cache (HEAD `2b787ff`, same machine, interleaved 20M
runs): the probe is **~5-10% SLOWER than the helper** on pure chain
reads — `o.m` on a prototype 424-440ms → 469-486ms; `f.apply`
620-638ms → 681-716ms — and the call-dominated `apply leaf call` row
(200K) did not move (jit ~18.3ms → ~19.0ms, within noise but trending
worse). The helper's ~13-16 L1 loads are better scheduled by LLVM than
the raw Cranelift probe, and the call overhead is tiny, so an inline
probe that duplicates the validation cannot win. Reverted; the
interp-side cache served inside the helper remains the win. Traps for a
future attempt: (1) `JsObject` and `MemberChainCell` are repr(Rust),
NOT `#[repr(C)]` despite the doc comments — `JsObject.id` sits at
offset 24 and `crux::Function.object` at 96 — `offset_of!`/`size_of!`
kept the probe consistent, but a stable ABI needs `repr(C)` first; (2)
to win, a probe must CUT the per-read validation (the link-generation
walk is the irreducible ~4 loads, the cell fields ~6-9 more), not
duplicate the helper.

### The interpreter's `LoadIdent` global fast path, mirroring the JIT's Cut 36 probe (measured 2026-09-01)

The `--jit-bench` `global read` row (a 1M `s += g` loop inside a function)
measured ~157ms interpreted — ~10x the same loop reading a local — while
the JIT ran it in ~6.5ms. The gap: a function body reads a script-global
through `Step::LoadIdent` (function bodies carry no `script_globals`, so
`binding()` classifies globals as env-path), and the interpreter's
`LoadIdent` handler walked the env chain + built a `PropertyKey` + ran the
full `Get` per read (~133ns/read).

- **The interpreter now mirrors the JIT's Cut 36 probe.** `Vm::run_inner`
  computes `clean_chain` at entry — the running context's env IS the
  global env record with no outer (the same gate `JitCallContext` bakes
  in) — and the `LoadIdent` handler, gated on `body.env_constant &&
  clean_chain` (an env-constant body adds no envs mid-run, so the entry
  flag stays valid), serves the warmed global-value cell directly when
  the captured name + the live global's id/generation match, else falls
  back to the full resolve. The miss path warms the cell with the JIT's
  exact `load_ident` gate (a Global-env binding with no top-level
  `let`/`const`/`class`), so a hot loop's second read is a native load.
- **The warm gate now records only own DATA properties** — a real bug fix
  that also corrects the JIT's probe: `warm_global_cell` and
  `load_global_value`'s fallback previously recorded the value cell even
  when the binding was an accessor or inherited property, so the probe
  served a stale first-read value forever (an accessor's getter ran once;
  an inherited `Object.prototype` member could change without the global's
  generation bumping). Both paths now skip recording when
  `resolve_global_cell` finds no own-data slot.

Measured: **`global read` interp 157.6ms → ~49ms (~3.2x)** — parity with
reading a local in the same loop shape — with the JIT row unchanged
(~6.4ms); no regressions on the other rows. The remaining ~49ms was the
row's general step-loop floor (the `i < n` limit is a slot, not a
literal, so the fused head did not apply); the fast-binding-limit fusion
(`RelLimit`, committed 2026-09-01) later closed that part — the row
measures ~21.6ms now — and the post-inc store slice below trims the
store shapes.

### Register-encoded post-increment computed stores (measured 2026-09-01)

The `indexed store` per-op floor (`a[l++] = i`, ~48ns/op) decomposed to
~7 step dispatches per iteration plus the store machinery; the body would
not lower to a register body because `l++` (a postfix `UpdateLocal`) had
no register form and the loop counter could not serve as a store value.
The register model now covers the shape:

- **`RegOperand::PostInc { slot, tdz, op }`** — a post-increment member
  key: loading the operand reads the slot (TDZ-checked), writes the
  update back through the shared `update_value` coercion (a non-Number
  `l` goes through ToNumeric), and yields the OLD value — the postfix
  `UpdateLocal` semantics, with the write-back landing before the
  store's nullish check (JS evaluation order). The lowering emits it
  from `Step::UpdateLocal` (`prefix: false`), and only the computed
  member store consumes it — a statement-position update
  (`a[l] = i; l++;`), a prefix form, a read key, or a binary operand
  keeps the body on the step path (where the ordering is preserved).
- **The loop counter as a store key/value** — a `RegOperand::Counter`
  key or value resolves from the dedicated `Vm::loop_counter` field at
  load time (mirroring `LeafOp::LoadCounter` and the JIT's
  `counter_bits`). Since Cut 35 slice 21 the register path never pushes
  the counter onto the value stack at run entry, so the operand must
  not pop one — the slice-16-style pop read a stale stack slot /
  `undefined` in the interpreter (the JIT was already reading the
  field). The single-read guard is unchanged.
- **The JIT mirrors the operand** — `leaf_operand`'s `PostInc` arm emits
  the `emit_update` fast/slow shape (inline f64 add + `UpdateValueSlow`
  fallback) and writes the slot back, so a compiled body containing the
  shape keeps compiling (no bail regression under the default JIT).

The body `a[l++] = i` compiles to `RunRegBody { LoadReg a;
StoreMemberComputed { key: PostInc(l), value: Counter } }` — one dispatch
instead of seven steps. Measured (release `--bench`, medians of 3):
**`indexed store` 47.2ms → ~42.7ms (~10%)**, the 5M-iteration post-inc
probe 246ms → ~222ms (~5.6ns/op saved). The `buildString shape` row (a
`l++` store plus an `if (l === 10000)` guard) stays on the step path —
the register lowering is whole-body and rejects the guard's branch; a
per-statement register-run segmentation is a listed follow-up. Spec-exact:
`register_post_inc_member_store_stays_spec_exact` plus the full sweep
(JIT default) at 0 fail / 0 crash / 0 hang.

### Per-statement register-run segmentation (measured 2026-09-01)

The post-inc lowering was whole-body: a loop body containing any
control-flow statement (an `if`, a break) rejected the entire body back
onto the step path, leaving the `buildString shape` row (the `l++` store
plus an `if (l === 10000)` guard) unlowered. The compiler now segments
the body's compiled steps into maximal straight-line runs
(`lower_leaf_ops_segmented`): each run lowers via the shared per-step
`lower_step` (the whole-body `lower_leaf_ops` became a wrapper over it)
and is replaced with its own `Step::RunRegBody`, with the branch and
completion steps between runs staying on the step path
(`apply_register_runs` re-bases the labels and fixups recorded from the
body start, and a label landing strictly inside a run keeps the whole
body unsegmented).

Three soundness constraints the segmentation must honor:

- **The list wrappers stay paired on the step path** — a run must never
  absorb a `ListBegin`/`ListEnd`: an absorbed `ListBegin` with a
  step-path `ListEnd` pops the ENCLOSING block's completion entry
  (nested loops in blocks restored a stale value into the script
  completion). The run commits at the statement boundary before a
  wrapper and the wrapper executes on the step path.
- **A run must not start at a `SetCompletion`** — absorbing a
  statement's `SetCompletion` without the statement's value-producing
  steps leaves the statement's result on the stack: a one-slot
  per-iteration drift in the compiled path, which pre-allocates its
  working area from `max_stack_usage` (the JIT crashed on
  `o[i] = i; b['x'] = i` with a Vec-corruption panic).
- **The register ops' literal values are traced** — a member store's
  `Const` key string lives in the `RunRegBody` ops; `Step::trace`
  missed them, so the per-allocation collector swept the box mid-run
  and the key read back as garbage under `--gc-stress`
  (`Step::RunRegBody` now walks the ops via `trace_leaf_op_heaps`).

Measured (release `--bench`, medians of 3, A/B against the post-inc
tree): **`buildString shape` ~193.8ms → ~181.9ms (~6%)** — the `l++`
store runs as a two-op register body while the guard `if` (condition,
branch, nested block) runs on the step path. The `indexed store` row is
unchanged (~45ms under load). Spec-exact: new tests
`segmented_loop_body_keeps_list_wrappers_balanced`,
`register_counter_member_operands_stays_spec_exact`, and
`register_run_ops_are_rooted_under_gc_stress` plus the JIT
script-completion table's segmented shapes; the full sweep (JIT default
and `--jitless`) at 0 fail / 0 crash / 0 hang, and the `statements/for*`
cluster clean under `--gc-stress --jitless`.

### Node comparison on the `--jit-bench` suite, JIT and JITless (measured 2026-09-02)

The 2026-08-21 Node comparison predates the bytecode-VM migration and the
Cranelift JIT; re-measured against node v24.12.0 on the same machine and
session, in both JIT (default) and interpreter-only (`--jitless`, V8's
Ignition bytecode interpreter) modes, over the full `--jit-bench` suite
(12 rows). Harness: `scratch/jit_bench/node_bench.js`, running the exact
sources. Slag columns are the best of 3 `--jit-bench` process runs (each
mode = one warmup eval + one timed eval in a fresh context); Node columns
are the best (steady-state) round of 3 warmup calls + 5 timed calls. All
four modes agree on every row's completion value (spot-checked
`buildString full` → 2162678 in all four).

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

(Gaps are slag ÷ node — how many times faster Node is. Times are ms.)

- **Interpreter vs interpreter is the closest picture.** Slag's
  interpreter trails V8's `--jitless` Ignition by a median ~3.4x (range
  2.6x–12.6x) — same order of magnitude for a young step-loop VM. The
  outliers are `compound assign` (12.6x) and `string concat` (7.1x), the
  shapes where V8's interpreter keeps a fast-path edge Slag's step loop
  lacks.
- **JIT vs JIT is the big gap.** Slag's Cranelift JIT trails V8's
  TurboFan/Sparkplug by a median ~17x (3.2x–258x). The worst rows are the
  whole-loop-specialization shapes: `vector leaf call` (258x — V8 inlines
  the 33-arg leaf into the loop and eliminates the dead args; Slag runs
  its per-iteration leaf-inline probe + register protocol), `typed-array
  write` (103x — no bounds-check-elided store loop), `compound assign`
  (41.5x) and `function calls` (32x). The narrowest are the
  inlined-arithmetic shapes — `buildString full` (3.2x), `string concat`
  (5.0x), `arithmetic` (5.7x) — where Slag's JIT keeps the hot
  counter/accumulator register-resident, closest to TurboFan's output.
- **Slag's JIT roughly matches Node's interpreter-only mode** — parity or
  better on 6 of 12 rows (`arithmetic` 3.1x faster, `global read` 2.0x,
  `property read` 1.8x, `typed-array length` 1.5x, and parity on
  `function calls`/`buildString shape`), trailing 1.2x–3.1x on the rest.
  This is the realistic "JIT vs a modern interpreter" read.
- **JIT efficiency over each engine's own interpreter**: Slag 1.1x–8.0x
  (median ~3.7x); Node 2.6x–82x (median ~25x) — V8's tier-up buys ~7x
  more, because its interpreter is already fast and TurboFan specializes
  aggressively (hidden classes, inline caches, callee inlining).

Methodology note: Slag's jit column is slightly pessimistic — the timed
eval re-parses the snippet and pays a fresh Cranelift compile (~1ms per
tiny body, see `bench_once`) — while Node is at full tier-up after 3
warmup calls, so the true JIT gaps are a bit smaller than shown. These
are micro-benchmarks of the JIT's supported subset, not a workload
comparison.

### L1a — the warm-store fast path (measured 2026-09-03)

The performance plan's L1a (a store-side cell mirroring the read cells)
landed: `put_value`'s member write to an Ordinary object/function with
receiver == base and a string key now probes a generation-validated cell
— `(object id, name, generation, slot)` on `Agent::member_write_cells` —
recording "at this generation, `name` is an own writable data property
at property-vector `slot`". On a hit the write calls
`JsObject::write_data_property_slot` (a direct vector-slot write +
inline-field mirror + generation bump) and refreshes the read-side value
cell, skipping `put_value`'s namespace/receiver probes, the primitive
boxing, `find_ecma_accessor`'s own-property lookup, and
`set_with_receiver_key`'s second descriptor lookup. Sound because an own
writable data property shadows the whole chain (spec 7.3.3 step 3
consults the chain only when the own property is absent) — no setter
tracking needed — and the slice-11 discipline (every own-property
mutation bumps the generation) invalidates on redefinition/delete/
accessor-conversion. The cell is filled after a cold full-[[Set]] write
that leaves an own writable data property. Interpreter-only: the JIT's
compiled fast member-store is a separate path (its `call_slow`
fallbacks inherit the win through `put_value`).

Probe (the plan's next-experiment gate): the `compound assign` row is
`o.x += 1; s += o.x` per iteration — the write term of the row's ~186ns/
iter (2026-09-03 decomposition, ~146ns of it the write). Interleaved
A/B of `--jit-bench` on this worktree vs its parent (ba9a69b), same
machine: compound assign interp drops 17.5ms (parent, 4 runs 17.1-17.9)
-> 6.16ms (4 runs 6.08-6.24) — ~2.8x, ~175ns -> ~62ns per iteration. The
JIT column is unchanged (~1.45-1.5ms); the other `--jit-bench` and
`--bench` rows are unchanged within the machine swing.

Two slices beyond the cell itself close the write path: (1) the store
cell is now probed directly from `assign_member`'s Assign/compound/logical
branches BEFORE `fast_fresh_store` and `put_value` — the hot existing-
property write skips the fresh-store map check, the `member_reference`
build, and the `put_value` call layer (put_value keeps its own probe for
its other callers: updates, destructuring, eval); (2) `member_reference`/
`super_reference` no longer round-trip a `Name` atom through
`crux::lookup` + re-intern — `PropertyKey::String(id)` is the canonical
form, so the clone-and-rehash was pure waste on every member write. The
second read per iteration also hits the value cell the store refreshed
(before, the write's generation bump forced it to re-resolve).

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green; the three release sweeps are identical
to the parent (ba9a69b) sweep on every count — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang on both binaries; the edge probe
(`scratch/l1a_store_probe.js`: accessor conversion, delete+recreate,
chain setters present and added later, non-writable + strict,
function objects, multi-prop thrash, computed keys, super writes,
proxies, own accessors) passes under the JIT, `--jitless`, and
`--gc-stress`.

Next experiment: L1c (shapes with inline-property offsets), the plan's
primary architectural investment. The L1b fuse was skipped ahead of it:
L1b's probe showed the compound row's residual ~20ns of non-write
overhead (of ~62ns/iter) is only partly the read-modify-write split, and
L1c's timing note — the cell model only accretes more property paths the
longer it stays — argues for the shape work first. The first L1c write
slice landed below.

### L1c-1 — pinned inline-field mirror in the store cell (measured 2026-09-03)

The performance plan's first L1c write-path slice: the L1a store cell now
records the property's inline-field mirror — the (map id, in-object
field offset) assigned by the object's map (`JsObject::map_store_field`)
— alongside the property-vector slot. A warm write mirrors the value
into `in_fields` at the pinned offset directly (one map-id compare)
instead of re-scanning the map's descriptors via `map_set` on every
store. Sound under the same generation gate that already pins the slot:
every structural change (define, delete, accessor conversion, map
transition — all of which run through `define_property_key`'s entry bump
or `delete_key`) bumps the object generation, and a map id pins its
descriptor layout (maps are immutable after creation). The pinned write
re-checks the map id as a backstop for a missed bump; a mismatched or
dropped map (dictionary mode) and vector-only properties (non-w/e/c
defines, the >4-property spill) fall back to the `map_set` scan. The
write cell is interpreter-only, so the cell layout is not JIT-visible.

Probe (the plan's next-experiment gate): the mirror-scan share of a warm
write. A/B of the `compound assign` row (a 1-descriptor object, scan
depth 1) between fresh full builds of this worktree and its parent
(20be822), interleaved at high priority on a CPU-saturated machine
(llama-server pegged all 16 cores, so absolute deltas carry extra noise;
identical-source control builds moved ±12% on the untouched `arithmetic`
row from code-layout luck alone): compound assign interp is neutral —
6.10ms (parent, 5 runs 6.08-6.27) vs 6.08ms (5 runs 6.01-6.09). The
depth-sensitive probe (`scratch/l1c_ab.js`, a certified-leaf row writing
the LAST of a 4-descriptor map, 4M iters) moved 255.5ms (parent median)
-> 247.5ms — ~3%, ~2ns/iter at scan depth 4 — with the depth-1 control
moving ~1.6% in the same direction, so the net scan saving is ~1-2ns at
depth 4 and below resolution at depth 1. The row's residual cost is not
the mirror scan.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. four new crux tests:
`map_store_field_*`, `pinned_field_write_*`); the three release sweeps
are identical to the parent on every count — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang; the edge probe (`scratch/l1c_store_probe.js`:
4-descriptor warm writes, sibling/overflow props, accessor conversion
after warm, delete+recreate on a full map, vector-only non-enumerable
defines beside a live map, shared-shape instances, function objects,
super receivers, proxies, >16-key thrash) passes under the JIT,
`--jitless`, and `--gc-stress`; the L1a probe still passes.

The slice's value is the pinned-offset mechanism: the store cell now
carries the map/field pair the deeper L1c write phases need (shape-check
field writes that drop the per-write vector/descriptor machinery).

Decomposition probe (the recorded next step, 2026-09-03): where the
~63ns/iter `compound assign` row actually spends, isolated in the
certified-leaf regime (`scratch/l1c_write_decomp.js`, 2M iters, medians of
5 runs): the loop+var floor is ~10ns/iter (f6, `s = i`); the warm member
WRITE is ~22ns (f5 `o.x = i` minus the floor); a serial dependent `+=`
chain costs ~12-14ns per chain (f0 `s += i` = 22 vs f6 = 10), and the row
carries TWO such chains (`o.x`'s RMW and the `s` accumulator); the warm
member read is ~3ns. The compound step is NOT a tax: `o.x += 1` (f2,
~49ns) equals the plain `o.x = o.x + 1` (f9, ~47ns) — both pay the read +
serial chain on `o.x`. So the row is roughly floor 10 + write 22 + RMW
chain ~14 + s chain ~14 + reads ~4 ≈ 64. Conclusion: the L1b fuse was
right to defer — fusing the compound buys nothing the plain RMW already
pays (the earlier "~20ns of non-write overhead" premise was the chain
latency, which fusion cannot remove).

Attribution probe (what the structural L1c write could actually reclaim,
2026-09-03): a throwaway variant removing the property-vector write
entirely from `write_data_property_slot` (the RefCell borrow, the
backstop re-checks, and the vector slot store — the field mirror,
generation bump, and both cell records kept) drops f5 from ~31.5 to
~26.5ns/iter (write ~22 -> ~17ns) and leaves the row (f3) unchanged at
~62ns. So the vector write is only ~5ns of the ~22ns write; the
field-authoritative rewrite (making the map field the storage authority
for mapped keys, shrinking SmallProps to overflow) removes ~5ns of the
~63ns row (~8%) at the cost of making every descriptor/enumeration/
structural consumer field-aware. That trade is NOT justified: the
remaining ~17ns is the field mirror + generation bump + write-cell
re-record + value-cell front + interpreter dispatch — the generation/
record discipline the JIT's compiled probes share. Cutting it requires
the deep L1c+JIT step (shape-based reads in BOTH engines so the
value-cell records die), and even then the row stays ~45-50ns because of
the two ~14ns dependent chains + 10ns floor. The row is now executor-
bound, not property-bound: reads + vector write are ~8ns of the 63, and
further interpreter property slices (L1c write or read) have low measured
ceiling on it. The measured levers left are the register executor's
dependent-add latency (shared with the `arithmetic` and `property read`
rows) and the L1c-JIT record-discipline redesign.

### Register-run coverage: the loop counter as a binary/store operand (measured 2026-09-03)

Refining the decomposition above: the ~12ns "dependent-chain" cost on
pure-var chains was NOT register-executor latency — it was a step-path
fallback. Certified acc-path loop bodies stayed on the step path whenever
the loop counter (the `PushAcc` operand, the dedicated `loop_counter`
field) appeared as a binary RIGHT operand (`for (var i..) { s += i }`) or
a plain member-store VALUE (`o.x = i`): the register lowering rejected
`RegOperand::Counter` in both positions, so the whole body dispatched per
step. The lowering now admits it: the binary arm loads the counter first
and combines a spilled / late-readable frame-slot left
(`LoadCounter` + `BinAccPop`/`BinLeftReg` — safe for `tdz=false` slots,
where nothing in the straight-line run writes the slot between the load
and the combine), and `StoreMemberName` resolves the counter from the
dedicated field at execution time (after the object load and nullish
check). Both reuse existing leaf ops, so the executor and the JIT need no
new arms — newly-register-run bodies are `RunRegBody`s of ops both
engines already emit.

Measurement (certified-leaf probes `scratch/l1c_write_decomp.js`,
high-priority interleaved runs, new vs the parent fresh build a962bb6):
f0 `s += i` ~21 -> ~13ns/iter; f5 `o.x = i` ~31 -> ~22.5; f4
`o.x = i; s += o.x` ~44.5 -> ~34. Controls flat: f6 `s = i` 9.5, f1
`s += o.x` 16.5 (the earlier 23.5 reading was a code-layout artifact of an
incremental build), f2/f8/f9 (member compound, step path by design)
47-52. The 12 `--jit-bench` rows are unchanged (fresh-build A/B:
arithmetic 11.99 vs 11.97, property read 23.30 vs 22.94, compound assign
6.01 vs 6.04) — the suite has no counter-fed var-accumulator or
member-store row; the slice widens register-run coverage to the common
`for`-loop shapes that feed a var or store the counter onto a member.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. a new eval test asserting both
shapes lower to `RunRegBody`); the edge probes pass under the JIT,
`--jitless`, and `--gc-stress`; the three release sweeps are identical to
the parent — language 23721/23724 (3 skip), built-ins 23657/23812
(155 skip), annexB 1086/1086, all with zero fail/crash/hang.

Next: the compound row's residual cost is the member-compound step path
(f2/f3 unchanged here) and the member-read-fed chains.

### Member-store register fusion: `o.x op= v` and `o.x = <computed>` in runs (measured 2026-09-03)

The member-RMW bodies land on the register executor. The row's `o.x +=
1` body dispatched 9 steps/iter (LoadLocal o; Dup; GetMemberName; Push;
AssignMemberName compound; SetCompletion; ListEnd; head) and a plain
computed-value store (`o.x = o.x + 1`) stayed step-path purely because the
value was live in the accumulator. The lowering now handles all three
blockers: `Step::Dup` (a loadable shadow operand duplicates by re-read);
the compound member assign decomposes into the plain binary on the cached
old value plus a plain store — sound because `apply_compound(op, l, r)`
IS `apply_binary(compound_binary(op), l, r)`, so `o.x op= v` ≡ read +
binary + plain store (a setter/chain receiver runs exactly once, on the
write); and a computed (`Acc`) member-store value with a frame-slot object
stores via the new `StoreMemberNameLocal` leaf op, which reads the object
from its slot at store time so the value never round-trips the
accumulator. The RHS must be a pure operand (Reg/Const/Ctx/PerIter, plus
the loop Counter via a PushAcc+LoadCounter+BinAccPop spill) and the object
a `tdz=false` frame slot (the late-read contract). The logical assigns
(`&&=`/`||=`/`??=`) stay on the step path (they short-circuit). The JIT
emits `StoreMemberNameLocal` with the step path's inline validated store
(`obj_ok` gate + member-value-cell probe + `set_member_slot`), so the
compiled compound column does NOT regress to the slow helper.

Measurement (certified-leaf probes `scratch/l1c_write_decomp.js`, 2M
iters, 5-run medians, high-priority interleaved; the machine was noisy so
the floor drifted 9.5-11): f2 `o.x += 1` ~48.5 -> ~29.5ns/iter; f8
`o.x += i` ~49 -> ~33; f9 `o.x = o.x + 1` ~47 -> ~26.5; the full row
(f3) ~62 -> ~40.5. The `--jit-bench` compound-assign interp row drops
6.0 -> ~3.9ms/100k (~35%, 62 -> ~39ns/iter) with the JIT column flat
(1.44ms vs 1.43) and the other rows unchanged within the machine swing.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. two new eval tests asserting the
counter-fed and member-RMW bodies lower to `RunRegBody`); the edge probe
(`scratch/l1c_compound_probe.js`: every binary compound op, string +=,
own getter/setter accessors, chain accessors with and without an own data
shadow, non-writable, delete+recreate, logical assigns, counter RHS,
expression-position compounds, nullish receivers, undefined +=
NaN, descriptor/enumeration integrity, mid-loop break) passes under the
JIT, `--jitless`, and `--gc-stress`, as do the L1a/L1c store probes; the
three release sweeps are identical to the parent — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang.

The same mechanism then covers statement-position member UPDATES
(`o.x++` / `o.x--`): the lowering decomposes `Step::UpdateMemberName` into
the read + a new `UpdateAcc` leaf op (ToNumeric ±1 — NOT the binary `+`,
which would concatenate a numeric string — with the number case inlined
and `update_value` as the fallback) + `StoreMemberNameLocal`. The update's
expression result (old/new) is discarded at the statement boundary, so
prefix and postfix lower identically; an expression-position update
(`s += o.x++`) leaves its value unconsumed and stays on the step path
(asserted by the new eval test). The JIT emits `UpdateAcc` with the
`emit_update` fast/slow shape (inline f64 ±1, `UpdateValueSlow`
fallback). Measured: `o.x++` ~48 -> ~27ns/iter and `o.x--` ~27.5 in
certified loops; the compound row and all jit-bench rows unchanged.
Gates: clippy clean, workspace tests green, the update probe
(`scratch/l1c_update_probe.js`: numeric/numeric-string/NaN/Infinity
updates, postfix/prefix values, own accessors, chain accessors,
non-writable, delete+recreate, nullish, mid-loop break) passes under the
JIT, `--jitless`, and `--gc-stress`; the three release sweeps are
identical to the parent.

### Computed-member RMW on the register executor (measured 2026-09-03)

The computed forms of the member RMW — `o[k] += v` (statement position),
`o[k]++`/`o[k]--`, and the explicit `o[k] = o[k] + 1` — never reached the
warm member cells: they ran the full Get/Set/Computed machinery per
iteration (each iteration converting the key and re-resolving the
property), measuring ~158ns/iter vs the named `o.x += 1` at ~28ns (the
probe `scratch/computed_probe2.js`). The statement-position compound and
update now lower to ONE fused register op per iteration
(`CompoundMemberComputedLocal`, `UpdateMemberComputedLocal`; the
`Dup2`/`GetMemberComputedKeep` read is deferred into it — sound because
the RHS must be a pure operand, which emits no op between), and the
plain-with-computed-value store (`o[k] = o[k] + 1`) lowers to the
computed read + `StoreMemberComputedLocal`.

The fused ops convert the key ONCE per evaluation and share it between
the internal read and write — mirroring the step path's
`GetMemberComputedKeep`, whose write reuses the converted key (spec
13.15.3). A decomposed read+store with the store re-deriving the key
from its slot would re-run an object key's ToPropertyKey after the
read's getters and was rejected in design (the once-key probe
`scratch/rmw_key_once2.js`, whose `toString` yields a fresh name per
call, asserts the read and the write hit the same property per
iteration under the JIT, `--jitless`, and `--gc-stress`).

Measurement (2M-iteration certified loops, `scratch/computed_probe2.js`,
multi-run, the machine quiet): `o[k] += 1` ~158 -> ~54ns/iter and
`o[k]++` ~160 -> ~51.5, in both engines (the JIT's fused-op slow-helper
call keeps the compiled loop). The register executor's residual is the
once-per-iteration key conversion + the member-cell read and warm-store
write. A computed compound with a side-effectful RHS (`o[k] += o.y`), an
expression-position update, and constant-key forms (`o['k']`, `o[0]`)
stay on the step path — the constant-string form is the compile-time
name-normalization slice, and the numeric-keyed dense-array RMW (a
canonical runtime Number key) still takes the general (string-converted)
path, pending the fused core's numeric fast paths.

Gates: clippy clean; `cargo test --workspace` green (new lowering test
asserting the three shapes reduce to their single fused ops and that the
member-read-RHS and expression-position forms stay step-path); the edge
probe (`scratch/l1c_computed_rmw_probe.js`: every binary compound op
against a name reference, string `+=`, own getter/setter, chain
accessors, shadowed own data, non-writable, delete+recreate, logical
assigns, counter RHS, numeric-string/NaN updates, prefix/postfix,
explicit `o[k] = o[k] + 1`, expression-position values, nullish, the
once-per-evaluation object key, descriptor/order integrity, mid-loop
break) and the L1a/L1c/compound/update probes pass under the JIT,
`--jitless`, and `--gc-stress`; the three release sweeps are identical to
the parent — language 23721/23724 (3 skip), built-ins 23657/23812
(155 skip), annexB 1086/1086, all with zero fail/crash/hang.

### Numeric-keyed array RMW fast paths in the fused core (measured 2026-09-03)

A canonical runtime Number key on a dense Array or typed array
(`arr[i] += 1`, `ta[i]++` — the byte/count-loop shape) still ran the
fused core's general path, converting the number to a string key and
re-resolving the property every iteration (~740ns/iter on a dense
array). The two fused cores now serve a canonical Number key through the
element paths directly — `array_element_get`/`array_element_write` for
dense/spilled arrays (a hole reads `None` and falls to the general path,
so a prototype-chain read still sees the chain) and
`typed_array_element_get`/`typed_array_element_set` for typed arrays
(OOB reads undefined and the setter no-ops, spec 10.4.7.5/10.4.7.6). The
numeric paths run no user code (ToPropertyKey of a Number is pure), so
they need no key conversion and are exact; when the element write doubts
after the compound's coercion mutated the receiver, the ALREADY-computed
value is written through the converted key — never re-read/re-applied.

The cores are push-neutral (`assign_member`'s result push is popped
inside): the numeric paths return without any push, so a JIT helper that
unconditionally popped would steal a caller stack slot (a compiled
`o[k]++` on a Uint8Array underflowed `do_call_fast` at the next call
until the helpers stopped popping).

Measurement (2M-iteration certified loops, `scratch/numeric_rmw_probe.js`):
dense-array `o[0] += 1` ~740 -> ~20ns/iter and `o[0]++` ~17.5ns (JIT,
interp ~28/25.5), typed-array (Float64Array) `ta[0] += 1` ~53.5 and
`ta[0]++` ~51 (interp ~65/59.5). Gates: clippy clean, workspace tests
green, the numeric edge probe (`scratch/numeric_rmw_edge_probe.js`:
dense in-range RMW, a hole reading through a prototype and writing an
own element, append-position NaN semantics, uint8 modulo wrap /
uint8clamped clamp / float64 +=, typed OOB no-op, a coercion-mutated
receiver writing exactly once, numeric-string elements) passes under the
JIT, `--jitless`, and `--gc-stress`; the three release sweeps are
identical to the parent.

Next: the compile-time string-literal-key normalization (a literal
computed key like `o['k']` is observationally a name and should compile
to the named machinery).

### Literal-string computed keys compile as names (measured 2026-09-03)

A computed member key that is a plain string literal (`o['k']`, `o['a-b']`,
`o?.['m']`) is observationally identical to the named form: ToPropertyKey
of a string is the string itself, so both resolve through the same
atom-keyed property machinery. The parser now normalizes the AST at the two
member-access `[index]` sites (`parse_subscripts`, `parse_optional_link`;
`super['k']` stays computed) — `member_property_from_index` interns the
literal and emits `MemberProperty::Name`, so every compiler path (reads,
writes, compounds, updates, calls, `delete`, optional chains) automatically
runs the fast named machinery. Excluded: the `"length"` atom, where the
named read path's typed-array length shortcut (a slot read) differs from the
computed path's prototype accessor invocation — keeping the literal form on
the computed path preserves today's behavior for that one atom. Object
literal `{['k']: v}` properties are a separate AST (`PropertyName`) and are
untouched (`{['__proto__']: x}` stays a plain property).

Measurement (2M-iteration certified loop, `scratch/computed_probe3.js`):
`o['k'] += 1` ~157 -> ~12.5ns/iter (JIT) / ~28.5 (interp) — now identical
to the named `o.x += 1`. Gates: clippy clean, workspace tests green, the
literal-key probe (`scratch/literal_key_probe.js`: literal reads/writes/
compounds/updates, non-identifier and empty-string keys, `delete`, accessors,
optional chains, literal-key calls, `__proto__` writes, certified loops, the
typed-array `['length']` exclusion, `super['k']`) passes under the JIT,
`--jitless`, and `--gc-stress`; the three release sweeps are identical to
the parent.

Next: the remaining literal-COMPUTED key is a NUMBER literal (`o[0] += 1`
on a dense array, ~915ns): the register RMW lowering rejects `Const` keys,
so it stays on the step path — accept `Const` keys in the computed member
read/RMW lowering (a `Const` Number key then reaches the fused core's
numeric element fast paths, and a `Const` string key is moot now that
string literals normalize to names).

### Const-key computed RMW on the register executor (measured 2026-09-03)

The register computed RMW lowering only accepted a frame-slot (`Reg`)
key, so a literal Number key (`o[0] += 1` on a dense array) stayed on the
step path: every iteration converted the number to a string key and
re-resolved the property (~915ns/iter). The fused ops
(`CompoundMemberComputedLocal`/`UpdateMemberComputedLocal`/
`StoreMemberComputedLocal`) now carry the key as a `RegOperand` — a
`tdz=false` frame slot OR a `Const` (`is_stable_computed_key`) — resolved
by the interpreter handler / JIT leaf_operand at op-execution time (a
`Const` is immutable, so the same late-read/deferred-read soundness
holds). A `Const` Number key reaches the fused core's numeric element
fast paths (dense/typed arrays) with no key conversion.

Measurement (2M-iteration certified loops, `scratch/computed_probe3.js`):
dense-array `o[0] += 1` ~915 -> ~20ns/iter (JIT) / ~29.5 (interp) — the
numeric element paths — and the explicit `o[0] = o[0] + 1` and `o[0]++`
literal forms lower too. Gates: clippy clean, workspace tests green (the
lowering test asserts the Const-key fused shape), the const-key probe
(`scratch/const_key_rmw_probe.js`: literal vs runtime-key agreement,
separate literal indices, append-position NaN, plain-object index-string
properties, explicit `o[0] = o[0] + 1`, typed arrays incl. OOB,
oversized non-index keys) and the heap-const-key probe (`o[1n]` — the
BigInt-literal key rides the `load_const` field plumbing, rooted under
`--gc-stress`) pass under the JIT, `--jitless`, and `--gc-stress`; the
three release sweeps are identical to the parent.

Next: the computed-member family is now fast across runtime keys (string
and number), literal string keys (names), and literal number keys
(Const). The remaining measured member-op gap is the NAMED side's own
exotic cases and the L1c shapes work (the plan's structural item);
re-probe the jit-bench rows and pick the next mechanism by measurement.

### PostInc keys on a Number slot resolve on the raw f64 (measured 2026-09-03)

The buildString rows' interpreter column is the computed-store machinery:
`a[l++] = i` runs one `RunRegBody` op whose `PostInc` key resolution went
through the general `update_value` (a full `to_numeric`
ToPrimitive/ToNumber dispatch) on every iteration just to add 1 to a
slot that provably holds a Number. The register executor now handles a
`PostInc` key whose slot holds a Number inline (`n ± 1`, one slot write,
the old value yielded unchanged) and falls through to `update_value` for
everything else. Sound because Number is closed under `++`/`--` (NaN and
the infinities stay put) and `to_numeric` of a Number is the value
itself. Interpreter-only: the JIT resolves `PostInc` in machine code, so
no JIT arm and the compiled columns are untouched.

Measurement (interleaved parent/child A/B, 3 rounds each, 2026-09-03):
`buildString shape` interp ~171-192ms -> ~145-162ms (best-of-3 171.3 ->
145.0, ~15%), JIT column unchanged (~40ms); the isolated `a[l++]=i`
append drops ~41.0 -> ~33.2ns/iter interp (~19%), while the
step-path `a[l]=i; l++` control (a separate `UpdateLocal` step, not the
fused key) is unchanged; `buildString full` (apply/fromCodePoint-bound)
is unchanged. Gates: clippy clean, workspace tests green (new
`post_inc_key_number_fast_path_matches_update_value` regression covering
NaN/±Infinity/-0/fractional/over-2^32 keys and the BigInt fallback),
the computed-RMW probe set passes under the JIT, `--jitless`, and
`--gc-stress`; the three release sweeps are identical to the parent.

Separate pre-existing finding (reproduced at parent HEAD, not introduced
by the interpreter slice): a register computed store whose PostInc key is
NaN or Infinity raised an illegal instruction under the JIT — the
compiled dense-array-append gate ran a NON-saturating `fcvt_to_uint` on
any double key before its range branches could route a non-canonical key
to the slow helper, and the x64 lowering traps on NaN, an infinity, a
negative, or a value >= 2^63. Fixed in the same pass: the append gate
converts with `fcvt_to_uint_sat`, which never traps; the round-trip
integrality + range gates below send every non-canonical value to the
legacy helper exactly as before (a canonical index < 2^32 is never
saturated, so the fast path is unchanged — the buildString rows measure
identically). Covered by the e2e `installed_jit_non_canonical_post_inc_keys_fall_back_cleanly`
(NaN/±Infinity/negative/fractional/over-2^63/BigInt PostInc keys under
the real JIT; the parent-HEAD binary dies on its script).

Next: the remaining interp cost on the append store is the object
machinery itself (chain-clean verdict, buffer push, length mirror,
generation bump) — the plan's structural L1c element-storage item; the
step-path `if (l === N)` guard overhead (~16ns/iter on the shape row) is
the second measured term, reducible only with a completion-elision or
fused-test landing.

### Loop-body list wrappers elided for abrupt-free `for` bodies (measured 2026-09-03)

`compile_for` emitted the body block's per-iteration `ListBegin`/`ListEnd`
statement-list pair — a real `list_stack` push/pop plus the
empty-restore — even when the body could never end an iteration in a
state the loop's own completion machinery must restore. A braced `for`
body whose statements cannot transfer out of it (no
`break`/`continue`/`return`/`throw`, no `yield`/`await` — an inner
nested loop keeps its own wrapper) now compiles the block interior
without the pair (`compile_for_body`). Sound because the loop head's
`ResetCompletion` empties the register before the first iteration and
control statements normalize their own completion, so a body that can
end an iteration empty (only empty/declaration statements) ends every
iteration empty from an empty pre-iteration register — the wrapper's
save/restore never changes the result. The JIT already lowers these
steps as no-ops (Cut 65), so the interpreter converges on the compiled
path; the guard `if`'s own consequent block keeps its pair.

Measurement (interleaved prev/child A/B on `scratch/trunc_decomp.js`,
3 rounds each, 2026-09-03): the three guard shapes drop ~4ns/iter interp
(shape 156->~143ms per 3M, ifappend 151->~138, appendif 157->~144).
`--jit-bench`: `buildString shape` interp ~159->~141ms (~11%) and
`buildString full` ~85->~78ms (~9%), JIT columns flat (~41ms/~25ms) —
per-iteration dispatches removed, not the store machinery. Gates: clippy
clean, workspace tests green (new `for_body_list_wrappers_drop_only_without_abrupt_control`
asserting the drop and its absence for break/continue/return bodies, and
`dropped_for_body_wrapper_keeps_loop_completion` covering the
completion-value edges), the three release sweeps identical to the
parent (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang on both binaries).

Next: the guard region's residual per-iteration cost is the step-path
`if` machinery (its own reset/test/dispatch), which the plan folds into
L1b's fused guard; the structural L1c shapes item remains the primary
investment.

### L1c read path: warm member reads fold inline into the register executor (measured 2026-09-03)

The interpreter's warm member read on the register path measured ~9-10ns
per op (the `property read` row's `o.a + o.b` reads) against a ~2-3ns
plain-register-op floor. The register executor's `GetMemberNameLocal`
arm called the full `member_cell_get` (an out-of-line call through the
big `Vm` helper: `cell_object` re-derivation + the value-cell probe + the
map live-field probe + the slot/vector tail). The warm probes are now
extracted into `member_cell_warm_probe` (`#[inline(always)]` — the
value cell, then the map live-field read) shared by `member_cell_get`
and folded directly into the executor arm: the arm derives the object
part once and probes inline, so a warm read pays no out-of-line call;
only a miss falls back to `get_member_name`. Pure refactor — the probe
order and every fallback are unchanged (`member_cell_get` reuses the
same helper, so the step path and the JIT-mirrored semantics are
identical).

Measurement (interleaved A/B both orders, 3 rounds each, 2026-09-03):
`--jit-bench` `property read` interp ~29.3 -> ~24.1ms (~18%) with the
JIT column flat (~5.0ms); the isolated 2-read probe (`scratch/
l1c_read_probe.js`, 3M iters) drops ~91 -> ~79ms (~1.7ns/read) with the
`var`-loop floor flat; `arithmetic` unchanged within noise (the apparent
first-order gain reversed under order interleaving — code-layout luck).
Gates: clippy clean, workspace tests green, the three release sweeps
identical to the parent (language 23721/3 skip, built-ins 23657/155
skip, annexB 1086/1086, zero fail/crash/hang), and a register-path
member-read differential probe (own data, accessors, chain methods,
typed-array `length`, delete/redefine mid-loop, vector-only props past
the inline fields, proxy receivers, own-shadow over a chain data
property, function own props) is byte-identical under `--jitless` and the
JIT.

Next (L1c read path continued): the remaining ~7ns/read is the
value-cell probe itself under the register-op dispatch; the plan's
shape-compare end state (per-site map validation, L2) is the structural
follow-on, and the write-side record discipline remains gated on the
interpreter-vs-JIT generation-bump split.

The same fold then covers the accumulator-object arm. `GetMemberName`
(a receiver computed into the accumulator — a captured or chained
object) previously called `get_member_name` out of line on every read:
the nullish check, the typed-array `length` atom probe, AND another
out-of-line `member_cell_get` inside it. The arm now derives the object
part and runs the same inline warm probe; non-cell receivers (nullish,
proxies, typed arrays) and misses fall back to `get_member_name`
unchanged. Measurement (tight per-pair alternation, 6 pairs, 3M iters,
2026-09-03): a monomorphic chain (`o.a.b + o.a.c` — two acc reads per
iteration) drops ~118 -> ~103ms (~13%, ~2.5ns/acc read, every pair); a
computed-receiver row (`arr[i % 8].a + arr[i % 8].b`) drops ~289 ->
~278ms (~4%, diluted by the element reads); the `--jit-bench` rows are
unchanged within noise. Gates: clippy clean, workspace tests green, the
three release sweeps identical to the parent (language 23721/3 skip,
built-ins 23657/155 skip, annexB 1086/1086 — a single load-dependent
built-ins reduceRight flake reproduced in isolation 5/5 PASS and the
rerun swept clean), and the extended member-read differential probe
(chain tails, getters mid-chain, computed receivers, own-absent chain
reads, nullish mid-chain, primitive receivers) is byte-identical under
`--jitless` and the JIT.

### Warm named-member stores probe the L1a cell directly in the register executor (measured 2026-09-03)

The write-side mirror of the read fold: the register `StoreMemberName`/
`StoreMemberNameLocal` arms called `assign_member` on every store, which
op-matched to its Assign branch, ran `name_atom`, called `warm_store_put`
(the L1a cell probe + direct slot/field write + value-cell refresh), and
pushed the result onto the value stack (discarded by the `RunRegBody`
truncate). The arms now run the nullish check and probe `warm_store_put`
directly — a warm write skips the `assign_member` call, its op match, and
the per-store value-stack push (the register path discards the result
anyway) — and only a miss falls back to `assign_member` (which re-probes;
cold writes are rare). Pure refactor: the L1a gate (receiver == base on an
Ordinary object/function, own writable data property at the recorded
slot) and every fallback are unchanged.

Measurement (fresh builds of both trees, tight per-pair alternation, 3M
iters, 2026-09-03): a warm named-store row (`o.x = i; s += i`) drops
~99 -> ~87ms (~11%); the compound row (`o.x += 1; s += o.x`) drops ~115
-> ~109ms (~4%); the `var`-loop floor is flat and an isolated arithmetic
row is flat. The `--jit-bench` suite does NOT show the compound win: the
row is only 100k iterations, so a ~1.7ns/iter saving (~0.17ms) is inside
the row's run-to-run noise — clean-context probes are the measurement,
as with the earlier coverage slices. Gates:
clippy clean, workspace tests green, the three release sweeps identical
to the parent (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang), and the member-store differential
probe (warm stores, accessor receivers, chain setters under an own data
shadow, non-writable, delete+recreate mid-loop, accessor conversion
mid-loop, array `length`, vector-only props, function own props) is
byte-identical under `--jitless` and the JIT.

### Counter-keyed computed member access register-runs (measured 2026-09-03)

The typed-array element rows access `ta[k]` with the acc-path loop
counter as the computed key, but the register lowering rejected a
`Counter` key (the `GetMemberComputed` arm lumped it with `Acc`/`Spilled`)
and capped counter reads at one per run — so `s += ta[k]` and the bench's
`ta[k] = k & 255` body (the counter as the key AND in the value
expression: two reads) dispatched per step (~6-7 steps/iteration). Three
lowering relaxations register-run them: a `Counter` key is admitted to
the computed-read arms; `Counter` joins `is_stable_computed_key` (the
fused computed ops may re-read it at store time — sound because the
acc-path head updates the dedicated `loop_counter` field only between
iterations, so it is run-invariant, like a `Const`); and the `PushAcc`
guard allows two counter reads per run (each read resolves the same
field value — no entry push since slice 21). The executor and the JIT
already resolved `Counter` operands (`leaf_operand_value`/`leaf_operand`
`counter_bits`), so no op or machine-code arm changed: the typed-array
write body now lowers to `[LoadCounter, BinImm, StoreMemberComputedLocal
{ key: Counter }]` and a computed read to `[GetMemberComputedLocal
{ key: Counter }, BinLeftReg, StoreReg]` — one dispatch each.

Measurement (interleaved A/B both orders, 4+2 pairs, 2026-09-03):
`--jit-bench` `typed-array write` interp ~38.3 -> ~30.3ms (~21%),
order-independent, JIT column flat (~12.2ms); the isolated row probe
matches (39 -> ~30ms over 800k) and the counter-keyed read row drops
~54 -> ~45ms; `arithmetic`/`property read` moved faster in this build
(code-layout luck — no regression). Gates: clippy clean, workspace tests
green (new `counter_keyed_computed_access_lowers_to_register_runs`
asserting the read and write bodies lower to one `RunRegBody` with a
`Counter` key and covering the uint8 wrap behavior), the three release
sweeps identical to the parent (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang), and the
counter-keyed differential probe (Uint8/Int16/Float64 arrays, plain
arrays and objects, nested receivers, float-step counters, OOB
read/write, over-2^32 keys) is byte-identical under `--jitless` and the
JIT. Computed compounds/updates (`ta[k] += 1`) still lower to the
`Dup2` general path and stay step-path — the fused compound form does
not yet reach them.

### Counter-keyed computed compounds/updates fuse (`ta[k] += 1`, `ta[k]++`) (measured 2026-09-03)

The follow-on to the counter-keyed access landing: computed compounds
and updates on a `Counter` key stayed on the step path because the
`Dup2` that duplicates the `(object, key)` reference pair rejected a
`Counter` operand. A `Counter` is re-readable — the dedicated
loop-counter field is run-invariant, so the read-side and write-side
resolutions see the same value — so the `Dup2` arm admits it, and the
existing fused-op arms (`CompoundMemberComputedLocal`/
`UpdateMemberComputedLocal`, already `Counter`-key-enabled by the
previous landing) now fuse `ta[k] += 1` into ONE op and `ta[k]++` into
ONE op per iteration. This also fixes a step-path pathology: the
step-path compound on a typed array converted the key to a string and
re-resolved the property per iteration (~2150ns/iter), so the register
run is ~30x faster.

Measurement (fresh builds, tight per-pair alternation, 800k iters,
2026-09-03): `ta[k] += 1` on a Float64Array drops ~1720 -> ~54ms and
`ta[k]++` ~1700 -> ~50ms (~30-33x); the `--jit-bench` rows are
unchanged (no suite row exercises counter-keyed compounds). Gates:
clippy clean, workspace tests green (new
`counter_keyed_computed_compound_and_update_lower` asserting the single
fused ops and covering uint8 wrap / Int16 negatives), the three release
sweeps identical to the parent (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang), and the
compound differential probe (typed-array kinds, plain arrays/objects,
getters reading once, sparse-hole NaN, string-concat compounds,
object-key once-conversion) is byte-identical under `--jitless` and the
JIT.

### Typed-array element reads allocate nothing per element (measured 2026-09-03)

The typed-array element read path (`typed_array_element_get` and the
`[[Get]]`/`[[GetOwnProperty]]` helpers) decoded each element through
`SharedBuffer::read`, which returned a fresh `Vec<u8>` per call that
`decode_element` then re-read — the write path had already moved to a
stack buffer (`encode_element_into`), and the read row measured
conspicuously heavier than the write row. Add `SharedBuffer::read_into`
(copy into a caller buffer) and decode from a `[u8; MAX_ELEMENT_SIZE]`
stack buffer, so a hot element read allocates nothing.

Measurement (fresh builds, tight per-pair alternation in both orders,
800k iters over a `Uint8Array`, 2026-09-03): the `s += ta[k]` read row
drops ~35-37ms -> ~16-18ms (~2x, ~41ns -> ~19-21ns per element); the
`ta[k] = k & 255` write row and the empty-loop floor are unchanged
(~12ms / ~2ms). Gates: clippy clean, workspace tests green, and the
three release sweeps identical to the parent (language 23721/3 skip,
built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang).

### L4 probe: the buildString-shape interpreter cost is loop-body step dispatch, not allocation (measured 2026-09-04)

The plan's L4 premise is that the `buildString`-shape rows are
allocation churn (a bump arena would help). Probe at HEAD (3M iters,
interpreter): the suite row `a[l++] = i; if (l === 10000) { c++;
a.length = l = 0; }` runs ~138ms, but REMOVING the array entirely
(the same loop with only `s += i` in the body) still runs ~133ms —
allocation is not the row's cost. Decomposition (interp, 3M iters):
bare `s += i` ~35ms (~12ns/iter — one `RunRegBody` + fast-loop head);
adding a plain `var l++` (a step-path `UpdateLocal`, NOT a register op)
~78ms (~26ns/iter — one extra step dispatch per iteration); adding the
`if (l === 10000)` test ~133ms (~44ns/iter — the per-iteration
conditional dispatches ~3 steps on the step path, because a register
run is straight-line and cannot contain the branch). The JIT column
handles all three shapes at ~2-5ns/iter (the branch compiles).

Conclusion: L4's bump arena is not indicated by this row. The
interpreter gap is step-dispatch coverage of (a) statement-position
local updates (`l++`/`l--` in a certified body have no register form)
and (b) per-iteration conditional tests inside certified loops. Both
are two-engine changes (the JIT must mirror any new `LeafOp`), with
(b) the larger machinery and the one that moves the 138ms suite row.

### Statement-position local updates fuse into register runs (`UpdateReg`) (measured 2026-09-04)

Slice (a) of the probe above: a statement-position local update
(`l++;` / `++l;` / `l--;` — an `UpdateLocal` step immediately followed by
the `SetCompletion` that discards its result) had no register form, so
every statement after the first in a certified body dispatched on the
step path. `lower_step` now recognizes the `UpdateLocal` + `SetCompletion`
adjacency and emits a new `LeafOp::UpdateReg { slot, tdz, op }` — read the
slot, apply the ToNumeric update (inline f64 ±1 for a Number, the general
`update_value` otherwise), store back, push nothing — in both engines (the
JIT `emit_leaf_op` mirror inlines the same f64 add + `UpdateValueSlow`
fallback; the expression-position `x = l++` / `a[l++] = v` shapes keep the
deferred `PostInc` operand). `s += i; l++;` now lowers to ONE `RunRegBody`
of `[LoadCounter, BinLeftReg, StoreReg, UpdateReg]`.

Measurement (fresh builds, tight per-pair alternation in both orders, 3M
iters, interpreter, 2026-09-04): the `s += i; l++;` body drops ~78-82ms
-> ~41-45ms (~1.8x, ~26ns -> ~14ns/iter); the branchy body with a trailing
`l++` drops ~132-134ms -> ~98-103ms (the fused update; the per-iteration
`if` test stays on the step path — that is slice (b)); the bare-loop floor
is flat. The `buildString shape` suite row barely moves (~138 -> ~136ms):
its body has no standalone `l++` (the key increments via `a[l++]`'s
`PostInc`), and the `if (l === 10000)` test dominates. Gates: clippy
clean, workspace tests green (new `statement_position_local_updates_
fuse_into_register_runs` lowering/behavior test and an `installed_jit`
interpreter-vs-compiled parity test), and the three release sweeps match
the PARENT at d58caea — language 23721/3 skip, annexB 1086/1086, and
built-ins crash-for-crash on every targeted group (entries 4/4, subarray
48/48). NOTE: the built-ins sweep at parent d58caea itself deterministically
kills sweep workers in the Array/TypedArray fixture groups (~323-328
"batch process died mid-fixture" crashes; each fixture passes in
isolation); that regression predates this slice (introduced somewhere
between 6dae441 and d58caea, likely the memory-bounding fix) and is
unrelated to it — worth its own investigation.

Next (slice (b)): the per-iteration conditional test inside a certified
loop stays on the step path — the remaining ~98ms floor of the
`buildString shape` row.

### Strict-equality `if`/`while` conditions fuse into one jump (`JumpIfEqImm`/`JumpIfNeqImm`) (measured 2026-09-04)

Slice (b) of the L4 probe: the per-iteration `if (l === 10000)` guard in
a certified loop body compiled to LoadLocal + BinaryImm + JumpIfFalse
(~3 step dispatches per iteration — the `buildString shape` row's
remaining floor). Strict equality against a numeric literal is
coercion-free (only the Number `imm` itself matches — a String, BigInt,
or Object never `===` a Number), so a fused step needs NO general
evaluator: `compile_if`/`compile_while` now recognize `ident ===
<number>` / `ident !== <number>` over a frame-slot binding and emit
`JumpIfEqImm`/`JumpIfNeqImm` (read the slot TDZ-checked, f64 compare,
jump when the encoded test is false — the `JumpIfLtImm` family
convention), one dispatch in both engines.

Measurement (fresh builds, tight per-pair alternation in both orders, 3M
iters, 2026-09-04): the `if (l === 10000)` rows drop ~99-101ms ->
~62-64ms interp (~1.6x) and ~13-14ms -> ~8-9ms under the JIT (the fused
branch shortens the compiled path too); the `buildString shape` suite row
(`a[l++] = i; if (l === 10000) { c++; a.length = l = 0; }`) drops
~135-139ms -> ~102-108ms interp (~1.3x) and ~42ms -> ~35ms JIT;
straight-line rows are flat. Gates: clippy clean, workspace tests green
(new `strict_eq_if_conditions_fuse_into_one_jump` lowering/semantics test
and an `installed_jit` parity test over Numbers, a numeric-string, and
`!==`/`while`), and the three release sweeps at exact parent parity
(language 23721/3 skip, annexB 1086/1086, built-ins identical crash sets
323/323 vs the parent d58caea run — the pre-existing worker-death
regression unchanged).

### Certified loop-body `if`s drop the redundant per-iteration completion reset (measured 2026-09-04)

An `if` inside a certified loop body compiled to ResetCompletion + test/
branch + NormalizeCompletion. The reset exists only so the if's OWN
NormalizeCompletion can turn an empty register into Normal(undefined) —
but in a certified loop body the register runs never touch the
completion register and the loop's own trailing NormalizeCompletion
defines the loop's completion, so the reset was a redundant
per-iteration dispatch on the branchy hot path (the `buildString shape`
row: 5 dispatches/iteration -> 4). `compile_if` now emits it only
outside certified loop bodies (`scope.is_some()` and inside a `Loop`
scope); non-loop `if`s and env-path/step bodies keep it unchanged.

Measurement (interleaved A/B in both orders, 3M iters, interpreter,
2026-09-04): the `buildString shape` suite row drops ~99-104ms ->
~94-97ms (~5%); the `s += i; if (l === 10000) { c++; } l++` row drops
~62-64ms -> ~59-61ms; the JIT column is flat (the compiled bodies drop
the same redundant completion write). Gates: clippy clean, workspace
tests green (new `certified_loop_ifs_skip_the_redundant_completion_reset`
lowering test — a loop-body if leaves only the loop's own leading reset,
a non-loop if keeps its own), a 15-case completion battery (for/while/do/
nested, step-path statements before the if, closure-creation before the
if) byte-identical under the JIT and the interpreter, and the three
release sweeps at baseline (language 23721/3 skip, annexB 1086/1086,
built-ins 23657/155 skip, zero fail/crash/hang).

### Statement-position slot compounds fuse the bin+store tail (`BinStoreReg`) (measured 2026-09-04)

A statement-position local compound whose RHS resolved into the
accumulator — `s += <rhs>` / `s = s <op> <rhs>` — lowered to a
`BinLeftReg` + `StoreReg` pair on the register executor: the binary read
the slot (left) and combined with the accumulator, then the immediately-
following store wrote the result back into the SAME slot (the `n += i * 2`
bench tail). The store directly consumed the binary's accumulator result
(nothing sits between them), so the register lowering now collapses the
pair into ONE `LeafOp::BinStoreReg { op, slot }` — read the slot, combine
with the accumulator (`binary_inline`), write the result back — at the
`StoreLocal`/`FusedStoreLocal` step, matching a same-slot `BinLeftReg`
tail with a `tdz=false` store (`fused_tail` in `lower_step`; the slot
value is copied before the combine, so a coercion side effect that writes
the slot cannot change the left operand — the same late-read discipline as
`BinLeftReg`). A store into a DIFFERENT slot (`x = s + i`) keeps the pair.
Both engines: the JIT `emit_leaf_op` mirror is the `BinLeftReg` emit plus
`store_slot`. This is the register-executor dependent-add-latency lever
(the 2026-09-03 L1c-1 attribution note): each `+=` loses one of its
dispatches.

Measurement (interleaved A/B, fresh release builds, current vs parent
856ba28, 3+ runs each, tight): `--jit-bench` arithmetic interp ~13.2-13.5ms
(parent, matches the recorded baseline) -> ~11.3-11.45ms (this) (~15%),
JIT column flat (~2.4-2.5ms both); compound assign interp ~3.68-3.73 ->
~3.22-3.33ms (~12% — the row's `s += o.x` tail fuses); other rows flat
within noise. Gates: clippy clean, `cargo test --workspace` green (new
`slot_compounds_fuse_the_bin_store_tail_into_one_op` lowering test,
`fused_slot_compounds_match_the_pair_semantics`, and
`installed_jit_fused_slot_compounds_match_the_interpreter` parity over the
number and string-concat paths; `loop_counter_operands_lower_to_register_runs`
updated to the fused op), and the three release sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang).

Next: the RHS-not-in-acc compound shapes (`n += 1` = `LoadReg` +
`BinConst`/`BinImm` + `StoreReg`; `s += t` = `LoadReg` + `BinReg` +
`StoreReg`) still pay three dispatches — the same store-step fuse can
collapse them once the left-load is recognized. The L1c read/write end
state (shape-based reads in both engines) remains the plan's structural
follow-on.

### Direct-operand local compounds do NOT fuse profitably (measured 2026-09-04, REVERTED)

The follow-up to the `BinStoreReg` accumulator-RHS landing: generalize
the store-step fuse to the RHS-as-direct-operand shapes (`s += 1` =
`[LoadReg, BinConst, StoreReg]`, `s += t` = `[LoadReg, BinReg,
StoreReg]`, plus the captured/context forms), collapsing the whole
read-modify-write into ONE fat right-operand op (frame read + operand
match + `binary_inline` + write-back). Interleaved A/B,
round-by-round alternate builds vs the parent (`83b7bea`, the
`BinStoreReg` landing), 3 rounds: `--jit-bench` arithmetic interp
11.46/11.61/11.60 (parent) -> 12.27/14.06/12.87 (this) and bare loop
10.34/10.56/10.80 -> 10.83/11.94/10.95 — a consistent ~1ns/iter
REGRESSION on both rows, including with the accumulator-right path
restored to the parent's exact straight-line code. The executor's
per-op match dispatch is cheap (~1-2ns) and the shared combine work
small, so a single fat arm (extra discriminant branches + a cold
`leaf_operand_value` tail hurting the hot arm's layout) does not beat
the composed minimal ops it replaces — unlike the `BinStoreReg` fuse,
which removed a whole extra `StoreReg` dispatch from an already-
work-heavy `BinLeftReg`. REVERTED to `83b7bea`; the direct-right shapes
stay three ops. Conclusion: the register-run local-compound arc's
remaining shapes are closed by measurement — the per-op dispatch floor
dominates, and the row levers move on to the L1c read/write end state.

### CORRECTED (2026-09-07): the composed-ops route for direct-RHS compounds WINS — the fat-op dismissal does not transfer (landed)

Re-evaluated under the binary-freshness discipline (the 2026-09-04
measurement predates it): the rejected FAT single-op form was not the
only way to fuse the direct-right shapes. Routing the compound through
the PROVEN composed shape — in `lower_step`'s `Step::Binary` arm, a
frame-slot LEFT with a direct right operand (`Reg tdz:false` / `Const` /
`Ctx` / `PerIter`) loads the RHS into the accumulator and combines via
`BinLeftReg`, so the store tail's existing fuse collapses it to
`BinStoreReg` — turns `s += t` / `s += 1` from three ops
(`[LoadReg left, BinX, StoreReg]`) into two (`[load RHS, BinStoreReg]`).
Semantics identical (both reads pure, tdz-free slots, order
unobservable; a battery of 14 shapes incl. self-compound `s += s`,
string compounds, captured RHS, and cross-slot `s = a + b` is
node-identical in jit/jitless/`--gc-stress`). A/B (fresh builds,
interleaved 3 rounds, jitless): slot-RHS `s += t` ~22.9-23.1ms →
~21.8-22.2 (~4%), `s += 1` and `s = s + t` flat-to-positive; jit flat;
full corpus within noise. Gates: clippy clean, workspace green (one
lowering-shape test updated to the fused form + a new
`direct_rhs_local_compounds_fuse_into_bin_store_reg`), the three release
sweeps at baseline.

### L1c record discipline — warm stores stop bumping the generation (measured 2026-09-04)

The write-side half of the L1c program (row 1.1, stones 1-3). The
plan's M9 correction (spec 7.3.3 step 3 consults the chain only when the
OWN property is absent) makes the interpreter's per-value-write generation
bump unnecessary: an own writable data property shadows the whole chain,
so a warm in-place value write needs no setter/chain invalidation. What
the bump WAS protecting was the read-side (id, generation)-stamped VALUE
caches — a warm store that did not bump would leave them serving stale
values. Before dropping the bump, the three such caches converted to the
L1c oracle pattern (cache the RESOLUTION, never the value):

- `construct_this_object` reads a constructor's `prototype` through the
  shared (object id, "prototype") member value cell when warm — every warm
  store fronts it, every structural change bumps past it. The Cut 26
  generation-keyed `construct_prototypes` cell is deleted; a VALUE write
  to `prototype` can no longer leave a stale cached prototype behind. The
  cell is keyed on the JsObject id (the warm-store front's id), not the
  Function-record id — keying on the latter orphaned the oracle from every
  front (the stone-1 stale-construct bug, caught and fixed before this
  landing).
- `member_chain_cells` cache the chain RESOLUTION (the walked links + the
  found property's vector slot), not the value: a chain hit re-reads the
  found property LIVE (through the found link's member value cell when
  warm — the resolve warms it — else the recorded slot), so a warm value
  write to a prototype link's own property is always observed. The cells
  are now all-scalar; the value trace arm is gone.
- The for-of fast verdict additionally oracles %ArrayIteratorPrototype%'s
  own `next` through its member value cell (the resolve warms the cell
  with the stock value; a warm store to AIP.next fronts it, so the probe's
  value compare catches a no-bump replacement). %Array.prototype%'s own
  @@iterator is a SYMBOL-keyed property, so replacing it never takes the
  warm-store cell and still bumps through the full-[[Set]] path.

`write_data_property_slot` then dropped the bump — the interpreter now
matches the JIT's compiled-store discipline (in-place value write, front
the read value cell, generation unchanged) — and `warm_store_put` keeps
its store cell valid at the unchanged generation (no structural change
means the recorded slot/map/field still hold; only the read cell is
refreshed). Every other value write still bumps through its own path
(`set_key`, defines, deletes, accessor conversion), and the
constructor-boilerplate cache re-validates the CURRENT prototype object
id, so a warm `C.prototype = p2` swaps the construct's proto without a
stale-map hit.

Measurement (2026-09-04): the change removes the bump's Cell RMW in
`write_data_property_slot` + `warm_store_put`'s write-cell re-record from
each warm interpreter member write. Row-level A/B (interleaved, 5 rounds,
full `--jit-bench` of a bump-restored intermediate vs this tree) is
INCONCLUSIVE: compound assign interp 3.54-3.84ms (bump) vs 3.11-3.21ms
(no-bump, ~11-17%), but the pure-register `arithmetic` row (no property
writes at all) moved 15.2ms -> 11.5ms (~24%) across the same two
binaries, and two byte-identical rebuilds of each variant agree to <1% —
so the gap is dominated by cross-build code layout, not the write path. A
within-binary isolation (certified loops f5 `o.x = i; s = i` minus f6
`s = i`, 2M iters, medians of 5) puts the warm write at ~12.2ns/iter
(bump) vs ~11.5ns (no-bump) — ~0.5-1ns/write, the mechanism's size, but
still inside the layout band of the f6 floor (which itself moved ~22%
between the binaries). Recorded as measured: the no-bump discipline is
primarily the L1c RECORD change — it retires the last generation-stamped
value caches so the L1c read/write end-state can drop the (id, generation,
name) probes entirely — not a row lever by itself, and the row A/B is not
a clean number on this machine.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (new `construct_observes_warm_prototype_
value_writes` and `chain_reads_observe_warm_value_writes_to_the_found_
link` eval tests); the three release sweeps at baseline — language
23721/23724 (3 skip), built-ins 23657/23812 (155 skip), annexB
1086/1086, all with zero fail/crash/hang. The single language failure
seen on an earlier intermediate build of this change
(`expressions/new/spread-sngl-iter.js` Strict: arguments[1] read 5 vs 2,
batch-state only, passes isolated) does not reproduce on the landing
tree: full language sweep clean, and the reconstructed 32-fixture batch
clean over 10 runs + 4 `--gc-stress` runs — consistent with the
documented heap-state flake class (the `TypedArray/prototype/reduce`
`callbackfn-arguments` flake has the same strict-unmapped-arguments +
stale-value signature), not a regression of this change.

### Read-side direct-mapped thrash FALSIFIED; write cells get a separate 256-entry table (measured 2026-09-04)

Two probes on the member-cell machinery, one falsification and one
landing:

**Read probe — the per-site-IC premise does not reproduce.** The
recorded next step (per-site map-validated member reads on the register
executor, premised on "the read residual is in-suite direct-mapped
thrash") was measured directly with certified register-run bodies: a
warm member read is ~3.5ns/op (the `o.x` row over the var floor), and
reading 64 DISTINCT same-map objects in one straight-line body (`s = s
+ o0.x + … + o63.x`, 500k iters — 64 (id, name) pairs through the 16
value cells, ~4-way aliasing) costs ~4.2ns/read — the map-cell layer
(member_map_cells, one entry for the shared map) absorbs every
value-cell miss at +0.7ns. A 16->256 `MEMBER_CELLS` experiment did NOT
move the cycling rows and REGRESSED every warm row ~25% (the inline
`for_of_fast_cells`/`member_map_cells`/… arrays bloated the Agent hot
struct — the documented inline-table-bloat trap). Interpreter member
reads are near their floor; per-site read ICs are not the next slice.
(Step-path loops over computed indexes — `objs[i & 63].x` — cost more,
but that is register-run coverage of computed keys, a separate matter
from the member-cell machinery.)

**Write probe + landing — the store cells were the thrash victim.** A
store-cell miss falls back to the FULL [[Set]] (~140ns, the pre-L1a
write cost), not a cheap second level. A 64-distinct-object cycling-
store register loop (32M stores) aliases the 16 write cells ~4-way, so
every store misses and pays the full [[Set]]: interp ~4.4-4.5s vs a
single-object warm-store control at ~23ms/1M (~20ns/store) — ~7x. The
L1a store cells moved to a SEPARATE 256-entry direct-mapped table
(`MEMBER_WRITE_CELLS`, interpreter-only — the JIT never reads the
write cells; a Boxed table, so no Agent hot-struct bloat), with the
store probe/record indexing by its own mask while the value-cell front
keeps the READ table's mask (the compiled probe and every read path
mask by `MEMBER_CELLS - 1`; a shared index once wrote the 16-wide read
table at a >16 write index and panicked out of bounds — now a
regression test). Interleaved A/B (parent `0d70d3e` + probe rows vs
this tree, 3 rounds): the cycling-store row drops ~4.4-4.5s -> ~0.42s
(~10x, ~140ns -> ~13ns/store); the warm rows moved within the
cross-build layout band (the no-property-write `arithmetic` control
moved ~20-33% between the two binaries — recorded as noise, not a
regression). Read cells stay at 16 (the read probe). Mirrors the
`GLOBAL_CELLS` 32->256 bump (Cut 35 slice 5).

Gates: clippy clean; `cargo test --workspace` green (new
`warm_stores_across_many_distinct_objects_keep_separate_cells`); the
three release sweeps at baseline — language 23721/23724 (3 skip),
built-ins 23657/23812 (155 skip), annexB 1086/1086, zero fail/crash/
hang.

### Nested non-arrow functions' own this/arguments no longer bail the enclosing body (measured 2026-09-04)

The scope gate (analyze_scope) rejected ANY `this`/`arguments`/`super`/…
inside a nested closure, including inside a nested NON-ARROW function
whose `this`/`arguments` are its OWN (bound at its own call) — so a body
containing a nested constructor or helper that read its own `this` never
certified and ran the env path (every `var` access paid the env walk).
The closure walker (`closure_allows`/`closure_stmt_allows`/`closure_expr_
allows`/`closure_arrow_allows`) now threads an `own` flag: entering a
nested non-arrow function (declaration/expression/method/getter/setter)
sets it, under which `this` and `arguments` resolve to the nested
function itself; an arrow propagates the caller's flag (an arrow created
directly in the analyzed body still observes the body's lexical
`this`/`arguments` and keeps bailing). `super`/`class`/private/tagged/
`import` constructs stay rejected under `own` — their bodies would need
machinery this walk does not model. Arrows' lexical-this tests, the
sweeps, and the workspace suite are the backstop for a wrong
certification.

Measurement (2026-09-04): probe A/B in the `--jit-bench` list — the
construct-churn loop function-wrapped with a NESTED `function C(x) {
this.x = x; }` vs the same loop with C a global (control, certifies):
interp ~117-129ms -> ~18.5-21ms (nested now matches the ~19-21ms
control, ~6x), JIT columns flat (~1.0 both — `new C()` is a
`Construct` step the JIT does not lower). New eval test
`nested_function_own_this_and_arguments_keep_the_body_certified` asserts
the construct body certifies (`scope.is_some`), the nested `arguments`
are the nested call's, a method call binds the nested `this` to the
receiver, and an arrow's lexical `this` still flows (env path).

Gates: clippy clean; `cargo test --workspace` green; the three release
sweeps at baseline — language 23721/23724 (3 skip), built-ins
23657/23812 (155 skip), annexB 1086/1086, zero fail/crash/hang.

### L3 scope-gate probe: the general path is narrower than the plan assumed (measured 2026-09-04)

The plan's L3 premise — "bodies with env machinery (try/catch, with,
eval, closures that capture with `this`) never reach the JIT" — was
probed directly with `--jit-bench`/`--bench` rows:

- **try/catch certifies and reaches the JIT.** A per-iteration `try {
s += o.x } catch (e) { s += 1 }` loop runs interp ~125ms / jit ~72ms
for 1M (ratio 0.57 — compiled), and a try AROUND the whole loop equals
the certified control (~17-19ms interp, jit ~3.7ms). The ~68ns/iter
per-iteration-try residual is the handler-table frame cost — real
machinery, not dispatch — and it is already covered by the compiled
path. The plan's motivating example does not hold in this engine.
- **The residual uncertified hot shape is a function containing an
  arrow that captures `this`** (callbacks in methods — forEach/map
  bodies referencing `this`). Such a method is scope=None (the arrow's
  `this` is the method's lexical `this`, which the certified model has
  no env to hand it), so the whole method — including its hot loops —
  runs the env path. Isolated (arrow created ONCE, hot loop calls it
  per iteration, 1M iters, `--bench`): a method whose arrow captures a
  LOCAL (certified) measures ~35ms; the identical method whose arrow
  captures `this` (uncertified) measures ~1.4s — **~40x**. A per-call
  this-arrow created-and-invoked-once is ~75x (16.5s vs 221ms for 3M)
  but that shape is dominated by per-call closure instantiation
  (~1.65µs/call even certified — the var-arrow control) — an
  instantiation-cost matter, not coverage.
- **Redirected next candidate**: rather than L3's general-path compile
  (which would target these scope=None bodies but is a large
  architectural effort), the cheaper fix for the dominant shape is
  certifying `this`-capturing arrows: the enclosing certified non-arrow
  body captures its `this` value into the arrow's context at creation
  (a synthetic context entry sourced from the this slot), and the arrow
  body reads it as a depth-0 context slot. Measured ceiling ~40x on the
  callback-in-method shape; both engines' arrow creation must mirror.

### This-capturing arrows certify (row 2.3, measured 2026-09-04)

The probe above landed. An arrow created in a certified non-arrow body
that references `this` no longer bails the body: the closure walker
records a reserved capture-context marker (`\u{1}captured-this`, a name
no source identifier can equal), and `analyze_scope` then:

- a NON-ARROW body captures its own `this`: it allocates a context slot
  for the marker (forced `this_slot`), and `compile_body` emits an entry
  store copying the this slot into the context (after
  `OrdinaryCallBindThis`);
- an ARROW body certifies only when its own `outer_chain` carries the
  marker (an enclosing certified body captured it): its direct `this`
  reads compile to `LoadContextSlot` resolved through the outer chain
  (the `ExprKind::This` compile arm), and deeper this-arrows resolve the
  same way — no per-arrow re-capture is needed because the marker flows
  through `outer_chain` and the runtime context chain nests correctly.
  A this-arrow under a body with no `this` to give (a standalone arrow, a
  class constructor) stays on the env path.

Env-path arrows (rest params etc. fail certification) created inside a
certified this-capturing body resolve their lexical `this` through the
capture context: `DeclarativeEnv::has_captured_this`/
`captured_this_value` make an env holding the marker serve as the
this-environment (`EnvRecord::has_this_binding`/`get_this_binding`) —
without it, such an arrow walked past the Declarative context to the
global object (the `built-ins/Object/keys/proxy-keys.js` regression:
the proxy trap getters' rest arrows saw the global object instead of the
handler; fixed by the env-side change).

Measurement (2026-09-04, `--bench` MC rows): a method whose arrow
captures `this` (arrow created once, hot loop calls it per iteration,
1M iters) dropped ~1.4s -> ~41-43ms (~33x) — now close to the
certified var-arrow control (~35-36ms; the residual is the entry context
store). New eval test `this_capturing_arrows_certify` covers the direct
escaping arrow, arrow-in-arrow, a nested function's own-this arrow,
sloppy global-object coercion, standalone arrows, and the env-path
rest-arrow case.

Gates: clippy clean; `cargo test --workspace` green; the three release
sweeps at baseline (with the JIT installed) — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, zero
fail/crash/hang.

### L4 counting probe: the two hot rows allocate 1 box/iter (construct) and ~390 boxes total (buildString full) (measured 2026-09-04)

Row 3.1's re-scope mandates counting arena boxes per iteration on the
`construct churn` and `buildString full` rows before any arena work. Probe
method: a temporary cumulative counter + rounded-size histogram bumped by
`Gc::new`/`Gc::new_in_place` (TLS writes), drained before and read after
the timed `--bench` eval of each row; counts are deterministic and stable
across repeated runs. Box sizes: a JsObject is the 448B class, an
ArraySlots/string/rope/concat box the 64B class; the 112B singleton per row
is the per-eval script record.

- **`construct churn` (100k `new C(i)` iterations): 100,007 boxes —
  100,002 x 448B (the fresh `C` instances, one per iteration) plus the
  per-eval setup constants (C's function object, ~5 misc). Exactly 1 arena
  box per iteration, and it is the instance object itself; the construct
  path allocates no context/env/key/per-iteration extras.**
- **`buildString full` (one full-Unicode-range build, ~1.1M code points
  pushed): 390 boxes TOTAL** — 329 x 64B string/rope boxes (one per
  `String.fromCodePoint` chunk/final result + one concat node per `+=`
  append, ~137 of each across the CHUNK=10000 spills and per-range
  finals) and 59 x 448B array objects (the `lone`/`ranges`/`codePoints`
  arrays). The ~1.1M dense `codePoints[length++] = codePoint` element
  writes allocate ZERO arena boxes.
- Controls that attribute the buckets: `{}`-literal loop = 1 x 448B/iter;
  `[]`-literal loop = 1 x 448B (JsObject) + 1 x 64B (ArraySlots) per iter;
  `s += 'x'` loop = 1 x 64B concat node per append; an intrinsic builtin
  call (`Math.abs`, `String.fromCodePoint`) allocates NOTHING beyond its
  result on the call path itself.

A side finding (the probe's isolated `s.length` reads): **reading a
property off a primitive string allocates a fresh String-exotic wrapper
per read** — `String.fromCodePoint(…).length` in a loop costs 1 x 448B
(wrapper JsObject) + 1 x 64B (the [[StringData]] copy) per access, and a
hoisted-constant `'abc'.length` loop costs the same 448+64 per read. The
member machinery has no primitive-string fast path, so `.length`/index
reads on strings (an extremely common loop idiom) box every time. That is
a real allocation lever, but it is a missing primitive-receiver read fast
path (string-exotic reads served off the raw string, in the L1c read
machinery), not an arena question.

Conclusion: L4's premise is falsified on both rows it was built on. The
bump arena the plan proposes ALREADY exists (A5.1: every counted box is an
arena slot from a bump + size-classed free-list; GC-5 measured the
free-list half net-neutral and `Gc::new` registration near its floor at
~11ns/alloc). There is no measured second hot shape to give a dedicated
arena: `construct churn` allocates only the instance the program is
constructing (an arena cannot remove that box), and `buildString full`
allocates ~390 boxes across the whole row — an arena has nothing to save.
The rows' residual interpreter cost is the certified-construct path and
the branchy step dispatch (per the earlier 2026-09-04 probes), not
allocation. Row 3.1 closes with this record; no arena code lands.
The allocation-adjacent lead this probe surfaced is the primitive-string
property-read boxing above (probed and landed as row 1.4 below), not
the arena.

### Primitive-string member reads serve length/units without boxing (row 1.4, measured + landed 2026-09-04)

The L4 probe's side finding, taken to its end: **reading a property off a
primitive string boxed a fresh String-exotic wrapper on EVERY read path.**
The certified-body probe (isolated rows in `--jit-bench` shape, 200k
`s.length` reads on a string param / local / hoisted const, boxes counted
via the `Gc::new` TLS counters): top-level eval, certified interpreter,
and the JIT ALL allocated 2 x 200k boxes (448B wrapper + 64B [[StringData]]
copy per read) — the register-run warm member cells and the compiled path
have no primitive-receiver fast path, so a string `.length` (or index)
read falls through to the generic Get, which ToObject-boxes the primitive.

Fix (two shared-helper shortcuts mirroring the existing typed-array
`length`/element fast paths in `Vm::get_member_name` /
`get_member_computed`, so the step path, the register ops, and the JIT
slow-helper ABI all inherit them):
- `s.length` on a string primitive returns the code-unit count directly
  (spec 10.4.3.4 StringGet step 1 — an own virtual property of the boxed
  receiver, so no prototype consult);
- an in-range canonical numeric index read on a string primitive returns
  the single code unit as a 1-unit string (spec 10.4.3.5
  StringGetOwnProperty — an own data property, so it shadows the whole
  chain; the astral first unit is a lone surrogate). Out-of-range and
  non-index keys fall through to the exact machinery (a patched
  `%String.prototype%` numeric key is still found).

Verification of the box counts (200k reads): 400,001 boxes (top-level) /
400,065 (fn) -> 0-1 boxes on every path. Clean interleaved A/B on the
certified 200k-`.length` row (min-of-3 per-call, `bench_once` harness,
probe timings only, no counters in): interp ~106-119ms -> ~4.2-4.8ms
(~23x, ~550ns -> ~22ns/read); jit ~108-308ms (noisy median ~111ms) ->
~2.4-2.8ms (~40x, ~12ns/read). Suite rows unaffected (construct churn and
buildString full read no string primitives in their inner loops). New eval
test `string_primitive_member_reads_serve_length_and_units_without_boxing`
pins the semantics: code-UNIT `.length` (an astral char is 2), in-range
index = single unit (lone-surrogate first unit), out-of-range and
non-index keys fall through (a patched `%String.prototype%[5]` is found),
and an in-range index shadows a prototype patch at the same key. Gates:
clippy clean, `cargo test --workspace` green (4648/0), three release
sweeps at baseline — language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang.

### Primitive-string METHOD reads resolve on the prototype chain without boxing (row 1.5, measured + landed 2026-09-04)

The 1.4 fix left the METHOD-read shape boxing: `s.charAt(0)` reads the
`charAt` METHOD off the string, and that member read (a chain data
property) still paid the per-read wrapper. Probe (200k calls, `Gc::new`
TLS counters): `charCodeAt`/`indexOf` allocated 448B wrapper + 64B
[[StringData]] per CALL, and `charAt` +1 x 64B result — identically on
top-level eval, certified interp, and JIT. The compiled call path has no
primitive-receiver fast path for chain keys.

Fix: `Vm::get_string_primitive` — the shared member helpers' string
fallback now resolves the key against the realm's cached
`%String.prototype%` (`Intrinsics::string_prototype`, a `string_prototype`
cache mirroring `object_prototype`) with the PRIMITIVE as the [[Get]]
receiver. Exact because the boxed wrapper's only own properties are the
virtual `length`/in-range indices (the 1.4 shortcuts, re-checked inside
the helper for the named-path `s["3"]` and computed-key shapes) and this
engine threads Receiver=primitive through OrdinaryGet (spec 10.4.3.4) —
so a read starting at `%String.prototype%` reproduces the boxed read for
data properties, accessors (a strict getter sees `this` = the primitive;
only sloppy this-coercion boxes it), proxy links (the get trap's
receiver is the primitive), and symbol keys. Box counts on 200k calls:
charCodeAt/indexOf 400k -> 0; charAt 600k -> 200k (the inherent
result-string boxes); s[i] unchanged (200k result boxes only).

Clean interleaved A/B (probe timings only, no counters): charCodeAt interp
~413-430ms -> ~246-250ms (~1.7x), charAt ~348-371ms -> ~207-216ms
(~1.7x), indexOf ~550-578ms -> ~394-409ms (~1.4x) per 200k; jit moves
proportionally (~1.65x/1.7x/1.35x); the s[i] row (~24ms) is flat. The
residual ~1.2µs/call is the intrinsic CALL dispatch (builtin frame
setup), not the read — that cost belongs to the 4.1/L5 call lever. A
17-line semantic battery (sloppy vs strict getters, patched method
`this`, proxy-in-chain receiver type, numeric-OOB patches, code-unit
length/index, `new String` wrapper reads) is byte-identical vs the boxed
baseline. New eval test
`string_primitive_method_reads_resolve_on_the_prototype_chain`. Gates:
clippy clean, `cargo test --workspace` green (4649/0), three release
sweeps at baseline — language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang. Number/Boolean/Symbol primitives
still box on method reads (n.toFixed etc.) — same pattern, unprobed.

### Non-string primitives resolve method reads on their prototype chain without boxing (row 1.6, measured + landed 2026-09-04)

The 1.5 fix was string-only; the generic `[[Get]]` still boxed a fresh
wrapper per READ for the other primitives. Probe (200k, `Gc::new` TLS
counters, both engines): every Number/Boolean/Symbol/BigInt method
read/call allocated a 448B wrapper per read (Number/Boolean wrapper
creation also inserts an agent boxed-value-table entry) — `n.toFixed` /
`n.toString` / `b.toString` / `sy.toString` reads 200k x 448B,
`sym.description` + 200k result strings, call rows + their inherent
result boxes, `123n.toString()` + a 48B box. The number method reads were
~530-595ns/call interp (clean, no counters) — the heaviest measured
per-operation cost in the engine after the L1a store.

Fix: `Vm::get_string_primitive` generalized to `Vm::get_primitive_member`
(a String arm with the 1.4 length/index virtuals, a Number/Boolean/
BigInt/Symbol arm, else the generic path) and BOTH shared member helpers
route every primitive receiver through it — the `ValueKind::String`
read-gates became "not Object/Function". New `Intrinsics::primitive_prototypes`
(the four non-string kind prototypes, an array indexed like
`function_prototypes`, traced) sits next to the existing
`string_prototype`. Exactness is the 1.5 argument, simpler: these
wrappers are ordinary objects with NO own properties, so a chain read
starting at the kind's %X.prototype% with the primitive receiver
reproduces every boxed read — data properties, the `Symbol.prototype.
description` accessor (a strict accessor sees `this` = the primitive),
proxy links, and missing keys.

Box counts on 200k rows: reads 200k -> 0; `sym.description` 400k -> 200k
and call rows retain only their inherent result boxes. Clean interleaved
A/B on Number method-read rows (200k, no counters in): interp ~106-119ms
-> ~13.3-14.1ms (~8.4x, ~550 -> ~68ns/call), jit ~97-104ms ->
~7.9-9.7ms (~11-13x). The residual ~67ns/call interp is the chain read
itself (the %Number.prototype% own-scan) — the chain-read primitive the
4.1 probe measured, deferred to L2. An 18-line semantic battery
(toFixed/toString/toPrecision, Boolean methods, Symbol description /
`Symbol().description === undefined` / toString, BigInt toString,
boxed `new Number(5)`/`Object(5)` reads, a data-prop patch read live,
strict vs sloppy getter `this`, a proxy link's receiver type) is
byte-identical vs the boxed baseline. New eval test
`non_string_primitive_method_reads_resolve_on_the_prototype_chain`.
Gates: clippy clean, `cargo test --workspace` green (4650/0), three
release sweeps at baseline — language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang.

### 4.1 probe: the apply/.call residual is diffuse — no narrow slice (measured 2026-09-04)

Fresh A/B decomposition of the `.apply`/`.call` residual on the `apply
leaf call` shape (200k, per-call, probe rows in `--jit-bench` form, both
engines, min-of-3-style steady-state counts):

| row | interp ns/call | jit ns/call |
|---|---|---|
| apply leaf 9 (`f.apply(null, arr9)`) | ~98-105 | ~36-44 |
| apply leaf 1 | ~97-107 | ~46-48 |
| .call 9-arg (`f.call(null, 1..9)`) | ~100-107 | ~26-32 |
| direct 9-arg call `f(1..9)` (the floor) | ~58-62 | ~6.2 |
| own-data read row (`o.x`) | ~16-17 | ~2.8 |
| chain method read rows (`f.apply === ap` / `.call` / `o.m` / `a.push`) | ~54-56 | ~27-35 |

Readings: the apply/.call overhead over a same-leaf direct call is interp
~40-44ns / jit ~20-37ns per call, and it is allocation-free (boxes 0 for
the whole 200k row). The arg-array fill is NOT the term in the interp
(.call ≈ .apply), and jit apply-9 ≈ apply-1 (the array length does not
scale the cost). The pieces that remain — the per-iteration prototype-
chain member read of the method, the intrinsic identity compare, and the
CallApply dispatch — each measure in the ~10-30ns band with no clean
dominant term: prototype-chain member reads cost ~4x own-data reads in
interp (~55 vs ~16ns) and ~10x in jit (~28 vs ~2.8ns) across function,
object, and array receivers, but a narrow `.apply`-only inline has no
clean target (the chain read is shared with every `o.m()` shape, and the
2026-09-01 inline-validation experiment measured slower).

Conclusion: 4.1's premise — a distinct `.apply`/`.call` member-read
residual worth inlining — does not re-derive. The row's remaining cost is
the general chain method read plus the intrinsic dispatch, so the read
side defers to L2 (per-site shape/offset ICs once the L1c representation
lands) and the dispatch side to L5. Row 4.1 closes with this record;
no code lands. (No gates run — a probe-only turn; the tree is untouched.)

### Agent-dependent builtin handlers register O(1) — String module (row 4.2, measured + landed 2026-09-04)

The L5 intrinsic-call dispatch floor, localized. Probe (200k calls,
certified rows, both engines): `s.charCodeAt(i)` ~1.18µs/call interp
(~1.14µs jit) and `a.push(i)` ~1.7µs/call, vs `Math.abs` ~150ns (a plain
native closure) and a same-work JS leaf ~90ns. Mechanism: the methods
that need the agent (ToString on object receivers, @@match/@@split/
@@replace/@@search delegation) are placeholder-closure builtins that must
dispatch by intrinsic identity; `call_inner` memoizes only the MODULE in
`agent.builtin_dispatch_cache`, so every warm call re-runs the module's
LINEAR `dispatch_call` chain — each `intrinsics.get` arm allocates a
JsString and hash-looks-up the entries table (charCodeAt is arm ~5, so
~5 allocs+lookups per call). Only `array::handler_for` and
`regexp::handler_for` registered O(1) per-function-id handlers
(`BUILTIN_HANDLERS`) today.

Fix: `string::handler_for` — the ~39 non-HTML `dispatch_call` arms mapped
to their `(agent, this, args)` handlers (String ctor via an adapter) —
consulted by `Intrinsics::define`, which registers each String method by
function id at install time, so a warm call is a TLS HashMap get + direct
handler call in both engines. HTML wrappers and anything unmapped keep
the existing chain. charCodeAt per 200k (clean, both engines): interp
~1.18µs -> ~380ns/call (~3.1x), jit ~1.14µs -> ~350ns (~3.3x); the
residual is the primitive-string chain READ of the method (~250ns — the
L2 read lever) plus the native call itself. Math.abs and the leaf rows
are flat. Behavior is identical by construction (registration is the
chain's own identity match, hoisted); new eval test
`string_agent_builtins_dispatch_identically_via_registered_handlers`
exercises the registered arms (identity, @@-delegation, object receivers,
boxed receivers, the String iterator + next, a live prototype patch).
Gates: clippy clean, `cargo test --workspace` green (4651/0), three
release sweeps at baseline — language 23721/3 skip, built-ins 23657/155
skip, annexB 1086/1086, zero fail/crash/hang. The other agent-dependent
modules (Number/Boolean/BigInt/Object/...) share the same chain pattern;
extending them is the same mechanical `handler_for` map.

**Extended to Number + Boolean + BigInt** (2026-09-04): the same
`handler_for` maps — Number's 7 arms (NUMBER ctor via adapter,
toString/toFixed/toExponential/toPrecision/valueOf/toLocaleString),
Boolean's 3 (ctor, toString, valueOf), BigInt's 6 (ctor/asIntN/asUintN
adapters, toString/toLocaleString/valueOf; the `&Agent`-taking toString
wraps in a closure). Clean interleaved A/B per call on 200k rows: `n.
toFixed(1)` interp ~1170ns -> ~680ns (~1.7x), `b.toString()` ~615-653 ->
~300-307 (~2.0-2.3x), `123n.toString()` ~926-963 -> ~346-361
(~2.6-2.8x); jit proportional; the charCodeAt control is flat (no
regression). New eval test
`number_boolean_bigint_builtins_dispatch_via_registered_handlers`
(wrapper receivers, radix/fraction handling, static/ctor call forms, the
fraction-range error path). Workspace tests 4652/0; three release sweeps
at baseline — language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang. Remaining unregistered agent-dependent
modules (Object/Date/Keyed/...) stay on their chains pending a corpus
probe.

### The >256-object store ceiling and the write-cell capacity bump (row 1.7, measured + landed 2026-09-04)

The write-side >256 follow-on probe (recommended order (a)): cycling
member stores over distinct-object working sets (1M stores, certified
rows, both engines). The 256-entry direct-mapped write cells hold up to
~256 objects; at 1024+ objects every store thrashes out of the table and
falls to the full [[Set]]: interp ~55ns/store (1-256 objects) -> ~180ns
(1024/8192), jit ~40 -> ~165ns. READ rows do NOT cliff (59-61ns at both
64 and 1024 objects — the read map/proto-cell layers absorb misses), so
the cliff is write-cell capacity specifically. Such loops are realistic
(per-frame entity/record updates over thousands of objects), so the
ceiling matters.

Fix: `MEMBER_WRITE_CELLS` 256 -> 4096. The write table is a BOXED Agent
field (unlike the inline read tables the 1.1 probe found bloated the
Agent struct), so growing it costs only per-Agent heap (~128KB at 4096)
and no warm-row struct footprint. `Agent::new` must build it heap-
direct: `Box::new(std::array::from_fn(|_| None))` materializes the array
ON THE STACK first (~128KB in debug at 4096) and overflowed the 1MB-
stack embed doctest — the init now sizes a `Vec` on the heap and converts
to the boxed array. Measured: the 1024-object store row drops interp
~180 -> ~55ns/store (~3.3x) and jit ~165 -> ~40ns; working sets <=4096
fit; warm rows (1-obj/64/256 stores), the suite rows, and the charCodeAt
control all move within the cross-build layout band (property read —
which never touches the write table — moved a similar ~±18%, confirming
noise). Working sets beyond 4096 objects still fall back to the full
[[Set]]; that residual is the L2 per-site store-IC slice, deferred
behind the L1c shape representation. Gates: clippy clean, `cargo test
--workspace` green (4652/0, including the embed doctest that caught the
stack temporary), three release sweeps at baseline — language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang.

### Warm prototype-chain read marginal: fixed validation cost, not the walk (measured 2026-09-04)

The chain-read candidate (recommended order (a)), measured cleanly: the
earlier "~55ns chain read" rows were confounded by `===`-compare +
branch overhead (a bare function-equality row alone was ~31ns). New rows
use NUMERIC prototype values (`o.m` where `m: 2` on the prototype — no
compare, no branch) over 2M reads with a bare-add control subtracted,
both engines:

| read | interp marginal | jit marginal |
|---|---|---|
| own-data (`o.x`) | ~1-3ns | ~0.5ns |
| chain 1-link (`Object.create({m})`) | ~18ns | ~17ns |
| chain 2-link | ~19ns | ~17.5ns |

Warm chain reads are ~7-18x own-data reads and their cost is FLAT in
link depth (1 vs 2 links identical): the fixed `member_chain_get`
validation — receiver-generation compare, the cached links' (id,
generation) walk, and the found link's value-cell re-read — dominates,
not the chain walk. The 2026-09-01 JIT inline-probe experiment (the same
validation inlined) measured slower, and no shape-free interpreter slice
obviously clears it, so the fix is L2's per-site shape/offset IC (serve
the read at own-data cost via a shape compare + slot) once the L1c shape
representation lands. Candidate (a) closes with this record; no
code lands. (No gates run — probe-only; the tree is untouched.)

### The remaining-module registration probe: Map/Set are O(n)-scan bound, not chain-bound (measured 2026-09-04)

Candidate (c) — extend the 4.2 O(1) handler registration to the other
agent-dependent modules — probed with warm 200k-call rows (both
engines): Map.get ~2.9µs/call, Map.set ~3.7-4.3µs, Set.has ~5.5-6.2µs,
Object.hasOwn ~5.0-5.4µs, hasOwnProperty ~950-980ns, DataView.getUint8
~630-720ns, vs Math.abs (pure closure, no chain) ~155-175ns and
registered charCodeAt ~350ns. Two distinct causes:

1. **Object and DataView methods are chain-bound** (their late dispatch-
   chain arms pay ~35 `intrinsics.get` per call — each allocates a
   JsString + hash-lookup — which is why Object.hasOwn at arm ~35 costs
   ~5µs). Registration would fix them, but Object's `dispatch_call` arms
   are INLINE closures (not the named `(agent, this, args)` fns the other
   modules map), so registering means refactoring them to named handlers
   first — deferred to L2 or a dedicated mechanical pass.
2. **Map/Set/WeakMap are NOT chain-bound — they are O(n) per op.**
   `keyed.rs` stores the entries in a `Vec` and `find_index`/
   `find_set_index` do `map.iter().position(...)` per get/has/set: a
   1024-entry map scans up to 1024 `same_value` compares per op (~2.8-5.5
   ns each). That is a structural data-structure lever (hash-index the
   entries) that registration cannot touch, and it is invisible to the
   bench rows (none exercise Map/Set).

Candidate (c) closes by probe; the actionable follow-up is a hash-indexed
`map_data`/`set_data` (find_index via a key index with the Vec kept for
insertion order), likely the largest remaining lever for Map/Set-heavy
code. No code lands. (No gates run — probe-only; the tree is untouched.)

### Hash-indexed Map/Set entries: the keyed collections land their key index (measured 2026-09-04)

The (c)-probe follow-up (candidate (d)) lands: every strong Map/Set now
carries a SameValue-consistent hash index over its LIVE entries, keeping
`find_index`'s O(n) scan only as a collision net. The `[[*Data]]` List
semantics are untouched — `map_data`/`set_data` still hold the
insertion-ordered, tombstoned entries `Vec` (deleted slots stay so
suspended iterators keep scanning), now bundled with the index in a
`MapCollection`/`SetCollection` (agent.rs; the `Trace` covers only the
entries, and `WeakMap`/`WeakSet` keep their plain `Vec` cells — their GC
compaction renumbers slots, so a position index would need clearing at
every sweep, and they are not in the measured rows).

**The word function.** `key_word` maps a canonicalized key to a u64 with
the property that SameValue-equal keys always share a word, so the index
can never miss a live key: numbers fold to their bits with NaN (any
payload SameValue-equals any other) and the ±0 pair folded to constants;
strings and BigInts hash content (equal content SameValue-equals across
DISTINCT boxes — `m.set('a'+'b', 1); m.get('ab')` must hit); a Function
value and an Object value aliasing the function's object side are
SameValue-equal (spec 7.2.12 step 7), so both hash the object's stable
id; symbols/objects hash their id. Words never reference a GC box, so
the index needs no tracing.

**The O(1) shape.** The index maps word -> live slot, one row per live
key. Because every live key owns its row, a word with no row is an
authoritative miss (no scan), a delete drops its row in O(1), and a set
appends + indexes in O(1). The single-slot table is exact unless two
live keys share a 64-bit word (a genuine hash collision); when an insert
would shadow a live key (`collided`), lookups/deletes fall back to the
exact `find_index` scan and deletes rebuild the index, so a collision
can only cost time, never return a wrong entry. Every mutation keeps the
index over the live slots: a delete tombstones the slot and removes its
row; `clear` empties both; the direct-construction sites (`groupBy` and
the set-methods' result sets via `new_set_from_data`) build the index
once.

**Measurement** (fresh release builds of parent `908256c` + probe rows vs
this tree, 200k-call rows, both engines interpret the row bodies):

| row | parent | this |
|---|---|---|
| Map.get (1024-entry, hit) | ~606ms (~3.0µs/call) | ~388ms (~1.9µs/call) |
| Map.get (16-entry) | ~378ms (~1.9µs/call — floor) | ~383ms |
| Map.get (1024-entry, MISS) | ~908ms (~4.5µs/call — full scan) | ~386ms |
| Map.set (1024-entry, overwrite) | ~850ms (~4.3µs/call) | ~598ms |
| Set.has (1024-entry) | ~1245ms (~6.2µs/call) | ~1033ms |
| Set.has (16-entry) | ~1006ms (floor) | ~1015ms |
| Map delete+set churn (1024 live) | ~34.5s (~172µs/iter) | ~0.9s (~4.5µs/iter) |

The hit rows are now FLAT in collection size (1024-entry == 16-entry,
within run noise), so the O(n) per-op scans are gone: a hit, a miss, and
a delete/set each probe the index once. The churn row — which tombstones
a slot and re-appends on every iteration — was ~38x scan/retain-bound
and now runs at the delete+set dispatch floor. The residual per-call
cost (~1.9µs Map.get, ~3-5µs Set.has) is the module's linear
`dispatch_call` identity chain (Set.has's arm sits late), not a scan —
that is the 4.2 `handler_for` registration lever (candidate (c) for the
keyed module), whose arms are already the named `(agent, this, args)`
handlers the pattern wants.

**Gates**: clippy clean (`--workspace --all-targets -- -D warnings`);
`cargo test --workspace` green (new
`hash_indexed_collections_agree_with_the_exact_scan` — a 3000-op
pseudo-random differential of the indexed Map/Set against the exact
scan model over NaN-payload/±0/cross-box-string/object keys, asserting
identical Vec length, live order, and slot answers after every op — and
`indexed_map_and_set_survive_gc_stress`, which drives rope/object/NaN
keys through set/delete churn under per-allocation collections); the
three release sweeps identical to the parent (language 23721/3 skip,
built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang).
Next: register the keyed module's handlers O(1) (candidate (c), now the
row floor), and extend the index to WeakMap/WeakSet only behind a
measured probe (their compaction interplay is real work).

### Keyed builtins register O(1): Map/Set/WeakMap/WeakSet skip their dispatch chain (measured 2026-09-04)

Candidate (c) lands for the keyed module. The index landing's residual
per-call cost (Map.get ~1.9µs, Set.has ~5µs — Set.has's arm sat ~40
`intrinsics.get` calls into the module chain) was the keyed
`dispatch_call` linear identity chain, exactly the cost 4.2's
`handler_for` registration removes for String/Number/Boolean/BigInt:
every arm allocates a JsString and hash-looks-up the intrinsic.
`keyed::handler_for` now maps each `Intrinsics::define`'d keyed function
(Map/Set/WeakMap/WeakSet methods, the statics, the size/species getters,
and the two iterator `next`s) to the named `(agent, this, args)` handler
the chain already calls — the four constructors register their
call-without-new TypeError, and their `new` path keeps
`dispatch_construct` (a construct cannot be a warm call-by-id dispatch).
Realm.rs wires the module into the `define` chain, so install registers
every id; a warm `m.get`/`s.has` call dispatches through
`builtin_handler(id)` in O(1). The chain stays for anything unregistered
(the `%Set.prototype.keys%` alias, prototype patches, cross-realm
function objects).

**Measurement** (fresh release builds of parent `3cd5c9b` — the index
landing — + probe rows vs this tree, 200k-call rows, both engines):

| row | index-only (3cd5c9b) | + registration |
|---|---|---|
| Map.get (1024-entry) | ~356ms (~1.78µs/call) | ~45.6ms (~228ns/call) |
| Map.set (1024-entry) | ~513ms (~2.6µs/call) | ~44.3ms (~221ns/call) |
| Set.has (1024-entry) | ~891ms (~4.45µs/call) | ~48.9ms (~244ns/call) |
| Map delete+set churn (1024 live) | ~770ms (~3.9µs/iter) | ~94ms (~470ns/iter) |

~7.8x / ~11.6x / ~18x / ~8x. Set.has's late chain arm collapses to
Map.get's cost (~244ns vs ~228ns/call), confirming the residual was the
chain position; the keyed row floor is now the registered-call floor
(the (c) probe's registered charCodeAt ~350ns), not a scan or a chain.
The JIT column matches (the compiled loop's native calls route through
the same O(1) dispatch).

**Gates**: clippy clean; `cargo test --workspace` green (new
`keyed_builtins_dispatch_via_registered_handlers` pinning the
registered handlers behave exactly like the chain arms — results,
receiver TypeErrors, constructor-without-new, getOrInsert/groupBy,
set-methods, iterator pairs, and prototype-patch liveness); the three
release sweeps at baseline (language 23721/3 skip, built-ins 23657/155
skip, annexB 1086/1086, zero fail/crash/hang). Next: the same
registration for the remaining agent-dependent modules is bounded by
their measured hotness — the (c) probe's residual was Object's (its
`dispatch_call` arms are inline closures, so registration needs a
named-handler refactor first) and DataView's; extend per probe.

### Object and DataView builtins register O(1): the (c) registration arc closes (measured 2026-09-04)

The last chain-bound modules the (c) probe measured hot were Object's
~40-intrinsic chain (Object.hasOwn ~5µs/call at arm ~35 — every
`intrinsics.get` allocates a JsString + hash-lookup) and DataView's
(getUint8 ~630-720ns/call). Object's dispatch arms were INLINE
closures, which the 4.2 registration pattern cannot wrap, so each is
now extracted into a named `(agent, this, args)` handler
(`prototype_has_own_property`, `prototype_is_prototype_of`,
`prototype_property_is_enumerable`, `prototype_to_locale_string`,
`object_create`, `object_define_property`, `object_entries`/`values`/
`keys`, `object_get_own_property_descriptor(s)`, `object_has_own`, the
integrity-level statics, ...) and the dispatch arms call those fns —
the linear chain and the new `object::handler_for` share one
implementation. DataView's 22 element get/set codecs register per
element type (the handlers bind `ElementType`; the codec fns take it by
value) and its buffer/byteLength/byteOffset accessors and the
constructor's call-without-new error register directly. Both modules
wire into `Intrinsics::define`; the chain stays for anything
unregistered (aliases, prototype patches, cross-realm function
objects), so behavior is identical — only the dispatch is shorter.

**Measurement** (fresh release builds of parent `9f9cb54` + probe rows
vs this tree, 200k-call rows):

| row | chain-only (9f9cb54) | + registration |
|---|---|---|
| Object.hasOwn | ~1085ms (~5.4µs/call) | ~126ms (~632ns/call) |
| hasOwnProperty | ~283ms (~1.4µs/call) | ~150ms (~751ns/call) |
| DataView.getUint8 | (c)-probe ~630-720ns/call | ~291ns/call |
| DataView.getFloat64 | — | ~286ns/call |

~8.6x on Object.hasOwn (the late-arm extreme the probe measured at
~5µs); hasOwnProperty is an early arm (arm ~3) and its row is mostly
real work, so ~1.9x; DataView reads drop to ~290ns/call (~2.2-2.5x vs
the recorded probe — its residual is the view-state/check work, not a
chain). Object.keys' row is allocation-bound (a 64-key object builds 64
fresh key-string boxes per call) and unchanged. Candidate (c) is now
CLOSED: every module whose methods a probe showed hot — String /
Number / Boolean / BigInt / Keyed / Object / DataView — dispatches
warm calls in O(1) by function id.

**Gates**: clippy clean; `cargo test --workspace` green (new
`object_and_dataview_builtins_dispatch_via_registered_handlers` pinning
the registered handlers behave exactly like the chain arms — statics,
receiver coercion, integrity levels, __proto__/legacy accessors,
groupBy/fromEntries, the DataView codecs + accessors, and error
surfacing); the three release sweeps at baseline (language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang). Next: the remaining linear chains belong to modules
no probe has shown hot (Date, typed-array, iterator, ...); the
structural levers revert to the L2 per-site IC slices (a)/(b) behind
the L1c shape representation.

### Shape-keyed store cells: the >4096-object store cliff (L2 slice b) (measured 2026-09-04)

The first L2 slice lands on the write side. The L1a store cells
(`MEMBER_WRITE_CELLS`, 4096 entries) are keyed by (object id, name), so
a store loop whose object working set exceeds the table thrashes and
every store falls back to the full [[Set]] — even when all the objects
share ONE shape. Probe (200k-call rows, fresh release build of parent
`ebe30cc`): 64- and 1024-object store rows ~56ns/store interp (~40 jit),
8192 ~181ns (~162 jit) and 16384 ~182ns — a ~3.2x cliff that the object
count, not the shape count, drives.

**The mechanism.** Every instance of a shape shares the object's map,
and a map id pins its descriptor layout (maps are immutable after
creation; a structural change transitions the object off the map), so a
store cell keyed by (map id, name) is valid for ANY ordinary object
whose current map matches — no per-object identity or generation. A
second direct-mapped table `member_write_map_cells` (same 4096 size,
heap-direct like the identity table) is probed only when the (id, name)
cell misses (`warm_store_put`'s fallback); a hit stores through
`write_data_property_slot` with the pinned inline mirror, re-keys the
(id, name) cell so the same instance's next store keeps the cheaper
identity probe, and fronts the read-side value cell under the L1c
no-bump discipline — byte-identical to the identity fast path.

**The one safety gate**: a vector-only property (a spilled or
non-transitionable key on a live map) is NOT map-pinned — two objects
can share a live map yet hold different vectors after it — so only
map-described inline keys are ever recorded (`warm_store_record` records
the map cell when `map_store_field` pins a field). The pinned mirror is
therefore always real, and `write_data_property_slot`'s stored-key
recheck backstops a stale slot.

**Measurement** (fresh release builds, parent `ebe30cc` + probe rows vs
this tree, 200k-call rows, both engines):

| row | parent (ebe30cc) | this |
|---|---|---|
| store 64 (64 objects) | ~11.3ms (~56ns/store) | ~11.3ms |
| store 1024 | ~11.2ms (~56ns/store) | ~11.2ms |
| store 8192 | ~36.2ms (~181ns/store) | ~11.6ms (~58ns/store) |
| store 16384 | ~36.4ms (~182ns/store) | ~13.4ms (~67ns/store) |
| store same 16384 (1 object) | ~3.8ms | ~3.9ms |

The 8192/16384 rows drop ~3x (interp ~181-182ns -> ~58-67ns/store; jit
~162 -> ~44ns) and now sit AT the 64/1024 warm level — the object-count
cliff is gone for same-shape working sets of any size (the ~13ms row for
16384 still pays one identity-table miss + one map-cell probe per
store). The single-object warm row is unchanged — no warm regression.

**Gates**: clippy clean; `cargo test --workspace` green (new
`stores_over_many_same_shape_objects_stay_exact` — 9000 same-shape
instances written distinct values with interleaved map transitions and
deletes dropped to dictionary mode, every value read back — pinning
that the shape-keyed slot never crosses objects); the three release
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang). Residual: vector-only keys on
shared maps (a hot 5th+ field of a many-field shape) still take the
(id, name) table and cliff beyond 4096 objects — that is the per-STEP
store IC (row 1.2), and the chain-member-read slice (a) stays
behind L1c's shape end-state.

### The vector-only store fallback: direct own-data writes past the map-pinned fields (measured 2026-09-04)

The 1.8 residual probed: a store loop writing a key the map does NOT
pin (the 5th+ field of a many-field shape is vector-only — maps stop
describing keys at `INLINE_FIELDS`, and a vector-only key's slot is
per-object, since two objects can share a live map yet interleave
non-transitionable defines that shift their vectors). Probe (200k-call
rows, this tree): 64/1024-object 5th-field stores ~57ns/store interp,
8192 ~214ns and 16384 ~223ns — a ~3.8x cliff on BOTH engines, and NOT
fixable by a per-step IC: nothing shape-pins a vector-only key's slot,
so no shape/offset record can validate it.

The tractable slice: on a (id, name) cell miss the store jumped
straight to the full `[[Set]]` (~180-220ns) even when `name` was an
already-existing own writable data property — a warm in-place value
write. `warm_store_put`'s miss chain now falls through the shape-keyed
cell to a DIRECT resolve-and-write: resolve the object's own vector
slot (`property_slot`), verify the stored key holds a writable data
property, and write in place through `write_data_property_slot` (with
the pinned inline mirror when the map does describe the key). Exact:
an own writable data property shadows the whole chain (spec 7.3.3), and
accessor/non-writable/absent cases fall through to the full `[[Set]]`.
The no-bump discipline is unchanged (the read-side value cell is
fronted and the (id, name) cell re-keyed at the current generation).

The vector-only rows drop ~3.1-3.3x (8192 ~214 -> ~69ns/store interp,
16384 ~223 -> ~68ns; jit ~184-196 -> ~54ns) and now sit AT the
64/1024 warm level; the inline-field and single-object rows are
unchanged (no warm regression).

**Gates**: clippy clean; `cargo test --workspace` green (new
`stores_over_many_vector_field_objects_stay_exact` — 9000 six-field
instances with distinct values, non-transitioning `defineProperty`
interleaved (shifting later vector slots on a subset), and deletes
dropping the map to dictionary mode, all read back); the three release
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang). What remains of the per-step
store-IC case: none of the measured store rows — the direct fallback
removes the identity-table cliff for every warm in-place write; the
only remaining full-[[Set]] stores are true defines (a genuinely new
key), which no IC can make faster without the L1c storage migration.
The chain-member-read slice (a) still waits on L1c's shape end-state.

### Suite re-baseline after the cell/registration/Map-Set landings (measured 2026-09-04, HEAD 7312c72)

`--jit-bench`, all rows result-ok, one quiet-machine run (interp / jit,
ms unless noted): arithmetic 13.3/2.46, bare loop 12.6/2.32, property
read 26.7/5.69, string concat 3.96/1.96, function calls 5.67/0.762,
global read 16.6/3.49, compound assign 3.56/1.43, buildString shape
92.9/33.4, buildString full 74.2/24.4, typed-array write 32.3/12.3,
typed-array length 11.9/1.86, wide leaf call 18.7/1.71, apply leaf call
19.2/7.09. Since the last recorded table (2026-09-02) the interpreter
has closed most of the row gaps the earlier landings targeted
(arithmetic 26.2 -> 13.3, property read 54.5 -> 26.7, string concat
10.6 -> 3.96, compound assign 19.7 -> 3.56, typed-array write 75 ->
32.3, buildString shape 180 -> 92.9). The largest remaining
interpreted rows are now the dense-array/string machinery
(buildString shape/full ~93/74ms) and typed-array write (~32ms); the
interpreted property read (~27ms) is ~2x node jitless, consistent with
the 1.1 read-floor probe (~3.5-4ns/read interp vs the row's ~13ns per
read+add pair). JIT ratios are 0.09-0.49 across rows — the compiled
bodies run 2-11x under the interpreter. No row shows a mechanism cliff
comparable to the closed Map/Set chains or the store-cell thrash; the
open structural items remain the L1c shape/storage end-state (true
defines, the JIT shape-compare end-state, and the chain-member-read
slice (a) behind it).

### Compiled shape-compare inline member read for map-pinned inline fields — Slice 1 (measured 2026-09-05, HEAD eedd670)

The compiled `GetMemberName` read fast path was ONLY the (id, name,
generation) 16-entry value-cell probe; a read over a working set bigger
than the table (any cycling-object loop) missed every iteration and fell to
the `get_member_name` helper. Gate probe (`scratch/l1c_option3_gate.js`,
six-field same-shape objects, 2026-09-05): the many-object compiled read
cliff (~8x: 3.8 → ~30ns/op) is object-count-driven and ordinal-independent
(ord 5 ≈ ord 1 at every scale), because the JIT has no shape-based read for
ANY ordinal — so the first slice is a machine shape read for map-pinned
INLINE fields (ord < INLINE_FIELDS), with no storage migration.

Mechanism: on a value-cell miss the machine code now probes the
interpreter's shared (map id, name) → slot map cells (`member_map_cells`)
and, on a match, reads the receiver's `in_fields[slot]` directly. A map id
pins the descriptor layout for every instance of the shape (maps are
immutable; a structural change drops the object to dictionary mode or
transitions it off the map — accessor conversion and mapped-key delete
drop it), so a hit needs no per-object identity or generation and is exact
for any object count. Failures fall to the helper: no map (dictionary), an
unrecorded shape, a recorded slot ≥ INLINE_FIELDS (vector storage), or a
hole (a boilerplate pre-sized field the body skipped — not an own
property, so the prototype chain must be consulted).

Supporting changes: `in_fields` became `[Cell<Value>; INLINE_FIELDS]` with
the reserved tag-9 `Value::uninitialized()` pattern as the frozen "unset"
marker (a machine read tests presence with one load + one exact-bits
compare; a stored value can never equal it — the Number constructor
canonicalizes doubles whose top 16 bits are TAG_PREFIX, and no tagged
value uses tag 9), and the field is `pub` for `offset_of!`. `MemberMapCell`
became a `#[repr(C)]` non-`Option` cell with an impossible-id `empty()`
(map ids start at 1), mirroring `MemberValueCell`, so the compiled probe
reads the shared cells at fixed offsets (`JitCallContext.member_map_cells`
added to all three ctx constructors). `crux::map::MAP_ID_OFFSET` and
`crux::value::UNINITIALIZED_BITS` expose the frozen offset/pattern to the
JIT.

Measurement (release, both engines, 3-run minima): compiled many-object
ordinal-1 reads 8192 ~30.5 → ~20ns/op and 16384 ~30.5 → ~21 (~1/3 off) —
the shape path serves them with no helper call; single-object warm reads
unchanged at 3.8ns (the value-cell hit path still runs first); ordinal-5
reads unchanged (~32-33ns — vector keys stay on the helper until the
backing-store slice); `--jit-bench` rows unchanged within the machine swing
(property read 22.3/5.26 vs the 26.7/5.69 recorded at 7312c72 — no
warm-path regression). The literal gate (collapse to the 3.8ns floor) is
NOT met: the residual ~20ns is the value-cell probe (which misses every
iteration on a cycling set) plus the shape path's dependent loads,
latency-bound under Cranelift's scheduler — the interpreter reaches ~4ns
on the same probe sequence because LLVM overlaps the independent loads.
Closing the rest needs a cheaper combined probe or a per-site recorded
shape gate; defer to the write-side / backing-store slice.

Gates: clippy clean; `cargo test --workspace` green (new crux
`map_described_inline_holes_read_absent_until_written` and jit e2e
`installed_jit_shape_read_serves_cycling_same_shape_objects`); the L1c edge
probes (`l1c_option1_edge`, `l1c_store_probe`, `l1c_compound_probe`) pass
under the JIT and `--jitless`; the three release sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang).

### Compiled member-store shape gate — Slice 2 (measured 2026-09-05)

The compiled plain member stores were value-cell-validated only: a store
whose (id, name) value cell missed (any working set larger than the
16-entry table) fell to the full `SetMemberName`/`assign_member` helper on
every store — the cycling-object store rows measured ~44-57ns/op at scale
vs ~12ns single-object, on both the register (`StoreMemberNameLocal` for
frame-slot objects, `StoreMemberName` for accumulator objects — which was
a blind helper call) and the step (`AssignMemberName`, plain assigns)
paths. The register and step store paths now share one inline shape gate:
on a value-cell miss the machine probes the shared (map id, name) MAP
cells (the Slice-1 read cells) and routes a hit to the narrow
`set_member_slot` write (which re-checks writability authoritatively and
falls back to the full [[Set]] internally, so a map-cell hit pins only
data-ness — non-writable, accessor-converted, and presize-hole receivers
stay correct). The step compounds keep the old miss-to-helper behavior
(their cached old value came from the value-cell-validated read).

Why the read map cells, not the L2 shape WRITE cells: the write cells are
recorded only by `warm_store_record` (the full-[[Set]] tail), but the
interpreter's own direct own-data fallback (`warm_store_direct_put`)
handles warm in-place stores without ever reaching `put_value`, so on pure
store loops the shape write cells stay cold and a machine probe of them
never fires (measured flat at ~44ns). The read map cells ARE recorded by
`fast_fresh_store` on every fresh define and by own-data reads, so they are
the reliable shape record; a hit routes the vector write through the same
narrow helper the value-cell fast path already used. No new helper, cell
layout, or ctx field beyond Slice 1's.

Measurement (release, `scratch/l1c_slice2_probe.js` + the gate probe,
3-4 run minima): cycling 8192/16384 same-shape stores drop ~44-57 →
~27-31ns/op on both the acc and slot forms and for BOTH ordinal 1 and the
map-recorded ordinal 5 (`SetMemberSlot` writes the vector for ord ≥ 4
keys, so the store side serves them even though the READ side still can't)
— ~35-45% off, no full-helper round trip; single-object warm stores
unchanged (~12-13ns); `--jit-bench` rows unchanged within the machine
swing; reads unchanged. The gate probe store rows at 16384 objects: .b
~45.8 → ~28.6ns and .f ~49.6 → ~31.5ns (JIT); interp ~84-89 unchanged.

The store side of the same shape now has the same O(1) object-count
behavior the read side gained in Slice 1; the remaining scale costs are
the per-store double probe (value-cell miss + map-cell probe) under
Cranelift, and the ord ≥ 4 READ gap (the Option-3 backing store).

Gates: clippy clean; `cargo test --workspace` green (new jit e2e
`installed_jit_shape_store_serves_cycling_same_shape_objects`); the L1c
edge probes (`l1c_store_probe`, `l1c_compound_probe`, `l1c_option1_edge`,
`l1c_update_probe`) pass under the JIT and `--jitless`; the three release
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang).

### The ordinal >= INLINE_FIELDS compiled read: a narrow map-slot helper (Slice 3, measured 2026-09-05)

The Slice-1 shape gate served only ordinals < `INLINE_FIELDS` inline; a
map-described ordinal at or above it (vector storage — no
machine-addressable inline field) fell to the FULL `get_member_name`
helper on every read (~30-33ns/op at scale vs ~20 for an inline ordinal,
an ~12ns ordinal gap on the compiled column). The Option-3 backing store
(an out-of-line overflow mirror) was the assumed fix, but scoping it
showed the machine inline read through a mirrored `Box` would be
indirection-bound (object → overflow handle → box → values box →
element), gaining little over a call; and the full field-authoritative
migration is blocked by insertion-order (the vector is the only place
described and vector-only keys interleave). The measurement-first step:
route the shape gate's map-and-name hit with `slot >= INLINE_FIELDS` to a
NEW narrow helper (`get_member_map_slot`), which the machine probe has
already validated — it reads the map-described vector slot live (no
full-Get re-derivation) and warms the value cell like the interpreter's
map-path hit; a hole or a divergent shape falls back to the full Get
inside the helper. The usual four-file helper mirror
(`runtime/jit.rs` field + extern fn + `JIT_SLOW_PATHS`, `helpers.rs`
enum/name/field/none/get/test double, `lib.rs` runtime_helpers and
helpers_all, the compiler's shape block reroute + the `set_member_slot`
3-arg sig reuse).

Measurement (release, the gate probe's `.f` rows, 3-run minima):
cycling 64/8192/16384 same-shape ordinal-5 reads drop ~30.5-33.4 →
~23.0-24.8ns/op (~25%) — the ordinal gap over the inline ~20ns collapses
from ~12ns to ~3-5ns (the residual is the machine call vs the inline
field read). Single-object ordinal-5 reads unchanged at 3.7ns (the
value-cell hit path warms and serves them). Stores and the inline rows
unchanged; `--jit-bench` flat; the edge probes pass under both engines.

The measured verdict for the Option-3 backing store: its remaining
JIT-read ceiling is now ~4ns/op at scale (call vs inline), the machine
inline read of a mirrored overflow `Box` is indirection-bound, and the
store side already serves ordinals >= 4 through the Slice-2 shape gate's
`set_member_slot` routing — so the field-authoritative/overflow-mirror
migration is NOT justified by any measured JIT row. Its remaining
rationale would be the interpreter's per-read vector borrow (~5ns from
the 2026-09-03 attribution probe) or a future per-site IC needing
offset-addressed storage — both small/deferred.

Gates: clippy clean; `cargo test --workspace` green (new jit e2e
`installed_jit_shape_read_serves_cycling_overflow_fields`); the L1c edge
probes pass under the JIT and `--jitless`; the three release sweeps at
baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang).

### Per-site member-IC gate for own reads: measured flat, reverted (Slice 4, measured 2026-09-05)

The remaining cycling same-shape compiled-read gap over the single-object
floor is the shared-probe work on a value-cell miss: index the (map id,
name) map cells, validate map-and-name, then read the slot. Slice 4
tried to remove that shared probe from a monomorphic site's steady
state with a per-call-site IC: a direct-mapped `MemberIcCell` (128
entries, indexed by a deterministic per-body site id) recorded a site's
LAST own-data resolution (receiver map id + key slot + name), and a
compiled read that missed the value cell validated the receiver's LIVE
map id and name against the record and served the recorded slot inline
(or via `get_member_map_slot` past the inline fields), bypassing the
map-cell probe; a one-link chain variant (kind 2) was designed
alongside it. One real soundness bug surfaced while wiring the gate: a
site index alone does not identify a property — every compiled body's
sites start at 0, so a collision could serve ANOTHER property's slot.
The cell was fixed to store and validate the read `name`; any future
per-site member IC MUST validate name the same way.

Measurement (release, the gate probe's cycling same-shape `.f` rows,
interleaved, vs the Slice-3 parent): reads stayed ~19-23ns/op — FLAT
against the map-cell table path it replaced. Root cause: the cycling
cost is dependent-load latency, not a lookup — the map probe's chain
(map handle → map data → map id → cell → slot) and the value-cell probe
are serial dependent loads that Cranelift's scheduler does not overlap,
and the per-site gate's own validation loads (site cell → kind/name/a
→ the receiver's live map id) are a comparable dependent chain that
ADDS its own latency. Chain reads are latency-bound the same way (a
machine one-link fast path would be per-link dependent loads +
validation, low ceiling), so the chain variant was dropped with the
whole gate. Verdict: per the measurement-first discipline the gate did
not pay and was reverted wholesale (tree restored to the Slice-3 state);
no gate record was kept. The remaining ~8-16ns over the ~4ns
single-object floor on this path is dependent-load latency, targetable
only by cutting probe depth (the shape-end-state offset model where a
single validated map id serves slot arithmetic inline) or by the
interpreter-vs-JIT record-discipline redesign, not by another cache
front.

### Dense Array push/pop fast paths and the chain-read shadow scan (measured 2026-09-06)

Corpus probe (`push_pop`-family probes, both engines): `Array.prototype
.push`/`.pop` member calls cost ~1.7/2.3µs per call even on a fast dense
Array, ~40-80x the direct dense-append cost (`a[l++] = i` ~30-45ns), and
the compiled column matched the interpreter (jit ~= jitless) — a pure
native-call row. Two landings, one commit each:

1. **Dense handler fast paths** (`dfb4ab1`): `push` read a dense Array's
   length from the cell (skipping the LengthOfArrayLike [[Get]]) and
   skipped the redundant trailing [[Set]] of `length` when every argument
   appended through `array_element_write` (which already maintains the
   length + mirror); `pop` fast-pathed a dense Array whose last slot is an
   own data element as a truncate (`array_pop_dense`). Also fixed a latent
   dense-append placement bug: after `a.length = N` grows the length past
   the (short) buffer, an append now stores the element at its real index,
   materializing the intervening holes. push ~101-105 -> ~53-57ms,
   pop ~139-144 -> ~53-55ms per 60k ops.
2. **The chain-read shadow scan** (this commit): the residual ~700ns was
   NOT the call — the per-iteration member read `a.push` re-resolved the
   prototype chain because every dense append bumps the receiver's
   generation, thrashing `member_chain_cells` (receiver-generation-keyed;
   probes: reading the method through the receiver ~52ms/60k vs through a
   stable `Array.prototype` ~11.5ms, in both engines). `member_chain_get`
   now accepts a mismatched receiver generation for an Array receiver
   when the map is still empty (Array defines bypass the map), the name
   is not an array index (dense elements are buffer-owned, invisible to
   the vector scan), the own-key set is small (<= 32), and the name is
   absent from the properties vector — an authoritative, generation-free
   shadow check, since dense element writes bump the generation but never
   touch the vector's names. push/pop member calls drop to ~225-270ns/call
   (the registered-call floor); the push+pop corpus row 250 -> ~26ms jit /
   push+pop corpus row 250 -> ~26ms jit /
   ~30ms jitless (~9.4x) with values unchanged.

Gates (both commits): clippy clean; workspace tests green (new crux
dense_append_after_length_growth test); a 19-case push/pop/chain edge
battery (dense paths, own shadows mid-loop, delete-restore, string-index
reads after pop, spill, frozen/non-writable, array-likes, subclass)
matches node on both engines; test262 sweeps at baseline — built-ins
Array 3082/3082 in jit and jitless, annexB 1086/1086, language 23721/3
skip, zero fail/crash/hang.

### The rope flatten-cache defect: per-call owned clones re-flattened (measured + landed 2026-09-06)

The corpus re-baseline after the chain-read landing showed the `strings`
family as the outlier (mean jit gap ~310x vs node; every other family
< 140x), led by `char_ops` ~1145x jit (~458.8ms / 300k
`s.charCodeAt(i & 255)` calls, ~1.5µs/call, JIT == jitless — a shared
native-helper cost, not dispatch). Probe (rope-built 256-unit string vs
the same content forced flat via one `slice`, both engines): rope ~416ms
jit / ~411ms jl vs flat ~104 / ~110 — a ~4x flatten penalty. Cause: a
string builtin gets `this` through `to_string`, which hands back an OWNED
`JsString::clone` of the rope box — and a clone's `OnceLock` flatten
cache is FRESH, so every `charCodeAt` re-flattened a 256-unit transient
(Vec + Arc alloc + node walk per call); the boxed original's own cache
never engaged. (First fix draft — an `Rc`-shared per-node cache —
measured a ~2x regression on `+=` rows; see Failed experiments.)

Fix: `JsString::owned_of(handle)` — leaves clone O(1) as before, but for a
rope it materializes the BOX's cache once and seeds the owned copy's
fresh cache with the same `Arc` (content is immutable, so the seed is
exact). The three handle->owned conversion sites use it: crux
`convert::to_string` (the String-receiver arm every `to_string(agent,
this)` reaches) and runtime `this_string_value` (String primitive +
String-wrapper object). A per-call `this` conversion now reads a rope at
leaf cost from the second call on; node construction is untouched.

Results (corpus, both engines): `char_ops` 458.8 -> ~107ms jit /
424.5 -> ~113ms jl (~4.3x / ~3.8x, now AT the flat-string floor — the
residual ~350ns/call is the registered-method-call floor, tracked as
open); `search_slice` 386.6 -> ~94-101ms jit / 401.6 -> ~97-108ms jl
(~3.8-4.1x — `indexOf`/`slice` had been re-flattening the 1200-unit
`hay` per call); `concat_loop`/`coercion_concat`/`split_join` and every
non-string row within cross-run noise (isolated `concat_loop` A/B vs the
parent: jit 11.7-12.4 -> 12.3-12.6ms, jitless 8.4-8.6 -> 8.4-9.0 — no
append cost). Corpus overall mean-jit gap 109.39 -> ~79.6, zero parity
mismatches.

Gates: clippy clean; workspace tests green (new crux test
`owned_of_seeds_the_owned_copy_from_the_box_flatten_cache`); test262
sweeps at baseline — language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang.

### For-in per-key deletion checks skip on an unchanged base (measured + landed 2026-09-06)

Probe of the corpus `control/for_in` row (~325ms both engines, 150k
re-entries of `for (k in o) { s += o[k] }` over a stable 5-key object;
node-jl 13.6ns/key-iter): isolated variants (for-in only vs same member
reads via a manual key loop vs break-after-first) showed the row is ~90%
ForInBegin — each re-entry eagerly re-enumerates the whole chain
(own_property_keys + a per-key descriptor lookup + HashSet/Vec/key-box
allocation; ~1.7µs per 5-key begin, ~1.0µs per 1-key begin) — with the
per-iteration ForInNext deleted-key check costing only ~40ns/key. Landing:
`ForInState` (now a struct) records the base's generation at enumeration
start plus a `fast` flag (base is an ordinary/array/External/IsHTMLDDA
object whose own-property structure the Cut-22 generation contract tracks
exactly, and every collected key is level 0). On the fast path ForInNext
yields each key directly while the base's generation is unchanged — any
delete/define/attr flip bumps it and drops back to the exact per-key
check, so the deleted-during-enumeration semantics are preserved by
construction. Both engine copies (interpreter `Step::ForInNext`, JIT
`for_in_next` helper) share the gate. `control/for_in` ~323 -> ~290ms jit /
~325 -> ~305ms jl (~10%); the head-let amplified fixture ~1660 -> ~1590ms.
A 16-case node-differential battery (deletes mid-loop incl. current/next/
readd, proto and multi-level deletes, mid-loop adds and shadowing, array
indices, proxy base, null-proto, attr flips, function objects, re-entry
with mutation) is byte-identical to the parent. Gates: clippy clean;
workspace tests green; test262 sweeps at baseline — language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang.
The remaining ~90% of the row is the ForInBegin re-enumeration itself —
the next slice is a per-object/generation-validated enumeration cache
(mirrors the per-array for-of fast verdict).

### For-in deleted-key filter matches V8: presence in the CURRENT chain, not enumerability (measured + landed 2026-09-06, follow-up)

The slice above left a pre-existing divergence the node-differential
battery exposed: `defineProperty` flipping a not-yet-visited key
non-enumerable mid-loop — slag skipped it (a,c), node/V8 yields it
(a,b,c). Probing the full mutation matrix (attr flips at own and proto
levels, delete-only, delete+re-add enumerable/non-enumerable/accessor,
lower-level shadows added before a pending higher key's turn, prototype
swap-away/swap-keep, own delete revealing a same-named proto/ancestor
key) showed the reference is V8's `ForInFilter`
(`Runtime_ForInHasProperty` → `HasEnumerableProperty`, runtime-forin.cc):
walk the receiver's CURRENT prototype chain and let the FIRST object that
owns the key decide — ordinary holders visit regardless of the property's
present enumerability (it was fixed when the key was collected), a Proxy
holder consults its [[GetOwnProperty]] trap and visits only an enumerable
own property (non-enumerable proxy-own shadows deeper links), and a key
absent from the whole chain was deleted before its turn and is skipped.
So a deleted own key is visited if a same-named enumerable key is later
revealed anywhere in the chain (node: delete own b -> proto's b visited),
which the old check-at-the-recorded-level could not produce.

Fix: `key_enumerable_at_level` (obj, level, key) became
`for_in_key_still_visited` (obj, key) with the chain-wide presence
semantics above; the recorded level no longer participates (all three
call sites — the interpreter `Step::ForInNext`, the JIT `for_in_next`
helper, and the AST `eval_for_in` — updated). The generation-skip fast
path stays sound: an unchanged generation-tracked base still owns its
level-0 keys, so the filter would find them at level 0.
Node differentials: the 11-case attr-flip/shadow/swap matrix plus the
13-case regression battery (incl. proxy attr-flip and delete-via-trap)
are byte-identical to node in both engines, where slag previously
produced a,c / a,p on the attr-flip cases. Gates: clippy clean;
workspace tests green; test262 sweeps at baseline — language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang.

### The ForInBegin enumeration cache: repeated for-in stops re-enumerating (measured + landed 2026-09-06)

The per-key slice's probe decomposition showed ~90% of the `control/for_in`
row is `ForInBegin` itself: every re-entry of `for (k in o)` re-enumerates
the whole chain (own_property_keys + a per-key descriptor read for the
enumerable flag + HashSet/Vec/key-box allocations) — ~1.7µs per 5-key
begin, and ~1.75µs even when only one key is consumed (the whole
Object.prototype walk happens regardless). Landing: an Agent-level,
direct-mapped (`FOR_IN_CELLS` 64) for-in enumeration cache
(`ForInEnumCache`, mirroring the `for_of_array_cells` Cut-27 table pattern
and traced like `ForOfFastVerdict`). Each entry holds the base handle
(traced — it retains the base and, through the prototype cell, the whole
chain, so no chain object's arena id can be recycled under the entry), the
full chain's (id, generation) snapshot, and the enumerated (level, key)
list. A cache is built only when the base and EVERY chain link are
generation-tracked (ordinary/array/External/IsHTMLDDA — an exotic link's
[[GetOwnProperty]] can change without a bump, so proxies/module
namespaces/arguments/typed arrays are never cached). Each ForInBegin
probe re-walks the live chain comparing every (id, generation): an exact
match reuses the cached list; any own-property or prototype change
anywhere in the chain misses and re-enumerates. Both engine copies
(interpreter `ForInBegin`, JIT `for_in_begin`) probe and fill it. The
ForInNext generation-skip (previous slice) remains the per-iteration
companion: with begin now ~a chain walk + list copy, the per-key check is
what the skip eliminates. `control/for_in` ~323 -> ~39.5ms jit / ~325 ->
~54.0ms jl (~8x / ~6x); node-jl gap ~33x -> ~5.4x (the residue is the
member reads + push/bind, not enumeration). A 20-case battery — mid-loop
deletes/adds/shadowing on base and protos, a proto gaining an enumerable
key between re-entries, 8/64/256 rotating objects (direct-map pressure),
re-entry with mutation, sparse arrays, gc-stress — is byte-identical to
node in jit, jitless, and `--gc-stress`. Gates: clippy clean; workspace
tests green; test262 sweeps at baseline — language 23721/3 skip, built-
ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang.

### Dense-array element creation: literals, from-values, and spread define by index (measured + landed 2026-09-06)

Probe: an EMPTY `[]` literal cost ~1.1µs per creation (vs ~70ns for `{}`)
and `[i, i+1, i+2]` ~1.65µs. Crux-level decomposition of the fresh-array
path: `array_create` itself is only ~237ns (ordinary object ~105ns); the
rest is the literal's per-element work — every element ran a full
CreateDataProperty through `index.to_string()` + intern + a
`PropertyDescriptor` + kind dispatch (~180-530ns each), and `ArrayEnd`
ran a full `[[Set]]` of `length` (~960ns even when the dense length
already equaled the element count).

Landing: `JsObject::create_data_property_index(index, value)` — an
index-native CreateDataProperty whose dense-array case goes through a
shared `dense_index_define` (extracted from `array_define_own_property`'s
canonical-w/e/c branch, so the two cannot drift) and whose fallback is
the exact string-key define. The VM's `ArrayElement`/`ArraySpread` steps
(the literal's own array is the fresh dense Array `ArrayBegin` created,
unreachable before the literal completes) and `array_from_values` call it;
`ArrayEnd` skips the length `[[Set]]` when the dense length already
reached the element count (no trailing holes — assigning the own length
its current value is unobservable). The change is spread across the
dispatch match plus the JIT mirrors (the four-file helper mirror):
`array_begin` still calls `array_create`, but the compiled
`array_element`/`array_spread`/`array_end` helpers use the same dense
define + redundant-length-skip. Also: `%Array.prototype%` now resolves
through a cached intrinsic accessor (`Intrinsics::array_prototype`, like
`object_prototype`) instead of `Intrinsics::get`'s per-call `JsString`
alloc + hash, and `JsObject::array_create` initializes in place
(`new_in_place`) instead of building the ~200-byte `JsObject` on the
stack and memcpy'ing it.

Isolated A/B (200k literals, interleaved parent/new): `[]` 221.5 ->
53.6ms jitless / 168.6 -> 49.6ms jit; `[i, i+1, i+2]` 330.1 -> 71.6 /
273.4 -> 63.6 (~4.1-4.6x), and jit now matches jitless (the compiled
array path had been slower than the interpreter's). Corpus:
`builtins/object_keys` ~271 -> ~169ms jit / ~270 -> ~163ms jl (keys/
values/entries build arrays through the same creation), everything else
within cross-run noise. A 20-case node-differential literal battery
(holes, trailing holes/commas, spread incl. holes and custom iterators,
nested literals, prototype index setters not consulted, growth past the
literal, loop literals) is byte-identical to node in jit, jitless, and
`--gc-stress`. Gates: clippy clean; workspace tests green; test262
sweeps at baseline — language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang. Follow-up (measured, not done):
the slice/concat/join native handlers are still ~5-9µs per call on tiny
dense arrays — they build their results through their own machinery and
are the next dense-array slice.

### Dense `Array.prototype.slice`: whole-dense ranges copy by index (measured + landed 2026-09-06)

The element-creation landing above left slice/concat/join handlers at
~5-9µs per call on tiny dense arrays. Probe decomposition of slice:
~4.5µs FIXED per call (species create — the constructor chain read, the
@@species getter, and a full [[Construct]] — plus the length [[Set]] and
dispatch) and ~740ns per ELEMENT (generic per-index HasProperty + Get
through `index.to_string()` + string-key machinery): a 300-element slice
ran 224µs/call jitless vs node-jl 74ns.

Landing: `JsObject::dense_element(index)` (a dense element read that
returns `None` for a hole) and a slice fast path gated on the source
being a dense Array AND the whole copied range being dense-present (no
holes): for such a range each index's HasProperty is trivially true and
Get reads the OWN element, so the prototype chain is never consulted — a
chain that shadows a hole (e.g. a property set on %Array.prototype%,
which the S15.4.4.10_A4_T1 fixture does) cannot change the result.
Holes, non-dense results, and exotic receivers keep the exact
per-element HasProperty/Get path; the dense path also skips the result's
trailing [[Set]] of length (already `count` from the species create).

Results: a 300-element `slice(0)` 6712 -> ~278ms jitless / ~275ms jit
for 30k calls (~24x; per-element ~740ns -> ~30ns, the residual fixed cost
is the species create); 3-element 145 -> ~88ms. `arrays/slice_concat`
corpus row ~226 -> ~204ms jl; no regressions. An 18-case
node-differential battery (ranges, negatives, holes preserved incl.
deleted elements, subclass species, custom species, array-like and
string receivers, shallow copies, frozen sources, large copies) is
byte-identical to node in jit, jitless, and `--gc-stress`. Gates: clippy
clean; workspace tests green; test262 sweeps at baseline — language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang (the sweep caught the first dense draft's hole-shadowing
soundness hole on the S15.4.4.10 fixture; the whole-range-dense gate
fixed it). Follow-ups (measured, not done): the ~2.5-4µs fixed species
create cost (a stock-%Array%-species verdict, the for-of-verdict
pattern) and the concat/join handlers' same per-element + species shape.

### Dense `Array.prototype.concat`: fully-dense spreads copy by index (measured + landed 2026-09-06)

The slice landing's measured follow-up. concat's per-arg shape: species
create, then per spreadable arg a @@isConcatSpreadable chain read +
LengthOfArrayLike + per-element generic HasProperty/Get + string-key
defines. Probe (30k calls): `a.concat(b)` on two 3-element dense arrays
~8.7µs/call jitless, `a.concat()` ~5.8µs (species + the receiver's own
copy + length set), ~1µs per extra element.

Landing: extend the slice dense gate to concat — a spreadable arg that is
a dense Array whose whole `[0, length)` range is dense-present (no holes,
so no prototype-chain HasProperty consultation can matter) copies by
index (`dense_element` + `create_data_property_index`) into the dense
result; holes/non-dense/exotic args keep the exact per-element path. The
`@@isConcatSpreadable` read and LengthOfArrayLike stay exact per arg. A
fully-dense concat's sequential defines already grew the result length to
`n`, so the trailing [[Set]] is skipped (when any arg took the generic
path or the result is not dense it still runs).

Results (30k calls, jitless): `a.concat(b)` 261 -> ~141ms (~1.85x),
`a.concat()` 175 -> ~114ms, `a.concat(9)` 214 -> ~104ms; per-element
result ~141ms (~1.85x), `a.concat()` 175 -> ~114ms, `a.concat(9)` 214 -> ~104ms; per-element
drops ~1µs -> dense (~30ns), residual fixed cost is the species create +
per-arg @@isConcatSpreadable/LengthOfArrayLike reads + dispatch. A
15-case node-differential battery (nested/sparse/array-like/non-array
args, @@isConcatSpreadable true/false incl. on the prototype, holes and
deleted elements preserved, subclass species, large multi-arg) is
byte-identical to node in jit, jitless, and `--gc-stress`. Gates: clippy
clean; workspace tests green; test262 sweeps at baseline — language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang. Note: the built-ins `copyWithin/coerced-values-*-detached`
typed-array fixtures take ~12.5s each in isolation (at the 15s boundary)
and flip to load-classified `hang`s under sustained batch load across
runs — pre-existing, unrelated to these array changes; they pass
individually and clean runs record 0 hang. Follow-up: the shared ~3-4µs
species create (slice/concat/map/filter) is now the dominant residual.

### Switch dispatch: the empty block env was per-execution allocation (measured + landed 2026-09-06)

The corpus `control/switch_dispatch` row (a 3M-iteration `switch (i & 7)`
with eight arithmetic cases) sat ~7x over the equivalent if-chain: the
handoff probe measured it jitless ~1546ms/full-corpus run (~515ns/iter)
vs the if-chain's ~55ns/iter, flat in case count — a fixed per-switch
cost, suspected to be the block/env machinery. `compile_switch` emitted
`EnterBlock`/`LeaveBlock` (a fresh declarative environment + scope_count
bump) around EVERY switch, whether or not any case consequent contained a
lexical declaration. Spec `CaseBlockEvaluation` (14.13.4 step 2) creates
the environment only when the case block HAS a lexical declaration — an
empty-decls switch needs no env at all, exactly the block/`fast_block`
discipline.

Landing: gate the env on `!block_decls(flattened consequents).is_empty()`
(no let/const/function/class/using anywhere in the cases). When skipped,
the scope_count increment AND the recorded `Scope::Switch` break_count
are also skipped — the break/return unwinding emits `LeaveBlock` steps
from `scope_count`, so a no-env switch must contribute no pop (the
original draft kept `scope_count += 1` unconditionally, which would have
popped a never-pushed env on every `break` out of such a switch — the
benchmark's cases all break, so this was caught before building).
`scope_count`/`break_count` for the env-ful path are unchanged.

Results (isolated interleaved A/B, 3 runs each): jitless ~2696 -> ~238ms
(~11x); jit ~289 -> ~110ms (~2.6x). The full-corpus row moved 1637 ->
234ms jitless. Both engines improve because both execute the same step
stream; the win is per-iteration declarative-environment allocation
(plus its GC churn) disappearing. Correctness: a 12-case node-differential
battery is byte-identical in jit, jitless, and `--gc-stress` — `let`
shared across cases (binding visible after fall-through), TDZ across
cases, class decls, function decls (env kept), var hoisting, return/
continue/break through cases, labeled break past the switch, nested
switches, and direct `eval` with `let` inside a no-decl switch (binds to
the function env — no switch env exists). Full corpus (37 workloads, jit
and jitless) result-identical. Gates: clippy clean; workspace tests
green; test262 sweeps at baseline — language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang. All 13
`--jit-bench` rows result-ok.

### The Array-species fast verdict: stock species creates skip resolution + construct (measured + landed 2026-09-07)

After the dense slice/concat landings, `array_species_create` was the
shared dominant fixed cost on slice/concat/map/filter/splice/flat/flatMap
(~2.5-4µs/call): every result-building call paid IsArray, the chain
`constructor` read, the GetFunctionRealm / same-realm check, the @@species
getter invocation, an IsConstructor gate, and a full [[Construct]] of the
resolved constructor. For the common case — a plain Array whose chain
resolves to its realm's %Array% under the stock @@species accessor — that
resolution is exactly an ArrayCreate with the array's own prototype.

Landing: a two-tier verdict mirroring the for-of verdicts (Cut 24/27). The
shared tier (keyed on the %Array.prototype% id) resolves once that the
proto's own data `constructor` is this realm's %Array% AND %Array%'s own
@@species is still the SPECIES-intrinsic accessor (its getter returns
`this`, so the resolution is always the receiver %Array%); it re-validates
by generation (a structural mutation of either shared object bumps) plus a
value oracle — the (%Array.prototype%, "constructor") member value cell,
warmed at resolve and refreshed by every warm store, catches the compiled
member-store VALUE write to %Array.prototype%.constructor that does not
bump (the JIT no-bump path). The per-array tier (id, generation, realm
prototype id) covers a plain Array (real Array kind, never a proxy) with no
own `constructor` whose prototype IS the current realm's %Array.prototype%
— the realm gate is exact because the foreign-constructor collapse (spec
9.4.2.3 steps 4-6) and the construct are realm-sensitive (a foreign realm's
slice applied to this array must create with THIS realm's prototype):
those creates become `JsObject::array_create` with the realm's prototype.
The soundness case is exactly the compiled no-bump write — a
probe cycles a JIT-compiled writer between two subclasses and %Array% and
confirms every following slice observes the live constructor value.

Results (isolated corpus probes, 200k slices per bench call, min of 3
runs, both modes): the fixed-source empty `slice(0)` stock baseline
~497ms (recorded 2026-09-06 handoff) -> ~163-169ms jitless / ~151-160ms
jit (~3x; ~810ns/call). The in-build control — a source with own
`constructor = Array`, identical species result but the verdict defeated —
measures ~410-419ms jitless / ~405ms jit, so the verdict is ~2.5x over the
equivalent exact species machinery on the same binary; subclass sources
~575-583ms. The full `arrays/slice_concat` corpus row moved ~204ms (the
concat landing's record) -> ~113ms jitless on a single run. Correctness: a 28-case functional battery (stock
slice/map/concat/splice/flat/flatMap, subclass species, own-constructor,
custom @@species value, reassigned %Array.prototype%.constructor, species
null, stock-shaped custom species getter, proxy receiver, holes/sparse) is
clean in jit, jitless, and `--gc-stress`; plus the compiled-store
soundness battery. Gates: clippy clean; workspace tests green; test262
sweeps at baseline — language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang. All 13 `--jit-bench` rows
result-ok. Follow-up: map/filter/concat inherit the win only at their
species-create share — their per-element native-handler dispatch and the
@@isConcatSpreadable/LengthOfArrayLike reads are the next dense-array
residuals.

### Dense element visits in the higher-order array methods (measured + landed 2026-09-07)

The species verdict landed `arrays/hof_methods.js` (a 150-iteration
map/filter/reduce over a 1000-element dense array) still at ~340ms
jitless / ~309ms jit (vs node-jit ~2.4ms). Decomposition probe (isolated
150k element-steps): map ~900ns/step, filter ~950, reduce ~730, forEach
~750, with the JIT column barely moving — the cost is the BUILTIN loop's
general per-element machinery, not the callback: a manual interpreted
dense loop (`b[k]=a[k]+1`) is ~92ns/step and a plain read scan ~98ns/step,
and the `function calls` row puts a callback call near ~60ns. Each
element paid a fresh `key(k)` JsString + a chain HasProperty + a general
[[Get]] (and map/filter a string-key CreateDataProperty on the result) —
the general `context::get_property` path is ~600ns over the fused read
floor.

Landing: the element-visiting builtins (forEach/map/filter/reduce/reduceRight)
read each element through `dense_own_element` (a new helper) — when
the receiver is a dense Array whose buffer holds the index below the
current length, that own canonical w/e/c element IS the HasProperty and
[[Get]] result (a present own data property shadows the chain, spec
7.3.1), so the read is a buffer slot with no string key, no chain walk, no
general dispatch. A hole, a beyond-length index (a mid-loop length shrink
is observed), a spilled array, or an exotic receiver returns `None` and
the element takes the existing exact HasProperty + Get path — the
callback can delete/overwrite/append/shadow mid-loop and every later
element still observes it. Result writes use `create_data_property_index`
(the concat/slice dense define; falls back to the string-key define when
the result is not a dense extensible Array).

Results (isolated 150k element-steps, min of 3 runs): map ~135 -> ~25ms
jitless / ~128 -> ~20ms jit (~5x); filter ~143 -> ~28ms / ~134 -> ~23;
reduce ~109 -> ~24.5ms / ~103 -> ~19; forEach ~113 -> ~24ms / ~107 ->
~20. `arrays/hof_methods.js` ~340 -> ~65.5ms jitless / ~309 -> ~55-58ms
jit (~5x). Correctness: a 40-case node-differential battery (dense,
holes skipped, chain shadowing of holes incl. %Array.prototype% index
props and setPrototypeOf receivers, mid-loop delete/overwrite/append/
length-shrink observed per element, spilled and frozen and non-w-e-c
sources, proxies with has/get trap logs, array-likes with index getters,
subclass species sources and results, huge-hole-span sources) is
byte-identical to node in jit, jitless, and `--gc-stress`. Gates: clippy
clean; workspace tests green; test262 sweeps at baseline — language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang. All 13 `--jit-bench` rows result-ok; 37-workload corpus
result-identical jit vs jitless. Follow-ups: the same dense visit for the
some/every/find family and `Array.from`'s per-item reads, plus the ~60ns
callback call itself (the compiled/leaf path from a native loop) once the
the reads are at the floor.

### Dense result defines and element reads in split/join (measured + landed 2026-09-07)

`strings/split_join.js` (60k round trips of `"a,b,...,h".split(",")` +
`parts.join(",")`) sat at ~407ms jitless / ~364ms jit. Decomposition
(isolated probes): split costs a ~1µs fixed plus ~270ms per token, join
~2.7µs for 8 elements — node-jitless is ~43ns/split and ~118ns/join. The
per-token/per-element cost was the same pattern the HOF landing removed:
`array_from_list` (split's result builder, and the empty-separator split
branch) defined each element via `create_data_property` with a fresh
`index.to_string()` key, and `join` read each element through `key(k)` +
the general [[Get]].

Landing: `array_from_list` (and split's per-code-unit branch) defines
elements with `create_data_property_index` (the dense w/e/c element
define on the pre-sized result, string-key fallback kept); `join` reads
each element through `dense_own_element` (a present dense own element IS
the [[Get]] result — holes/spilled/exotic receivers fall back to the
exact `get`). Results (isolated 60k-call probes, min of 3): 8-token split
~2.8µs -> ~1.4µs, join ~2.7µs -> ~1.1µs; `split_join.js` ~407 -> ~215ms
jitless / ~364 -> ~202ms jit (~1.9x). Correctness: a 40-case
node-differential battery (limits, empty/undefined/null separators,
code-unit empty-separator splits incl. astral pairs, trailing/double
separators, multichar separators, holes/sparse/deleted joins, chain
shadowing of holes, separator coercion order, array-like and string
receivers, index getters) is byte-identical to node in jit, jitless, and
`--gc-stress`. Gates: clippy clean; workspace tests green; test262 sweeps
at baseline — language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang. All 13 `--jit-bench` rows result-ok;
37-workload corpus result-identical jit vs jitless. Follow-up: split's
remaining ~0.8µs fixed cost is the primitive-string method dispatch +
per-token string boxing floor (substring copies), not the result
defines.

### Small-integer number-to-string and the fused string+primitive add (measured + landed 2026-09-07)

`strings/coercion_concat.js` (`"value=" + i + ":" + (i * 2)` per iteration)
sat at ~395ms jitless / ~332ms jit (node-jitless ~15.5ms) — a ~25x gap
with the JIT column flat. Decomposition (isolated probes): a string+string
concat is ~80-97ns/iter, but `"value=" + 100` (a CONSTANT number) was
~452ns and `"" + i` ~443ns — number->string via `crux::number::to_string`
ran ryu + a digit-vector/String rebuild on EVERY call (~350ns), and the
string+number `+` then fell into the general `apply_binary` tail
(ToPrimitive + agent ToString round-trips per operand).

Landing: (1) `crux::number::to_string` writes the exact decimal directly
for an exactly-representable integer (|x| <= 2^53, <= 16 digits, always
plain decimal — its own shortest round-trip, never the exponential form)
instead of the ryu path; (2) `Add` with one string operand and a
Number/Boolean/Null/Undefined on the other (whose ToString is the fixed
constant or the fast integer path) fuses to a two-string concat, in
`expr::apply_binary` AND the register executor's `binary_inline` (shared
`expr::concat_primitive`). Objects/symbols/BigInt/both-non-string shapes
keep the exact ToPrimitive machinery. Results (isolated, min of 2):
`"value=" + i` ~149 -> ~76ms jitless, `"" + i` ~133 -> ~55ms, full
fixture ~337 -> ~185-192ms jitless / ~320 -> ~160ms jit (~1.9x);
`coercion_concat.js` ~395 -> ~213ms; `json_roundtrip.js` ~220 -> ~188ms
(the stringify number path benefits). Correctness: a 45-case
node-differential battery (both operand orders, -0/NaN/Infinity,
fractions, 2^53 and 2^53+2 and 2^60 boundaries, sub-1e-6 and >=1e21
exponential thresholds, booleans/null/undefined, mixed chains,
template substitutions, toString/Number() parity, object/symbol/BigInt
fallbacks) is byte-identical to node in jit, jitless, and `--gc-stress`;
plus a small-integer parity unit test in crux. Gates: clippy clean;
workspace tests green; test262 sweeps at baseline — language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang.
All 13 `--jit-bench` rows result-ok; 37-workload corpus result-identical
jit vs jitless. Follow-up: the row's residual ~190ms is the remaining
concat + string-alloc machinery and the number+string+number chain shape.

### The concat-spread verdict: stock arrays spread without the symbol read (measured + landed 2026-09-07)

Decomposing `arrays/slice_concat.js` showed Array.prototype.concat carries a
~2.2us FIXED per-call cost on `a.concat(b)` of two 1-element arrays (30+30
only adds ~19ns/element): per spreadable element `is_concat_spreadable` ran
a full @@isConcatSpreadable symbol chain-read (~0.75us/element pair with
the general length [[Get]]) even though a plain Array's chain never
carries the symbol. Landing: a shared verdict (mirroring the species/for-of
pattern) records that a %Array.prototype% -> %Object.prototype% chain
(whose own prototype is null) has no own @@isConcatSpreadable,
re-validated by the two shared objects' generations (an absent own property
can only appear by a define, which bumps; symbol-keyed stores never take
the compiled no-bump member-store path). A concat element that is a real
dense Array on a certified chain with no own @@isConcatSpreadable spreads
with its dense length cell — skipping the symbol read and the length
[[Get]]; any other element (array-like spreadables, exotic/own/prototype
@@isConcatSpreadable, subclass chains, holes, proxies) keeps the exact
per-element machinery. Results (isolated 30k-call probes, min of 2):
`a.concat(b)` 66 -> ~28ms jitless (per call ~2.2us -> ~0.95us);
`a.concat()` (3-element receiver) ~42 -> ~22ms; 30+30 ~100 -> ~58ms;
`arrays/slice_concat.js` ~91 -> ~76ms. Correctness: a 26-case
node-differential battery (dense/multi-arg/holes/deleted/sparse, own and
instance/prototype/Object.prototype @@isConcatSpreadable true/false
including getter observation and delete-mid, subclass species sources and
results, array-like spreadables with holes, string/arguments receivers) is
byte-identical to node in jit, jitless, and `--gc-stress`. Gates: clippy
clean; workspace tests green; test262 sweeps at baseline — language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang. All 13 `--jit-bench` rows result-ok; corpus
result-identical jit vs jitless. Follow-up: the concat residual ~0.95us
per call is the method dispatch + species create + result-array dense
copies; `slice_concat.js`'s literal+slice+join portions are container
creation / join machinery.

### Function-expression instantiation probe: the closure-creation cost (measured 2026-09-07, NOT landed — the next-slice design)

After the concat/coercion landings the two largest rows are the amplified
fixtures `for-in/head-let-fresh-binding-per-iteration.js` (~1820ms
jitless, ~16x node-jitless, jit ratio 0.93) and `template-literal/
evaluation-order.js` (~1210ms, ~20x, 0.93): per-iteration ~18us and
~12us respectively. Decomposition of the for-in fixture's __t262Body
(isolated 100k-iteration probes, min of 2): object create + 3 defines
~320ns; same + 3 member stores ~380ns; + 3 arrow closures ~2.4us; + 3
`function(){}` closures ~4.7us; + a 3-key `for (let x in obj)` with
per-iteration binding ~1.4us. So a plain `function(){}` expression costs
~1.4us to instantiate and an arrow ~660ns (node-jitless ~40ns for both);
function-object creation is the dominant remaining machinery (it also
underpins `closure_capture` 147ms and `calls/*`). `register_function` per
closure pays: a Function box, an `ecma_functions` HashMap record insert,
`set_function_properties` (length/name defines), sloppy
`caller`/`arguments` restricted-property defines, an eagerly-created
`.prototype` ordinary object + MakeConstructor (its own `constructor`
define), and the %Function.prototype% wiring — vs V8's lazy `.prototype`
and shared boilerplate shapes. Sub-attribution (2026-09-07 probe): a
strict-bodied `function(){}` (~1.4us) vs arrow (~0.8us) vs sloppy
(~1.65us) splits the cost into core (~0.8us, arrow-level: Function box +
record insert + length define + proto wiring), `.prototype`+MakeConstructor
(~0.6us), and sloppy `caller`/`arguments` (~0.2-0.3us). Disabling
`capture_source` (the per-closure source-slice + hash) measured zero,
so the core is allocation/bookkeeping spread across many ~50ns steps
(Function box + 528B object part + EcmaFunction record node + intrinsic
wiring), not one removable term — cutting it 2-3x needs per-closure
boilerplate shapes (shared pre-sized maps keyed per kind/arity for the
function own-property set and the `.prototype` object) plus possibly lazy
`caller`/`arguments`, i.e. engine-level work best gated behind an
in-engine accounting pass.

**In-engine accounting pass (SLAG_FN_PROFILE, temporary env-gated phase
timers around register_function/instantiate_arrow, measured 2026-09-07):**
steady-state ns/closure at 300k creations (cal = 30ns empty-phase
overhead, so subtract ~30): `function(){}` sloppy fn_new ~115 + insert
~65 + props ~710 (3 defines: length/caller/arguments) + proto_alloc ~380
(ordinary_object_create of the `.prototype`) + make_ctor ~365 (2 defines)
+ set_proto ~36; arrow core fn_new ~110 + insert ~55 + props ~230 (1
define) + set_proto ~31; strict fn (props ~250, 1 define) confirms the
rest. A low-volume check (3000 creations, no GC pressure) measured the
SAME per-closure totals (fn ~1.63us, arrow ~0.81us), so the phase costs
are real machinery, not GC noise. OPEN (before building on these): the
~190-220ns per fixed define and ~380ns ordinary-object create in this
path are inconsistent with indirect object-literal measurements ({} +
3 member stores ~317ns, {} + 1 literal key ~117ns), and `props`/`proto`
phase boundaries may be misattributing work — the next step is a crux-
level ns/define counter (same SLAG_FN_PROFILE style) over both the
function path and a plain object-literal path from one build, which also
decides whether batching the fixed boilerplate (length[/name]/
caller/arguments/prototype + the `.prototype` constructor) is the right
slice.

**Direct crux-level define counter (SLAG_DEFINE_PROFILE, measured
2026-09-07): RESOLVES the discrepancy — the phase inference was wrong.**
Instrumenting fresh_data_define/fresh_data_define_attrs in crux with a
receiver bucket (function object-part vs ordinary) measured fresh defines
at ~69ns (fn object) and ~48-57ns (ordinary) INCLUDING ~30ns of Instant
overhead — i.e. ~40ns real on function objects, only ~1.3x the ordinary
path. The ~190-220ns/define phase numbers were misattribution (the phases
carried non-define work plus per-phase overhead). With ~1.8M fixed
defines over the 300k-closure probe at ~40ns real each, defines are only
~15% of the ~1.6us/closure total — the batched-boilerplate slice would
cap at ~7-15% and is NOT worth its risk. The ~1.2us core is elsewhere
(Function box + 528B object-part allocation, the ecma_functions record
insert, the compiled-body cache lookup, and the instantiate prelude) and
needs finer alloc/record-level attribution before any engine surgery.

Typed-array element access also
re-probed at ~50-55ns/op interp (node ~12ns, jit flat): the store/read
per-op index handling is the target if the boilerplate work lands first.

### The lean typed-array element path: no per-op length borrow, u64 bounds, in-range integer encode (measured + landed 2026-09-07)

The typed-array row re-probe decomposed to per-op costs above a ~23ns
step-path loop skeleton: a masked-key read adds ~37ns, a store ~37.5ns,
a dense-Array read only ~27ns — the typed-specific delta was the crux
element machinery. Two structural facts framed the slice: (1) the corpus
row's jit column EQUALS its jitless column (~230ms) not because the JIT
is slow at element ops but because the body constructs its array
(`var ta = new Uint8Array(...)` → a `Step::Construct`), which the whole-
body JIT compile rejects — the row is interpreter-bound in both columns.
The same masked store+read loop with the array passed as a parameter
compiles and drops 232→106ms, so JIT coverage of `Construct` is a
separate, later lever. (2) The interpreter element op carries removable
fixed work: `typed_array_effective_length` borrowed the block's live
byte length (a `RefCell` borrow) on EVERY read/write and re-checked
detachment, although a fixed view over a non-resizable buffer was
validated at creation and only detach can invalidate it.

Landed (crates/crux):

- `typed_array_effective_length` returns the stored `[[ArrayLength]]`
  for a fixed view over a non-resizable buffer without touching the
  live byte range (resizable-buffer and auto-length views keep the
  byte-length recompute, which is exactly where a shrink can occur).
- The runtime's numeric element fast paths (`typed_array_element_get`/
  `set` and the `element_bytes_into`/`write_element_bytes` helpers they
  share with `[[Get]]`/`[[Set]]`/`[[GetOwnProperty]]`/`[[DefineOwnProperty]]`)
  now bounds-check the canonical u64 index directly — no f64 round-trip,
  no redundant detached re-check (one gate), no re-derived index.
- Integer element encodes (Int8/Uint8/Int16/Uint16/Int32/Uint32) skip
  `wrap_signed`'s f64 `rem_euclid` when the value is an integral Number
  in [-2^31, 2^32) (the f64→i64 cast is exact there and its low bits
  reproduce the spec's trunc-mod-2^bits wrap); every other value keeps
  the exact general path.

A/B (interleaved, corpus arrays dir): `typed_array.js` ~230 → ~199ms in
BOTH modes (~13-15%); the isolated read loop 119→100ms, store loop
120→107ms (jl). The `--jit-bench` typed-array rows are unchanged
(write interp ~34/jit ~12.4, length ~12.7/jit ~1.9) — those store rows
use the register path, not the crux element helper.

Gates: clippy clean, workspace tests green, and the three release sweeps
at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang). A typed-array differential battery
(all element types × fractional/negative/huge/NaN/±Inf/-0 values, OOB /
negative / fractional index reads and writes, in-bounds own-property
descriptors + keys, `transfer()` detachment, and resizable-buffer
shrink/grow on both fixed-length and auto-length views) is byte-
identical to Node under jit, jitless, and `--gc-stress`, and the
37-workload corpus keeps jit/jitless/node result parity.

The row's remaining big lever is NOT the element path: a body that
constructs a typed array (or anything) locally never JITs, so the corpus
row stays interpreter-bound in the jit column. Supporting `Step::Construct`
(compile it as a slow construct call) would compile such bodies — the
param-array analog measured 2.2x (232→106ms). That is an L3-coverage
slice, separate from this element-path landing.

### JIT: `Step::Construct` lowers as a vector-form construct helper (measured + landed 2026-09-07)

The 2026-09-07 typed-array landing's closing note is now closed: a
certified body containing `new` reached the JIT and bailed whole-body at
the emit-step gate (`Construct` had no arm, so the corpus `typed_array.js`
row — whose `var ta = new Uint8Array(...)` sits inside `bench` — ran
interpreted in BOTH columns at ~jl speed). The construct CALL is lowered
as a vector-form helper mirroring `Step::Call`: the callee rides the work
stack, the arguments are built in `Vm::args` by the existing
`ArgsBase`/`ArgsPush`/`ArgsSpread` helpers, and a new `construct` helper
pops the argument boundary and runs the interpreter's shared
`step_construct` core (extracted from the `Step::Construct` handler) —
the certified base-constructor LEAF inline fast path (`run_leaf_construct`,
when `can_inline_leaf` and the leaf cache's `construct_inline` verdict
qualify) or the general `crate::function::construct` machinery. The new
arm is net-0 in `max_stack_usage` (pop callee, push result), is named in
`step_name`, and the helper runs the four-file mirror (`JitSlowPaths`
field/static/extern in runtime jit.rs, the `Helper` variant/name/field/
none/get/test double in jit helpers.rs, the `runtime_helpers()`/
`helpers_all()` copies in jit lib.rs, and the compiler `emit_step` arm).

A/B on the corpus rows whose bodies construct LOCALLY (previously never
compiled, jit == jl): `arrays/typed_array.js` jit ~197 → ~106ms (~1.9x),
`arrays/index_loop.js` jit ~139 → ~70ms (~2x, its `new Array(200000)`
blocked the whole body's compile). The interpreter columns are unchanged
and non-constructing rows move only with machine drift. `--jit-bench`
rows stay result-ok and unchanged. New `installed_jit_*` e2e tests drive
constructs through real compiled bodies: a base `function` constructor
in a loop, a builtin (`new Uint8Array`) in a loop, and a throwing
construct (`new Boom` whose body throws) caught by the compiled body's
own try. A construct differential battery (base fn, class + fields +
derived + `super` + `#priv` getter, `new.target`, Uint16Array/Date/
RegExp/Map/Array builtins, spread args, a param-throwing constructor
caught per iteration, a computed/conditional callee, inner constructs,
object-literal mixes) is byte-identical to Node under jit, jitless, and
`--gc-stress`.

Gates: clippy clean, workspace tests green (177 jit incl. the 3 new), and
the three release sweeps at baseline (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang); 37-workload
corpus parity across all four modes.

### The post-Construct row decomposition: the remaining mass is allocation walls or node-IC domains (probed 2026-09-07, no code)

With the crux element path and the `Construct` JIT arm landed, the corpus
was re-baselined (jl ~9.34s total, jit ~7.2s) and the remaining flat rows
decomposed to pick the next lever. Findings:

- **The interpreter's native-call floor is at node-jitless parity.** An
  isolated 600k-loop split a Set op into a ~26ns member read (`st.has`,
  proto-chain, flat in jit) + a ~160ns native call; a *trivial* native
  (`Math.abs`) is ~115ns and `parseInt` ~190ns, and node jitless shows
  the SAME ~100ns floor for `Math.abs`. So slag's generic native dispatch
  is NOT the gap — the wide gaps are node's per-domain interpreter ICs
  (Set.has ~18ns, Object.keys ~0.4µs/3-call, string ops) that slag has no
  analog for, plus genuine allocation. Registered builtins do pay a
  per-call `is_eval_function` realm lookup + a TLS `RefCell` HashMap probe
  (`BUILTIN_HANDLERS` by function id) before the handler — real but small
  against the floor, and matching node's Set ICs would need per-domain
  fused ops (a big program, not a slice).
- **`set_churn`/`map_churn` (~276/224ms jl, flat in jit at ~0.9) are
  node-IC-artifact rows**: the hash-index + O(1) registration landings
  left each op at ~190ns of call machinery, and jit cannot help because
  the callee is native (a member read + a general native call per op).
- **Generator resume is a ~1.05µs/`next()` driver in BOTH engines**
  (200k-yield rows ~216-222ms jl and ~196-206ms jit, body-cost
  invariant — empty/const/`i++` yields all identical). Per resume the
  driver does a generators-HashMap probe + Rc clone, an
  execution-context push/pop, `run_jit_resume`'s fresh work-buffer
  rebuild + JitCallContext re-setup + jit_work save/restore, and an
  `iterator_result` object per yield. Diffuse; node jl is ~45ns/next.
- **The register-run model cannot fuse the masked computed-key shape**
  (`ta[k & 65535]`): the computed key lands in the accumulator, and the
  member ops reject an `Acc` key (only `Reg`/`Counter`/`Ctx`/`Const`);
  there is no scratch-slot spill. Fusing it needs a new Acc-keyed
  `*ComputedLocal` op form (the object is a frame slot, so the
  accumulator is free to hold the key) — a compiler slice, bounded but
  intricate.
- **The dominant jl mass is allocation walls** (the ~34% of the jl total
  in the two language fixture amplifiers at 1.89s + 1.28s — closure
  instantiation + per-iteration envs + template/rope — plus destructure
  437ms, construct_churn 332, spread_assign 313): the L4 bump arena for
  fresh ordinary objects / closures is the standing fix.

No code lands (probe-only; the tree is untouched beyond the scratch
probes). The recommended next egg by measured torque is L4's arena for
fresh-object/closure allocation, or — if a bounded compiler slice is
preferred — the Acc-keyed computed-member register-run fusion.

### L4 RE-OPENED: the 2026-09-04 closure counted boxes, not box cost — fresh-object creation is now the dominant lever (probed 2026-09-07, no code)

The 2026-09-04 L4 probe closed the arena idea by counting BOXES per
iteration (construct = 1/iter, buildString ~0) and concluded the bump
arena already exists. That closure never measured the per-box COST; the
interpreter-series landings since then have made everything around
object creation cheap enough that the creation itself is now the
bottleneck. Fresh box counts (re-added TLS counter in `Gc::new`/
`new_in_place`, drained per workload through the corpus driver —
identical to the 2026-09-04 method): destructure 2 boxes/iter, a `{}`
1/iter, a `[]` 2/iter, `generator next()` 1/iter, a closure 4 boxes,
head-let 25/body, json_roundtrip ~18/iter — all spec-required and
UNCHANGED by perf work. But timing the isolated create rows (jl,
min-of-corpus): `{}` ~79ns/iter, `[]` ~110ns/box, a closure create
+capture ~1.4µs — vs node jitless `{}` ~5ns, `[]` ~2.7ns/box, a
closure ~31ns (10-50x). The object/closure churn rows (destructure
437ms, construct_churn 332, spread_assign 313, the language fixture
amplifiers at 1.89s + 1.28s with 25/15 boxes per 18µs/13µs body) are
now ~25-50% per-object creation cost, not step dispatch.

Reading the create path, `{}` = realm resolution + `intrinsics
.object_prototype()` (per object) + `canonical_empty_map` (a TLS
`RefCell` VEC LINEAR SCAN keyed by prototype id, per object — grows
with the number of distinct prototypes) + `Handle::new_in_place` (bump
alloc + free-list + live-list register + `init_ordinary`'s ~17 field
writes) + `link_self_handle`. The 79ns splits across several ~15-25ns
pieces; a nursery would NOT help (allocation is already bump — the
2026-09-04 closure is still right on that point), but trimming the
per-object realm/proto + canonical-empty-map resolution (pre-resolve
the realm's Object.prototype empty map once per realm, or store the
canonical empty map on the prototype object) and the register/init cost
is the concrete next slice. First step is an instrumented split of the
~79ns before building.

### Design: object-literal register-run fusion (scoped, 2026-09-07)

Target: the interpreter pays ~25-30ns/iter of step dispatch on object
literals because they can never fuse into `RunRegBody` (obj_lit jl 90ms
vs jit 58ms with an identical shared crux create; no `LeafOp` exists for
`ObjectBegin`/`ObjectInitName`). Full design, both engines:

- **The blocker.** A literal's fresh object lives on the value stack
  across its prop-value steps; the register executor is single-acc, so
  the object must be spilled (the existing `PushAcc` real-stack spill)
  while values compute in acc, and the shadow needs a marker that
  re-spills it per prop. Add `RegOperand::Literal` (the object is on the
  real stack): `ObjectBegin` lowers to `LeafOp::ObjectBegin` +
  `PushAcc`; each fast-form `ObjectInitName` (ident key, no shorthand /
  `set_name` / `__proto__`) pops the value (acc or a direct
  late-read operand) + the `Literal`, defines via the existing
  `create_data_property_key` fast path, re-pushes the object, and
  re-pushes `Literal`; `load_operand(Literal)` emits a `PopLiteral`
  (pop the real stack into acc) so any terminal (`InitLocal`, a member
  read, a binary) consumes it. The spills are self-balancing per run.
  The compiler's exhaustive `RegOperand` matches surface every arm that
  must bail (`Dup`, computed-store keys, ...) — most already have
  `_ => None` fallbacks.
- **Both engines.** `RunRegBody` ops are lowered by the interpreter's
  `run_leaf_ops` AND the JIT's `emit_leaf_op` (the four-file mirror:
  executor, JIT emit, `trace_leaf_op_heaps` for the `Const` operand,
  `max_stack_usage`). The JIT arms reuse the existing step helpers
  (`Helper::ObjectBegin`, `Helper::ObjectInitName`) with its working-
  stack push/pop discipline. JIT side is plumbing only (the JIT already
  compiles literals step-wise at ~58ms); the win is interpreter-only.
- **Evaluation order is preserved**: props define left-to-right on the
  unexposed object; a throwing value just leaves an unobservable partial
  object (spec-identical), and the fast define runs no user code.
- **Validation**: obj_lit/destructure jl A/B, `--jit-bench` unchanged
  (results), a literal semantic battery (shorthand/computed/method/
  accessor/spread/`__proto__`/set_name stay step-path via `None`),
  gc-stress, full sweeps.

Estimated ~300-450 lines across `ir.rs` + the jit mirror — a dedicated
slice, not a tail-of-session change.

**Instrumented split (2026-09-07, same probe run): the ~70-83ns `{}` is
DIFFUSE — no single dominant removable piece.** Per-create cost is flat
~70ns from 100k to 2M creates (the free-list keeps GC amortization
flat), ~20-25ns of it is the interpreter's step dispatch on the object
literal (a `{}` loop: jl ~93ms vs jit ~60ms on obj_lit — the register
executor has NO object/array-literal ops, so every literal pays step
dispatch inside an otherwise-register loop) and ~45-55ns is the crux
create: the `canonical_empty_map` TLS+`RefCell`+Vec scan, a ~530B
`JsObject` in-place init (~17 field writes incl. 3 `RefCell`s and the
4-wide `in_fields` Cell array), the arena register, and `link_self_handle`.
On the literal rows the subsequent FIRST DEFINES add ~40-60ns each
through the map-transition machinery (`create_data_property_key` on a
fresh canonical-empty-map object). Node jl does the whole
`{a:i,b:i+1,c:{d:i+2}}` + reads shape in ~38ns/iter vs slag ~380. So
there is no first micro-slice: closing the 10x is the object-
representation program (cheaper `JsObject` init / shape-cached literal
creation / cheaper map-transition defines / register-op literals to kill
the ~20ns dispatch), which is L1c-adjacent, not an allocator change.
The register-op-literal piece is feasible with the EXISTING spill
mechanism (a `PushAcc` spill of the fresh object + a new
`ObjectInit*Pop`-style op reusing the spilled object), but it is a
compiler+executor slice (~200-300 lines) that needs its own focused
turn and full gate.

### FALSIFIED by direct A/B: the object-literal register-run fusion is a small net REGRESSION — REVERTED (measured 2026-09-07)

The design above was implemented in full (Cut 71): `RegOperand::Literal`
shadow marker, `LeafOp::ObjectBegin`/`ObjectInitName`/`PopLiteral`,
`lower_step` arms for `ObjectBegin` and the fast `ObjectInitName`
(ident key, no shorthand / `set_name` / `__proto__`), a `PopLiteral`
load for the literal's consumer, the JIT `emit_leaf_op`/`leaf_operand`
mirror arms (reusing `Helper::ObjectBegin`/`Helper::ObjectInitName`),
`trace_leaf_op_heaps`/`leaf_op_has_heap_const`/`load_const` coverage,
and the counter-read cap relaxed 2 → 64 (the cap is vestigial since Cut
35 slice 21 moved the counter to a field each read resolves directly;
a multi-read literal `{a:i,b:i+1,c:{d:i+2}}` reads the head var three
times, and the 2-read cap kept the whole literal on the step path).
Semantics were byte-identical to node across a ~28-case battery in jit,
jitless, and jit+`--gc-stress`; workspace tests and clippy were green.

The mechanism WORKED (a `--dump` of `{a:i,b:i+1,c:{d:i+2}}` in a slot
loop collapsed the literal's 12 step dispatches into one 20-op
`RunRegBody`) but the interpreter rows did not move: a 3-round
interleaved A/B (baseline binary vs fused binary, same machine, jitless)
showed the fused build CONSISTENTLY SLOWER — obj_lit 86→92ms (~7%),
obj_survive 128→137 (~7%), obj_die 84→87 (~4%), obj_2m 142→146 (~3%),
destructure flat (~395 both). The slice's premise — that the jl-vs-jit
gap on the literal rows is removable interpreter STEP DISPATCH — is
false: `run_leaf_ops` match-dispatches each op exactly like the step
path, so a small literal run (ObjectBegin+PushAcc+…+PopLiteral) costs
MORE op matches than the 2-3 step dispatches it replaces, and the
crux-level create/define machinery the register form still calls is
unchanged. The jl-jit gap on these rows is per-op cost inside
`ordinary_object_create`/`create_data_property_key` (the crux
machinery the JIT shares), not the interpreter's outer dispatch.
Reverted in full; the only lasting value is this falsification — do not
re-propose register-op literals without first moving the crux
create/define per-object cost.

### The crux create/define split: every near-term lever is measured flat (probed 2026-09-07, no code)

Direct release-mode crux probes of the create + first-define machinery
(a `tmp` timing test in `crates/crux/src/object.rs`, since removed) split
the ~395ns/iter jl destructure row into per-key defines (~50-65ns each)
and creates (~65-80ns each) and then falsified each bounded lever in
turn:

- **The canonical-empty-map lookup is NOT the create cost**: the TLS+
  `RefCell`+Vec scan measures ~2.5ns; `ordinary_object_create` is
  ~65-80ns with the rest in `Handle::new_in_place` (bump + free-list +
  live-register), `init_ordinary`'s ~17 field writes on the 408B
  `JsObject`, `next_object_id`, and `link_self_handle`.
- **Pre-built final maps do NOT cut the define cost**: an object created
  on a pre-transitioned 4-field map and filled in place measures the SAME
  ~50-65ns/key as the transition path (the map already describes each
  key, so there is no transition/descriptor-search cost to remove).
- **Batching does NOT cut it either**: four fills under ONE `properties`
  `RefCell` borrow measure flat vs four separate borrows — the per-key
  cost is the dual-store write itself (the `in_fields` mirror + the
  authoritative `SmallProps` push + `Property`/key construction), i.e.
  the irreducible tax of keeping two synchronized stores per property.
- **`map_set` alone is ~9ns**, so the field write is cheap; the ~40-50ns
  remainder per define is the vector push + property record + bookkeeping.
  The earlier L2846 rejection (vector write ~5ns of a warm store) does NOT
  transfer to defines: a fresh define PUSHES (with the 2-entry inline
  spill to a heap `Vec` on the 3rd key) rather than rewriting a pinned slot.
- **Real-row cross-check**: construct_churn (constructor boilerplate path,
  `new Item(i)` with two `this.x =` stores) is NOT faster than the literal
  path — slag jl ~318ms / jit ~281ms vs node-jl ~31ms for 500k (~10x off,
  same as destructure). There is no in-repo reference path that is fast;
  the constructor final-shape machinery shares the same per-store tax.

Conclusion: no bounded crux slice moves these rows. Closing the ~10x on
object/closure churn is the object-representation program — fewer/smaller
per-object fields (the 408B `JsObject` init is a large fixed cost), or a
single-store representation where the map descriptors own keys/attrs and
the object owns ONE value array (the field-authoritative Option 3, whose
L2846 rejection was measured on warm pinned-slot stores, not on fresh
defines). Do not re-open the arena/L4 or register-op-literal levers;
re-probe Option 3's define path specifically (a fresh-define prototype
that writes one store) before a full migration.

### CORRECTED (2026-09-07): the numbers above were a NO-GC harness artifact; steady-state GC overturns the "no bounded slice" conclusion

The in-process split above timed tight create loops with NO collection —
the arena bumped into ever-fresh cache-cold pages, inflating every
absolute number ~5x (create ~70-80ns → ~13.5ns) and muddying the deltas.
Re-run at steady state (objects dropped + a `Heap::collect` every 512
batch, maps/proto rooted, min of 9; stable across runs):

| shape (ns/op, steady state) | result |
|---|---|
| `create(empty)` alone | 13.5 |
| `create(empty)` + 4 defines (current literal path) | ~270-315 |
| `create(final-map)` + 4 in-place defines | ~230-285 |
| `create(final-map)` + 4 `fresh_data_defines` (no `has_own` pre-check) | ~200 |
| `create(final-map)` + 4 `map_set` ONLY (Option-3 store, no vector) | ~22 |
| `create(final-map)` alone | 9.4 |

So a fresh define is ~55ns = ~8ns `has_own_property_key` pre-check + ~44ns
authoritative-`SmallProps` push/`Property` record/borrow + ~3ns `map_set`
field write — the push/record is ~90% of the define, and the map-field
write is ~3ns. The earlier L2846 rejection does NOT transfer: it measured
warm pinned-slot REWRITES (~5ns); fresh DEFINES push (with the 2-entry
inline spill) and pay ~44ns/key for the vector. The Option-3 single-store
(field-authoritative: keys/attrs in the map descriptors, values in the
map-assigned fields, no per-object vector for mapped keys) removes ~44ns
of each ~55ns define — a 4-field literal drops from ~300ns to ~22ns at
the crux level (a ~13x), which is the node-jl order for destructure. The
create is also ~13ns, not ~65-80ns — so the object-churn rows are
overwhelmingly DEFINE-side, not create-side. Re-opened for planning: a
field-authoritative fresh-define prototype is the highest-torque lever on
the object/closure rows, pending the Option-3 consumer-migration plan.

### Define-path trims measured within noise on real rows — REVERTED (2026-09-07)

First attempt at a bounded slice toward the define cost: (1) a map-aware
presence gate in `create_data_property_key`'s fast path (an aligned
ordinary object's vector is exactly the map's described prefix, so
presence is `map.find` — no full `has_own_property_key` [[GetOwnProperty]]
scan) and (2) single map resolution in `fresh_data_define` (one `find`,
with the aligned-transition child's offset written to `in_fields`
directly, dropping `map_set`'s re-find and the double generation bump on
transitions). Semantically clean: 214 crux tests + workspace green, a
14-shape define battery (duplicate keys, attr drift, delete-then-define,
spread, 5-key literals, frozen objects, symbol keys, __proto__) is
node-identical in jit/jitless/`--gc-stress`. But the interleaved A/B on
destructure_lite / construct_churn / obj rows is within the machine's
~±2% noise (destructure_lite ~1.5% better, construct_churn ~1.5% worse,
5 rounds) — the expected ~7-10% from the probe's ~8-11ns/define
attribution did NOT transfer, so the real-engine define is even more
vector-push-dominated than the probe suggested. REVERTED per the row-gate
discipline. Confirms: only the Option-3 vector-free migration (removing
the push itself) moves these rows; the map/`has_own` bookkeeping around it
is not the lever.

### Standing note: dismissals are conditional on the machinery at measurement time (2026-09-07)

Re-evaluating dismissed ideas under the binary-freshness discipline exposed
that several dismissals were not just possibly-stale measurements — they
were "tested too early," conditional on optimizations that had not landed
(or not fallen) yet:

- **Per-site member-IC (Slice 4)** is conditional on the read probe chain:
  its own verdict names the ceiling — "the shape-end-state offset model
  where a single validated map id serves slot arithmetic inline." On that
  model a per-site record is one compare; today it adds validation loads on
  top of a dependent-load chain. Re-test AFTER Option-3's offset reads.
- **Object-literal register fusion (Cut 71)** is conditional on the define
  cost: the A/B ran while crux defines (~55ns each) dominated, so fusion
  could not pay. Once Option-3 cuts defines to ~3-8ns (create ~13ns), the
  literal's remaining cost is the per-op dispatch + field fills, and a
  fused/map_set-only literal is exactly the shape that would eat it. Re-test
  AFTER Option-3.
- **The L2846 field-authoritative rejection** measured warm pinned-slot
  rewrites — a surface the L1a cells had ALREADY optimized, so the vector
  looked ~5ns; the un-optimized fresh-DEFINE surface carries the ~44ns
  push. Overturned above.
- **Read-side direct-mapped thrash FALSIFIED** is a statement about the
  map-cell design (the map work had just landed and absorbs misses), not
  about reads generally.
- **"The per-op dispatch floor dominates"** (cited by the register-run
  arcs and #3) is a property of the current Rust-match register executor;
  an executor redesign (superinstructions, wider fusion, threaded dispatch)
  moves the floor and reopens every dismissal that cited it.

Rule of thumb going forward: record each dismissal's dependency conditions
at dismissal time so a later machinery change is checked against the list.
Option-3 is the keystone — it cuts the object rows directly AND unlocks
re-testing the literal-fusion and per-site-IC dismissals on top.

### Option-3 vector-free defines — implementation plan (committed program, 2026-09-07)

The committed multi-session program. Goal: fresh ordinary objects keep their
map + `in_fields` as the ONLY store while every own key is map-described
(default w/e/c attrs, offsets < `INLINE_FIELDS`, values live in the fields),
so `fresh_data_define` skips the ~44ns/key authoritative-`SmallProps` push;
the vector is materialized lazily the first time a structural consumer needs
it. Established ground truth (this session): the vector is the sole crux
spec authority (`get_own_property_key` is a pure vector scan; a
map-described key with no vector entry is invisible to spec ops, and today
only exists in the unobservable post-`create_with_map` window before a
constructor's first store); the JIT reads `JsObject` fields only through
compile-time `offset_of!`, so layout changes are safe; and the map/`has_own`
bookkeeping around the push is NOT the lever (measured within noise) — only
removing the push moves the rows.

Encoding decision: an explicit `map_authoritative: Cell<bool>` on `JsObject`
(init false in every create site). Inference is UNSOUND — a constructor-
boilerplate object with some unwritten pre-described fields (a hole) is
indistinguishable from a vector-free object by (vector empty, descriptors
non-empty) alone, and materializing a hole would fabricate an own property.
The flag says "the vector is NOT authoritative; every map descriptor is an
own default-attrs property with a live `in_fields` value".

Invariant while set: the map is non-empty and describes exactly the own
properties, each with default w/e/c attrs at an ordinal < `INLINE_FIELDS`,
value live in `in_fields[ordinal]`; the vector and `property_index` are
empty. Entry: the first `fresh_data_define` append from a canonical-empty
map on an ordinary object (vector empty, descriptor_count 0) — or,
alternatively, only objects built by the fast literal/construct define path
enter it. Exit: `materialize_properties()` clears it (any structural
consumer, or a define whose ordinal would reach `INLINE_FIELDS`).

Slice order (each lands clippy-clean + workspace green + three sweeps at
baseline + row A/B where a row exists):
1. Layout + plumbing: add the flag + init it false in every creation site
   (`init_ordinary`, `basic_object_create_with_map`, `with_kind`, the
   host/htmldda/external constructors, arrays, strings, functions); no
   behavior change; verify JsObject size delta and the create rows do not
   move.
2. `materialize_properties()`: when the flag is set, build the vector from
   the map descriptors in order (key, attrs) + `map_field` values, clear
   the flag. Unit-test it reproduces the exact vector a normal build
   produces (enumeration/descriptor/delete parity on the same keys).
3. Define-path entry: `fresh_data_define` on an aligned ordinary object
   with the flag clear enters it (skips the push); duplicate-key updates
   (map describes key) write `in_fields` in place; a define at
   `INLINE_FIELDS` materializes first then pushes. `create_data_property_key`
   routes presence via the map while the flag is set. Both `fresh_data_define`
   and `fresh_data_define_attrs` stay exact.
4. Consumer guards (materialize-first at the ~6 choke points that can be
   reached on a vector-free object by user code; every other `.properties`
   site is internal to arrays/exotics/helpers and only runs after one of
   these materialized): `ordinary_property_lookup` (covers
   `get_own_property_key`/`has_own`/descriptor reads), `own_property_keys` /
   the ordinary own-key iteration, the `delete` path, `define_property_key`
   /`validate_and_apply`/`sync_map_after_define`, `property_slot`,
   `has_own_property_atom`. Audit the runtime for direct `properties`
   reach-ins that can observe a vector-free ordinary object and guard those.
5. Hot-read confirmation: reads of map-described keys stay on
   `map_field`/member cells and never materialize (the whole point); verify
   destructure/construct rows drop toward the ~60ns/object crux floor
   (4 defines + 1 create, no pushes) with the sweeps at baseline.
6. Overflow / full Option-3: objects past `INLINE_FIELDS` keys, the
   out-of-line value array for ordinals >= 4, and dropping the vector for
   mapped keys entirely (enumeration from descriptors) — deferred beyond
   the vector-free slices.

Re-test gates after the vector-free slices land: Cut-71 literal fusion and
Slice-4 per-site IC (both conditional on the cheap define/offset reads).

### Option-3 vector-free defines — slices 1-4 LANDED (implemented + measured 2026-09-08, uncommitted at HEAD 70f6cfd)

Implemented per the committed program above (the flag field is named
`props_deferred`). What changed:
- `map.rs`: `descriptor_at(i)` exposes (key, offset, attrs) for the rebuild.
- `object.rs`: `materialize_properties()` (private; rebuilds the vector
  from the map descriptors in order + the `in_fields` values, clears the
  flag, keeps the map — the object returns to the normal aligned vector
  state with the read-side map cells still valid). Entry + continuation
  in `fresh_data_define`: the first fresh define on a fresh Ordinary object
  (empty map + empty vector — constructor-presize objects start on a
  NON-empty map and can never enter, so an unwritten pre-described hole
  can never be fabricated) transitions the map and writes the field with
  NO authoritative push; subsequent fresh keys continue while
  `descriptor_count() < INLINE_FIELDS`; a define at `INLINE_FIELDS`
  materializes first and appends through the standard path (the 5th key
  lands at vector slot == ordinal 4). `fresh_data_define_attrs`
  materializes-if-deferred defensively. Guards: `ordinary_property_lookup`
  and `has_own_property_atom` serve map-described own properties WITHOUT
  materializing (presence/read consumers — a literal's duplicate-key
  probe, [[GetOwnProperty]] on a hot constructed object — never rebuild
  the vector; the hot interpreter read path is unaffected because the
  member-value/map cells serve `in_fields` first); `property_slot`,
  `delete_key` (ordinary), `ordinary_define_own_property`, and
  `ordinary_own_property_keys` materialize first. `has_index_keyed_own_property`
  scans the map descriptors while deferred (chain-clean verdicts must see
  a deferred link's index-keyed own props). A missed in-place init in
  `array_create`'s `new_in_place` path (the flag was never written there
  — garbage on every array) was caught by the workspace tests and fixed.
- `runtime/function.rs`: the construct `prototype` probe called
  `property_slot` while holding a `properties` borrow — reordered (a
  materialize inside the borrow would have panicked the RefCell). The
  other direct `properties` reach-ins (globals, arrays, spilled arrays,
  the chain/member cell paths) are unreachable on a deferred object or
  call `property_slot`/materializing entry points first.

Gates run: crux 219 tests green (5 new: entry/continuation, materialize
parity incl. index+symbol keys, delete/redefine materialize, 5th-key
overflow, presize-never-enters), workspace tests green, clippy
`-D warnings` clean workspace-wide, a 67-line node-parity battery
(literals incl. accessors, spread/assign/destructure, delete, descriptors
+ accessor conversion, 5+-key objects, freeze/seal/preventExtensions,
classes, Array.from/Sets, for-in order, symbols/index keys, hot loops)
is output-identical across node 24 / jit / jitless / `--gc-stress`.

Row A/B (release, same machine, interleaved 2026-09-08; the no-props
control `obj_lit` is flat so the machine held): `destructure_lite`
(`{a:i,b:i+1,c:{d:i+2}}` + reads, 1M) HEAD 460/444/446 ms → 371/396/397/403
ms jl (~-11%), jit ~406 → ~300-347 ms (~-15%). The first Option-3 slice
moves the literal rows the ~44ns/define push savings predict at the crux
level; the remaining row gap is the interpreter's per-literal step
dispatch + create cost (the Cut-71 literal-fusion and cheaper-create
items, both re-testable now that defines are cheap).

Outstanding before this is called landed per the repo gate: the three
release test262 sweeps at baseline (language/built-ins/annexB) on the
final tree.

### Constructor rows probed post-landing: the per-field cost is the field-store path, NOT the vector push or the chain probe (measured 2026-09-08)

Slice 5's literal half landed (destructure jl -11%); the constructor half
DID NOT — constructors stay ~2.3x the literal cost for the same shape, and
jit == jitless there (shared crux machinery, not dispatch). A shape probe
(release corpus, 3M constructs / 4M literals, ns/iter):

| row | jl | jit |
|---|---|---|
| `{}` | 72 | 61 |
| `{a}` + read | 121 | 94 |
| `{a,b,c}` + reads | 233 | 184 |
| `new F()` (empty body) | 107 | 111 |
| `new F` 1 field + read | 213 | 202 |
| `new F` 2 fields + reads | 352 | 319 |
| `new F` 4 fields + reads | 750 | 647 |

So a constructor per-field store costs ~100ns vs the literal path's ~35ns
(define + read); an empty-body `new F` is ~107ns vs `{}` ~72 (construct
overhead ~35ns, not addressed here).

Attempted next slice: extend the vector-free state to constructor-presize
objects (hole-aware: presence = map-described AND field written;
`materialize_properties` skips unwritten presize fields; entry on the
first body store of a presized `this`). Semantically clean — 220 crux
tests, workspace 4744 green, clippy clean, two new presize-hole tests —
but the interleaved row A/B is ~flat: c2 352 -> ~338ms (~4%), c1/c4 flat,
obj_lit control flat. REVERTED per the row gate. Conclusion: the
constructor per-field cost is NOT the authoritative push (a 1-2-field
vector never spills past the 2-entry inline capacity, so those pushes are
cheap; the ~44ns figure was the spilled 3rd+ push) and NOT the
chain-probe (a null-prototype constructor — `F.prototype = null` — is
identical, ~1000ms/3M both ways). The ~100ns/field is the field-store
dispatch itself: a `this.x =` write on a fresh receiver runs the full
[[Set]]/receiver machinery where a literal key runs
`create_data_property_key`'s lean fresh-define. The next constructor
lever is that store path (make a certified body's `this.x =` on a fresh
this take the fast fresh-define route), opened with a probe of where the
~100ns splits. The presize extension is sound and validated if that path
is ever fixed first. (CORRECTED by the landing below: the split found
that a ctor field define already takes the fast fresh-define route —
`assign_member` → `fast_fresh_store` — and the ~100ns was the doomed
`warm_store_put` FALLBACK that runs before it, whose `property_slot`
materialized the empty-vector deferred object per store.)

### LANDED (2026-09-08, uncommitted): warm-store fallback skips empty-vector receivers — the constructor field-store split

The `this.x =` dispatch split (follow-up to the probe above) found the
~100ns/field: a ctor field store runs `assign_member` → `warm_store_put`
→ cell miss → `warm_store_fallback` (map-put + direct-put). On a
canonical-empty ctor `this` the vector-free landing leaves the vector
EMPTY through the body, so `direct_put`'s `property_slot` MATERIALIZED
the deferred object (rebuilding the accumulated vector) on EVERY field
store — a growing O(n) rebuild per define that then failed anyway (the
key is a fresh define, not an existing warm property). Instrumentation
confirmed the flow: the ctor is NOT presized here (map count grows
0→1→2→3 across the body; the certified-construct presize did not fire),
so each define entered the deferred state and stayed vector-free.

Fix: `warm_store_fallback` bails when the receiver's property vector is
empty — an empty-vector object has no vector slot and no pinned field a
store cell could address, so the map/direct probes are doomed (exact,
not just a fast path: store cells are only recorded against a real slot,
and `write_data_property_slot` fails on an empty vector anyway). Interleaved
row A/B (release, jl ns/iter): c1 (1 field + read) 213 → ~190 (-11%),
c2 (2 fields) 352 → ~300 (-15%), c4 (4 fields) 750 → ~500 (-33%), jit c4
647 → ~426 (-34%); the no-field c0 and the `{}`/literal controls flat;
destructure_lite jl (which mixes 4 defines + reads on deferred objects)
~403 → ~323 ms (-20%) on top of the landing's earlier -11%. Gates:
workspace 4743 green, clippy clean, 67-line node-parity battery identical
across node/jit/jitless/--gc-stress, and all three release test262 sweeps
at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086, zero fail/crash/hang). The constructor rows are now ~2.2x
closer to node; the residual per-field cost is the fast-fresh-define
wrapper itself (existing check + chain probe + fresh_data_define), the
next slice if the ctor rows stay hot.

### LANDED (2026-09-08, uncommitted): lazy function `prototype` (MakeConstructor deferral)

Implements the scoped design above: plain ordinary functions (non-method,
non-async, non-generator — the hot closure/function-expression shapes) no
longer create their `prototype` object + property eagerly at register; the
EcmaFunction record carries `prototype_pending`, and the first observation
of the own `prototype` property materializes it through
`ensure_function_prototype` (which creates the prototype object inheriting
%Object.prototype% from the function's OWN realm — a cross-realm construct
must see the constructor's realm intrinsics — plus the `constructor`
back-reference and the fixed w/e/c descriptor). Arrows/methods/async/
generators/class-constructors never defer (eager path unchanged).

Read/write barriers (each a cheap key compare + `function_self` read on the
cold path; the hot member cells never see the absent property):
- Reads: `context::get_property_key`'s Function arm (covers user
  `f.prototype`, `instanceof`, species, the construct fallback).
- Writes: `context::put_value` and `ir.rs` `fast_fresh_store` (a fresh
  define would otherwise create a w/e/c property instead of updating the
  fixed descriptor in place).
- Own-introspection: Reflect defineProperty/deleteProperty/getOwnProperty
  Descriptor/has/ownKeys; Object defineProperty/defineProperties,
  getOwnPropertyDescriptor(s), hasOwn/hasOwnProperty, getOwnPropertyNames
  (own_keys_of), freeze/seal (SetIntegrityLevel), legacy accessor defines;
  the `in` operator's has-property walk.
- Proxies: a trap-less proxy forwards its own-property ops agent-free in
  crux, so any pending function target is materialized at Proxy/Proxy.
  revocable creation (the fixture-triggered gap found by the built-ins
  sweep: defineProperty/deleteProperty/getOwnPropertyDescriptor through a
  proxy onto a fresh function).

Interleaved release A/B (same machine, ~minutes apart; the eager HEAD is
c882570): sloppy closure create 1550-1615 -> 1070-1085 ns (-31%),
per-iteration capture 4030-4190 -> 3000-3080 ns (-26%), the head-let
fixture 2072 -> 1668 ms (-19.5%); strict create 1168 -> ~720 ns (-38%).
Gates: workspace 4743 green, clippy clean, a 40-line lazy-prototype
semantics battery node-identical across jit/jitless/--gc-stress (descriptor
flags, `in`/hasOwn, own-key enumeration incl. order, reassignment keeping
attrs, delete/defineProperty rejection, new/instanceof, freeze/seal,
arrays-vs-methods, per-iteration closures), the vecfree battery still
node-identical, and all three release test262 sweeps at baseline (language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang). Two sweep-found bugs fixed during the landing: the
materialize used the CALLING realm instead of the function's own (cross-
realm construct), and the barrier never fired through trap-less proxy
forwards.

### Closure-instantiation probe: ~1.4-2.1us/create, composed of boilerplate defines + eager prototype (measured 2026-09-08)

Corpus standings after the two landings (release, jl, vs node): total ~9.0s
vs ~1.36s (6.6x); the two language-fixture amplifiers (head-let 1738ms,
template-literal 1219ms) are the top mass and are jit-equal (general
path). Decomposing head-let (jl, 100k iters) showed the fixture is
~17.4us/iter ≈ 8 assert JS-calls (~11us, 33x node — general-path calls)
+ 3 per-iteration captured closures (~2.1us each) + base. Isolated
closure-create rows (200k creates, jl, jit-equal):

| kind | slag/create | node | x |
|---|---|---|---|
| arrow | 603ns | 21ns | 28x |
| strict fn (no caller/arguments) | 1168ns | 22ns | 53x |
| sloppy fn | 1553ns | 23ns | 67x |
| + per-iteration env capture | 3563ns | 55ns | 65x |

Composition per sloppy create (register_function + helpers): ~600ns core
(Function::new record + the ~530B JsObject + length/name boilerplate via
2 full `fresh_data_define_attrs` + `capture_source` (a per-create source
JsString slice) + ecma/pattern inserts), ~565ns `make_constructor` (an
EAGER prototype JsObject alloc + its `constructor` define + the
function's `prototype` define — for a property these hot closures never
read), ~385ns sloppy caller/arguments (2 more full defines). The
full `fresh_data_define_attrs` boilerplate defines measure ~190ns each at
this level (vs ~40ns for a deferred literal define) — multiple map.find
scans + RefCell borrows + push each.

Recommended slice (scoped, not yet implemented): LAZY function
`prototype` — defer `make_constructor` (the ~565ns line, ~36% of a
sloppy create) until the property is observed. V8 does exactly this.
Touch points (read barrier sites, each has the agent):
1. `context::get_property_key` when the key is the prototype atom on a
   Function value (covers every full-get read: user `f.prototype`,
   `instanceof`'s OrdinaryHasInstance get_property at expr.rs 1422,
   species paths, and the construct fallback).
2. `construct_this_object` (function.rs 2431) — every `new F` reads
   new_target's prototype through its member-value-cell oracle + get_property
   fallback; must materialize before reading.
3. Own-introspection builtins that call crux own-ops directly with the
   prototype key on a Function: `Object.getOwnPropertyDescriptor`,
   `'prototype' in f` (has_property_key), delete (the non-configurable
   rejection needs presence), defineProperty (the existing descriptor for
   invariant checks), getOwnPropertyNames/Reflect.ownKeys (own_property_keys).

Materialize = run the recorded make_constructor-equivalent for the
function's kind (prototype object inherits %Object.prototype% or the
resumable intrinsic). The pending state needs a per-function marker
(EcmaFunction record or the Function object's generation-synced bit) set
at register for kinds that would have created one eagerly; functions
that get `f.prototype = x` assigned or are never constructed/read never
pay. The read barrier fires only on the prototype atom on a Function — a
name+kind compare on cold paths, nothing on the hot member-cell path
(the property is absent, so the cells never cache it).

Follow-ups in the same family (ranked, not started): cache the per-site
source slice (drop the per-create `capture_source` JsString); batch the
length/name/caller/arguments boilerplate into a pre-built shape with
in-place field fills instead of 4 full defines; the per-iteration capture
env (~2us) after those.

### LANDED (2026-09-08, uncommitted): explicit-attrs boilerplate enters the vector-free define path (+ the transition-tree attrs fix it exposed)

With `prototype` lazy, an ordinary function's eager boilerplate is exactly
four keys (length/name/caller/arguments), all below `INLINE_FIELDS` — so
`fresh_data_define_attrs` (the boilerplate define) no longer needs to
materialize-then-push. Merged it and `fresh_data_define` into one shared
`define_fresh(key, value, writable, enumerable, configurable)`: the map
carries the explicit attrs, the deferred state (empty vector + map + fields
as the only store) represents non-default attribute sets too, and
`materialize_properties` rebuilds the vector from the descriptors with
their real attrs. Entry stays canonical-empty-only (empty map AND empty
vector), so a constructor-presize object with unwritten holes never
fabricates one; duplicate defines on a described key set the bit and write
the field.

Interleaved release A/B vs HEAD 72fc404 (jl, cfprobe, paired runs ~2 min
apart; the machine swings thermally, so each pair brackets a build):
cf0_call ~197-205 -> ~263 ms (-25%), cf0_sloppy ~184-197 -> ~212-219 ms
(-11-13%), cf2_strict ~151-182 -> ~182-184 ms (mixed, ~4-18%), cf4_periter
~625-670 -> ~729-848 ms (-15-25%, noisy). cf5_readproto flat (~250-265) —
that row forces `prototype` per iteration, so the deferred state never
survives. Jit column moves with the interpreter (closure creation is
runtime machinery in both modes).

The language sweep then caught 3 failures in the changed surface —
statements/function/S13.2_A4_T1/T2 and 13.2-17-1 all assert the fresh
function-prototype's own `constructor` is NON-enumerable, and for-in
enumerated it. Root cause: the map transition tree (`Map::transitions`)
was keyed by PropertyKey ONLY, so two defines of the same key with
different attrs from the same parent map (a `{ constructor: 1 }` literal
vs a prototype's non-enumerable `constructor` back-reference) silently
reused the FIRST define's child map. The old attrs path masked it — it
always pushed a true-attrs vector entry, so descriptor reads never served
the poisoned map attrs; the merged vector-free state made the map
descriptors authoritative (`materialize_properties` rebuilds from them)
and exposed it. Also a latent bug: HEAD 72fc404 alone already fails the
reverse order (a function prototype first makes a later literal's
`constructor` non-enumerable, so `Object.keys({constructor: 1})` = `[]`).

Fix: key the transition tree by `(PropertyKey, MapAttrs)` so same-key
different-attrs defines fork distinct children (the V8 model — transitions
keyed by key + property details). Shape sharing is unaffected for
attribute-consistent keys (all function boilerplate walks identical
(key, attrs) sequences from the shared empty map). Gates: workspace 32
suites green, clippy clean, both batteries node-identical across
jit/jitless/--gc-stress, and all three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang). Regression tests: map.rs same-key-different-attrs forks
distinct children; object.rs cross-object same-key-different-attrs keeps
each object's own descriptor attrs.

### NEGATIVE PROBE: caching the per-site source slice is not a lever (measured 2026-09-08)

Ranked follow-up #1 proposed caching the per-closure `capture_source`
JsString slice (each `register_function` stores the definition text for
Function.prototype.toString, so a loop creating closures from one site
allocates + copies the body text per create). Interleaved release A/B
(baseline 5b67e98 vs an instrumented build whose `capture_source` returns
None — the MAXIMAL win, bigger than any cache), alternating builds
~2 min apart: cf0_call/cf0_sloppy/cf2_strict/cf4_periter/cf5_readproto
all overlap within noise (the first single-run probe showing cf4 -25% /
cf5 -22% was a cool-machine outlier, falsified by the interleave). A
long-body probe (100k creates of a ~365-unit arrow body) showed at most
~4% (baseline ~117-128ms, no-slice ~114-120ms). The slice copy is NOT on
the closure-create critical path; the follow-up is dismissed until a
probe on a realistic toString-heavy or longer-body corpus says otherwise.

### LANDED (2026-09-08, uncommitted): the function boilerplate as ONE pre-forked shape (Cut 67)

Ranked follow-up #2: batch the length/name(/caller/arguments) boilerplate
instead of 2-4 sequential defines. A mechanism probe first — skipping
just the caller/arguments defines (an instrumented build, cf2_strict as
the no-restricted-props control stayed flat) showed each restricted
boilerplate define costs ~100-150ns even vector-free: the per-key
`define_fresh` machinery (map find/transition lookups, `intern_utf8` key
construction, RefCell borrows, 2 generation bumps) on a FRESH object.

The boilerplate shape is (key, attrs) — CONSTANT per function kind
(length/name = (f,f,t); caller/arguments = (f,f,f); the length count and
name text are field VALUES, so every ordinary function of a kind shares
ONE map). Landing: pre-fork the 2- and 4-descriptor maps once from the
prototype-less canonical empty map (the map a fresh `Function::new`
object starts on — identical to the chain the defines walk) and adopt
map + in-field values in one step. New crux `adopt_vector_free_fields`
(sets map + deferred bit + direct in-field writes, offset-guarded;
rejects a non-fresh object so the no-hole invariant holds; no generation
bump — a brand-new object has no cached readers). Runtime
`set_function_properties_batched` replaces `set_function_properties` +
the AddRestrictedFunctionProperties loop at both the register_function
and arrow sites; sequential defines remain as the fallback. Atom ids are
process-global u32s, so the pre-interned boilerplate keys live in a
`OnceLock` (a PropertyKey static would not be Send/Sync — the Symbol
variant holds a GC handle).

Interleaved release A/B vs HEAD 5b67e98 (jl, cfprobe): cf0_call
~197-207 -> ~118-127 ms (-40%), cf0_sloppy ~185-188 -> ~103-105 ms
(-44%), cf2_strict ~144 -> ~122-126 ms (-13%), cf4_periter ~631-687 ->
~428-443 ms (-33%), cf5_readproto ~254-262 -> ~210-218 ms (-17%). jit
mode moves with the interpreter (closure creation is shared runtime
machinery): cf0_sloppy ~104, cf2 ~111-119. The saved ~400ns/create on a
sloppy closure matches the per-define probe. A 30-line boilerplate
semantics battery (descriptor flags, own-names order, restricted
props, anonymous name, arrows, materialize-then-add, toString source)
is byte-identical between HEAD and the batched build.

Gates: workspace 32 suites green, clippy clean, both node-parity
batteries node-identical, all three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip — the two copyWithin
coerced-values detach fixtures hung once under batch load at ~12-15s
each and passed a re-run, the known load-wobble — annexB 1086/1086,
zero fail/crash). Two new crux tests: adoption matches sequential
defines field-for-field + guards.

Next ranked item: the per-iteration capture env (~2us on the head-let
closure row) — not started.

### PROBE: per-iteration capture env — elision already present; env alloc ~90-125ns/iter; the residual is general-path call cost (measured 2026-09-08)

The ranked per-iteration-capture item rested on a ~2us/create figure
measured pre-lazy-prototype/pre-batch. Re-decomposed on the current tree
(release, jl, HEAD d3c636f): a certified `for (let i...)` loop creating an
arrow that captures the head costs ~790ns/iter; the same loop with a
non-capturing arrow ~445ns; an arrow capturing a `var` head ~700ns
(thermal spread ~±10%). Interleaved A/B with the compiled
`Step::PerIteration` env creation no-op'd (ceiling probe, semantically
wrong) saved ~90-125ns/iter (~15%) on the capture row and NOTHING on the
no-capture loop — the compiler ALREADY elides the per-iteration env when
the body cannot capture the head. The env alloc premium over a var-capture
closure is essentially the whole per-iteration overhead.

The cf4-shape rows (an inner closure created inside a called IIFE each
iteration) initially looked like a ~2x create-context differential
(create inside an IIFE ~1025ns vs ~520ns in a top-level loop), but a
controlled matrix FALSIFIED that: identical creates in an inline loop, a
called helper, and inside an IIFE body all cost the same ~557ns. The
residual rows decompose into STACKED parts, not a context penalty: pure
closure create ~557ns/iter; a per-iteration certified function CALL
~160ns (h_nocaps 719 vs a_inline 557); a nested function whose body
references an enclosing frame var (env capture) ~80ns/call on a
called-once helper (h_caps 799 vs h_nocaps 719) and ~167ns/iter on an
IIFE re-created per iteration (i_caps 1407 vs i_nocaps 1240); an IIFE
create + call stacks to ~1240ns with a second inner create
(i_nocaps ≈ a_inline + IIFE-create + call). The base closure create
~557ns (register_function after the boilerplate batch removed ~400ns of
defines) is the remaining fish, not any create-context effect.

The per-iteration env is NOT the head-let/cf4 residual: those rows are
general-path function-call cost (the L3/jit-coverage mass the records
already flag), and a slim per-iteration env would harvest only the
~90-125ns/iter on true per-iteration-let capture rows. Deferred unless a
per-iteration-let-heavy corpus probe shows otherwise. Next thread: the
~557ns base create itself (register_function record/insert/pattern-store
composition probe).

### PROBE: the ~557ns base closure create is at its architectural floor — no single register_function component dominates (measured 2026-09-08)

Composition probes on the base create (a sloppy `function(){return 1}`
created 200k times in a register-run loop, ~557ns/create on HEAD
d3c636f): skipping `set_function_prototype` per create measured ~0 (the
intrinsic is realm-cached and the proto set is cheap); `shared_compiled_body`
is a per-site cache HIT after the first create (Cut 43 — cheap); the
`ecma_functions.insert` cannot be skipped to isolate its cost — the
create flow reads the just-inserted record back immediately (a skip
crashes with "Function body is not registered"), so the insert is
architecturally required mid-create. The boilerplate batch's own residual
is small (the batch itself removed ~415ns of defines).

The remaining ~500ns is spread across the per-closure machinery with no
dominant piece: the Function GC box (crux::Function embedding the fresh
JsObject + in_fields), the FULL per-closure EcmaFunction record (name
Option + Rc clones of params/body/compiled-body/ir + the outer_chain and
per_iteration_chain Vec fields) + its HashMap node, the boilerplate map
resolution (canonical_empty_map + 2-4 cached child lookups + adoption),
body_is_strict, realm/env wiring. Every counted candidate is either ~0 or
architecturally required per create — the base create is at the floor of
the "one full record + box per closure" design.

The architectural gap vs node's ~22ns/create is the THIN-CLOSURE model:
Cut 43/64 already share body/params/compiled per SITE, but each closure
still inserts a full standalone record; node's closure is a small
instance (env + id) over shared Code. A real slice would shrink the
per-instance record to an id + env + name instance pointing at a shared
site record — a wide refactor (every `agent.ecma_functions.get(id)`
consumer) gated on its own probe, not started.

### PROBE: the head-let/template-literal "jit-equal" rows are NOT a JIT-coverage gap — the bodies compile; the mass is runtime member+call machinery (measured 2026-09-08)

The two largest jl masses (head-let ~1.3-1.7s, template-literal ~1.2s)
were recorded as "jit-equal (general path)", implying the bodies never
reach the JIT (the L3 Sparkplug thread). Instrumented ground truth
falsifies that: `compile_body` scope traces show bench() AND the fixture
body __t262Body certify (scope Some) in every head-let variant, and no
Cranelift lowering bail fires (the compile trace stayed silent). The
fixture body DOES machine-compile — a payload-controlled A/B proves it: a
nested __t262Body with a 200-add arithmetic payload drops 2.85us ->
0.53us/iter under jit when alone (5.4x, body compiles), and adding the
fixture constructs leaves the arithmetic machine-fast while the ~10us/iter
fixture machinery (3 closure creates + per-iteration for-in-let envs +
property stores + the 16 member/call steps of the 8 assert.sameValue(
fns.a(), ...) lines) stays runtime-bound — jl 12.9us vs jit 10.3us per
iteration reconciles exactly as payload-compiled + fixture-runtime.

A for-in-let + closure body INLINE with heavy arithmetic also compiled
(machine arithmetic visible under the runtime create cost), and a
closure-in-a-plain-loop body compiled too — earlier "ratio ~1 means not
compiled" readings on create-dominated rows were wrong (closure creation
is runtime machinery shared by both modes, so machine loop control moves
those rows only a few percent regardless).

So the rows are NOT the L3 coverage mass: the head-let residual is the
runtime member+call machinery — ~1.4us per assert.sameValue(fns.a(), ...)
step (member load + argument closure call + call) — which is the L5
call-breadth territory (method-call inline paths for o.m()) already noted
in the plan, and the ~10us/iter fixture base is closure create +
per-iteration env + property-store machinery measured across this
session's other probes. No L3 code lands from this probe.

### PROBE: L5 method-call breadth is already warm — the head-let assert lines inherit the fresh-object machinery (measured 2026-09-08)

Follow-up on the entry above: decompose the assert.sameValue(fns.a(), 'a')
line to find the member-call overhead (the L5 o.m() slice). Isolated
200k rows (release, jl): a direct certified call ~206ns/call; a member
call on a STABLE object ~240ns/call (+34ns — the member-call path is
already near the direct-call floor); a full head-let assert line
(assert.sameValue + fns.a() arg over a freshly-built fns) ~470ns/line.
The real head-let shape (200k iters): building fns (for-in over a
3-key object creating 3 per-iteration closures + fresh objects) costs
~6.77us/iter; the 8 assert lines add ~3.77us (~0.47us/line). The lines
are slow because fns and its closures are FRESH every iteration — reads
and calls on fresh-object machinery, not the member-call path (a stable
receiver's method call already costs ~direct-call). No L5 member-call
slice lands; the earlier ~1.4us/call attribution was pre-batch and
stale. The head-let row's residual is the closure-create + per-iteration
+ fresh-object machinery this session's create probes already
characterized (the thin-closure record model is the one architectural
lever on it).

### PROBE: template-literal row — tagged-template machinery is NOT the mass; same per-create/capture/call cluster (measured 2026-09-08)

Decomposition of the other big jl mass (language/expressions/template-
literal/evaluation-order, ~1.2s at 100k iters = ~12us/iter) on 200k
rows (release, jl): an untagged template with 3 ${i++} substitutions
costs ~1.06us/iter (the substitutions are 3 post-increments + rope
concat — the string machinery, not the template form); the sameValue
compare adds ~160ns; a TAGGED template call with a trivial tag adds only
~0.22us over the untagged form (template objects are cached per site; a
direct tag call rides the certified path) — the tagged-template machinery
is NOT the mass. The full body (per-iteration tag closure create
capturing callCount + 4 assert.sameValue member calls + the tagged call)
is ~3.85us/iter; the extra ~2.6us over the parts is the closure create +
capture-context wiring + the member calls — the SAME per-create/capture/
cold-call cluster every closure-family row in this session decomposed to.
Jit moves nothing (runtime machinery, machine loop control moot). Both
top jl masses are now attributed to that single cluster; the thin-closure
record model remains the one architectural lever on it.

### SCOPED (not started): the thin-closure record refactor — consumer survey + gate probe (2026-09-08)

The one architectural lever the session's probes converge on: replace the
full per-closure `EcmaFunction` record (name + environment + Rc clones of
params/body/compiled/ir + outer_chain + per_iteration_chain Vec fields +
kind/strict flags, ~300B inserted into the `ecma_functions` HashMap per
create) with a small per-instance record (id + environment + name) over a
SHARED per-site record (Cut 43/64 already share body/params/compiled per
site; the site record would own the rest). Blast-radius survey:
`outer_chain` 53 sites, `per_iteration_chain` 44, `ecma_functions` 51
across function.rs/ir.rs/eval.rs/jit.rs — the lookup `fn(agent, id) ->
&EcmaFunction` feeds the IR compiler, call machinery, leaf/construct
verdicts, the JIT, and the env machinery, so every consumer dereferences
fields the split would move behind the site record. A full in-place
refactor is a dedicated multi-session effort, NOT a same-session landing.

Required gate probe BEFORE any refactor (the discipline the session
applied elsewhere): instrument register_function phase timings (Function
box alloc, record construction, HashMap insert, boilerplate batch,
pattern store) to confirm the record+insert share of the ~500ns base
create is the largest phase — the skip probes could not isolate the
insert (the create flow reads the record back mid-create), so the split's
ceiling is unmeasured. Then slice 1 would move the site-constant fields
(params/body/compiled/kind/strict/is_async/generator/method flags) into
an Rc site record and re-point the consumers that need them, keeping
per-closure name/environment on the instance; each slice gated on
workspace + clippy + the three sweeps.

### LANDED (2026-09-08, uncommitted): one-push per-iteration head bindings

The first contained slice of the per-iteration env cost (the probe above
measured the whole env alloc at ~90-125ns/iter): the per-iteration env
copy per head did create_mutable_binding (a duplicate scan over a fresh
empty env) + initialize_binding (a re-find), three RefCell borrows per
name. New `push_initialized_binding` on DeclarativeEnv/EnvRecord pushes
the fully-initialized Binding in one borrow — the env is freshly created,
so no duplicate/missing-binding error can arise — and all four per-
iteration creators use it (ir.rs `Step::PerIteration`/
`EnterPerIteration`, eval.rs `create_per_iteration_environment`, jit.rs
`per_iteration_env`). Interleaved release A/B (jl, 200k c_let_capture):
~148-152ms vs ~156-158ms head (~35ns/iter of the env cost, ~4-5% on the
row); c_let_call moves with it. Gates: workspace 32 suites green, clippy
clean, both parity batteries node-identical, all three release test262
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086). The remaining per-iteration env cost is the
DeclarativeEnv GC box itself (Handle::new of the 5-RefCell struct) — a
dedicated slim per-iteration env variant would harvest more but carries
the EnvRecord match-site churn; deferred.

### LANDED (2026-09-08, commit 1efedbd): the whole-simple object literal takes the `ObjectFast` batch path (Cut 72)

The "destructure" workload (832ms, 19.5x — the third-largest jl mass) is
object-literal creation: a fresh `{a,b,c:{d}}` + reads costs ~400ns/iter
with reads adding only ~23ns. Per-key literal defines route through
`create_data_property_key` → the vector-free `fresh_data_define`, which
still pays ~60-70ns/key (map borrow, find, transition HashMap lookup, two
generation bumps). Cut 71's register-run literal fusion was falsified
(2026-09-07), but the session's `adopt_vector_free_fields` (built for the
function-boilerplate batch) is the primitive a literal can use instead:
when the WHOLE literal is simple (all `Init` props with static,
non-`__proto__`, unique keys; no methods/accessors/spread/computed /
anonymous-function set_name; ≤ INLINE_FIELDS keys), the half-built object
never escapes, so the compiler emits the N values first then ONE
`Step::ObjectFast { names }`: the executor pops the values, creates the
object, forks the N-key shape from its empty map (cached child chain), and
adopts all fields in one map set + N field writes.

JIT: an unhandled step would bail whole bodies (literals are everywhere),
so the JIT expands `ObjectFast` into the existing ObjectBegin +
ObjectInitName helpers — values popped into locals (the FIRST attempt
defines in reverse pop order and broke enumeration — caught by the
`installed_jit_captured_for_in_head`/`for_in_key_loop` tests), then
defines forward in source order. Interpreter rows take the fused adopt;
JIT rows keep the per-key helpers (no regression: jit ~flat).

Interleaved release A/B vs HEAD (jl, 1M iters): nested-literal+reads
~276-291 vs ~353-368ms (-20-23%), nested create ~243-248 vs ~368-383
(-35%), flat 3-key ~188-210 vs ~268-274 (-23-30%). A 25-line literal
battery (fast shapes, key order incl. numeric/string keys, duplicate
keys, `__proto__` incl. the string form, computed, methods,
anonymous-function set_name, shorthand, 5-key boundary, spread,
accessors, mixed method-in-literal, side-effect evaluation order,
post-materialize growth) is node-identical across jit/jitless/gc-stress;
a new eval.rs regression test covers ordering + the boundary exclusions.
Gates: workspace 32 suites green, clippy clean, all three release test262
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086). Follow-up: fuse multi-key runs WITHIN mixed literals
(a run of batchable props between slow props).

### LANDED (2026-09-08, uncommitted): the ObjectFast batch path extends past INLINE_FIELDS keys (Cut 72 follow-up)

The >4-key follow-up: `fast_object_names` no longer caps at
`INLINE_FIELDS`, so a whole-simple literal of ANY length takes the batch
path (the gate is now just uniqueness/static-key/__proto__/set_name — no
length bound). The interpreter executor splits the work: the first
`min(n, INLINE_FIELDS)` keys fork the head shape from the empty map and
adopt vector-free in one map set, then each tail key defines via
`create_data_property_key` — the 5th define materializes the vector and
appends, the identical end-state the sequential path produced (and the
≤4 landing already proved for post-adopt growth), so the tail needs no
new machinery. The defensive sequential fallback is unchanged. The JIT
expansion always iterated every name, so JIT bodies were already
>4-capable and stay flat. Interleaved release A/B jl vs the capped parent
(min-of-3, recorded in-session): f6 (6-key churn) -22-35%, f8 -18-33%;
re-measured on the final tree, f6 churn lands ~126-150ms/200k iters jl.
Gates: workspace 32 suites green, clippy clean, a 23-case >4-key battery
(5/8-key values+order, growth/delete/readd after materialize,
integer-like keys interleaved, tail side-effect order, nested >4,
`__proto__: null`, descriptors, and the duplicate/computed/method/anon-fn/
spread/accessor exclusions at the boundary) node-identical across
jit/jitless/gc-stress, the objlit/vecfree/proto-lazy batteries
node-identical, all three release test262 sweeps at baseline (language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086). Next: fuse
multi-key runs WITHIN mixed literals (probed below — measured-closed
2026-09-08).

### Corpus re-baseline at HEAD 1625bf6 (measured 2026-09-08)

Four-mode corpus scan (37 workloads, bench.js, jl = the machinery
comparison since node-jit folds the create-churn loops to closed forms):
overall mean-jlGap 10.93, mismatches 0. The objects family is now the
LOWEST jl gap (mean 4.23x — compound_assign 2.4x, own_read 1.8x,
warm_store 1.9x) — the closure/literal landings closed it. The top
remaining jl masses are the language rows (both fresh-object/closure per
iteration): for-in head-let ~1455ms/100k (12.6x), template-literal
evaluation-order ~1156ms (21.4x), then construct_churn ~453ms (15.6x),
destructure ~337ms (6.5x), set_churn ~278ms (20.6x), generator_loop
~234ms (27.4x, jit ~equal — the resume machinery is interpreted). vs
the pre-landing jl capture (corpus_postlazy, 17:19, HEAD 72fc404 — the
five landings 5b67e98..1625bf6 in between), the closure/object rows move
broadly -8-22% (destructure -22%, direct_leaf -19%, for_in/try_catch
-15%, head-let -14%, recursive_fib -13%, generator -16%, typed_array
-21%, math_intrinsics -15%; single-run day-to-day variance is ±10-15%,
so directional only).

### PROBE: mixed-literal run fusion — no in-suite hot row; synthetic deltas are closure-create or define-tax bound (measured 2026-09-08)

Cut 72's queued follow-up (fuse runs of batchable keys WITHIN a literal
that also has method/accessor/computed/spread/anon-fn props) opened with
its probe. Corpus scan: NO per-iteration hot literal mixes fast+slow
props — every churn literal in-suite is whole-simple all-data
(destructure, spread_assign's `{x,y,z}`, method_call's one-time
`counter`) — so there is no in-suite row for a partial-batch slice to
move. Synthetic quantification (jl, 100k iters, min-of-3): v0 = 4-key
all-data literal (ObjectFast path) 32-33ms; the SAME keys with ONE slow
prop — v1 `m: function(){}` (method) 171-182ms; v2 `cb: function(){}`
(anon-fn set_name gate) 167-182ms; v3 `...src` 2-key spread 89-101ms;
v4 `[k]` computed 77-87ms. The v1/v2 +1.4us/iter is the per-create
FUNCTION machinery (closure create ~1us, not the literal defines); v3/v4
(+0.5-0.6us) mix the general define/spread tax with the lost data-run
batch. A partial batch would need a vector-append adopt primitive (the
object is mid-transition after a slow prop) whose only measured
beneficiary is synthetic. Disposition: measured-closed — do not build
the slice until a measured row needs it (reopen if a spread-churn or
callback-object-churn workload enters the corpus); the real per-iteration
mixed shapes are function-value-bound, which is the closure-create lever
below.

### PROBE: register_function phase split — the thin-closure ceiling is ~21%, and props_batch is the largest phase (measured 2026-09-08)

Executes the 6806 gate probe (temporary SLAG_RF_STATS instrumentation
in register_function, since removed). 800k closure creates (bare
create+call churn, jl): total ~410ns/create including ~9 Instant::now
reads (~380ns true). Phases: prelude 26ns (6.5%), record 28ns (6.9%),
func_new 79ns (19.4%), compiled 46ns (11.4%), insert 58ns (14.2%),
props_batch 112ns (27.4%), gen_proto 25ns (6.2%), set_proto 33ns
(8.0%). The thin-closure record split moves only record+insert ≈ 86ns ≈
21% of create — its realistic net (indirection added for consumers) is
~10-15%, NOT the create lever the closure family needed; the SCOPED
entry's ceiling is now measured and it is modest. The create is instead
~55% object-side (func_new box + props_batch boilerplate + set_proto),
with props_batch alone 27% — Cut 67's batched boilerplate still costs
112ns for a 3-key function shape. Bare create+call churn frame: slag jl
~850ns/create+call uncaptured / ~1.2us captured vs node-jl ~40ns (~21x).
The next create lever, if any, is props_batch (why 112ns for the batched
adopt) or the box/func_new side — not the record split.

### PROBE: head-let body decomposition — the 14.6us/body is diffuse across call/env/create machinery (measured 2026-09-08)

jl variants of the corpus body (ms/100k bodies, min-of-2; full body
~1400 matches the ~1455 corpus row): dropping the 9 assert.sameValue
calls ~380ms (27%); dropping the 3 fns.x() invocations ~170ms (12%);
non-capturing closures instead of head-binding capture ~330ms (24%); no
closures (plain strings) ~370ms; fresh objects + defines only ~146ms.
No single mechanism dominates: the residual is ~40% general function-
call machinery (asserts + method calls at ~200-500ns/call), ~24%
per-iteration capture env creation + captured reads, ~26% fresh-object /
for-in machinery. Node-jl runs the whole body in ~1.18us. Together with
the register_function split this closes the closure-family queue: the
language rows' residual is the aggregate interpreter gap on call/env/
create machinery (each op ~10-20x node-jl), not any single removable
piece — the standing L1c shapes + call-breadth programs are the levers,
not another micro-slice.

### LANDED (2026-09-08, uncommitted): the per-agent cached function boilerplate shape (Cut 73)

The register_function phase split (above) measured props_batch at 112ns
(27% of the ~410ns create) — Cut 67's "batched" boilerplate still
re-forked the `length`/`name`[/`caller`/`arguments`] shape on EVERY
create: `canonical_empty_map` + a 2-4-child `get_or_create_child` walk
per `register_function`. The (key, attrs) inputs are process constants, so
the final map is forked ONCE per agent (two slots indexed by `restricted`,
stored on the Agent and traced with it — a process-static GC handle would
be swept, the Handle=Gc liveness rule — and never invalidated); every
later create reads the cached handle and goes straight to the adopt.
Interleaved release A/B (jl, bare create+call churn, 200k, min-of-3, two
rounds of cache-on/off flips): t1 uncaptured ~160 vs ~149ms (~6-7%), t2
captured ~226-244 vs ~209-229ms (~5%); the tight before/after triples
rule out drift. props_batch's residual is the adopt itself (~40-50ns of
the former 112ns) — the fork is gone. Gates: workspace 32 suites green,
clippy clean, a 27-case function-boilerplate battery (name/length/desc
flags across sloppy/strict/arrow/method/async/generator, restricted
caller/arguments, props forked off the shared shape, no value bleed)
node-identical across jit/jitless/gc-stress (pre-existing node-vs-slag
divergences excluded: var-binding SetFunctionName, the restricted
caller/arguments OWN VALUE undefined-vs-null — no fixture pins it), the
objlit/vecfree/proto-lazy/objfast batteries node-identical, and the three
release test262 sweeps at baseline (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086). The remaining create cost is
object-side (func_new box + adopt + set_proto, ~55%) — the
object-representation program, not the record split.

### LANDED (2026-09-08, uncommitted): functions are born with their [[Prototype]] intrinsic (Cut 74)

The register_function phase split's `set_proto` phase (~29-31ns, 9%)
was a second `current_realm` + intrinsic lookup + `set_prototype_of` on
the freshly created function object — a generic call that runs the
kind/immutable/extensible checks and a CYCLE SCAN (walking
%Function.prototype% → %Object.prototype% → null) plus a generation
bump on an object nothing could have observed yet. New crux
`Function::new_with_prototype`: the creation site resolves the kind's
[[Prototype]] intrinsic (already realm-cached) from the realm it has in
hand and `ordinary_object_create(proto)` initializes the prototype Cell
at birth — the whole trailing `set_function_prototype` (and its generic
`set_prototype_of`) disappears for register_function and instantiate_arrow
alike. End state identical (same boilerplate map, same prototype Cell);
the only difference is the object's generation starts at 0 instead of 1
(nothing caches a pre-return function object, so the relative
validation is unaffected). Interleaved release A/B (jl, bare create+call
churn, 200k, min-of-3, born-with-proto vs create-then-set adjacent
builds): t1 ~139-144 vs ~145-160ms (~5%), t2 ~200-217 vs ~215-234ms
(~6%). Gates: workspace 32 suites green, clippy clean, a 20-case
function-prototype battery (chain identity + instanceof + chain reads
across ordinary/strict/method/arrow/async/generator/async-generator,
churn loops) plus the fn-boilerplate/objlit/vecfree/proto-lazy/objfast
batteries node-identical across jit/jitless/gc-stress, and the three
release test262 sweeps at baseline (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086).

### PROBE: construct_churn decomposition — the mass is the FRESH-RECEIVER chain read, not true defines (measured 2026-09-08)

The largest remaining property-row mass (construct_churn ~453ms jl, 15.6x,
jit equal) decomposed by isolated jl variants (500k, self-contained
bench-shaped loops, min-of-2/3; full body ~540ms here matches the row):
`new Empty()` bare construct ~174ns; `new Item(i)` (2 static this-stores)
~410ns vs the same data as an ObjectFast literal ~225ns; the corpus's
`.sum()` method call on the per-iteration-FRESH instance adds ~670ns
marginal (total ~1.08us), while the SAME `.sum()` call on a fixed
instance costs ~117ns. The 2 this.x stores are ~in-place (not the
define-class cost); true defines are NOT the story in this row.

The dominant term is the READ of a prototype property on a fresh
receiver. The chain-read cache (member_chain_cells) is keyed by receiver
IDENTITY (object id, name): a per-iteration-fresh object (every
construct, every fresh-literal method read) can never hit, so each read
falls to the full spec [[Get]] (context::get_property) then records a
cell the next fresh object never reuses. Isolated micros (jl, 500k,
min-of-3): a proto data prop read on a STABLE receiver ~70ns; the same
read on a per-iteration fresh receiver ~550-660ns (~8x); a fresh
literal's Object.prototype method read ~570ns over the literal create;
one warm member CALL o.hasOwnProperty('x') on a stable receiver ~340ns
vs ~830ns on a fresh receiver. So every fresh-object method/prototype
read pays ~500-570ns of full-Get that a SHAPE-keyed chain cell would
serve at the ~70ns warm cost: key the chain cache by the receiver's MAP
id (validated live; the map pins the own-key set, the recorded links pin
the chain — the construct_maps presize already gives every Item instance
the SAME map), exactly the standing "chain-member-read slice (a)" and the
L1c shape end-state's read application. Expected: construct_churn's
fresh-receiver read term collapses toward the warm cost; in-suite rows:
construct_churn, method-call-on-fresh shapes, spread/assign rows.

### LANDED (2026-09-08, uncommitted): the map-keyed chain read cell (Cut 75)

The decomposition's fix, implemented: a second chain-read table
(`member_chain_map_cells`, same scalar MemberChainCell layout) keyed by
(receiver MAP id, name) instead of receiver identity, so every fresh
object on a shared map hits one cell. Recorded in `resolve_chain_cell`
alongside the identity cell when the receiver carries a map (the found
link + slot + links are identical to the identity record). The HIT
re-validates per receiver: the current map is still the recorded one AND
`!has_own_property_atom(name)` — authoritative for both the deferred
map-field state and the materialized vector state, so a vector-only own
`name` (a non-enumerable defineProperty that keeps the map) or an
accessor-converted / deleted-to-dictionary receiver can never be masked;
then the shared `chain_cell_read` tail (extracted from the identity
probe) walks the recorded links' (id, generation) and re-reads the found
property LIVE, exactly as before. The empty-vector guard from the first
draft was WRONG (diagnosed with temporary instrumentation: hot
construct/literal objects are aligned dual-store — vlen == mdesc — never
vector-empty), and the map-descriptor-membership test alone would miss
vector-only owns; `has_own_property_atom` covers both. Soundness edges in
a 16-case battery: non-enumerable vector-only own shadow, accessor
conversion, delete-to-dictionary, setPrototypeOf replacement, proto
growth, live proto value updates, proto accessors, map isolation,
arrays (shared canonical empty map), function receivers (shared
boilerplate map), 2-link chains, instances that grow off the map,
churn — node-identical across jit/jitless/gc-stress, plus the
objlit/vecfree/proto-lazy/objfast/fn batteries. Interleaved flip A/B
(jl, min-of-3, probe on/off adjacent builds): fresh chain READ ~362 vs
~179ms (~51%), fresh chain CALL ~423 vs ~240ms (~44%), construct_churn
full body ~539-542 vs ~235-254ms (~54%, the corpus row ~330ms jl in the
noisy capture moved ~-19%); stable-receiver and pure-construct controls
flat. Gates: workspace 32 suites green (re-run on the final source),
clippy clean, the three release test262 sweeps at baseline (language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086). The fresh-
receiver prototype-read gap — the dominant term in every method-call-on-
fresh-object row — is closed; it was the read application of the L1c
shape end-state and needed no storage migration (the map was already the
shared shape).

### PROBE: the compiled chain path after Cut 75 — the JIT inherits the map-keyed cell through its slow call; an inline probe is unsound or has no hot case (measured 2026-09-08)

The queued follow-up (emit the chain shape-key inline in machine code so
a compiled fresh-receiver chain read skips the slow helper) opened with
its probe. Corpus rows post-Cut-75 (single runs): construct_churn jit
176ms vs jl 215 (PRE-Cut-75 it was jit 467 > jl 453 — the JIT now LEADS
jl and both dropped ~55-60%); method_call jit 123 vs jl 175; proto_read
jit 91 vs jl 147; closure_capture jit 91 vs jl 149. The compiled
GetMemberName inline probe (Slice 1) covers only OWN reads (value/map
cells at fixed offsets); a chain miss calls the shared get_member_name,
which now hits the map-keyed cell — so the compiled path already rides
Cut 75 and beats the interpreter. The residual compiled fresh-receiver
method-call cost over a warm receiver (~60ns/read on construct_churn's
.sum(), V10 jit ~425ns/iter = construct+reads ~290 + fresh call ~135 vs
warm ~76) is call-shaped, and a compiled INLINE chain probe cannot be
sound for materialized receivers: own-absence needs the vector scan or
the deferred map-find (has_own_property_atom) — not inlineable — and a
deferred-only probe (map pins own-absence there) has no hot beneficiary
(measured earlier: hot construct/literal receivers are materialized
vlen==mdesc; deferred receivers in practice are function boilerplate,
whose chain reads f.apply/f.bind sit on STABLE functions already served
by the identity cell). Disposition: measured-closed — do not emit a
compiled chain inline probe; the compiled path's correct shape is the
slow-call map-chain hit it now has. The remaining construct_churn mass
is the CONSTRUCT overhead itself (~140-180ns over the ObjectFast literal
for the same fields) plus the fresh-object method CALL — next lever.

### LANDED (2026-09-08, uncommitted): the constructor this-write pattern collector was dead — `this` is ExprKind::This, not an Ident (Cut 76)

The construct-overhead decomposition (the queued next lever) found the
root cause before any deeper slicing. Instrumented probes: every `new`
in the rows takes the certified-construct LEAF path (step_construct
leaf=100%, general=0), and `new Empty()` steady ~174-210ns is the
this-object alloc + leaf setup; but the 2 `this.x`/`this.y` constructor
stores cost ~110-160ns EACH vs the ~55ns warm in-place store, and the
B5.4 presize branch in construct_this_object NEVER fired (5M constructs,
count=0) even though store_construct_patterns was recording. Root cause:
the this-write collector in compile_member_assign checked
`ExprKind::Ident` whose identifier is the `this` keyword — but `this`
parses to `ExprKind::This`, a DISTINCT variant, so the pattern cache
(this_writes, and with it the B5.4 constructor presize AND the Cut 35
slice 30 member-store pre-warm) was silently DEAD since the parser
introduced the This variant. Fixed to match ExprKind::This. Interleaved
flip A/B (jl, construct_churn full body, min-of-3, adjacent builds): fix
ON ~236-248ms vs OFF ~256-259ms (~5-8%). Gates: workspace 32 suites
green, clippy clean, a 15-case constructor battery (basic, conditional
stores read absent / fall to the prototype, store order, compound and
computed this-writes, overwrite, real-class subclass after super(), lazy
prototype interplay, read-then-write, defineProperty-on-this,
delete-this, churn, setter-less proto accessor making the sloppy store a
silent no-op) node-identical across jit/jitless/gc-stress, all seven
prior batteries node-identical, and the three release test262 sweeps at
baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB
1086/1086). The presize's full intended win is NOT yet realized: a store
to a map-described-but-unset presize field still runs the full define
(field write + vector Property push + generation bump) because the
vector-free branch of define_fresh requires props_deferred and
ordinary_object_create_with_map does not set it — so the "fill the
presized slot without the push" fast path does not exist yet. Next
lever: that store-path slice (recognize a presized hole and fill it
without the dual-store push), which the vector-free machinery is the
precedent for.

### LANDED (2026-09-08): presize objects enter the vector-free state on their first define — the presized-hole store fill (Cut 76's queued next lever)

Cut 76's presize made the map describe the constructor's fields up front,
but a store to a map-described-but-unset field still ran the full define
(field write + vector Property push + generation bump): the vector-free
branch of define_fresh required props_deferred, and a presize object's map
is non-empty from birth, so it never entered. This slice removes the
dual-store push: the first define on a vector-empty object now enters the
deferred state whether the map is empty (the canonical path) or a
constructor-presize map (B5.4), so a fill of a described hole — or a
fresh-key transition below INLINE_FIELDS — is a field write with NO
authoritative-vector push. The object leaves the constructor vector-free;
reads of written fields serve from in_fields/member cells, unwritten
presize fields stay holes (absent), and the map + fields remain the only
store until a structural consumer (enumeration, delete, redefine, slot
resolution) materializes.

Hole-aware machinery (presize descriptors can be unwritten, unlike the
canonical-empty deferred state where every descriptor was created by a
define that wrote its field):
- `has_own_property_atom`'s deferred arm now tests field-written (map
  describes AND the field holds a real value), not just map membership —
  the Cut-75 chain-cell validation gets the true own-answer for a
  described-but-unwritten key.
- `materialize_properties` skips unwritten descriptors when rebuilding the
  vector (a presize hole contributes no own property); the debug_assert
  that no field is ever unset is gone.
- Exactness gate in `define_fresh`: the deferred rebuild emits written
  descriptors in ORDINAL order, which equals creation order only while
  writes ascend the descriptor ordinals, so a hole fill that lands BELOW an
  already-written descriptor (an out-of-order execution — a nested
  this-store in an RHS, a branch that stores later fields first)
  materializes first (the rebuild preserves creation order so far) and the
  fill appends through the standard path in execution order. Conditional
  skips (a mid-chain hole) stay vector-free safely: the rebuild skips them.

Measured (release, jl; the pre-change binary is the clean Cut-76 HEAD, the
same construct_store_probe file run ~40 min apart with the literal
controls flat — C0/C1 ~within the ~7% drift band, so the multi-field rows
are the signal): 500k constructs + reads, C2 (2 fields) 203-215 -> 179-186
ms (-13%), C3 335-363 -> 236-242 (-32%), C4 445-447 -> 291-303 (-33%),
C0 (no-field control) 114-118 -> 106-110, C1 ~130 -> ~125, L1/L2 literal
controls 79-80/111-115 -> 79-80/109-110. construct_decomp4 (500k, jl):
V11 (2-field + own reads) 195 -> 176-177 (-10%), V10-full (2-field + fresh
.sum()) 250 -> 229-230 (-8%). Jit rows on the same probes are flat
(pre-existing jit-vs-jl divergence on construct rows, below). Corpus
re-baseline (37 workloads, --corpus, results identical to the
corpus_cut75b_jl.tsv capture at HEAD 36d6260): every row improved
0.63-0.99 (machine ~10-15% cooler); construct_churn 275.6 -> 171-179 ms
stably (3 runs; bundles Cut 76's ~5-8% + this slice + drift); the top
movers are the fresh-object/construct family (construct_churn 0.63,
direct_leaf 0.72, spread_assign 0.72, generator_loop 0.73, try_catch 0.75,
for_in 0.76, template-literal 0.78, destructure 0.78), controls flat.

Gates: crux 227 tests green (the old presized_object_never_enters test
rewritten to presized_object_enters_vector_free_state_on_first_define,
plus presize_hole_stays_absent_and_materialize_skips_it and
presize_fresh_keys_transition_then_overflow_materializes_and_appends),
workspace 32 suites green, clippy `-D warnings` clean, eleven node-parity
batteries node-identical across jit/jitless/--gc-stress — the constructor
battery extended with 12 new cases exercising the changed surface
(out-of-order execution via a nested RHS this-store keeps creation order,
hasOwnProperty/`in`/getOwnPropertyDescriptor on a skipped hole vs a
filled field, conditional skip then a post-construct fill, an interleaved
fresh key between presize fields, delete/re-add of a presized field, a 5th
key past a 4-field presize, for-in over deferred presize objects,
Object.assign growth, post-construct overwrite, non-writable sloppy no-op,
non-writable strict throw) — and the three release test262 sweeps at
baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang), re-run on the final tree after the follow-up below.

Follow-ups (not this slice): the jit construct-row divergence — jit is
~2.7-5x SLOWER than jl on construct-heavy rows at HEAD (corpus
construct_churn jit 431 vs jl 173 in the re-baseline; scratch V10-full jit
~1173 vs jl ~229) — pre-existing (flat across this slice's before/after
probes) and uncharacterized: the compiled leaf-constructor body's member
stores route through the compiled set_member_slot shape gate, which on a
fresh presize-deferred this (empty vector) falls to the full store helper
per store, so the compiled path does not inherit the deferred fill. A
compiled-store arm for a map-pinned deferred field is the jit-side of this
same lever.

### Follow-up LANDED in the same change: the deferred in-place field-write arm in the runtime's warm-store fallback

The deferred state this slice introduces left one regression vector: a
post-construct warm store on a fresh presize-deferred object (a
construct-then-store-per-iteration loop) fell through the empty-vector
store-cell bail to fast_fresh_store's existing check (field written → not
fresh) and then the FULL [[Set]], which materialized the object — ~one
full [[Set]] per object where the pre-deferred dual-store path had served
the same store via warm_store_direct_put. Measured (release, jl, 500k
construct+2-post-construct-stores, construct_poststore_probe): V1 325-340
ms vs the store-in-body twin V2 ~175. Fix: warm_store_fallback's
empty-vector branch now tries `JsObject::deferred_field_write` (new crux
helper: the map describes the key, its field is written, and the
descriptor is WRITABLE — length/caller/arguments and accessor-converted
descriptors decline, so the full [[Set]] still enforces the writability
outcome), which mirrors the value into the field under the L1c no-bump
discipline (no vector slot exists, so no store cell is recorded — each
store re-probes) and fronts the read value cell. V1 -> 196-218 ms
(~= V2's 175-184). Batteries extended with post-overwrite,
non-writable-sloppy-no-op, and non-writable-strict-throw cases,
node-identical across jit/jitless/--gc-stress; workspace + clippy green,
and the three release sweeps re-run at baseline on the final tree.



### LANDED (2026-09-08, uncommitted): the compiled constructor store serves the presized-hole fill — jit construct rows flip from ~4x slower than jl to ~1.3x faster (the jit-side of the presize slice)

The presize-vector-free landing made jl construct rows fast, but exposed a
jit divergence the probes then pinned: jit was ~2.7-5x SLOWER than jl on
construct-heavy rows at HEAD (scratch V10-full jit ~860-1170 vs jl
~215-230ms/500k; corpus construct_churn jit ~430 vs jl ~175). Decomposition
(construct_jit_split, jit, 500k): a bare `new Empty()` is jit-fine
(~65ms), but each constructor `this.x =` store added ~350-370ns marginal
under jit vs ~60ns under jl — every compiled constructor-body store went
through the machine value-cell probe + shape gate (a HIT: the previous
iteration's define records the (map id, name) read cell) to the narrow
`set_member_slot` helper, whose `write_data_property` scans the property
VECTOR — empty on the now-deferred presize `this` — and declined, so the
helper fell to the FULL [[Set]] per store (its fallback was a straight
put_value, skipping the interpreter's lean `fast_fresh_store`). A
fresh object's first store of each key is a map-described HOLE fill, never
an in-place update, so every iteration paid the full path.

Fix (two small changes): `write_data_property` (crux) first tries the
vector-free in-place field write (`deferred_field_write`: described,
written, writable — the deferred arm from the interpreter slice) so a
second store to the same deferred key is a field write; and
`set_member_slot`'s decline path now runs the interpreter's `assign_member`
(warm-store probe + lean fresh define + the same full [[Set]] the old
fallback used) instead of a straight put_value, so the common constructor
fill — a shape-gated map-described hole on a clean chain — lands on
`fast_fresh_store`'s deferred fill. Exactness: `assign_member` is the
interpreter's own store dispatch; every old put_value outcome is preserved
(the final [[Set]] is identical), and the fast paths in front are exact.

Interleaved flip A/B (release, jit, whole corpus, min-of-2 adjacent
new/old runs, env-gated fallback then removed): construct_churn 480 -> 162
ms (~3x); every other row 0.97-1.05 (noise) — object_keys 0.94 (noise),
for_in/many_objects_read/concat_loop ~1.04-1.05 (noise). Scratch rows:
V6-one jit ~230 -> ~90, V7-two ~410 -> ~120, V11 ~430 -> ~125, V10-full
~860-1170 -> ~185-190 — jit now LEADS jl on every construct row (jl V7
~175, V10 ~225). Gates: workspace 32 suites green, clippy `-D warnings`
clean, the seven presize/chain/vecfree/objfast/fn/proto batteries
node-identical across jit/jitless/--gc-stress, and the three release
test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155
skip, annexB 1086/1086) re-run on the final tree. (The earlier whole-corpus
jit capture showing every row up 1.1-1.6x was machine heat — pure
machine-loop rows my change cannot touch moved identically; the
interleaved flip is the reliable read.)

Next (open): the compiled path still pays one set_member_slot/assign_member
FFI round-trip per constructor fill (the shape gate + narrow-write helper);
an inline machine fill for a shape-gated map-described hole (map-id compare
+ in_fields write) would close the remaining gap to the ~55ns warm floor,
and the construct rows' residual is the ~150ns bare-construct overhead
(new Empty vs a literal) plus fresh-object method-call machinery.


### LANDED (2026-09-09, uncommitted): stabilize the --jit-bench protocol for allocation-heavy rows

The suite's `string concat` row (s += 'x' x100k, a 100k-node rope build per
call) reported wildly unstable jit/interp ratios on a clean binary: 0.41-2.86
across consecutive runs (spread ~2.5), where every other row was stable at
<=0.11. Root cause: the row is GC-bound and the mark-sweep collector's rare,
LARGE pauses (safe-point gated, not per-allocation) land inside unlucky
single calls and double them; `bench_once` timed only 3 single calls after 2
warmups (min), so whichever column caught a pause lost. A robust 30-call
in-process measurement showed the true steady state is jit FASTER (jit ~2-4ms
vs jl ~4-6ms per build); the suite's single-call protocol was bimodal, not
the engine. (A false alarm along the way: an env-var counter added inside
the concat helper for call counting inflated jit ~10ms — ~100ns/call x 100k
— removed; the tree is clean.)

Fixes in `bench_once` + `run_jit_benchmarks` (crates/cli/src/main.rs):
1. Timed samples are now batched to a 100ms floor (`SAMPLE_FLOOR`, the
   corpus runner's existing `batch_target` mechanism) so each sample spans
   ~25+ calls and the row's own GC cadence is amortized into every sample.
   Rows whose single call clears the floor (reps = 1, e.g. the buildString
   rows) are unchanged. The same row now reads ~0.67-0.95 across runs (jit
   consistently ahead, no parity/slower outliers) and the suite's worst
   spread drops from ~2.5 to ~0.3. Suite cost: ~2.5s -> ~9-11s.
2. A forced collection right after `Context::new`: the crux heap is
   thread-global and shared across the suite's rows, so the previous row's
   garbage would otherwise set the allocation-heavy row's first-collection
   point. Every row now starts from a bounded heap.
3. The differential value is captured from a call after the identical
   WARMUP prefix instead of the last timed call: batching sizes `reps` from
   each column's probe, so a STATEFUL bench (compound assign mutates its
   argument object) would otherwise sit at a different state per column and
   false-flag `MISMATCH` (observed once at 100ms batching).

Verified: clippy `-D warnings` clean, workspace 32 suites green, and the
corpus sweep's 37 workload results are byte-identical to the pre-change
capture (bench_once is shared; the corpus keeps its own 20ms floor). The
amortized values for the non-allocating rows are unchanged (mean = min
there); the allocation rows now report their amortized steady per-call,
which is the representative number. Follow-ups if ever needed: a per-row
variance probe to batch only noisy rows (saves the suite time), or a
host-forced collection between timed samples (measured neutral-to-worse:
the fresh collection threshold makes every batch pay an extra early GC).


### LANDED (2026-09-09, uncommitted): the compiled vector-free constructor fill — a shape-gated machine `in_fields` write with a chain-clean gate

The e3ffc1e landing left every compiled constructor-body store paying one
`set_member_slot`/`assign_member` FFI round trip per fill (its decline
path runs the interpreter's chain-verified `fast_fresh_store`); the
recorded next slice was an inline machine fill for a shape-gated
map-described hole. That is what landed, in four coordinated pieces:

1. **Presize receivers are born vector-free** (function.rs
   `construct_this_object` + crux `JsObject::enter_vector_free`): a
   constructor-presize map pre-describes the pattern keys as unwritten
   holes, which is exactly the vector-free state (holes read absent through
   `field_written`), so the receiver now enters it at birth instead of on
   its first body-store fill. That is what lets the compiled fill serve the
   FIRST store of each fresh receiver (previously only the second+ fill on
   an already-deferred receiver could). `props_deferred` became `pub` for
   the JIT's `offset_of!` gate.
2. **The machine fill** (jit/compiler.rs `emit_deferred_hole_fill`, shared
   by the register `StoreMemberName*` path and — the step path that
   constructor bodies actually take — `Step::AssignMemberName`, which had
   its OWN shape gate to `set_member_slot` that never routed through
   `emit_validated_member_store`): on a shape-gate hit (the shared (map id,
   name) cells matched the receiver's live map), a vector-free receiver
   whose field is an unwritten hole with every higher field also a hole
   (fills ascend the ordinals; an out-of-order fill must materialize to
   preserve creation order) writes `in_fields[slot]` in machine code, bumps
   the generation, and refreshes the (id, name) value cell — exactly the
   interpreter's `define_fresh` described-key fill. Any doubt (written
   field, slot at/above `INLINE_FIELDS`, non-deferred receiver) falls to
   the unchanged `set_member_slot` write.
3. **The chain-clean gate** (the piece that made the naive fill unsound):
   a hole is spec-absent, so [[Set]] must consult the prototype chain — a
   mid-run accessor conversion / setter install on the prototype must
   intercept the next fill. A pure machine gate cannot walk the chain, so
   `MemberMapCell` gained `clean`/`proto_id`/`proto_gen`: the interpreter's
   chain-verified `fast_fresh_store` records `clean` 1 with the receiver's
   direct prototype (id, generation); read-records (`member_cell_get_map`)
   record `clean` 0 (an own property shadows the chain, so a read never
   validates it). The machine gate re-checks the fill-time receiver's live
   direct prototype (identity + generation) against that record — a
   prototype mutation (defineProperty, setPrototypeOf) bumps or changes it
   and declines the fill to the exact helper. A receiver with NO prototype
   (empty chain) passes unconditionally.
4. **Born-deferred + chain gate confirmed by a targeted probe that the
   naive inline fill FAILED**: warming the compiled fill then installing a
   setter on `C.prototype` bypassed the setter (jit created an own x;
   jitless ran the setter) until the gate landed; now byte-identical across
   jit/jitless/--gc-stress including the getter-only-inherited strict throw.
   A new `installed_jit` e2e test pins the mid-run-setter shape.

Interleaved A/B (release, jit, scratch/construct_jit_split.js, 500k iters;
pre = HEAD b17be2f from this session's first runs, post = this tree): S1
(1 store) ~102-105 -> ~78-82ms, S2 (2 stores) ~127-141 -> ~89-94ms, R1
(store+read) ~86-94 -> ~70-71ms; S0 (bare construct) unchanged ~65-77ms.
The `set_member_slot` FFI declined 3.9M/3.9M store calls pre-change (every
constructor fill) and ~0 post-change on these rows (verified with a
temporary counter, removed). The remaining per-store marginal (~26ns) is
close to the machine-write floor; the rows' mass is now the ~140ns bare
construct, not the fill.

Known residual (documented, not exercised by any fixture): the gate
validates only the receiver's DIRECT prototype. A mutation on a deeper
chain link (e.g. Object.prototype) mid-run after warm would not bump the
direct prototype's generation and the inline fill would not see it; the
interpreter's chain walk validates every link. Closing it needs a machine
walk or a chain fingerprint in the map cell — deferred until a probe shows
a real row (or fixture) needs it.

Gates: clippy `-D warnings` clean; `cargo test --workspace` green (32
suites; the new jit e2e passes); the presize/chain/vecfree/objfast/fn/
proto batteries + the new fill-chain probe byte-identical across
jit/jitless/--gc-stress; the three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang) re-run on the final tree.


### PROBE: the bare-construct residual is the un-inlined FFI construct cascade — architectural, not a bounded slice (measured 2026-09-09, no code)

The next-slice note above names the ~140ns bare `new C()` (S0) as the
rows' remaining mass. With the fill FFI gone, a fresh probe isolates the
construct machinery cleanly for the first time. scratch/construct_bare_probe.js
(500k iters, release, min-of-3, same machine):

| row (ns/iter) | jit | jitless |
|---|---|---|
| M0 `{}` literal + keep-alive | ~80 | ~70 |
| M1 `new C()` empty body, no args | ~128 | ~172 |
| M2 `new C(i)` one arg bound | ~126 | ~168 |
| M3 `C()` plain call of the same empty leaf | ~22 | ~82 |
| M4 `new C(i)` body `return x` | ~128-144 | ~174 |

construct minus literal is ~45ns (jit) / ~15ns (jl) of machinery ABOVE
object creation; construct minus a plain leaf CALL of the same empty body
is ~105ns (jit) / ~90ns (jl). The jit call floor (M3 ~22ns) proves the
machine leaf path is cheap: `emit_call`'s leaf-inline (the leaf-call site
cache + `emit_leaf_call_tail` — the callee's compiled body runs as a plain
machine call on the CALLER's frame region, no ctx rebuild). A CONSTRUCT
has no analog: `Step::Construct` emits one FFI (`Helper::Construct`) that
re-runs `step_construct` (boundary pop + `can_inline_leaf` + realm check +
`agent.leaf_lookup` HashMap probe + `construct_inline` verdict + args Cow
+ env logic) then `run_leaf_construct` (construct_this_object + args
re-materialization) then `run_jit_leaf` (the full per-run JitCallContext
rebuild: ~30 fields, the leaf-cache array, the frame-buffer fill loop,
global resolution, clean-chain walk, jit_roots push/pop) — for a body
that does nothing. The interpreter side is similar (`run_leaf_body` is
~90ns of construct glue over `run_inline_leaf`-class call machinery).

This is the third probe to call the construct overhead diffuse (L6382
"~35ns, not addressed"; the pre-fill decompositions), and it now pins the
CAUSE: the per-construct cost is the FFI + eligibility-re-derivation +
per-run ctx rebuild cascade, not any single piece. Bounded micro-trims
(step_construct's prelude, the redundant arg re-materialization, the
oracle read in construct_this_object) are each ~5-15ns and would measure
within noise on real rows.

Scoped (NOT started — needs its own focused turn + full gate): a
machine-inline leaf CONSTRUCT mirroring `emit_call`'s leaf path — a
construct-site cache validating (callee, `construct_inline`, leaf state),
an FFI `construct_this_object` (receiver creation), the leaf body run via
`emit_leaf_call_tail`-class machinery with `this` = the fresh receiver in
its frame slot, and the base-constructor return rule (object/function
wins, else `this`) applied in machine code. Two wrinkles vs the call path:
`Step::Construct` is the VECTOR form (the argc is in `Vm::args`, not the
step) so the site needs its static argc, and the `new.target`-free leaf
subset is the only body shape runnable this way (leaf bodies are already
`NewTarget`-excluded). Estimated ~200-350 lines across the four-file
helper mirror + compiler + a new runtime entry.

CORRECTED by the follow-up probe below: the intermediate "fused static-argc
ConstructFast step" sub-slice is FALSIFIED — M1 (`new C()`, no args: the
step stream is ArgsBase + Construct, no ArgsPush) measures the SAME as M2
(`new C(i)`, one extra ArgsPush FFI) at ~128ns jit / ~168ns jl. So the
ArgsBase/ArgsPush vector-form FFI chain is NOT the ~80ns mass over the
22ns machine-inline call floor; that mass is the CONSTRUCT CORE shared by
both: `step_construct`'s eligibility prelude (boundary pop, `can_inline_leaf`,
realm check, `agent.leaf_lookup` HashMap probe, `construct_inline`, args
`Cow`/truncate, `ir` clone) + `construct_this_object` + `run_jit_leaf`'s
full per-construct JitCallContext rebuild — the ctx rebuild leaf CALLS
only avoid because `emit_call` machine-inlines them on the caller's ctx.
A static-argc step would save one FFI at best (~15-25ns, within row
noise) and leaves the core untouched. The only real fix remains the
machine-inline leaf construct (above), which is architectural: the
receiver allocation + vector-form args keep the leaf off the caller's
frame model, so it needs the site cache + create-this FFI + return rule.
Deferred as a dedicated program, not a tail slice; the construct rows'
remaining mass is accepted for now.


### LANDED (2026-09-09, uncommitted): the shared-ctx construct leaf — the machine-inline construct's ctx-rebuild half

Rather than the full machine-inline construct (receiver allocation + the
vector-form args keep the leaf off the caller's frame model), this landing
removes the other half of the recorded core cost: `run_jit_leaf`'s full
per-construct JitCallContext/private-buffer/root rebuild, by running an
environment-free certified construct LEAF directly on the CALLER's live ctx
with its frame carved from the caller's working buffer just above the
caller's current `sp` — the exact model `emit_call`'s machine leaf-inline
already uses for leaf CALLS. The compiled `Step::Construct` now passes the
machine's current `sp` to the `construct` helper (a 3-arg signature through
the four-file mirror); `step_construct_shared`/`run_leaf_construct`
(`ir.rs`) attempt the carve first and fall back to the materialize-args +
`run_jit_leaf` path on any doubt (an env leaf, the body not compiled —
`lookup_info` null — the depth cap, or no room above `sp`). The carve is
inside the caller's already-rooted buffer, so no re-rooting; the leaf's own
helpers keep the VM stack balanced and the caller's leaf-cache slots/epoch
are only ever re-validated on a collision (the emit_call gate is exact).
The frame fill mirrors `run_jit_leaf` (this slot when present — a leaf may
have none, an empty body never references `this` and the receiver is
returned regardless — params from the construct args, TDZ/var slots); the
base-constructor return rule applies to the leaf's result.

Interleaved A/B (release, jit, scratch/construct_bare_probe.js, 500k, same
session): M1 `new C()` ~61-68 -> ~56-58ms (~11%), M2 `new C(i)` ~63-71 ->
~51-58ms (~15-20%), M4 (`return x` body) ~65-72 -> ~53ms (~20%); the
literal floor M0 and the leaf-call floor M3 (~10ms) are unchanged. So the
shared path saves ~15-25ns/construct — the ctx rebuild was only ~20ns of
the ~100ns core (the rest is the eligibility prelude + `construct_this_object`
+ the FFI, as the probes concluded). jitless is unchanged by construction
(the interpreter passes no shared ctx).

Correctness: 179 jit e2e tests green (incl. the existing construct-leaf /
construct-error e2e, which now exercise the shared carve, plus a new
`installed_jit_shared_ctx_construct_leaf_recovers_after_a_throw` pinning
a constructor leaf whose body CALLS a helper and throws mid-loop); a
10-shape stress probe (ctor body calls, throws caught, nested constructs,
closures, 6 args, object returns, env-capture fallback, deep chains,
throw-then-continue) byte-identical across jit/jitless/--gc-stress; the
presize/chain/vecfree/objfast/fn/proto batteries byte-identical across
modes. clippy `-D warnings` clean; workspace 32 suites green; three release
test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155
skip, annexB 1086/1086, zero fail/crash/hang).

Remaining construct mass (journaled, not started): the eligibility prelude
+ `construct_this_object` + the FFI — a machine site-cache + create-this
FFI would need the receiver allocation moved out of the core to cut further.

### LANDED (2026-09-09): tagged templates certify and JIT — the compiled `TaggedTemplate` step runs the tag through a machine slow-path helper (Cut 78)

The corpus #2 row (`template-literal/evaluation-order`, ~1.1-1.3s jit in the
row history) was a JIT-coverage gap mislabeled "diffuse": `FastScopeScan`
rejected `ExprKind::TaggedTemplate`, so a body whose hot call is a tagged
template never certified and jit == jitless on it (~1µs+ per tagged call vs
~60-70ns for an equivalent plain call). The fix has two halves:

1. **Certification** (`FastScopeScan::expr`, ir.rs): a `TaggedTemplate`
   certifies when its tag and every substitution expression scan — the tag
   is an ordinary callee expression and the substitution expressions are
   ordinary value expressions, so nothing about the step needs the env
   machinery. A TAIL-position tagged template still bails at JIT emit
   (`TailTaggedTemplate` has no emit arm; the compiler's catch-all returns
   `Unsupported`, the body caches as non-compilable and runs interpreted).
2. **JIT lowering** (compiler.rs emit arm next to `Construct`): the machine
   pushed `[this, tag]` (a plain tag's `this` is undefined, a member tag's
   is its base), the substitutions sit in `Vm::args` above an `ArgsBase`
   boundary — so the compiled step pops both, passes the step index, and
   `call_slow`s the new `tagged_template` FFI helper (jit.rs), which mirrors
   the interpreter handler exactly: pop the boundary (SyntaxError when
   absent), `StressSuppress` the unrooted-substitutions window, `split_off`,
   and run `ir::tagged_template` (the general call machinery). Helper
   registered through the four-file mirror (`Helper::TaggedTemplate`, the
   `JitSlowPaths` entry, both test tables). `max_stack_usage` nets -1.

The certification half is visible in BOTH engines (a certified body runs
frame-slot/register-lowered even interpreted). Interleaved A/B on
scratch/tagged_probe.js T1 (`tag`x${i}y`` per iteration, 200k): BEFORE
jit ~322-363ms / jl ~325-344ms (no certification at all); AFTER jit
155-164ms / jl 171-182ms — the ~2x is the certification verdict, and the
compiled step adds another ~10% over the interpreted certified path. The
corpus `evaluation-order` row (full-corpus bench, 37 workloads): jit
583.6ms / jl 628.7ms vs ~1.1-1.3s in the row history — the fixture is
closure/assert-dominated, so the row shows the certification win, not the
~2x of a pure tagged loop.

Correctness: 179 jit e2e green; a 8-edge probe (member-tag receiver
identity, throwing tag propagation, non-callable/undefined tag TypeError,
left-to-right substitution order, primitive tag result, template-object
identity + frozen-ness, raw props) byte-identical across jit/jitless/
--gc-stress; clippy `-D warnings` clean; workspace 32 suites green; three
release test262 sweeps at baseline (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity
37/37 ok, 0 mismatches.

### LANDED (2026-09-09): registered builtins call their handler directly from `fast_call_core` — the keyed-intrinsic call tax removed (Cut 79)

The map/set-churn corpus rows (~210-260ms jl, jit ≈ jl — the top builtins
mass) decomposed to ~220ns/op (jit) / ~310ns/op (jl) per Map method call
with NO single further cost center: a pre-bound intrinsic call (X3
`Map.prototype.has.call`) measured the SAME as the member call (X4
`m.has(k)`), so the member read is free and the tax is the CALL path
itself — a registered builtin callee paid, per call, the general
`fast_call_core` tail: `is_eval_function` (a `%eval%` JsString alloc +
HashMap get on EVERY non-EcmaScript call — a registered builtin can never
be eval, but the check ran anyway), the callable check, and `call_inner`'s
kind redispatch to the registered handler. A user leaf call in the same
loop shape is ~27ns (jit) / ~80ns (jl), so the intrinsic call carried an
~8x tax over the leaf floor.

Fix: a new arm in `fast_call_core` (the SHARED core both engines route
through — the interpreter's `do_call_fast` and the JIT's `call_slow`/
`call_vector` helpers) after the certified-leaf path: when the callee is a
Function whose id has a registered handler and the realm count is 1, run
`handler(agent, this, args)` directly — skipping the %eval% lookup, the
callable check, and the `call_inner` redispatch. Sound because
registration (`Intrinsics::define`) excludes the eval hosts, a registered
builtin is a function object (always callable), and the single-realm guard
preserves the general path's cross-realm owning-realm push (a registered
handler creates its errors in the CURRENT realm, which only matches the
owning realm when realm_count == 1).

Interleaved A/B (release, scratch/mapcall_probe.js, min-of-3): W4 `m.has`
jit 67-69 -> 34-35ms, jl 87-91 -> 50-51 (~2x both); W1 the corpus has+get+
set triple jit 199-207 -> 99-102, jl 221-227 -> 115-122 (~2x); bound-call
rows unchanged (not the corpus shape). Corpus re-baseline (4-mode):
map_churn jit 211.7 -> 94.0 / jl 206.4 -> 111.6 (~2.25x/~1.85x); set_churn
jit 262.1 -> 124.1 / jl 282.0 -> 138.3 (~2.1x/~2.0x) — the largest single
row-family cut since the Map/Set hash-index landing; every other row
within single-run noise.

Correctness: 180 jit e2e green (incl. a new
`installed_jit_registered_builtin_calls_match_the_interpreter` pinning the
compiled loop's Map churn against the interpreter plus the wrong-receiver
TypeError and a shadowed-method receiver); a 16-case battery (wrong-receiver
TypeError, .call/.apply/bind, subclassed Map, callback-running String/Array
methods, shadowed eval name + the REAL %eval% identity path, shadowed
method, delete-then-proto, iterators, a gc-stress churn loop) and a
re-entrant battery (registered handlers whose argument coercion or
callbacks re-enter the engine) byte-identical across jit/jitless/
--gc-stress; clippy `-D warnings` clean; workspace 32 suites green; three
release test262 sweeps at baseline (language 23721/3 skip, built-ins
23657/155 skip, annexB 1086/1086, zero fail/crash/hang — the one built-ins
run with 2 copyWithin "hangs" re-ran clean at baseline: those two ~8s
fixtures wobble between pass and hang under batch contention, each PASSes
standalone via `--single` in ~8.4s); corpus parity 37/37 ok, 0 mismatches.

Open (measured, not started): the ~50ns/op that remains over the leaf
floor is the `builtin_handler` thread-local RefCell<HashMap> get + the
handler call itself — a handler pointer cached on the crux function
object or a direct-mapped cell would close most of it, and the same
registered-handler shortcut applies to the constructor rows (a registered
construct path). The vector-form call sites still rebuild the fast layout
before the core.

### LANDED (2026-09-09): warm crux-native builtins run their native closure directly from `fast_call_core` — the Math/JSON/typed-array call tax removed (Cut 80)

Cut 79 covered the REGISTERED (agent-dependent) builtins; the corpus's
`math_intrinsics` row (~311 jit / ~358 jl, the biggest remaining
builtins mass after map/set) did not move — Math is a crux-NATIVE module
(no agent), and `Intrinsics::define` registration excludes it. Warm calls
still paid, per call: the %eval% intrinsic lookup (a JsString alloc +
HashMap get on every non-registered builtin call), the callable check,
and `call_inner`'s redispatch to the memoized dispatch verdict + the
native closure. Probe (scratch/math_call_probe.js, min-of-3):
`Math.floor`/`Math.abs` member calls ~220ns/call (jl) / ~160ns (jit) vs
the now-registered `Map.has` at ~160ns (jl) / ~110ns (jit) — the
unregistered crux-native path carried ~50-60ns/call over the Cut-79
floor.

Fix: a second arm in `fast_call_core`, next to the Cut-79 one, gated on
`agent.builtin_dispatch_cache.get(&id) == Some(&0)` — the memoized
verdict 0 means resolve_builtin_dispatch found no agent-dependent module
chain, so the function's own `NativeFn` is the whole call. When warm, the
arm runs `native(this, args)` directly, skipping the %eval% lookup, the
callable check, and `call_inner`. Sound because %eval%/%evalScript% are
never memoized (fast_call_core catches them by intrinsic identity BEFORE
any dispatch resolution), a memoized-0 function is a callable
`Builtin { call: Some(_) }`, and the single-realm guard preserves the
cross-realm owning-realm push for the multi-realm case.

Interleaved A/B (release, scratch/math_call_probe.js, min-of-3):
`Math.floor` jit 48-52 -> 16-17ms / jl 66-71 -> 30-32 (~2-3x); the
corpus floor+abs pair jit 147-153 -> 47-55 / jl 177-183 -> 68-69
(~2.6-3x); Map rows unchanged. Corpus re-baseline (4-mode): math_intrinsics
jit 311.7 -> 104.2 / jl 358.1 -> 120.7 (~3x) — now FASTER than node
(jitGap 0.71 / jlGap 0.68, the suite's first sub-1 builtins row); every
other row within single-run noise. Correctness: 181 jit e2e green (incl.
a new `installed_jit_native_builtin_calls_match_the_interpreter` pinning
compiled Math churn + a direct eval in a compiled function); a 13-case
battery (Math basics/specials/precision, extracted + bound methods, the
REAL %eval% identity path + shadowed eval, JSON round-trip, typed-array
methods, Number.prototype on primitives, a native RangeError, RegExp
methods) byte-identical across jit/jitless/--gc-stress; clippy
`-D warnings` clean; workspace 32 suites green; three release test262
sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip,
annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok, 0
mismatches.

Open (measured, not started): the remaining ~50ns/op over the leaf floor
on BOTH builtin paths is the per-call handler/native table lookup
(builtin_handler's RefCell HashMap get / the dispatch-cache HashMap get)
+ the remaining general-call tail — a handler pointer cached on the crux
function object (a type-layering change) or direct-mapped per-agent
cells would close most of it. The dispatch-cache HashMap get could also
be folded into a direct-mapped cell like the leaf cache.

### PROBE: the head-let jit-inversion is the per-iteration-env + capturing-closure FFI serialization — no slice (measured 2026-09-09)

The top corpus row (`for-in/head-let-fresh-binding-per-iteration`, ~1.2s)
stays jit ≈ jl while every other row sees jit < jl; direct interleaved
runs of the real fixture show jit ~8-12% SLOWER than jl (1.32-1.46 vs
1.20-1.25s) — the suite's one jit-inverted row. A progressive bisect of
the fixture's build core (Object.create(null) + 3 defines + for-in `let
x` + 3 capturing closures + 3 computed stores per body) pinned the
inversion to the CAPTURE step: N4 (non-capturing closures, everything
else identical) is jit +10% FASTER than jl, N5 (closures capturing the
per-iteration `let x`) is jit ≈ jl. Under jit the per-iteration machinery
lowers to one FFI slow call per piece (`EnterPerIteration`/
`PerIteration` env creation, `CreateFunction`, the compiled closure
body's `LoadPerIteration`) — the FFI boundary per key erases the machine
loop's edge over the interpreter, which does the same env/create work
inline in its step dispatch. No slice is warranted: the per-iteration
env (~100ns) + capturing closure create (~430-750ns) costs are at their
architectural floors, the FFI helpers mirror the interpreter handlers
without redundant work, and the row's real mass is general-path assert
calls + closure/object machinery in BOTH engines (~10-20x node-jl).

### LANDED (2026-09-09): the installed-builtin handler lookup goes direct-mapped on the Agent (Cut 81)

The Cut-79 fast arm's per-call `builtin_handler(id)` is a thread-local
RefCell<HashMap> get. A per-agent direct-mapped cell cache
(`Agent::builtin_handler_cells`, boxed per the Cut 27 lesson, filled
lazily from the registry on a miss) serves the same handler with an
array probe + id compare on a hit. Registrations complete at realm
bootstrap and never change, so a fill is final — no epoch needed, and
non-registered ids (eval hosts, crux-native builtins) just probe an
empty slot and return None. `fast_call_core`'s Cut-79 arm reads it via
the new `Agent::builtin_handler_lookup`. Interleaved A/B (release,
probes, min-of-3): map rows ~3-5% (W4 `m.has` jit 34-35 -> 32-33 / jl
50-51 -> 46-49; W1 the has+get+set triple jit 99-102 -> 94 / jl 115-122
-> 117) — small and broad, applied to every registered-builtin call.
The journal's earlier ~50ns estimate was too high; the RefCell<HashMap>
get is ~10-15ns. Gates: 181 jit e2e green, clippy `-D warnings` clean,
workspace 32 suites green, three release sweeps at baseline, corpus
parity 37/37 ok.

### LANDED (2026-09-09): Object.keys/values/entries box keys directly and define pair elements by index (Cut 82)

The `object_keys` corpus row (~160-167ms jl/jit, the third-largest
builtins mass) decomposed to per-call BODY cost, not the call:
`Object.keys` ~1.6us / `Object.values` ~1.0us / `Object.entries` ~5.6us
per 6-key call (probe, min-of-3) while a for-in over the same keys is
~50ns and a 6-string array literal ~400ns — vs node-jl's ~127ns for the
whole triple. Two body wastes: (1) `object_keys` re-copied every key
through `key.to_string_lossy()` + `str()` — a full UTF-8 re-encode +
re-alloc of an already-owned interned `JsString` (~100ns/key) that ALSO
corrupts lone-surrogate keys (lossy substitutes U+FFFD); the fix boxes
the owned `JsString` directly (`Value::String(Handle::new(key))`),
exactly as `own_keys_of` already does. (2) `object_entries` built each
[key, value] pair with `array_create` + two STRING-key
`create_data_property("0")` defines; the fresh pair's elements are
dense indices, so the defines now go through `create_data_property_index`
(no per-define `JsString` alloc + array-index canonicalization).
Interleaved A/B (release, scratch/object_keys_probe.js, min-of-3):
`Object.keys` jit 31-33 -> 22-24 / jl 36-39 -> 25-27; the corpus
keys+values+entries triple jit 165-168 -> 102-110 / jl 161-186 ->
110-114 (~35%). Corpus re-baseline (4-mode): object_keys jit 167.1 ->
104.5 / jl 160.2 -> 112.7 (~35%); map/set jl also down ~10-16% (the
Cut-81 cells); every other row within single-run noise. Correctness:
735 runtime release tests + 181 jit e2e green (incl. a new
`keys_and_entries_preserve_lone_surrogate_keys` pinning that a
lone-surrogate computed key round-trips through keys/entries — the lossy
path would have replaced it); clippy `-D warnings` clean; workspace 32
suites green; three release test262 sweeps at baseline (language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang); corpus parity 37/37 ok, 0 mismatches.

Open (measured, not started): `object_entries` still pays per key the
descriptor recheck + `get_property` + a fresh 2-element `array_create`
(~700ns/key of the ~1.9us/key residual over node) — a fused pair build
or a cheaper own-value read would cut the row further; and the same
direct-box pattern likely applies elsewhere (the remaining
`text.to_string_lossy()` re-copies in the builtins).

### PROBE: the post-Cut-82 corpus top rows decompose to architectural machinery — no bounded slice in any (measured 2026-09-09)

Fresh probes on the remaining largest rows (all interleaved jit/jl,
min-of-2/3):

- **recursive_fib** (~396 jit / ~518 jl, the #2 row after head-let): 3.44M
  self-recursive non-tail calls. A machine-inlined LEAF call is ~6ns jit;
  fib's recursive calls are ~126ns jit / ~150ns jl. Each certified non-leaf
  call goes `ordinary_call` -> `run_compiled_body` -> a POOLED fresh Vm
  (`take_vm`) + execution-context push per call — the general certified
  call path. Cutting it needs the non-leaf callee to run on a shared Vm
  with a carved frame (the leaf-inline / shared-ctx-construct pattern
  generalized to re-entrant bodies) — the L5 call-breadth core,
  architectural.
- **generator_loop** (~225 jit / ~233 jl, node-jl 9.6ms — 24x): the
  known interpreted-resume row. Per `g.next()` ~1.35us; the result-object
  literal is ~160ns of it and a leaf call ~10ns — the resume machinery
  itself is the ~1.2us. A fresh-generator-per-iteration row is ~30us per
  create (generator creation, out of the corpus row). The suspension
  resume driver is interpreted regardless of the body's compiled state;
  JIT coverage here is the suspension-driver program.
- **completion-values** (~311 jit / ~320 jl, the #3 row): NOT try/finally
  machinery — the amplified fixture is 1423 iterations x 12 CONSTANT-string
  direct evals. Each identical source is fully re-parsed per call: a
  22-char eval ~8.9us, the row-shaped eval ~57us (vs ~0.01us inline). A
  parsed-Program cache keyed by source would collapse the row, but eval
  sites are distinct per invocation (the engine bumps the template parse
  generation per eval so two identical-text evals get DISTINCT template
  objects) — caching the parse across calls would alias template-site
  identity and break conformance. Delicate, not sliced.
- **Object.keys/values/entries + destructure residuals**: after Cut 82 the
  `entries` per-key pair build and the `destructure` literal both sit on
  the fresh-object/array creation floor (~130-300ns per box: JsObject +
  ArraySlots/in_fields + proto + length + generation). Node-jl creates the
  same arrays ~20-40ns. The destructure "jit-inversion" (284 jit vs 276
  jl) is jl run-noise (256-372 across 3 samples); flat-literal rows are
  jit-faster. These are the object-representation/allocation program.

Together with head-let (characterized above), every remaining top corpus
row is architectural: the L5 general certified-call ctx model, the
suspension-driver JIT coverage, the eval-parse cache (with a template-
identity constraint), and the object/array allocation floor. No bounded
row slice is open; the next landable lever is one of these programs,
gated on its own design probe.

### PROBE: builtin constructors are 20-50x node — the construct side never got the call-side direct dispatch (measured 2026-09-09)

Fresh construct-creation probes (release, min-of-2, per 300k): `new
Object()` ~600ns jit/jl, `new Array()` ~870ns, `new Map()` ~5.4us,
`new Error('x')` ~7.3us, `new Date(0)` ~9.4us (node: ~20ns/~40ns/~60ns/
~100ns — a 20-50x gap), while the LITERAL equivalents are `{}` ~70ns
jit and `[]` ~230ns. EcmaScript certified constructors are healthy
(`new E(){}` empty ~100ns, `this.x=1` ~130-220ns jit after the
construct-leaf/fill work). The builtin-constructor gap decomposes:
`Object()` as a CALL (the Cut-79 direct registered handler →
`object_constructor`) is ~370ns vs the ~70ns literal — the
`intrinsics.get("%Object.prototype%")` JsString-alloc lookup +
`ordinary_object_create` body sits above the literal floor; and `new
Object()` at ~600ns adds the CONSTRUCT machinery the call path does not
have: `step_construct_impl`'s per-construct `split_off` Vec +
`function::construct`'s crux-hook round trip + `construct_inner`'s
un-memoized dispatch_construct chain walk (no analog of the call side's
`builtin_dispatch_cache`/`BUILTIN_HANDLERS`). `Array()` as a call is
~990ns (its `array_call` body). No corpus row is builtin-constructor-hot
(the rows use EcmaScript ctors or construct once), so this is a real-code
anomaly, not a row lever — the bounded slice (register builtin construct
handlers O(1) + a direct warm-construct arm in `step_construct_impl`, the
construct-side mirror of Cuts 79/80) would need a row or a real-code
justification before the full gate.

### LANDED (2026-09-09): registered builtin CONSTRUCT handlers dispatch O(1) from `step_construct_impl` — the construct-side mirror of Cut 79 (Cut 83)

The probe above measured every builtin constructor paying the full general
construct path: `step_construct_impl`'s per-construct `split_off` Vec +
`function::construct`'s crux-hook round trip + `construct_inner`'s realm
scan and UN-MEMOIZED `dispatch_construct` chain walk (the call side got
its O(1) `BUILTIN_HANDLERS` registration in Cut 79/80; the construct side
had no analog). New `CONSTRUCT_HANDLERS` (function id -> BuiltinCtor =
`fn(agent, callee, args, new_target)`) registered at `Intrinsics::define`
time from per-module `construct_handler_for(name)` tables, mirroring the
call-side `handler_for` arc; a direct-mapped per-agent `builtin_ctor_cells`
cache (Cut-81 pattern); and a warm arm in `step_construct_impl`'s general
path (single-realm guard, args copied into a small Cow buffer so the
handler's re-entry cannot touch the Vm's stacks, gc-stress suppressed for
the unrooted window) that runs the registered constructor directly. A
registered constructor is exactly what the chain arm would have run
(registration happens only for the intrinsic the chain matches by
identity); the arm serves direct `new` (new_target == callee), while
Reflect.construct / derived `super` still route through `construct_inner`
unchanged.

Registered in this landing (clean single-name arms): Object, Array,
Map/Set/WeakMap/WeakSet, Date, RegExp, Number, String, ArrayBuffer/
SharedArrayBuffer, DataView. The loop-shaped dispatches (the %Error%
subtypes' ERROR_CTORS loop, Boolean's inline closure, typed-array kinds,
Intl/Temporal) are left on the chain — their bodies dominate, and the
fn-pointer registry cannot capture the per-type flags without named
shims.

Interleaved A/B (release, scratch/object_call_vs_new.js + newx_probe.js,
min-of-3): `new Object()` jit 176-200 -> 102-110ms / jl 193-202 ->
113-126 (~1.8x — now equal to the `Object()` CALL form); `new Array()`
jit 252-278 -> 146-153 / jl 266-278 -> 162-169 (~1.8x, now faster than
`Array()` call); `new Map()` ~1.6s -> ~0.9s / `new Date(0)` ~2.8s ->
~1.7s (~1.7-1.8x, the dispatch share of constructors whose bodies
allocate). Error/Boolean/EcmaScript rows unchanged (not registered / a
different path). Correctness: 182 jit e2e green (incl. a new
`installed_jit_registered_builtin_constructs_match_the_interpreter`
pinning compiled `new Object/Map/Date` churn + a subclassed `new`); the
construct batteries (16 + 22 cases: empty/value/derived/Reflect.construct
new-target/subclass/species/iterator-arg/error paths/gc churn)
byte-identical across jit/jitless/--gc-stress (the one
`Reflect.construct(Map, ..., C)` TypeError is pre-existing — that path is
`construct_inner`, untouched); clippy `-D warnings` clean; workspace 32
suites green; three release test262 sweeps at baseline (language 23721/3
skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang);
corpus parity 37/37 ok, 0 mismatches.

Open (measured, not started): the residual `new Object()` ~350ns vs the
~70ns literal is `object_constructor`'s own body — the uncached
`intrinsics.get("%Object.prototype%")` + `ordinary_object_create` (the
realm's cached `object_prototype` accessor exists but is not used here)
and the per-construct arg-copy + step dispatch; and the loop-shaped
module dispatches (error/typed-array/Intl) could get named-shim
registration if a row ever shows them hot.

### LANDED (2026-09-09): `object_constructor` uses the cached prototype and the callee-identity shortcut (Cut 84)

The Cut-83 open note's residual: `object_constructor`'s body read
`%Object.prototype%` through the uncached `intrinsics.get` (a JsString
alloc + HashMap hit per construct/call) and re-resolved `%Object%`
identity for the active-constructor check the same way. Two fixes:
(1) the prototype reads use the realm's cached `intrinsics.object_prototype()`
accessor (the struct field the array/object creation paths already use);
(2) the active check `intrinsics.get(OBJECT) == new_target` becomes
`callee == new_target` — `object_constructor` is only ever reached with
`callee` = %Object% (the dispatch arms match the intrinsic by identity
and registration happens only for the intrinsic) or the call-form's
Undefined placeholder (whose new_target is also Undefined, so the
active branch is skipped either way), so "new_target is %Object%" is
exactly the callee comparison — no per-construct lookup.

Interleaved A/B (release, scratch/object_call_vs_new.js, min-of-3):
`new Object()` jit 102-110 -> 25ms (~4x; ~85ns/construct vs the ~60ns
literal — ~7x off the pre-Cut-83 ~590ns) / jl 113-126 -> 40-42 (~3x);
`Object()` call jit 106-113 -> 28-29 / jl ~116 -> 40-42 (the call form
pays the same body). `new Array()` unchanged (~145-155ms — its body's
`get_prototype_from_constructor` member read remains). Correctness: 182
jit e2e + 735 runtime release tests green (the existing
`object_constructor_respects_derived_new_target` pins BOTH the derived
`new O extends Object` / `Reflect.construct(Object, [], O)` path —
active false via callee != new_target — and the direct `new Object(5)`
boxing — active true); the construct batteries still byte-identical
across jit/jitless/--gc-stress; clippy `-D warnings` clean; workspace 32
suites green; three release test262 sweeps at baseline (language
23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang); corpus parity 37/37 ok, 0 mismatches.

Open (measured, not started): the remaining `new Array()` ~480ns vs `[]`
~195ns is `array_construct`'s per-construct `get_prototype_from_constructor`
member read of new_target.prototype (spec-required for subclassing; the
active-%Array% case could shortcut to the cached `array_prototype()`
with the same callee-identity trick); the other registered constructors'
bodies (Map/Date/etc.) similarly pay their own creation work on top of
the now-O(1) dispatch.

### LANDED (2026-09-09): `array_construct` takes the cached prototype and the callee-identity shortcut (Cut 85)

The Cut-84 open note's residual: `array_construct` read
`new_target.prototype` through `get_prototype_from_constructor` on every
construct — even when the active function IS %Array% (the common `new
Array(...)`/`Array()` path), where that own property is the realm's
immutable %Array.prototype% (non-writable, non-configurable at install),
already cached. Same shape as Cut 84: `array_construct` gains the
`callee` parameter and skips the member read when `callee == new_target`
— every dispatch arm (the Cut-83 registered construct handler, the
`dispatch_construct` chain for the Array() call form, Reflect.construct
and derived `super`) reaches it with `callee` = %Array% by identity, so
the active case is exactly the equality and a genuinely derived new
target (subclass / Reflect.construct) still member-reads its prototype
(spec 10.1.14, 23.1.1.1). The two call sites (the `construct_handler_for`
closure became `Some(array_construct)` — the signature now matches
`BuiltinCtor` directly — and `dispatch_construct`) pass `callee` through.

Interleaved A/B (release, scratch/cut85_ab.js, K=300000, the
object_call_vs_new.js harness): `new Array()` jit 149-186 -> 77-91ms (~2x)
/ jl 164-230 -> 90-98 (~2x); `new Array(3)` jit 167-197 -> 76-92;
`Array()` call jit 293-336 -> 217-251 (~1.4x — its residual is
`array_call`'s `intrinsics.get` + the `construct_inner` round trip, not
the proto read); derived `new (class A extends Array {})(3)` unchanged
(616-741ms — still member-reads the derived prototype). Correctness:
182 jit e2e + the workspace suites green (the Cut-83
`installed_jit_registered_builtin_constructs_match_the_interpreter` now
also churns `new Array(i)` in the compiled loop and checks the prototype
identity, plus a subclass and `Reflect.construct(Array, [3], A)` derived
arm; a new runtime `array_construct_respects_derived_new_target` pins
the active vs derived prototype choice in both engines); clippy
`-D warnings` clean; the construct batteries still byte-identical across
jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero
fail/crash/hang); corpus parity 37/37 ok, 0 mismatches.

Open (measured, not started): the other registered constructors' bodies
(Map/Date/etc.) each pay their own creation work on top of the now-O(1)
dispatch.

### LANDED (2026-09-09): the `Array()` call form runs `array_construct` directly (Cut 86)

The Cut-85 open note's residual: `Array()` as a call registered its own
handler in `handler_for` (Cut 79) but that handler (`array_call`)
re-dispatched through `intrinsics.get(ARRAY)` + a full
`function::construct`/`construct_inner` round trip — the same machinery
the construct path had just shed. The %Object% call form never had this
(`handler_for`'s OBJECT arm runs `object_constructor` with the Undefined
placeholders directly), and Array is the same shape: spec 23.1.1.1 has
no separate call behavior — an undefined NewTarget IS the active
function. Two changes: `array_construct` now promotes an undefined
new_target to the active case (`matches!(new_target.kind(),
ValueKind::Undefined) || callee == new_target` — the fast-path handler
passes Undefined placeholders for both, and the chain arm passes the
real %Array% callee with an Undefined new_target, so the promotion
clause is what keeps the direct dispatch correct); and both the
registered-call handler (fast_call_core's Cut-79 arm) and
`dispatch_call`'s ARRAY arm run `array_construct` directly. `array_call`
is deleted.

Interleaved A/B (release, scratch/cut85_ab.js, K=300000): `Array()` call
jit 217-251 -> 72-74ms (~3x, now equal to `new Array()`'s 77-83) / jl
~233-305 -> 82-85 (~3x); `new Array()` and the derived subclass rows
unchanged. Correctness: 182 jit e2e + runtime release tests green (the
Cut-83/85 e2e now also calls `Array(i)` in the compiled loop and checks
its prototype identity); clippy `-D warnings` clean; workspace suites
green; the construct batteries still byte-identical across
jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086,
zero fail/crash/hang); corpus parity 37/37 ok, 0 mismatches.

### PROBE: the unregistered-builtin chain tax — Date members cost ~µs/call (measured 2026-09-09)

After the ctor call/construct registration arc (Cuts 79-86), the
call-side handler registry covers only nine modules (array, regexp,
string, number, boolean, bigint, keyed, object, dataview — the
`Intrinsics::define` chain). The constructible modules left off it —
date, symbol, array_buffer, typed_array, error — still warm-dispatch
through their module `dispatch_call` chains, whose per-arm
`intrinsics.get` is a JsString alloc + HashMap probe; date.rs's chain is
~60 arms (its getters/setters are table-driven loops). Measured warm
member-call cost (scratch/date_chain_probe.js, K=300000): `d.getTime()`
jit 1150ms / jl 1180ms (~3.9µs per call — BOTH engines, i.e. not a JIT
coverage issue); `d.valueOf()` ~1220ms (~4.1µs); `d.getFullYear()`
~390ms (~1.3µs, fewer arms); `Date.now()` ~225ms (~750ns); `d.setTime`
~2100ms (~7µs); `Symbol()` ~150ms (~500ns) — vs the registered control
`m.has(k)` at 37ms (~125ns) in the same harness. Node runs the whole
probe in 0-13ms. So the unregistered members pay a 10-30x
member-dispatch tax on top of their (often trivial) bodies — the same
class of gap Cut 79 closed for the keyed intrinsics.

### LANDED (2026-09-09): the Date module members register their call handlers (Cut 87)

The probe's Date rows: `date::handler_for` now arms the call form
(`Date()` → `date_call`), `Date.now`, the getTime/valueOf raw read
(spec 21.4.4.10 / 21.4.4.43), getTimezoneOffset, and all 17 local/UTC
component getters (each a one-line non-capturing closure over the
shared `get_component` helper — same functions `dispatch_call` uses, so
no behavior drift), and the module joins the `Intrinsics::define`
call-side registration chain in realm.rs. The setters, string/format
methods, parse/UTC stay on the chain (heavier bodies / rarer — a
follow-up if a row shows them hot).

Interleaved A/B (release, scratch/date_chain_probe.js, K=300000):
`d.getTime()` jit 1138-1183 -> 19-20ms (~60x) / jl 1180-1250 -> 30-31
(~40x); `d.valueOf()` -> 20ms jit; `d.getFullYear()` 376-417 -> 24-26
(~15x); `Date.now()` 217-236 -> 20ms; the unregistered `d.setTime`
UNCHANGED (~2100ms — confirms the residual is the not-yet-registered
setter family, and the getters now run at or below the registered
`m.has` control). Correctness: 24 date-module runtime tests + 736
runtime release + 182 jit e2e green; clippy `-D warnings` clean;
workspace suites green; the construct batteries still byte-identical
across jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip with zero Date
failures, annexB 1086/1086); corpus parity 37/37 ok, 0 mismatches.

### LANDED (2026-09-09): the Date setters join the registered call handlers (Cut 88)

The Cut-87 probe's unchanged residual — `d.setTime(i)` ~2100ms (the
setter family still on the `dispatch_call` chain scan). `date::handler_for`
now arms all 13 table setters (12 via the shared `set_components` with
their literal local/present-mask/nan-is-zero tuples transcribed verbatim
from `dispatch_call`, `setYear` via its own body), plus `setDate`/
`setUTCDate` (`set_date` with the local flag) and `setTime` (the inline
brand-check + coerce + time_clip body). The string/format methods and
the parse/UTC statics stay on the chain (their bodies dominate).

Interleaved A/B (release, scratch/date_chain_probe.js, K=300000):
`d.setTime(i)` jit 2075-2166 -> 24-26ms (~80x) / jl 2110-2141 -> 35-39
(~55x); `d.setFullYear(2000+i%20,0,1)` jit 51-53; `d.setUTCDate(1+i%27)`
jit 41 / jl 52-56. The getters and control rows unchanged. Correctness:
736 runtime release + 182 jit e2e green; clippy `-D warnings` clean;
workspace suites green; the construct batteries still byte-identical
across jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip with zero Date
failures, annexB 1086/1086); corpus parity 37/37 ok, 0 mismatches.

### PROBE: the remaining unregistered families are body-bound or cold — the chain-tax arc stops here (measured 2026-09-09)

After the Date registration (Cuts 87/88) the remaining modules off the
call-side registry are symbol, array_buffer (call = TypeError), the
typed-array kinds (loop-shaped), the Date string/format methods +
parse/UTC statics, and the Error subtypes (loop-shaped). Probed each
candidate (scratch/chain_tax_probe2.js + ta_method_probe.js, K=300000):
`new Uint8Array(64)` jit ~620-670ms (~2.1µs) is FASTER than the
REGISTERED `new ArrayBuffer(64)` ~725-800ms (~2.5µs) — typed-array
construct is body-bound, not chain-bound; the Date format methods are
~8µs (`toISOString` ~2400ms, `toString` ~2600ms — format + string
alloc bodies); `Date.UTC` ~180ms / `Date.parse` ~140ms (~470-600ns,
body moderate but the statics are cold); `Symbol()` ~150ms (~500ns,
cold). The typed-array METHODS looked different — `a.indexOf(i%32)`
on a 32-element Int32Array ~11.7µs/call, `subarray` ~14.6µs, `dst.set`
~5.1µs, jit == jl (no fast path) — but the hand-rolled equivalent
scan (32 reads + compares, scratch/ta_method_probe.js) is ~0.9µs/call
(compiled typed reads ~18-55ns each), so the native bodies read
elements through the general per-element helper at ~350ns/read — a
13x BODY gap, not dispatch. Registration would not move them; closing
it is a typed-array body slice (fast same-type element reads inside
the native methods). Verdict: the call/construct registration arc has
collected its real wins (keyed, String, Object, DataView, Array,
Date); the rest are body-bound or cold — stop here rather than add
mechanical shims with no measured row behind them.

### LANDED (2026-09-09): typed-array native methods read elements directly (Cut 89)

The probe's one body-bound finding: the typed-array native methods read
elements through a per-element key-string + [[HasProperty]]/[[Get]]
dispatch (`index_of` also probed presence with a fresh `k.to_string()`
key every element) at ~350ns/read — 13x the compiled read. New
element_read helper (crux `JsObject::typed_array_element_get`: the raw
decode, no key-string, no dispatch) replaces the per-element reads in
at/copy_within/every/filter/find/findLast/forEach/includes/indexOf/
join/lastIndexOf/map/reduce/reduceRight/reverse/slice/some/
toLocaleString/toReversed/toSorted/with, the typed-source cross-type
copies (set, the constructor path), and the sort collector. Spec-exact
for a validated typed array: its integer-indexed elements in `0..len`
are always present and unoverridable, so HasProperty + Get reduce to
the decode — except the search methods' resizable-buffer semantics,
which the first landing got wrong and the sweep caught (see below).

A/B (release, scratch/ta_method_probe.js + chain_tax_probe2.js,
K=300000): `a.indexOf(i%32)` on a 32-element Int32Array jit ~3486-3561
-> 1128-1133ms (~3.1x, ~11.7µs -> ~3.8µs/call) / jl ~5500 -> ~1170;
the hand-rolled equivalent scan is ~0.9µs (the residual is the native
decode + Value boxing per read). Correctness — TWO sweep-caught
regressions fixed during the landing: (1) removing indexOf/lastIndexOf's
per-element HasProperty probe changed the resizable-buffer semantics
(the probe had doubled as the live presence gate: 6 built-ins failures
after a coercion detached/shrunk the buffer — a raw read of the
out-of-range index returned *undefined*, false-matching an undefined
search); (2) the first fix over-corrected by moving the length read
after the coercion (13 more failures: the length-zero fixtures require
the len==0 return BEFORE the fromIndex coercion, the loop bound and
negative-index math use the PRE-coercion length, and includes has NO
presence gate — a detached/shrunk read may match undefined — while
indexOf/lastIndexOf keep TypedArrayIndexOf's presence gate so
out-of-range reads are absent). Final shape: pre-coercion length for
the bound/index math + len==0 early return; post-coercion live length
as a one-time presence gate for indexOf/lastIndexOf only; includes
reads live unguarded. All 130 includes/indexOf/lastIndexOf fixtures
pass, then the full gate: 737 runtime release (incl. a new
element_read regression test) + 182 jit e2e green; clippy `-D warnings`
clean; workspace green; batteries byte-identical; three release sweeps
at baseline (language 23721/3, built-ins 23657/155 with zero failures,
annexB 1086/1086); corpus parity 37/37.

Open (measured, not started): the remaining ~3.8µs on the search rows
is the chain dispatch (typed_array is still off the call-side registry)
plus the per-element decode — the prototype methods are shared across
kinds (unlike the loop-shaped constructors), so they can register
cleanly like the Date members did, and the decode could get a
same-type fast path.

### LANDED (2026-09-09): the TypedArray members register their call handlers (Cut 90)

The Cut-89 open note: every TypedArray prototype method/accessor and the
%TypedArray% statics still rode the module `dispatch_call` chain scan.
Unlike the loop-shaped typed-array CONSTRUCTORS (per-kind element types),
the prototype methods are shared across the twelve kinds — the element
type comes from the validated receiver — and every member is already a
single named handler with the exact `BuiltinHandler` signature, so
`typed_array::handler_for` is a plain name->fn match (at/copyWithin/
entries/every/fill/filter/find/findIndex/findLast/findLastIndex/forEach/
includes/indexOf/join/keys/lastIndexOf/map/reduce/reduceRight/reverse/
set/slice/some/sort/subarray/toLocaleString/toReversed/toSorted/values/
with/@@iterator + the species/toStringTag/length/buffer/byteLength/
byteOffset accessors + the from/of statics + the Uint8Array hex/base64
family + the %TypedArray% call-form throw), and the module joins the
`Intrinsics::define` call-side registration chain in realm.rs.

Interleaved A/B (release, scratch/chain_tax_probe2.js, K=300000):
`dst.set(src)` (same-type byte copy) jit 1546-1589 -> 219-221ms (~7x —
the dispatch was ~4.4µs of the old ~5.2µs/call) / jl ~1550 -> ~238;
`subarray` jit 4375-4437 -> 890-938ms (~4.8x); `indexOf` over 32
elements jit 5501-5517 -> 278-307ms (~18x) / jl ~5500 -> ~290 (now
~1µs/call, ~31ns/read — at the compiled floor). Correctness: 737 runtime
release + 182 jit e2e green; clippy `-D warnings` clean; workspace
suites green; the construct batteries still byte-identical across
jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086,
zero fail/crash/hang); corpus parity 37/37 ok, 0 mismatches. This closes
the call-side registration arc: every agent-dependent builtin module
that can warm-dispatch now does.

### LANDED (2026-09-09): the attr-drift fork — a mapped key whose attrs change drops to dictionary mode (L1c gate prerequisite)

The Option-3 end-state's first gate: field-authoritative storage cannot
tolerate a shared map whose descriptor attrs lie for one object, and
today `defineProperty` could change a mapped key's writable/enumerable/
configurable while `sync_map_after_define` kept the map describing it
(the drift was invisible because the property VECTOR stays authoritative
for attrs; the map only mirrors values). `sync_map_after_define`'s
mapped branch now compares the applied property's attrs against the
map descriptor's recorded `MapAttrs` and drops the object to dictionary
mode on any mismatch — establishing the invariant a live map's
descriptor attrs equal the object's own attrs (the data→accessor
conversion and mapped-key delete already dropped). A value-only
redefine with unchanged attrs still mirrors into the field and keeps
the map; fresh non-default defines never entered the map, so no drift
is possible on append. Dropping is permanent (no map re-acquisition),
matching the existing accessor/delete ejection and the Option-3 plan's
"dictionary is the attr-drift home". No hot row moves (warm stores and
fast fresh defines never route through `sync_map_after_define`; only
descriptor redefinitions do).

Crux regression test pins the mechanism: two same-shape objects share
one map; a value-only redefine on one keeps the map, a `writable:false`
redefine drops THAT object to dictionary while the sibling keeps the
shared map and its shape-pinned reads, and later value updates apply on
the vector staying dictionary. One existing test
(`structural_ops_materialize_then_behave`) updated: its writable
round-trip no longer re-gains the map read path (dictionary is
permanent) — reads/sets now assert against the vector. Gate: 228 crux +
737 runtime release + 182 jit e2e green; clippy `-D warnings` clean;
workspace green; the construct batteries byte-identical across
jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086,
zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the Option-3 storage split itself — the
map-prefix values for ordinals >= INLINE_FIELDS move out of the vector
into an ordinal-indexed overflow array (machine-addressable, stable
base pointer) with enumeration/own-keys/gopd/delete re-pointed at the
descriptors; the attr-drift fork above is the prerequisite that makes
it safe. The l1c_current_probe residual (~30% jit read gap for ord >= 4
at scale) is the measured justification, per the decision section's
gate.

### LANDED (2026-09-09): the in-object field region grows to eight slots — >4-key shapes become machine-addressable (Option-3 split, slice 1)

The Option-3 gate probe's residual: ordinals >= 4 rode the narrow
map-slot helper into the movable, RefCell'd, sometimes-inline property
vector (~26-29ns/op compiled at scale vs ~22ns inline, a ~30% gap that
grew with object count). The full Option-3 overflow array was scoped
as indirection-bound (Slice 3), so the cheapest machine-addressable
form of "values live in a per-object ordinal-indexed region" is a
LARGER in-object region: `INLINE_FIELDS` 4 -> 8. Every consumer follows
the constant — the map mirror (`map_set`/`map_field` below the cap),
the vector-free deferred state (now field-only up to 8 keys;
materialize defers to the 9th), the vector-slot pinning (ordinals >= 8
keep the Option-1 pinned slot), and the compiled shape gate (a slot <
8 is a single machine load from `in_fields[slot]`, offset_of-based so
the layout change is safe). The l1c_current_probe 6-field objects
(a..f) now stay fully vector-free: `{a..f}` literals adopt all six
fields in the ObjectFast batch and never materialize. Cost: +4 `Cell`s
(32B) on every JsObject of every kind (~528 -> ~560B) — measured
acceptable below.

Interleaved A/B (release, scratch/l1c_current_probe.js): R4-vec-read
(ord 4, 16384 objects) jit ~9540-9997 -> ~7436-7537ms, now EQUAL to
R0-inline-read (~7331-7749); R4 at 1024 ~530 -> ~440ms = R0; writes
W4 ~13.7s -> ~12.4-12.8s = W0; jl equalizes likewise. `--jit-bench`
warm rows all within drift (arithmetic 2.48 vs 2.68, bare 2.51 vs
2.39, buildString shape 37.0 vs 37.2 — no bloat regression from the
+32B). Correctness: crux 228 green (the two 5th-key-overflow tests
rewritten to overflow at the new 8-key boundary), 737 runtime release
+ 182 jit e2e green; clippy `-D warnings` clean; workspace green; the
construct batteries byte-identical across jit/jitless/--gc-stress;
three release test262 sweeps at baseline (language 23721/3 skip,
built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang);
corpus parity 37/37 ok.

Open (measured, not started): objects past 8 keys still pin ordinals
>= 8 to the vector (the Option-1 slot) and pay the map-slot helper on
the compiled read — the out-of-line overflow region for keys beyond
the in-object cap remains the next Option-3 slice if a >8-key hot
shape shows up in a probe.

### LANDED (2026-09-09): the in-object field region grows to sixteen slots (Option-3 split, slice 2)

The slice-1 open note probed (scratch/l1c_overflow_probe.js, named
member reads on a 10-key shape): ordinals >= 8 showed the same ~30%
compiled-read gap at scale that ord >= 4 had before slice 1 (~30ns/op
vs ~22ns inline at 16384 objects). No corpus row uses a >4-key shape
(the object workloads are all a/b/c/d-class), so the extension is
beyond any hot row — but the 4->8 raise measured zero bloat regression
(+32B/JsObject), so the empirical question was whether pushing the
boundary is also cost-free: `INLINE_FIELDS` 8 -> 16 (another +64B,
~528B -> ~656B total vs the original). Every consumer follows the
constant again; the probe's 10-key shape now adopts all ten fields and
stays vector-free.

A/B (release, scratch/l1c_overflow_probe.js): ord 8/9 at 16384 objects
jit ~10.0-10.1s -> ~7.4-9.2s, into the ord 0/7 noise band (no systematic
ord-8 penalty); n=1024 rows all ~equal. `--jit-bench` warm rows within
drift at +128B total (+24% JsObject): arithmetic 2.45, bare 2.30,
property read 5.34, buildString shape 34.8, full 24.4 — no bloat
regression measured at any raise step. Correctness: crux 228 green,
737 runtime release + 182 jit e2e green; clippy `-D warnings` clean;
workspace green; the construct batteries byte-identical across
jit/jitless/--gc-stress; three release test262 sweeps at baseline
(language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086,
zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the boundary now sits at 16 keys — a
>16-key map-pinned ordinal still rides the map-slot helper into the
vector. The measured corpus has no shape above 4 keys, so a further
inline raise is unbounded +64B/8-keys speculative bloat per step and
the out-of-line overflow region is the only scaling answer; both stay
gated behind a real >16-key (or >8-key, if 16 proves too fat) hot
workload. STOPPING the extension arc here by the no-hot-row gate.

### LANDED (2026-09-09): substring views — `JsString::Sliced` makes slice/substring/substr O(1) nodes

The `strings/search_slice.js` corpus row (100k `hay.slice(0, i%400+1).length` + indexOf over a 1200-unit `hay`) still ran at ~66ms jit / ~83ms jl after the flatten-cache landing. Its slice term is a range COPY per call: the corpus reads only `.length` of each slice, so the copy is pure waste. Added a `JsString::Sliced` variant (V8's SlicedString): `{ parent, offset, len, flat }` — `len` O(1), and `as_slice` materializes the PARENT once (an Arc clone for a Flat parent) and windows it, so the range is never copied. `Trace` walks the parent (views keep it alive under GC); clone/owned_of/flatten-walk got arms (owned_of seeds the copy's fresh cache from the boxed whole-parent buffer, mirroring the rope path). New `JsString::slice_view(parent, from, to)`: windows > `SMALL_STRING_CAP` (16) units as a `Sliced` node; small windows stay eager inline `Small` copies (a node + later parent materialize would cost more than the inline copy).

Runtime wiring: `slice`/`substring`/`substr` in `crates/runtime/src/builtins/string.rs` now go through a `this_string_box` helper — a string PRIMITIVE receiver keeps its own box (the spec §ToString result) as the view parent, so the per-call `to_string` owned copy disappears too; non-primitive receivers coerce through the agent and get boxed fresh. Result: `slice`/`substring`/`substr` return views instead of eager `from_utf16` copies; the corpus `.length` reads never touch the parent's content. `split`'s per-token pieces, `startsWith`'s bounded compare, and `endsWith`/`includes` deliberately stay eager (their substrings are small or immediately content-compared).

A/B (release corpus, interleaved): `search_slice` jit ~60.5/60.7/77.0 (mean ~66) -> ~42.1/43.9/41.6/44.8/47.4 (mean ~44, ~1.5x) / jl ~82.2/83.7 -> ~55.7/55.7/54.4/52.8 (~1.55x). Every other corpus row within cross-run noise (char_ops ~72ms, coercion_concat ~165ms, split_join ~130-146ms jit — all at their recorded recent baselines). Correctness: 4 new crux tests (window/len-no-materialize/view-of-view-concat/owned_of-seeding); a 250-case x 3-parent x 3-method deterministic differential matrix plus ~55 spot checks (edge args, wrapper/number/object receivers, view-of-view, views feeding concat/indexOf/split/startsWith/replace/toUpperCase/iteration/trim/regexp) diff byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green; three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the same per-call copy pattern likely sits in `repeat`/`padStart`/`padEnd` filler paths and `String.prototype.split`'s empty-separator unit loop; and a view whose parent is a small `Flat` could adopt the parent's Arc directly. Neither has a hot corpus row behind it yet.

### LANDED (2026-09-09): the primitive-prototype own-data read cache

The `strings/char_ops.js` corpus row (~300k `s.charCodeAt(i & 255)` calls) sat at ~78ms jit / ~83ms jl after the view landing. Its per-call member read on a STRING PRIMITIVE was never cached: get_member_name's own-read and chain cells decline the primitive (no object id/map), so `get_primitive_member` ran the full spec [[Get]] on every call. Probing why even the DIRECT read `String.prototype.charAt` was equally slow (Q1 ~92-108ms jit / ~104-115ms jl, ~150-330ns/read) exposed the root: **%String.prototype% is itself an exotic `ObjectKind::String`** (spec: the String prototype's [[StringData]] is the empty string), and `cell_object` only admits Ordinary/Array kinds — so reads ON the string prototype (and through it, from primitives) could never warm the member-value cells.

Landing: a `primitive_proto_data_get` probe in `get_primitive_member` (both the string and the Number/Boolean/BigInt/Symbol tails) and in get_member_name's fallback for String-exotic OBJECT receivers. For a non-virtual atom name it serves the prototype's own DATA property from the member-value cell keyed by (prototype id, name), validated against the prototype's generation — the same oracle the JIT probe and warm stores use. Sound because a data property's value is receiver-independent and every prototype mutation invalidates the cell: structural writes (define/delete/accessor conversion) bump the generation, and an in-place value write either takes the warm-store path (which fronts the SAME member-value cell) for Ordinary prototypes or the full [[Set]] (a generation bump via define_property_key) for the exotic String prototype. The String prototype's virtual own `length`/canonical-index keys are excluded (not own vector entries; a boxed `new String(...)` keeps its exotic length/index handling). Deeper chain props (Object.prototype: `hasOwnProperty`) stay on the full path.

A/B (release corpus, interleaved same-machine): `char_ops` jit ~75.3/79.8/77.9 (mean ~77.7) -> ~30.6/31.0/31.1 (mean ~30.9, ~2.5x) / jl ~83.0/82.8 -> ~35.7/35.6 (~2.3x); `search_slice` jit ~47.4/49.1/46.2 (mean ~47.6) -> ~26.5/24.2/26.7 (mean ~25.8, ~1.85x — its `hay.indexOf` method read benefits too) / jl ~54.9/53.1 -> ~29.9/30.0 (~1.8x). Every other corpus row within cross-run noise. Synthetic rows (300k): direct `String.prototype.charAt` read 92-108 -> 4ms jit / 104-115 -> 32ms jl; primitive read `s.charAt === ...` 88-100 -> 19-20 jit / 37 jl; read-then-call 81-100 -> 35-38 jit / 47-48 jl. Correctness: a 13-case differential battery (kind reads incl. BigInt/Symbol; direct prototype reads incl. the exotic virtual `String.prototype.length`/`[0]`; boxed receivers; overwrite/delete/accessor-conversion/redefine-of-non-writable mid-loop invalidation; define-then-read; deep-chain `hasOwnProperty`/`__proto__`/`constructor`; per-kind mutations; the char_ops shape) diffs byte-identical vs node in jit, jitless, AND --gc-stress; a new `installed_jit_primitive_proto_reads_match_the_interpreter` parity test (182-suite). Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the residual ~35-38ms jit char_ops floor is the registered-method CALL side (~110ns/call, P1 jit ~25ms) — read and call now both near their floors; `charCodeAt`'s native body and the method-call dispatch are the next terms. A String-exotic read of a DEEP chain prop (a boxed receiver reading an Object.prototype method) still pays the full path per call.

### PROBE: recursive_fib is certification coverage, not a call-machinery floor (measured 2026-09-09, no code)

The corpus `recursive_fib` row (~4.2M calls across 1200 fib trees, jit ~419ms / jl ~512ms, ~200-240ns/call in BOTH engines) looked like the last remaining pure-call row. Decomposition probe (scratch/call_floor_probe.js, scratch/recursion_cert_probe.js):

- A certified LEAF call in a loop is ~6ns/call jit (leaf-inline) / ~46ns/call jl — so the recursion cost is not the call machinery per se.
- EVERY recursion shape measured jit ≈ jl: top-level self-recursion (fib ~1.2x, count ~1.1x jit speedup), nested self-recursion (the corpus shape), depth-1 self-recursion (~260ns/call), and top-level MUTUAL recursion — a mutual fib pair and a mutual count pair are NOT faster than their self-recursive twins in either engine.
- Root cause (code, ir.rs L23447): the `certified_functions` global-blind fixpoint cannot certify a function whose body reads an UNCERTIFIED name, and a recursive body reads its own (or its cycle's) name — self- and mutual recursion are dependency cycles, so neither ever certifies, never compiles, and every call runs the full general (env) path in both engines. Depth caps: ~126 native frames jit / ~80 jl before the stack guard trips.

Verdict: recursive_fib (and by extension the recursion part of the method/call rows) is JIT-coverage — the L3 compile-the-general-path family (recursion, closures-with-env, try/catch, generators all funnel here) — NOT a bounded call-path slice. The one targeted alternative, certifying recursion by generalizing the fixpoint to admit strongly-connected components of never-assigned function-declaration names, is unsound without compiler support: the recursive body's own-name reference is a CallFastGlobal that validates the live global generation (observing the global defeats the global-blind premise), so it would need the callee bound to its own stable entry record instead. The mutual-vs-self A/B shows no headroom is visible until that lands — both shapes sit at the same general-path cost today.

### PROBE: coercion_concat — expression-position string concat runs the STEP path (jit ≈ jl); the arena box is ~32ns, NOT the lever (corrected 2026-09-09, no code)

The `strings/coercion_concat` row (300k `t = "value=" + i + ":" + (i*2)` builds, ~570ns/iter jit, jit ≈ jl) decomposes two ways. First, allocation: a temporary env-gated TLS counter in `Gc::new`/`new_in_place` (since removed) counted boxes for isolated 200k loops — `"" + i` ~1.0 boxes/iter, `"value=" + i` ~2.0, the full 3-term chain ~5.0 (2 number-coercion boxes + 3 concat-result boxes). Second, a release-mode crux Rust probe of the allocator itself (temporary test, since removed) measured the REAL per-box cost: a tiny box (header + word) ~15ns, a Small `JsString` box ~32ns, and the OWNED Small construction ~1.3ns — so the initial estimate (~75-110ns/box from row deltas) conflated the box with the surrounding op machinery; the box is NOT the dominant term.

Timed file-path A/B (2M iters, jit): the number-add floor (no allocs, compiled) ~2.5ns/iter; `i + ""` (1 box) ~175ns; `"value=" + i` (2 boxes) ~250ns; the full chain (5 boxes) ~570ns; ALL of the concat shapes run jit ≈ jl, while the pure-number control compiles at 5x. The statement-position fused compound `s += i` (the concat_loop row) is ~42ns/iter jit — so a string+number coercion+concat in a REGISTER op is cheap; the EXPRESSION-position chain (`t = i + ""`, `t = "value=" + i + ...`) never reaches that fused form — it runs the step path in both engines, ~30-60ns per binary op plus the native concat boxes (~32ns each). Node does the same chain in ~25ns/iter (cons nodes, 2-4ns nursery boxes).

Verdict (corrected): the lever is EXPRESSION-position string-concat register-run coverage (both engines), not the arena box (which is ~15-32ns and already reasonable) and not formatting. The row's target: if the 3-op chain lowered like the fused statement compound, ~570ns -> ~150-200ns/iter (~3x). This is the same coverage class as the destructure row (jit ≈ jl) and the falsified object-literal register-run fusion (2026-09-07) — that precedent measured a net REGRESSION when the fused body merely re-dispatch-matched more ops than it saved, so this slice MUST be probed for net effect before landing (the fused form only wins if the concat ops it absorbs are themselves step-dispatched today, which the jit ≈ jl spread here indicates).

### LANDED (2026-09-09): JsString::from_utf8 encodes small strings inline — the number→string Vec tax removed

The probe's decisive follow-up found the real lever: `compound1` (statement-position `t += i`) was NOT cheaper than the expression form, and even a PURE `String(i)` coercion ran ~160ns/iter — independent of concat or register-vs-step. A release-mode crux Rust probe pinned it: `JsString::from_utf8` built an UNCONDITIONAL `Vec<u16>` (a malloc/free per call) even when the result fit the 16-unit inline `Small` form — 70.6ns vs 8.3ns for the equivalent `from_utf16` slice path, and `number::to_string` of an integer was 76.4ns of which that Vec was the mass. Every number→string coercion (`String(i)`, `i + ""`, template substitution, `toString`) ended in this constructor.

Fix (`crates/crux/src/string.rs`): `from_utf8` now encodes UTF-16 DIRECTLY into the inline `Small` buffer, counting units; only an input longer than `SMALL_STRING_CAP` (16) falls back to the Vec (whose buffer the Flat path still adopts, `Arc::from(Vec)`). Small strings allocate nothing beyond the eventual box. Rust probe after: from_utf8 70.6 -> 10.2ns, num_to_string 76.4 -> 15.1ns. A/B (release, 2M iters, jit): `String(i)` ~342 -> ~211ms, `t = i + ""` ~335 -> ~201ms, the two-coercion chain ~902 -> ~634ms; corpus `coercion_concat` jit ~159 -> ~122ms (~1.3x) / jl ~165-186 -> ~140-150 (~1.2x); every other corpus row within noise (char_ops ~30, concat_loop ~8, search_slice ~22-25, split_join ~115-120). Correctness: a new crux boundary test (16 units incl. 8 astral pairs -> Small, 17 -> Flat, long mixed content parity vs from_utf16, empty); the existing from_utf8/from_utf16 content-agreement tests still pass. Gates: clippy `-D warnings` clean; workspace green (32 suites, crux 233); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok (strings mean-jlGap 10.8 -> 7.5).

Open (measured, not started): the coercion row's residual (~120ms jit) still pays ~1 box + the per-op concat machinery per term (~100ns/coerced term — node ~25ns/iter for the whole chain); the remaining gap is the Small-box + concat dispatch floor, not formatting.

### LANDED (2026-09-09): JSON.stringify assembles UTF-16 — the per-member UTF-8 round-trip and format!/String parts removed

The `builtins/json_roundtrip` corpus row (15000 object-build + stringify + parse round trips, ~190ms jit / ~186ms jl, jit ≈ jl) decomposed: object-literal create ~0.9us/iter, JSON.stringify of the stable object ~5.4us, parse ~3.3us (sum ≈ the 11.3us/iter full row; node ~0.6us/iter total). Stringify scaling: ~1.6us fixed + ~0.4us/key per call (node 0.07 + 0.02). The serializer's assembly was built on `Vec<String>` parts + `format!` + repeated `to_string_lossy` — every key was converted UTF-16 → UTF-8 (Rust String alloc) in the loop, re-boxed to a fresh `JsString` inside `serialize_json_property`, quoted then lossy-converted back, and each object/array `format!`-joined a Rust String that `from_utf8` re-encoded to UTF-16.

Rewrite (`crates/runtime/src/builtins/json.rs`): `serialize_json_property` takes the property key as the `JsString` it already is (no per-key lossy/re-box); the object/array serializers assemble their member text DIRECTLY in UTF-16 units — a `quote_into` appends the JSON-quoted key/value, the serialized child `JsString`s extend the buffer by their unit slice — dropping the `Vec<String>` parts, the `to_string_lossy` calls, and the `format!` joins; the result is one `JsString::from_utf16` per container. The gap/indent (pretty-print) path is handled in the same u16 assembly. Also fixed a latent spec bug the diff exposed: an EMPTY array with a gap argument now serializes to `[]` (spec 26.6.3.5 step 4; the old pretty path emitted an empty indented block) — matched against node.

A/B (release, 15k iters, stringify scaling): flat-1 24 -> 19ms, flat-4 43 -> 26ms (~1.65x), flat-8 64 -> 35ms (~1.8x), str-4 47 -> 30ms, the corpus-shaped stable object 87 -> 63ms; corpus `json_roundtrip` jit ~190 -> ~130-135ms (~1.4x) / jl ~186 -> ~131-142 (~1.3x); every other corpus row within noise. Correctness: a 32-case node differential battery (escape cases incl. lone surrogates/astral pairs/control chars, number/bool/null/symbol/undefined forms, nesting, space gap 0/2/4/tab/string/>10, empty containers with a gap, replacer fn + array, toJSON + its key arg, integer-key ordering, BigInt TypeError) diffs byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the parse half of the round trip (~3.3us/iter, untouched) and the stringify residual (~4.2us/iter on the corpus object, still paying a fresh-key/property `get_property` + toJSON probe per serialized object) are the remaining json_roundtrip mass.

### LANDED (2026-09-09): JSON.parse skips the reviver record tree without a reviver, dense-defines arrays, and fast-paths escape-free strings

The parse half of the round trip (~3.3us/iter fixed-text) had three avoidable costs for the common NO-REVIVER call: (1) the parser always built the full `ParseRecord` tree the reviver needs — a `source` JsString copy per primitive (from the byte range), a `entries` Vec of (key-JsString, record) per object (cloning every key), and an `elements` Vec per array — all discarded when `JSON.parse` returns `record.value`; (2) array elements were defined one at a time with a fresh `index.to_string()` string key through `create_data_property_or_throw`, re-paying the slow define the split/join landing had removed elsewhere; (3) `parse_string` always built a `Vec<u16>` and decoded unit-by-unit, even for keys and typical values with no escapes.

Changes (`crates/runtime/src/builtins/json.rs`): a `records` flag on `JsonParser` (set from `is_callable(&reviver)`) gates the scaffolding — no-reviver parses record no primitive `source`, collect no object `entries`, and collect no array element records (values go to a local `Vec<Value>`); array elements are then defined DENSELY on the pre-sized array via `create_data_property_index` (mirroring `array_from_values`, no index-string keys) in both modes; and `parse_string` fast-paths a segment with no escape/multi-byte/control byte straight to `JsString::from_utf8` of the byte range (one constructor, no unit loop). `validate_json` and the rawJSON primitive check set `records: false` (validation-only).

A/B (release, 15k iters, jit): parse-only 49 -> 37ms (~1.32x, ~3.3us -> ~2.5us/iter), parse-var 35 -> 32ms; corpus `json_roundtrip` jit ~190 -> ~121-122ms / jl ~186 -> ~120-122 (cumulative ~1.55x with the stringify landing); stringify/create rows flat. Correctness: a 40-case node differential battery (number/string/escape/lone-surrogate/unicode shapes, nesting, duplicate keys, empty containers, 15 syntax-error forms each throwing SyntaxError, and the full reviver path: number/array transforms, undefined deletes in objects AND arrays, key-aware revivers, the context-object `source` for primitives incl. string forms, root-call semantics, nested reviver) diffs byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang — incl. the JSON-module fixtures that share `validate_json`); corpus parity 37/37 ok.

Open (measured, not started): the parse residual (~2.5us/iter) is now the per-key fresh-object `create_data_property` defines plus the whole-input UTF-16 -> UTF-8 conversion (`to_string_lossy().into_bytes()` per call) and number/string value construction — a UTF-16-native parser or a parse-into-the-final-shape fast path would be the next terms.

### LANDED (2026-09-09): the JSON parser is UTF-16-native — no per-call lossy UTF-8 conversion, raw surrogates preserved

Rewrote `JsonParser` to operate on the input's UTF-16 UNITS (`text: &[u16]`) instead of a per-call lossy UTF-8 byte buffer (`text.to_string_lossy().into_bytes()`). JSON's grammar tokens are ASCII, so every byte match carried over as the equal unit value (shared u16 consts; patterns cannot apply casts). Parsed STRING content is now produced from unit slices: the escape-free fast path is one `from_utf16` over the range, the slow path pushes raw non-ASCII units VERBATIM (an astral char is already a surrogate pair; a raw lone surrogate is legal JSON string content and now survives), the reviver's primitive `source` is the raw unit range, and numbers convert from the unit token. `json_parse`/`json_primitive_value` pass `text.as_slice()`; `validate_json` encodes once per JSON-module resolve.

This also FIXES a latent correctness bug the lossy conversion caused: a raw (unescaped) lone surrogate in the JSON source string was replaced with U+FFFD by `to_string_lossy` on the way in (escaped `\uD800` forms worked); a node-differential surrogate battery (raw lone high/low surrogates as values AND keys, raw astral chars, mixed) now matches node exactly.

A/B (release, 15k iters, jit): parse-only 37 -> 34ms, parse-var ~31 (flat); corpus `json_roundtrip` jit ~121 -> ~114-115ms (cumulative ~1.65x from the original ~190 with the stringify + parse landings; parse-only cumulative ~3.3us -> ~2.3us/iter). The conversion removal is a small share of the parse residual — the per-key fresh-object defines dominate. Correctness: the 40-case parse battery, the 32-case stringify battery, and a new raw-surrogate battery all diff byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang — incl. JSON-module fixtures through `validate_json`); corpus parity 37/37 ok.

Open (measured, not started): the parse residual (~2.3us/iter) is the per-key fresh-object `create_data_property` defines and the per-value construction (from_utf16 copies of every key/value segment) — the parse-into-final-shape fast path and zero-copy segment views (the input is a live `JsString`, so escape-free keys/values could be `slice_view`s of it) are the remaining terms.

### LANDED (2026-09-09): JSON number literals parse directly — the generic string->number dispatch per value removed

The remaining terms measured: JSON.parse of the corpus shape costs ~2.2us/iter while the equivalent object LITERAL is ~0.6us (create/defines already fast-pathed via `create_data_property_key`'s `fresh_data_define`; zero-copy `slice_view` keys only help >16-unit segments, absent here). The parse-text delta decomposed to the NUMBER VALUES: each JSON number built a JsString token + boxed it in a `Value::String` + ran the generic string->number parser (`string_numeric_literal`), ~5 numbers/parse in the corpus. `parse_number` now parses an INTEGER literal (no fraction/exponent) with at most 15 digits DIRECTLY from the units (u64 accumulation, exact since every integer <= 2^53 round-trips an f64; a leading `-` negates, so `-0` keeps its sign) — longer integers and any fraction/exponent form still need correct rounding and fall through to the generic conversion.

A/B (release, 15k iters, jit): corpus-shape parse ~2.2us -> ~1.6us/iter; parse of a 1-key object ~0.67us -> ~0.53us; number-heavy objects: 10-key ~3 -> ~1.7us, 40-key ~14.4 -> ~9.1us, 40-element integer array ~7 -> ~1.9us; parse_scale corpus 31 -> 22ms. `json_roundtrip` jit ~114-115 -> ~100-106ms (cumulative ~1.8x from ~190 with the stringify + parse landings) / jl ~103. Correctness: the 40-case parse battery (incl. `9007199254740993`, `1e21`, `-0`, `0.5`, `1e-7` falling through to the generic path) diffs byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok.

Open (measured, not started): the parse residual above the object-create floor is now the tokenizer/segment construction and the per-key string defines themselves (~0.5-0.9us/iter for the corpus shape vs node ~0.1us) — no single bounded native term left short of a full hand-tuned fast path.

### LANDED (2026-09-10): JSON builtins register O(1) and parse_string's fast path widens to any escape-free content

The "no single bounded native term left" close (2026-09-09) was premature on two counts. First, JSON was the ONE agent-dependent module never migrated to the O(1) builtin-handler table (`Intrinsics::define` chains `handler_for` for Array/RegExp/String/Number/Boolean/BigInt/keyed/Object/DataView/Date/typed-array but not JSON), so every warm `JSON.parse`/`stringify`/`rawJSON`/`isRawJSON` call ran `call_inner` -> `json::dispatch_call` -> `current_realm()` + `intrinsics.get(JSON_PARSE)` (a fresh-name string + HashMap probe per call). A bare `JSON.parse("12345")` measured ~150-200ns/call. Second, `parse_string`'s "native" fast path was ASCII-only — the scan broke on `unit > 0x7F`, so any escape-free NON-ASCII string (`"héllo"`, CJK, emoji — legal unescaped JSON content, copied verbatim by `from_utf16`) fell into the Vec-building slow path (the byte-oriented leftover): escape-free ASCII parse ~12-13ms vs the same content non-ASCII ~15-16ms/20k.

Fixes (`crates/runtime/src/builtins/json.rs`, `crates/runtime/src/realm.rs`): `json::handler_for` maps the four member intrinsics to their handlers, wired into the `Intrinsics::define` registration chain — a warm call dispatches the registered handler directly, skipping the realm + intrinsics probes. `parse_string`'s fast scan now breaks only on the closing quote, a backslash (escape), or a control unit (<= 0x1F, illegal unescaped); non-ASCII units (incl. raw lone surrogates and astral pairs) ride the single `from_utf16` over the range exactly as the slow path's verbatim pushes would.

A/B (release): bare-number `JSON.parse` 30k iters 5-6 -> 1-2ms (~150-200ns -> ~50ns/call); `[1]` 16-17 -> 12-13ms, `{a:1}` 15-17 -> 11-12ms, `[]` 13-14 -> 9-10ms; escape-free non-ASCII parse now EQUAL to ASCII (~10-12ms/20k both, was ~25% slower); corpus `json_roundtrip` parse-only ~26 -> ~22-25ms, full ~127 -> ~114-122ms. Correctness: the 40-case parse battery, the stringify battery, and the raw-surrogate battery (raw lone high/low surrogates as values AND keys, raw astral chars) all still diff byte-identical vs node in jit, jitless, AND --gc-stress. Gates: clippy `-D warnings` clean; workspace green (32 suites); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); corpus parity 37/37 ok.

### LANDED (2026-09-10): JSON container creates use the cached %Object.prototype% accessor

The parse-side container create was paying `intrinsics.get("%Object.prototype%")` — a fresh-name `JsString` + HashMap probe — PER object/array created inside a parse (the corpus text has 3 containers: outer, tags array, inner), when `intrinsics.object_prototype()` is a cached-field accessor (the array path already used its cached `array_prototype()`). Fixed `JsonParser::object_proto` to the cached accessor. A/B (release, per-process isolated): `{}` parse 60k iters 24-25 -> 13-14ms (~170ns/container saved), `{a:1}` 24 -> 10-11ms; corpus `json_roundtrip` parse-only ~22-25 -> ~17-18ms, full ~114-122 -> ~101-109ms. Gates as above (clippy clean; 32 suites green; three sweeps at baseline; corpus 37/37; parse + surrogate batteries jit/jitless/gc-stress).

Open (measured, not started): the parse residual above the object-create floor (~0.5-0.7us/iter corpus) is now the per-key defines and segment construction — a 16-key object parse measures ~1.63us vs a literal ~0.85us (~49ns/key above the literal), but the literal create itself is ~25x node's (the shared object-define machinery, not JSON-parse work). A key-path probe (interning the key's source range directly instead of building a JsString per no-reviver key) measured FLAT and was reverted: short JSON keys fit the Small inline form, so the string build was never the term.

### LANDED (2026-09-10): the compiled whole-simple object literal takes the interpreter's fused adopt (`ObjectFast`)

The Cut 72 landing gave the INTERPRETER a fused whole-literal create (`Step::ObjectFast`: one shape-fork + `adopt_vector_free_fields` + tail defines) but left the JIT expanding the same step into `ObjectBegin` + N per-key `ObjectInitName` helper calls — N+1 slow calls, each a full define through `create_data_property_key` (~60-70ns/key). On escape-forced literal churn loops (the object escapes into a ring each iteration) the compiled path therefore ran BEHIND the interpreter that now had the fused adopt: `scratch/lit_slope6.js` k16 (300k iters) measured jit ~314ms vs jitless ~208ms — the JIT was the slow engine on the very shape the interpreter had just sped up.

The fix reuses the interpreter's fused create for both engines. The interpreter handler's body was factored into a free `pub(crate) fn object_fast_create(agent, names, values)` (`runtime/src/ir.rs`) — create the ordinary object on the realm `%Object.prototype%`, fork the head shape through the cached child chain for the first `min(n, INLINE_FIELDS)` names, `adopt_vector_free_fields` them in one map set, then define the tail keys per-key (the same end-state the sequential path produces) — and the handler now calls it with the pushed stack slice. The JIT got ONE new helper, `object_fast(ctx, step, sp)` (`runtime/src/jit.rs`), which reads the `names` payload back from the running body (`step_at`) and the n values from the machine-code working region below `sp` (`from_raw_parts` over the rooted JIT buffer — the existing arg-pointer convention), calls `object_fast_create`, and returns the object; the compiled `Step::ObjectFast` lowering (`jit/src/compiler.rs`) passes the current `sp` and, after the call, drops the n consumed slots and pushes the result (new `sig_step_sp` = `(vm, step, sp)`). The standard four-file helper mirror (`helpers.rs` enum/name/field/`none`/`get`/double, `lib.rs` `runtime_helpers`/`helpers_all`, the runtime `JitSlowPaths` field + `JIT_SLOW_PATHS` entry + completeness assert) and a scaffold lowering test were added; the helper defaults to disturbing leaf eligibility, matching `ObjectBegin`. No GC concern: the whole `work` region is a root for the run (`with_jit_run`), so a shape-fork allocation under the live values is safe.

A/B (release, same-machine; the pre-change reference is the in-session `lit_slope6` k16 ~314ms jit / ~208 jl): after the fusion the JIT is FASTER than the interpreter on every slope shape and converges only at k32 (where both bottom out on the vector-spill tail). `scratch/lit_slope6.js` (300k iters, ms per row): jit k1 40-41 / k4 68-70 / k8 97-102 / k16 158-160 / k17 407-417 / k20 688-692 / k32 1460 vs jitless k1 58-65 / k4 97-103 / k8 134-161 / k16 200-218 / k17 461-501 / k20 748-756 / k32 1448-1529. `scratch/lit_cross.js` (200k iters, ring reuse): jit k1 31 / k2 33-36 / k3 37-40 / k4 43-44 / k6 53 / k8 63-68 / k12 79-83 vs jitless k1 42-48 / k2 47-61 / k3 51-58 / k4 55-62 / k6 66-73 / k8 76-83 / k12 95-101. `scratch/lit_ctl2.js` controls show the same flip (k1 43-45 vs 62-63, k2 51-52 vs 69-70) with the empty `{}` (not a fused literal) and the no-create ring loop flat. `--jit-bench` 13 rows all healthy (in the recorded ranges; no row regressed; typed-array write ~0.84 -> ~0.40 ratio this run).

Correctness: the >4-key battery (`scratch/objfast_gt4_battery.js`) and the objlit semantics battery (`scratch/objlit_semantics.js`) are byte-identical vs node in jit, jitless, AND --gc-stress; a compiled-churn checksum probe (`scratch/objfast_jit_gc.js`: 1/5/8/16/17/20-key loops, N=20000) is identical across jit, --jitless, and node under --gc-stress (the compiled fused path reads live values from the rooted work buffer while the shape fork allocates). A new scaffold test (`object_fast_lowers_to_the_fused_helper`) proves the emit path through the test double, and a new e2e (`installed_jit_fused_object_literal_matches_the_interpreter`) drives a compiled loop creating a 5-key and a 20-key literal per iteration (field values, 20-key enumeration order, grow-after define) and asserts JIT == interpreter. Gates: clippy `--workspace --all-targets -D warnings` clean; `cargo test --workspace` green (jit 185); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang).

Next: the remaining literal-create mass is the per-create SHAPE FORK (`get_or_create_child` chain walk, even cached) plus the fresh `JsObject` box — the L1c/Option-3 machine-addressable storage is the end-state that would let the JIT emit the create inline; pick by probe.

### LANDED (2026-09-10): FxHasher mixes whole integers — the map-transition lookup drops 21.5 -> 4.5ns (the create-path fork cost)

The fresh corpus scan after the fused-`ObjectFast` landing still showed `objects/destructure` (2 fused literals + 4 reads per iteration) at jit ~227 / jl ~310ms — only 1.36x JIT-over-interpreter, the weakest object row, i.e. the create itself dominates and is now shared by both engines. A throwaway `runtime` example split the per-create cost: realm+proto resolution 2.1ns, `canonical_empty_map` 2.9ns, `ordinary_object_create` ~118ns (page-fault inflated; the in-suite create base is ~75-95ns), `ordinary_object_create_with_map` 101-158ns (skipping the empty-map lookup saves only ~4ns), `size_of::<JsObject>()` = 504B, and — the surprise — a *warm* `Map::get_or_create_child` (the shape-fork step the fused create runs once per key) at **21.5ns**. That lookup, not the box, is the ~24ns/key term the `objfast_create_only` slope had shown.

This CORRECTS the 2026-09-07 "L4 RE-OPENED" slice premise, which pointed at trimming the per-object realm/proto + canonical-empty-map resolution: those three pieces are ~5ns TOTAL. The real per-create costs are the ~75-95ns box init and the 21.5ns transition lookup.

Root cause of the 21.5ns: `FxHasher` implemented only the byte-wise `write` and `write_u64`. The `Hasher` trait's default integer methods (`write_u32` for the atom id, `write_u8` for `MapAttrs`, `write_usize` for a derived discriminant or a GC address) all forward to `write(&n.to_ne_bytes())` — a byte-at-a-time rotate-multiply loop, 4-8 rounds per integer. The fix adds whole-word `write_u8`/`write_u16`/`write_u32`/`write_u64`/`write_usize` (one rotate-multiply round each) via a shared `mix`; the byte-slice `write` is unchanged (still the FxHash byte loop for strings). Consumers are `Map::transitions` and the GC's `AddrMap`/`AddrSet` — non-adversarial, process-local keys (atom ids, attrs, aligned addresses), no persistence or format implication, so a hash-value change is inert; determinism per process is all that is required.

Measurement: warm `get_or_create_child` **21.5 -> 4.5ns**. Create-only probe (`scratch/objfast_create_only.js`, 300k iters, jit / jl): c16 133 -> 57-63 / 156-161 -> 78-85ms (~2x), c8 75-76 -> 39-46 / 89-90 -> 51-52, c4 47 -> 28-32, c1 flat (~27-38, box-dominated), `c4c` (constant values) == `c4` (the value expressions were never the term). Corpus (fresh scan vs the immediate pre-change scan): `objects/destructure` jit 227.4 -> 174.5 / jl 309.6 -> 234.2 (-23%/-24%), `objects/spread_assign` 163.1 -> 142.9 / 186.2 -> 164.5 (-12%), `control/generator_loop` jit 199.1 -> 178.0; `calls/construct_churn` (~117/173) and the closure rows are FLAT — constructors and closures use the cached pre-forked boilerplate shape and the presized in-place store, so they never fork per key; `builtins/json_roundtrip` ~flat (its define path is a small share). objects family mean-jlGap 3.78 -> 3.64, corpus parity 37/37.

Gates: clippy `--workspace --all-targets -D warnings` clean; `cargo test --workspace` green (jit 185); eight semantic batteries (`objfast_gt4`, `objlit_semantics`, `construct_fastpath`, `vecfree`, `fn_boilerplate`, `slice`, `json_parse`, `json_stringify`) byte-identical vs node across jit, `--jitless`, AND `--gc-stress` (the GC address-map hashing changed, so the stress coverage is load-bearing); three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang); `--jit-bench` 13 rows healthy. The throwaway split example was deleted after recording these numbers.

Next: the box init (~75-95ns; `JsObject` = 504B) is now the dominant per-create cost — the pieces it contains (`init_ordinary`'s ~17 writes, incl. the 128B `in_fields` uninitialized-marker fill that the fused adopt immediately overwrites) are the remaining create lever; a smaller/faster initializing fresh-object box is the next probe. The >16-key literal tail-define materialize cliff (k17, `lit_slope6`) also remains.

### PROBE (2026-09-10): the fresh-object box is memory-footprint-bound at 504B — no write-strategy lever, only a smaller struct

The follow-up to the `FxHasher` landing's "next" (a smaller/faster-initializing box). A throwaway `runtime` example timed `Gc::new_in_place` with payloads of increasing size (the same in-place path `init_ordinary` uses), a 504B template-memcpy variant, and the real `ordinary_object_create_with_map` (release, 100k iters): gc 8B 15.8-16.4ns, gc 64B 26.2, gc 256B 53-59, gc 504B 82.1, gc 504B memcpy 86.4, `ordinary_object_create_with_map` 89.6-103.9. The template memcpy is NOT faster — the write side already streams at the memory rate, and the memcpy adds a 504B read on top. So the create cost is a ~16ns allocation floor (`with_heap_mut` TLS + free-list/bump + header + register + live-list push + stress check + the alloc counter) plus ~0.12-0.13ns/byte of touched payload: the 504B `JsObject` costs ~66ns of writes/footprint. Only a SMALLER struct reduces it — every 64B removed is ~8ns/create.

Struct breakdown (`size_of::<JsObject>()` = 504B): `in_fields` 128B, `properties: RefCell<SmallProps>` ~136B (the 2 inline `(PropertyKey, Property)` entries ~96B + `len` + heap `Vec`; the inline `MaybeUninit` array is NOT written by `SmallProps::new`), `store_chain_clean` 32B, `property_index` ~48-56B, `private_elements` 32B, `kind` + the six hot `Cell`s ~64B, and ~8 assorted 8B handles/cells. The candidates, all trade-offs rather than bounded slices (no code lands):

- **`in_fields` 16 -> 8** saves 64B (~8ns/create) and costs the compiled read for ordinals 8-15 (inline ~22ns -> the map-slot helper ~30ns) — but the measured corpus has NO shape above 4 keys, so the cost is synthetic-only. This probe also CORRECTS the 2026-09-09 slices-1/2 "no bloat regression" note, which measured the `--jit-bench` rows (not create cost): the 4->16 raise added 128B = ~16ns to EVERY object create.
- **Cold-field boxing** (`property_index` ~48B, `private_elements` 32B, `store_chain_clean` 32B) is ~112B -> ~14ns/create, at an indirection on the rare paths that use them (property-index lookups; private elements; the Array element-write chain verdict).
- **`SmallProps` inline capacity / `Property` size** (the ~96B inline entries) would shrink the struct but regress the small-property-vector objects (a heap `Vec` per object).

Disposition: probe-only. The create floor is a struct-size decision; the objects family is already the lowest-jl-gap family (mean 3.64, destructure improved ~23% by the previous landing), so a shrink needs a probe-justified hot row before it is worth the trade. Recorded so the next create-side attempt starts from the footprint number rather than the byte-count-free "no bloat regression" reading. The throwaway example was deleted.

### PROBE (2026-09-10): the arithmetic loop is FLOOR-bound — compiled loop floor ~2.5ns/iter vs node's ~0.5, and the ops are nearly free

The `arithmetic` row (interp 11.3 / jit 2.48ms at 1M; ratio 0.22) is ~5x node's compiled code. Re-measured directly on the same source (`tools/jit_bench/arithmetic.js`): slag jit 2.50ms, node jit 0.50ms, slag jitless 12.50ms, node jitless 9.90ms. Decomposition (N=2M iters, ms, jit): sandbox `const0` (`s += 0`) 5, `add` (`s += i`) 5, `mul2` 6, `twoadd` 7, `muli` 8, slot-limit 5; node `const0` 1, `add` 2, `mul2` 2, `twoadd` 3, `muli` 2. So the loop FLOOR (head + safepoint + counter + one tagged add) is ~2.5ns/iter in Slag vs ~0.5ns in node (~5x), while each extra Number op costs only ~0.25-0.75ns (~2-3x). The row sits essentially ON the floor — optimizing the arithmetic ops would barely move it.

The `JIT_DUMP_CLIF=1` dump shows why: the per-iteration hot path is ~40 instructions across 6-8 basic blocks with ~6-8 branches. Three components: (1) the **GC safepoint poll** at the loop head (ctx `gc_ticks` load/dec/store/cmp/branch + a jump, ~6 ops); (2) the counter add + limit compare + branch; (3) for EVERY arithmetic op a **NaN-box tag check + canonicalization** — each operand gets `and mask; cmp TAG_PREFIX; cmoveq canonical` (~3-4 ops) plus a branch to an (untaken) slow helper, repeated for the `i * 2` product and the `n +=` sum. The counter is already a raw f64 Cranelift variable, but the register-op body re-tags each operand into a `Value` and re-checks/re-canonicalizes it.

Verdict: NOT an architectural redesign. The NaN-boxed `Value` representation is fine (V8 uses tagged values too). What is missing is a **type-specialized loop lowering**: when operands are statically Number (the compiler already proves this for the counter via `for_init_counter_number`/`acc_expr_safe`, and for `number`-typed slots), keep the accumulator + counter as raw `f64`/xmm registers across the loop and emit bare `fmul`/`fadd`, dropping the per-op tag check and canonicalization; plus amortize the safepoint (a register tick + a poll every N, or a single-load poll) and tighten the branchy test/merge structure. That is a bounded JIT codegen project — the `FastLoopVar::Counter` accumulator path is the existing template — not a redesign.

Side finding (unrelated to this row; the loop-lowering code is untouched by the recent landings): a certified function with an EMPTY loop body HANGS the JIT — `function bare(n) { for (var i = 0; i < n; i++) {} } bare(1000000)` never returns, while `--jitless` and any non-empty body work. The CLIF shows the empty body's `body_start` collapses onto the `FastLoopHead`'s own step/block, so its back edge re-enters the head with the pre-increment counter (the increment is never written back) and the test never fails. A fixture-free latent hang, fixed the same day (see the FIXED entry below).

### LANDED (2026-09-10): known-Number register operands drop their tag checks — slice A of the type-specialized loop lowering

The first cut of the arithmetic-loop floor work. The register executor
(`crates/jit/src/compiler.rs`) now tracks whether its accumulator holds a
canonical `Value::Number` by construction (`Lowerer::acc_is_number`), and
`emit_binary` takes an operand-knownness pair (`emit_binary_known`). A known
operand's `is_double` check is dropped; when BOTH operands are known the
arithmetic fast result is returned with no slow block and no `brif` at all.
The `i * 2` of `n += i * 2` (accumulator from `LoadCounter`, right a
`Value::Number` constant) therefore lowers to a bare `fmul` + canonicalization.

Provenance is per-`RunRegBody`: reset at the step boundary and at the
register-body entry (which seeds the accumulator with `undef`), set by
`LoadCounter`/`LoadConst(Number)`/arithmetic results, cleared by `LoadReg`,
member reads, and comparisons — `binary_yields_number` only yields Number for
the `InlineBin::Arith` shapes on two Numbers. The string boundary needs no
special case: `i + '!'` has a non-Number right operand and still takes the
concat slow path. JIT-only — the interpreter's `run_leaf_ops` and the step path
are untouched.

Measured on `--jit-bench`'s `arithmetic` row (release, min-of-batched-samples,
4 runs each side): baseline 2.489 / 2.459 / 2.450 / 2.479ms, slice A
2.368 / 2.359 / 2.351 / 2.326ms — ~2.47 -> ~2.35ms (~4.5%). As the FLOOR probe
predicted, one of the two bin ops losing its branch moves the floor only a
little: `n += ...`'s `BinStoreReg` still checks its slot-left operand.

Verification: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` 4775 pass / 0 fail (incl.
`installed_jit_known_number_operands_match_the_interpreter`). Uncommitted.

### LANDED (2026-09-10): the JIT-only loop-carried Number slot — slice B Route A of the type-specialized loop lowering

Slice A removed the tag checks for operands the compiler already knows are
Numbers. The remaining body cost in `arithmetic`/`bare loop` was the slot-left
operand of the reduction: `n += i * 2` lowered to `BinLeftReg`/`BinStoreReg` on
an untyped frame slot `n`, re-checked every iteration. Slice B is the
loop-carried half of the plan — keep `n` in a raw f64 register for the loop's
duration and sync it to the frame only at the loop edges, the existing
`loop_counter` / `FastLoopBind` / `FastLoopStore` template generalized.

Target shape (probe of `--print-bytecode` on `var n = 0; for (var i = 0; i < 3;
i++) { n += i * 2; }`): the loop body is exactly one run,
`RunRegBody { ops: [LoadCounter, BinImm { op: Mul, imm: 2.0 }, BinStoreReg { op:
Add, slot: 0 }] }`, with `n` = slot 0 and `i` = slot 1, and the only reference
to slot 0 in the body IS that RMW. The Number init (`var n = 0`) is visible in
the step stream as `Push(Number(0.0))` + `InitLocal { slot: 0 }` just before
`FastLoopBind`. Both hot rows (`arithmetic`, `bare loop`) have this shape.

The loop-carried Number slot landed in two cuts. The first was JIT-only; the
second moved the proof into the compiler and added the interpreter op, so
`--jitless` runs the same specialization. The JIT-only planner was deleted
when the compiler took over.

Common restriction (both cuts). A slot `s` is specialized only when:

- every reference to `s` in the loop body is the statement-position RMW
  `s op= <number-provable>` — no other read or write (a read or a step-path
  access would observe the stale frame slot mid-loop);
- the RMW's op is arithmetic and its right operand (the accumulator) is
  Number-provable, so the f64 result is exact; and
- `s` is a Number at loop entry (the route-specific proof below).

Neither route builds a runtime entry guard or a versioned (specialized +
generic) body — the entry value is proven at compile time.

#### Cut 1 (LANDED 2026-09-10, JIT-only — planner since superseded)

The first cut did the candidate scan and the entry proof inside the JIT
(`plan_num_slot` + `Lowerer::num_slots` in `crates/jit/src/compiler.rs`), so the
interpreter kept the generic `BinStoreReg` and `--jitless` was untouched. Cut 2
(below) moved the proof into the compiler and deleted the JIT planner.

- Candidate: a `FastLoopHead { var: Counter }` whose body is exactly one
  `RunRegBody` step (`head == body_start + 1`), containing exactly one
  arithmetic `BinStoreReg { op, slot }` and no other op referencing that slot;
  `!has_try && !has_suspension` (a throw or suspension escaping the loop would
  skip the flush, and a suspension would not carry the register).
- Entry proof: let `w` be the last step index `< body_start` that writes the
  slot; require `steps[w - 1]` is `Push(Number(_))` and `steps[w]` writes the
  slot (`InitLocal`/`StoreLocal`/`FusedStoreLocal`); and require that no step
  in `[0, body_start)` targets an index `< body_start`, no unconditional
  `Jump`, and no `Return`/`Throw`/`Break`/`Continue` — so the prefix to `w` is
  straight-line and `w` executes exactly once with the Number just pushed.
- Codegen: one `declare_var(types::F64)` per candidate, seeded at its
  `FastLoopBind` from the frame slot, lowered as `var = fop(var, bitcast(acc))`
  with `acc` re-canonicalized, and flushed (`store_slot(slot,
  canon(bitcast(var)))`) at the top of the `FastLoopHead::after` block — the
  single merge of the normal exit, the zero-iteration initial test, and every
  `break`.

At this cut the interpreter kept running the generic `BinStoreReg`, so
`--jitless` (and any un-specialized run) was unaffected; the whole change was
`crates/jit/src/compiler.rs`.

Measured on `--jit-bench` (release, two runs):

| row | before (slice A) | after (slice B) | ratio |
|---|---|---|---|
| `arithmetic` | 2.33-2.37ms | **1.17ms** | 0.18 -> 0.08 |
| `bare loop` | 2.30-2.34ms | **0.75-0.78ms** | 0.20 -> 0.06 |

That is the body's last branch gone: both rows now run as raw f64 register ops
with no body tag check, no body slow block, and no body branch; the remaining
cost is the loop head (slice C). The win is far larger than the FLOOR probe's
"the ops are nearly free" estimate because what was removed was the branchy
block structure, not the ALU ops. Node's `arithmetic` is ~0.50ms, so the row is
now ~2.3x node (from ~5x).

Verification: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` 4776 pass / 0 fail (incl.
`installed_jit_loop_carried_number_slots_match_the_interpreter`, which also
guards the shapes that must NOT specialize: a conditional entry write and a
String-init slot); test262 sweeps at baseline — language 23721/0/3, annexB
1086/0/0, built-ins 23657/0/155, zero fail/crash/hang. Uncommitted.

#### Cut 2 (LANDED 2026-09-10) — the compiler plan + the interpreter op (Route B)

`Compiler::plan_loop_num` (`crates/runtime/src/ir.rs`) runs once, right after an
acc-path loop is fully emitted, doing the candidate scan and entry proof (the
JIT's former planner, moved) on the step stream. On success it marks the loop
(`Step::FastLoopBind`/`Step::FastLoopStore` gain `num: Some(slot)`) and rewrites
the RMW op to `LeafOp::BinStoreNum { op, slot }`.

- **IR**: `Vm::loop_num` (f64, the `loop_counter` sibling); the two new step
  fields and the one new op.
- **Interpreter**: `fast_loop_bind`/`fast_loop_store` seed/flush `loop_num`, and
  the `run_leaf_ops` arm does `loop_num = loop_num op acc; acc = result`.
- **JIT**: discovers the marked slot from the steps, keeps it in an f64 register
  (seeded at the bind, flushed at the store) and lowers `BinStoreNum` to the
  same `fadd`/`fmul` (+ canon) code it emitted before.
- **The entry proof changed shape**: it no longer resolves jump targets (at this
  point they are still unpatched label ids, and the initial test's target is a
  fixup placeholder) — it requires the already-emitted prefix to be
  straight-line (`prefix_is_straight_line`: no step with a jump target, no
  control exit), which is the property that makes the entry init execute
  exactly once unconditionally. The loop's own initial test sits after the
  bind, so it is outside the checked range.
- **Confinement unchanged**: the register run must reference the slot only in
  the RMW, so the stale frame slot is never observed mid-loop.

Measured on `--jit-bench` (release; plan-on vs plan-off, since the plan now
drives BOTH engines and "off" is the pre-Route-B lowering):

| row | `--jitless` off-plan | `--jitless` on-plan | JIT (either) |
|---|---|---|---|
| `arithmetic` | ~14.2ms | **~12.6ms** | 0.60-0.64ms |
| `bare loop` | ~11.8ms | **~9.9ms** | 0.67-0.69ms |

So `--jitless` gains ~11% / ~16% on the two rows, and the JIT keeps its
loop-carried-slot win (0.6ms vs 2.34ms with the plan disabled). The JIT's
`bare loop` reads ~0.07ms above the pre-Route-B build; the generated loop is the
same shape (one `fadd` + the safepoint probe + the counter head), so the delta
is build/code-layout variation, not a code-shape regression.

Verification: clippy `--workspace --all-targets -D warnings` clean; `cargo test
--workspace` 4777 pass / 0 fail; six test262 sweeps at baseline — language
23721/0/3, annexB 1086/0/0, built-ins 23657/0/155 with the JIT AND with
`--jitless`, zero fail/crash/hang. The four structural eval tests that pinned
`BinStoreReg` now accept `BinStoreNum` too. Uncommitted.

### LANDED (2026-09-10): the loop head — slice C of the type-specialized loop lowering

With slice B the body was nearly free, so the head dominated. `JIT_DUMP_CLIF`
on `function f() { var n = 0; for (var i = 0; i < 5; i++) { n += i * 2; } }`
showed **two GC safepoint probes per iteration** (the `FastLoopHead` block and
its own `body_start` block, which `back_targets` also probes) and **three
`band`/`icmp`/`select` canonicalizations** — the `LoadCounter` value, the `i *
2` product, and the `n +=` result were each round-tripped through a NaN-boxed
`Value` even though nothing consumed them as a `Value`. Three changes
(`crates/jit/src/compiler.rs`):

1. **Duplicate probe removed.** `emit_all` now records each `FastLoopHead`'s
   `body_start` whose SOLE back edge is that head (`probe_suppressed`); the
   head's own block polls every iteration, so the body's second probe is
   skipped. (Measured `arithmetic` 1.17 -> ~1.00ms, `bare loop` 0.75 -> ~0.60ms.)
2. **Deferred `Value`-bits for a Number accumulator.** A second register
   (`acc_num_var: F64`) holds the accumulator's live numeric form when it is a
   known Number; `acc_bits()` materializes the canonical `Value` only when a
   consumer needs it (a member op, `StoreReg`, `PushAcc`, `ReturnAcc`). The
   arithmetic chain stays in f64 end to end (`BinForm::Num` operands, no
   per-op canon). (Measured `arithmetic` ~1.00 -> **0.607ms**, `bare loop`
   ~0.60 -> 0.607ms.)
3. `cond_jump_i8` — the fused head test passes its `fcmp` I8 straight to the
   branch instead of `uextend`+`icmp`; measured neutral, kept as a
   simplification.

The hot loop is now ~2 flops (`fmul`/`fadd`) plus one safepoint probe and the
counter inc/test, per iteration.

| row | probe (2026-09-10) | slice A | slice B | slice C |
|---|---|---|---|---|
| `arithmetic` | 2.50ms | 2.35 | 1.17 | **0.607ms** |
| `bare loop` | 2.30 | 2.32 | 0.75-0.78 | **0.607ms** |

Node's `arithmetic` is ~0.50ms, so the row went from ~5x node to **~1.2x** (a
~4.1x cut from the probe). The remaining gap is the per-iteration GC safepoint
poll (load/dec/store/cmp/branch); a register-held tick was tried next and lost
(see the NEGATIVE PROBE below).

Verification: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` 4777 pass / 0 fail (incl.
`installed_jit_deferred_number_accumulator_matches_the_interpreter`); test262
sweeps at baseline — language 23721/0/3, annexB 1086/0/0, built-ins
23657/0/155, zero fail/crash/hang. Uncommitted.

### NEGATIVE PROBE (2026-09-10): the register-held GC tick is SLOWER — the safepoint stays memory-based (reverted)

The slice C "next micro-lever": hold the loop's GC tick in a Cranelift register
for the specialized reduction loop (`plan_num_slot`'s candidate), decrement it
per head iteration, and touch `ctx.gc_ticks` only on underflow — seeded at the
loop's `FastLoopBind` and written back at the loop exit. The correctness story
held (the body is one register run, so the loop's own poll keeps collections
paced while the ctx field is stale; the exit flush re-syncs it), and the change
compiled and ran.

Measured (release, two runs): `arithmetic` 0.607 -> 0.756 / 0.771ms,
`bare loop` 0.60 -> 0.678 / 0.684ms — ~20-25% SLOWER. The memory probe's
load/dec/store ride a pipe off the branch's critical path, while the register
tick adds a loop-carried `Variable` chain (the underflow block's `def` forces a
phi at the head, and the loop-carried def/use lengthens the recurrence) plus an
extra `icmp` + `brif`. Reverted; the per-iteration `ctx.gc_ticks` probe remains.
Recorded so a later attempt at the head floor starts from this measurement
rather than re-tripping on it.

Verification after the revert: clippy `--workspace --all-targets -D warnings`
clean; `cargo test --workspace` 4777 pass / 0 fail; `--jit-bench` back at
`arithmetic` ~0.61ms, `bare loop` ~0.60ms. Uncommitted.

### LANDED (2026-09-10): the RHS chain folds into the loop-slot RMW — and the register-op DISPATCH is not the cost

Follow-up to Route B, prompted by the guess that the interpreter's register-op
dispatch was the remaining `--jitless` hot spot. A corpus probe (`--jitless
--corpus`, 1M iterations, ops in the run) settled it:

| shape | ms | delta vs empty |
|---|---|---|
| empty loop | 5.0-5.4 | — |
| `n += 1` (`[LoadConst, BinStoreNum]`) | 9.4-9.5 | +4.1 |
| `n += i` (`[LoadCounter, BinStoreNum]`) | 9.9-10.3 | +4.9 |
| `n += i * 2` (`[LoadCounter, BinImm, BinStoreNum]`) | 11.9-12.5 | +7.1 |
| `n += i * 2 + 3` (4 ops, `Acc` path) | 15.3-16.4 | +9.9 |

Two corrections to the hypothesis:

1. **The register-op dispatch is cheap** (~0.15-0.9ns/op): collapsing
   `n += i * 2` from 3 ops to 1 changed the row by only ~0.6ms.
2. **The `binary_inline` `Value` round-trip is the cost** (~1.5-2ns per call):
   the same op computing `counter * 2` through `binary_inline` (two
   `Value::Number` constructions + two `as_number` checks + the op match)
   versus raw f64 differs by ~2ns. The per-iteration floor
   (`FastLoopHead` + the `allocation_budget_exceeded` TLS read + the
   `RunRegBody` dispatch) is ~5ms of the ~12ms; skipping the GC budget check
   measured ~0.9-2.0ns/iter (a throwaway probe, reverted — the check stays).

Landing: `LeafOp::BinStoreNum` carries a `NumRhs` recipe
(`Acc`/`Imm`/`Counter`/`CounterImm`). `Compiler::plan_loop_num` collapses a
recognized RHS chain into the store (`rhs_recipe`; the leading ops are dropped)
and the interpreter's recipe arms run **raw f64** via `num_arith` — no `Value`
boxing, no `binary_inline`. `Acc` (the general form, e.g. a frame-slot RHS)
keeps the shared path.

`--jit-bench` (release), `--jitless` (`interp` column):

| row | pre-Route-B | Route B | + recipe |
|---|---|---|---|
| `arithmetic` | ~14.2ms | ~12.6ms | **7.51ms** |
| `bare loop` | ~11.8ms | ~9.9ms | **6.71ms** |

~-47% / ~-43% from the pre-Route-B interpreter. The JIT is unchanged
(0.60/0.67ms — it already computed the RHS in f64).

Follow-up (same day): the 2-inner-op recipe landed — `NumRhs::CounterImm2`
(`(counter op1 imm1) op2 imm2`) folds `n += i * 2 + 3` from 4 ops to 1
(`[BinStoreNum { Add, rhs: CounterImm2 { Mul, 2.0, Add, 3.0 } }]`): ~15.9 ->
**8.47ms** (`--jitless`). A 3-inner-op chain (`n += i * 2 + 3 + 4`, ~20.8ms)
still takes `Acc`; the recipes stop at two inner ops (a `Vec`-based
`CounterChain` would generalize, but 3+ ops on the counter is synthetic).

Verification: clippy `--workspace --all-targets -D warnings` clean; `cargo test
--workspace` 4777 pass / 0 fail; six test262 sweeps at baseline (language
23721/0/3, annexB 1086/0/0, built-ins 23657/0/155, with the JIT and with
`--jitless`). Three structural eval tests updated (the collapse changes the
documented op shapes). Uncommitted.

### NEGATIVE PROBE (2026-09-10): the pure-loop safepoint skip is a code-PLACEMENT artifact, not a probe cost (reverted)

Motivation: the FLOOR probe and the register-tick probe both point at the per-iteration GC
safepoint as a candidate for the head floor. A loop whose body is one register run of
provably pure ops (`LoadCounter`, a `Value::Number` `LoadConst`, an arith `BinImm`, a
`BinStoreNum`) can neither allocate nor re-enter user code, so its allocation budget cannot
change inside it and it provably needs no per-iteration poll — a `FastLoopHead` whose
`body_start` is such a `RunRegBody` was marked `pure_loop_heads` and skipped the probe.
The proposal was bounded and the classification is sound. The gate was whether removing the
probe actually moves the compiled loop.

Method — A/B by PROBE COUNT on the emitted loop, holding everything else fixed (release,
`--jit-bench`, 4 runs each):

| probes/iter | how | `arithmetic` jit | `bare loop` jit |
|---|---|---|---|
| 1 (clean) | committed shape | 595-629µs (≈614) | 666-681µs (≈673) |
| 2 | a duplicate `emit_gc_probe()` on the head | 744-748µs (≈747) | 693-698µs (≈696) |
| 0 | the pure-loop skip enabled | 758-798µs (≈769) | 654-696µs (≈668) |

The `arithmetic` column is **non-monotonic**: relative to the single-probe shape, *adding* a
probe costs ~+133µs and *removing* the probe costs ~+155µs. A real per-iteration cost cannot
be more expensive when the work is deleted, so the probe is not the lever — the loop is
sensitive to where its instructions land (the head is a ~5-µop latency-bound loop with idle
issue slots; a size change shifts the hot `jnbe`/`vaddsd` alignment across a fetch/uop-cache
boundary). `bare loop` moves ≤~25µs and nearly monotonic, i.e. within/below noise across the
three shapes. The skip was deterministic per binary (toggling the guard reproduced ≈614µs vs
≈769µs on rebuild), so this is a codegen-placement effect, not run-to-run variance — and not
a control we have (Cranelift exposes no loop-header alignment knob, and the earlier
address-shift test found the same non-responsiveness).

This is the same wall as the register-held GC tick above: the head floor is not reachable by
moving the *safepoint*, by removing it, or by adding to it. Verdict: no clean win; the
`pure_loop_heads` machinery, `leaf_op_is_safepoint_pure`, the duplicate probe, and the
`&& false` guard were all reverted — the tree is byte-identical to HEAD `e943872`. Recorded so
the next attempt at the head floor starts from a probe count A/B rather than the plausible-
but-false "the probe is pure dead weight" premise.

Verification: clippy `--workspace --all-targets -D warnings` clean on the reverted tree;
`git status` clean (no source delta). The scratch probes (`scratch/pureprobe.js`,
`scratch/arithprobe.js`, `scratch/layout/`) were deleted.

### FIXED (2026-09-10): the empty-body certified `for`-loop hang

Root cause: in the two `FastLoopHead` loop paths, `compiler.compile_for` places `body_start`, compiles the body, then places `continue_label`. With no body statements those two labels collapse onto the same step — the head's own index. `emit_fast_loop_head` then resolves the backward-edge target via `ensure_block(body_start)`, which returns the head's own block, and the compiled self-loop re-enters the head without carrying the incremented counter variable, so the loop test never fails.

Fix (`crates/runtime/src/ir.rs`): a `compile_fast_loop_body(body, continue_label)` helper emits the body and, when it produced no steps, emits an explicit forward `Jump(continue_label)`, keeping the body a distinct block (the backward edge then flows head -> body -> head exactly like any non-empty loop). The interpreter pays one dispatch on a do-nothing body. Both `FastLoopHead` sites (the acc/`Counter` path and the slot path) use it; the general (non-fused) loop path never collapsed because its `continue_label` always has the trailing update/back-jump after it.

Verification: the empty-block, empty-statement (`;`), lexical-head, `while`, `do`, and bare-`for` shapes all terminate and agree across node / jit / `--jitless` / `--gc-stress` (`scratch/empty_loop_battery.js`). Regression tests: `eval::tests::empty_for_body_keeps_a_distinct_body_block` (structural — fails fast where a behavioral test would hang) and `jit::tests::installed_jit_runs_an_empty_loop_body` (e2e). Gates: clippy `--workspace --all-targets -D warnings` clean; `cargo test --workspace` 4772 passed / 0 failed; three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang).

Side finding (SEPARATE, fixed the same day — see the next FIXED entry): `function e6(n) { var c = 0; for (;;) { c++; if (c === n) break; } return c; } e6(100000)` SEGFAULTED under the JIT at large iteration counts (6 iterations were fine; `--jitless` was fine; reproduced on the committed pre-fix binary). A `for(;;)` with a non-empty body takes the general (non-`FastLoopHead`) loop path, so the empty-body fix neither caused nor addressed it.

### FIXED (2026-09-10): the test-less `for(;;)` head leaked a stack slot per iteration (the compiled segfault)

Same-day follow-up to the side finding above: `function f() { var c = 0; for (;;) { c++; if (c === 100000) break; } return c; } f()` segfaulted under the JIT, while `--jitless` and short loops were fine. Bisect: n=50 completed, n=100 segfaulted — the `INLINE_JIT_BUF` (64 slots) threshold.

Root cause: in `compile_for`, the test-less head arm (`test == None` — `for (;;)` / `for (init;; update)`) emitted `Step::Push(Value::Boolean(true))` at the `test_label`. Nothing consumes that dummy test value: no `jump_if_false` is emitted for the test-less form, and the loop's backward `Jump(test_label)` re-enters the push every iteration — so the value stack grew by one slot PER ITERATION. The interpreter's stack is a heap `Vec`, so it merely grew; the JIT's working buffer is sized from the static step depth (`max_stack_usage`), so the leak ran the machine code past `buf_end` and segfaulted once the loop exceeded the inline buffer.

Fix (`crates/runtime/src/ir.rs`): emit nothing for a test-less head. `test_label` is only the backward-jump target (the `continue` target is `continue_label`), so with no test the back-jump lands on the body start — an unconditional loop with no per-iteration stack growth. It also drops one wasted dispatch per iteration in the interpreter.

Verification: `scratch/for_no_test_battery.js` (bare `for(;;)`, with update, with init, `continue`, nested, `while(true)`, `do/while`) all agree with node across jit / `--jitless` / `--gc-stress`. Regression tests: `eval::tests::testless_for_head_emits_no_dummy_test_push` (structural) and `jit::tests::installed_jit_runs_a_testless_for_head` (e2e, 100k iterations). Gates: clippy `--workspace --all-targets -D warnings` clean; `cargo test --workspace` 4774 passed / 0 failed; three release test262 sweeps at baseline (language 23721/3 skip, built-ins 23657/155 skip, annexB 1086/1086, zero fail/crash/hang).

## Deferred milestones

Each milestone is deferred with its gate from PLAN Phase 18. A milestone is
"done" only when it passes its gate; none are correctness gates.

| Milestone | Gate | Status |
|---|---|---|
| NaN-boxed `Value` (u64 with tag fast paths) | arithmetic micro-benchmark ≥ 2x vs snapshot | **Done** — correctness landed (migration below) and the shapes work closed the gate: real-loop arithmetic is ~2.2x the corrected baseline. |
| Bytecode VM replacing the tree-walker | `--print-bytecode` dumps real bytecode; hot-path bench ≥ 5x | **Done (2026-09-01)** — everything compiles and runs on the `Vm` (`--print-bytecode` prints the compiled `Step` stream, and the gate is met by one to two orders of magnitude: arithmetic 2.52s → ~15.2ms (~166x), property ~28.3ms (~114x), array iteration ~25ms (~617x), function calls ~27.5ms (~208x) vs the 2026-08-18 corrected baseline). The ≥5x gate that was "still open" closed with the GC-5 `Copy`-value win and the JIT-era interpreter cuts. |
| Object shapes / hidden classes + inline caches | property-access micro-benchmark ≥ 2x | **Done** — the cache layer below (interner memo, own-data fast paths, lazy property index) measured 2.1x on the corrected property-access baseline. |
| String rope representation | string-concat micro-benchmark ≥ 2x | **Done** — the rope below measured ~5x on the corrected concat baseline (0.88s → ~0.15s). |
| `--gc-stress` + leak-detection harness | stress runs clean, no leaks | Deferred: requires the arena heap + mark-sweep GC milestone (below). |

## NaN-boxed `Value` milestone (done)

The enum representation (`enum Value { Undefined, Null, Boolean(bool),
Number(f64), BigInt, String, Symbol, Object, Function }`, ~16 bytes with
the `Rc` handle variants) becomes a single `u64`, so every value is one
machine word: tag dispatch is one compare, and values in arrays/arguments
vectors/VM stacks halve in size. `Handle<T> = Rc<T>`, so the box cannot be
`Copy` (dropping a copy would release the ref); it is `Clone` with manual
refcount reconstruction. The concrete layout:

- **Tagged region** — quiet NaNs whose top 16 bits are `0x7FF8` (exponent
  `0x7FF`, quiet bit 51 set, bits 50-48 zero): `tag` in bits 47-44,
  `payload` in bits 43-0 (the `Rc` pointer shifted right 4 — the
  16-byte-aligned allocation base, so a 48-bit address space).
- **Doubles** — every other bit pattern, preserved exactly: signaling NaNs
  and quiet NaNs with bits 50-48 ≠ 0 survive as-is. A quiet NaN with bits
  50-48 = 0 collides with the tag region and is canonicalized on box to
  `0x7FF9_0000_0000_0000`; this is unobservable from JS (no NaN-payload
  introspection), and the `DataView`/`Float64Array` fixtures stay green
  (verified by the full sweep).
- **Tags** — `0x0` undefined, `0x1` null, `0x2` false, `0x3` true, `0x4`
  BigInt, `0x5` String, `0x6` Symbol, `0x7` Object, `0x8` Function;
  `0x9`-`0xF` reserved. Payload capacity is 44 bits (17.6 TB) of shifted
  pointer, i.e. a 48-bit address space — far above any real `Rc`
  allocation.
- **Refcounts** — `Clone` reconstructs the `Rc` via `Rc::from_raw(ptr)`, clones
  it, and forgets the reconstruction (`Rc::into_raw` on the clone); `Drop`
  reconstructs and drops. Refcounts stay exact, and heap values move by
  plain `u64` copies until a clone/drop actually touches the refcount.
- **`PartialEq`** preserves the current derived-enum semantics: `Number`
  compares via `f64::eq` (`NaN != NaN`), heap values via their `Handle<T>`
  `PartialEq` (the `Rc` deref structural comparison the derive produces).
- **Source compatibility** — constructors keep the current variant spellings
  as associated functions/consts (`Value::Number(x)`, `Value::Undefined`, …)
  so construction sites compile unchanged; only `match`/`if let`/`matches!`
  sites move to `kind()` + `as_*` accessors.

**Migration order** (the type change is atomic across the workspace, so it
lands as one change: crux → runtime → test262, then gates):

1. Rewrite `crates/crux/src/value.rs` (layout, tags, constructors, accessors,
   `type_of`/`is_callable`/`is_constructor`, `Display`, `PartialEq`, `Clone`,
   `Drop`) with unit tests for the bit patterns, the NaN canonicalization,
   and the refcount round-trip.
2. Migrate the `match`/`if let`/`matches!` sites: crux (7 files), then
   runtime (53 files, ~4,200 sites), then test262 (1 file).
3. Gates: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo
   test --workspace`, and the full release sweep (48,622 fixtures) — the
   NaN canonicalization and `PartialEq` semantics are the regression risk.
   **All three are green** (sweep: 0 fail, 229 skip — the standard
   taxonomy, 0 crash).
4. Re-run `--bench` and compare against the snapshot; arithmetic ≥ 2x
   marks the milestone done. (The `Copy` win and the refcount-free moves
   arrive with the GC milestone, which replaces `Rc` with an arena heap.)

**One correctness trap surfaced during the migration**: the `is_*`/`as_*`
accessors must reject doubles before reading the tag — a double's bits
47-44 can collide with a heap tag (e.g. `65.0` is `0x4050_4000_0000_0000`,
whose bits 47-44 read as the BigInt tag), and an unguarded `as_bigint()`
would reconstruct an `Rc` from the double's low bits and crash. Every tag
accessor now checks `!is_double()` first.

### Benchmark analysis (the gate, closed by the shapes work)

The tag fast paths from the design landed first (direct-double arithmetic in
`apply_binary`/`numeric_binary`/`abstract_relational`), moving the real
release loop times as follows:

| Benchmark | pre-migration (real) | post-migration (real) | final |
|---|---|---|---|
| arithmetic | 2.52s | 2.18s | **1.14s** (2.2x) |
| property access | 3.22s | 2.86s | **1.50s** (2.1x) |
| string concat | 0.88s | 0.55s | 0.72s |
| function calls | 5.73s | 5.51s | 4.36s |

(The pre-migration column is the `var`-methodology measurement of the last
green mainline build; the post-migration column is after the NaN-boxing
fast paths; the final column is after the cache layer below. Same machine,
median of runs.)

The arithmetic loop is dominated by the tree-walker's per-iteration work —
identifier resolution (an env-chain walk with linear binding scans),
statement/expression dispatch, and per-iteration loop environments — and
an empty 1M-iteration `var` loop alone measured ~1.1s. Profiling the
identifier path showed the real culprit: every identifier read converts
`AtomId`→`JsString` and back through the global `Mutex`-guarded interner
four to five times (~50ns each). The cache layer below removed those
round-trips and the redundant property lookups, which is what actually
closed both the arithmetic and property-access gates.

## Shapes / inline-cache milestone (done)

A cache layer over the property and environment machinery — the shapes/IC
work deferred from the NaN-boxing milestone, done in four parts:

- **Thread-local interner memo** (`crates/crux/src/string.rs`): `intern` and
  `lookup` keep a 64-entry per-thread cache, so the identifier hot path
  (which converts the same handful of names several times per read) scans a
  few cached entries instead of taking the global interner lock and
  re-hashing/copying. The memo is a pure cache of the append-only interner
  and can never go stale.
- **Single-lookup global get** (`runtime/src/env.rs`): `GetBindingValue` on
  the global environment fetched with `has_property` + `get` (two interns,
  two hash lookups); sloppy mode now issues one `[[Get]]` and re-checks
  only when strict mode needs to distinguish a real `undefined` from an
  absent binding.
- **Own-data fast paths** (`crates/crux/src/object.rs` +
  `runtime/src/context.rs`): `get_key`/`set_key` and the runtime's
  `get_property_key` return/update an own data property on a plain
  object (Ordinary/Array) directly — no receiver construction, no
  prototype-chain accessor scan, no descriptor machinery. Array `length`
  is excluded from the write path (its define intercept validates
  non-uint32 lengths).
- **Lazy property index** (from the NaN-boxing milestone): objects with
  ≥16 properties keep a key→slot hash index, invalidated only by
  structural changes (insert/delete); in-place updates keep it valid.

Validation: `cargo clippy --workspace --all-targets --all-features -- -D
warnings` clean; `cargo test --workspace` 4,194 pass, 0 fail; full release
sweep over 48,622 fixtures: 0 fail, 229 skip (unchanged taxonomy), 0 crash.

## String rope milestone (done)

`JsString` is now either a contiguous buffer or a rope — a binary tree of
concatenation nodes (`crates/crux/src/string.rs`). `concat` appends in O(1)
once the accumulated string is large enough, and the contiguous form is
materialized lazily on first access and cached (strings are immutable):

- **Flat threshold** — concatenations of ≤16 units stay flat (a Vec copy),
  so ordinary small `+` operations never see the rope.
- **Empty operands** — `s + ""` and `"" + s` return the other side directly.
- **Depth cap** — a rope whose left side would exceed depth 64 is flattened
  first, keeping the tree shallow: a chain of small appends cannot overflow
  the stack on drop, and the amortized copy cost is one re-flatten of the
  accumulated string per 64 appends (quadratic with a tiny constant).
- **Lazy flatten** — `as_slice` materializes the concatenation once into a
  `OnceLock<Box<[u16]>>` inside the shared node (thread-safe, so `JsString`
  stays `Send` for the well-known-symbol table) and returns a stable
  reference; `len` is O(1) cached. The flatten walk is iterative (an
  explicit stack), so deep trees cannot overflow it.
- **Accessors** — `code_unit`/`code_point_at`/`code_points`/
  `to_string_lossy`/`PartialEq`/`Hash` all route through `as_slice`, so a
  rope behaves exactly like its flattened content. `Debug` prints the text
  (the derived rope Debug would recurse the tree).

The `+` operator (both the both-strings fast path and the general string
path in `apply_binary`) uses `JsString::concat`, so the O(n²) repeated copy
in `s += 'x'` loops becomes ~O(1) appends. The concat benchmark dropped
from 0.88s (pre-migration baseline) / 0.72s (post-shapes) to **~0.15s
(~5x)**, closing the ≥2x gate. Validation: clippy clean; `cargo test
--workspace` 4,199 pass, 0 fail (five new rope unit tests in crux); full
release sweep over 48,622 fixtures: 0 fail, 229 skip (unchanged taxonomy),
0 crash. (`HashSet`/`HashMap` keys that hold `JsString`/`PropertyKey` carry
a documented `#[allow(clippy::mutable_key_type)]`: a rope's first hash
materializes its flat cache, but the hash output is content-stable.)

## Bytecode VM milestone (Cut 1-4 + script-level bindings + evaluator fast paths landed, gate met 2026-09-01)

The tree-walker is gone from normal execution: every expression and
statement compiles to `Step` bytecode at creation (`compile_expr`/
`compile_statements` in `crates/runtime/src/ir.rs`), and a `Vm` dispatch
loop runs the compiled body for ordinary calls/constructs, generators,
async functions, and top-level scripts. Cut 3 gives simple-param
functions and arrows frame-slot bindings; Cut 4 fuses the loop test and
update into slot ops and adds a primitive fast path to the relational
evaluator; the script-level binding fast path reads declared top-level
vars directly off the global object; and the evaluator numeric fast paths
hoist the number-number check above the ToNumeric/ToPrimitive round-trips
in `apply_binary`'s arithmetic/bitwise paths. All at zero conformance
regressions.

The batching defaults that cloned
suspension-free subtrees into `Step::Expr`/`Step::Stmt` for runtime
tree-walking are deleted. Removing the walker exposed a long tail of bugs
the batching used to mask (assignment-reference timing, member/private/
super `&&=`/`||=`/`??=` short-circuits, computed-compound key conversion
order, destructure/for-of iterator-close semantics, catch env unwinding,
finally routing, super base capture, template-object caching, Annex B
call-assignment targets, `using` disposal on abrupt errors); all fixed, and
the full release sweeps are at zero regressions vs the parent commit with
122 net fixes in `language` (152 fail vs the parent's 274).

The compiled path is at or near walker parity on every benchmark — nothing
lost. The ≥5x gate (measured against the later, already-optimized walker
baseline in `bytecode-plan.md` — arithmetic 1.14s → the plan's "≤0.23s") is
met by a wide margin as of 2026-09-01; the interpreter rows vs that
baseline:

| Benchmark | walker baseline | now (2026-09-01) |
|---|---|---|
| arithmetic | 1.14s | ~15.2ms (~75x) |
| property access | 1.50s | ~28.3ms (~53x) |
| string concat | ~0.15s | ~4.0ms (~38x) |
| array iteration | 13.6–15s | ~25.0ms (~550x) |
| function calls | 4.2–4.4s | ~27.5ms (~155x) |

The gate closed with the cuts since the early bytecode work (fused loop
heads, register bodies, the raw-f64 loop counter, the member/element
value caches) plus the GC-5 `Copy`-value win; the interpreter rows are
now 15-30ns/iter on the hot loops (see the Current status and floor
sections).

## GC milestone (PLAN Phase 18 item 2) — landed, perf gate closed 2026-09-01

The plan's GC milestone (arena heap + mark-sweep; root tracing;
ephemeron-aware WeakMap/WeakSet; `WeakRef`/`FinalizationRegistry` semantics
activated; `--gc-stress` mode) is a rewrite of the value/object model from
`Rc`-based ownership to GC-managed handles. It is **landed** (GC-1..4:
`Handle` → GC heap, collector wiring, per-allocation `--gc-stress` root
audit, ephemerons, weak-ref semantics) — see `.notes/gc-plan.md`.

**GC-5 measured (2026-08-26)** — the eight `--bench` rows vs the pre-GC Rc
model on the same machine (interleaved medians):

| Row | Rc model | GC model | delta |
|---|---|---|---|
| arithmetic | ~25ms | ~12.5ms | ~2.0x faster |
| property access | ~58ms | ~28ms | ~2.1x faster |
| array iteration | ~52ms | ~23ms | ~2.3x faster |
| function calls | ~45ms | ~23ms | ~2.0x faster |
| closure capture | ~53ms | ~25ms | ~2.1x faster |
| per-iteration | ~16.7ms | ~8.8ms | ~1.9x faster |
| string concat | ~10.7ms | ~20ms | ~1.9x slower |
| construct churn | ~36ms | ~74ms | ~2.1x slower |

The GC delivered the predicted ~2x on the machinery rows (the `Copy`-value
win removes Rc clone traffic), but the allocation-bound rows (construct
churn, string concat) regressed ~2x: `Gc::new` is heavier than `Rc::new`
(the live-set registration + stress checks), and mark-sweep reclaims a
loop's garbage in one batch at the script boundary (inside the timed
window) instead of per iteration. The plan's slot-arena allocation (the
original recommendation) is **deprioritized**: `gc-plan.md` GC-5 measured
the free-list half net-neutral and attributes the remaining construct/
concat gap to the engine hot path (the 16-23x gap to V8's interpreter is
hot-path, not collector, cost). The hot-path cuts since then have closed
the allocation-bound rows anyway — construct churn is now ~17ms and
concat ~4ms on `--bench`, ~2x FASTER than the Rc model's 36ms/10.7ms —
so the GC perf gate is met (via the engine cuts, not the collector).

## Accepted no-op CLI flags

`--stack-size` and `--max-old-space` are accepted by the CLI for
compatibility. They are no-ops because the corresponding machinery (call-
stack depth control, a heap to cap) does not exist yet. (`--print-bytecode`
is live — it prints the compiled `Step` stream via
`runtime::ir::debug_print_body`.)
## Closed-plan archives (superseded text, retained verbatim)

The two earlier planning documents are archived here so no measurement or
disposition is lost. They are superseded by the summary sections at the top
of this file (and the dated journal below); treat them as the original
milestone write-ups and design notes for provenance. Content overlaps the
journal; where the two disagree, the dated journal record wins.

### Archive A - the first, row-based plan (closed 2026-09-03)

## Plan: closing the Slag–Node gap

> **Superseded (2026-09-03) by the mechanism-based plan (Archive B below).** This document
> is kept as the closed historical record of the first (row-organized,
> estimate-heavy) performance push; the current plan is mechanism-based
> and covers both engines.

> **Status: closed (2026-09-03).** The gap-close work stops here: the
> remaining wide gaps need machinery beyond this engine's step-VM design
> (callee inlining, a store-side hidden-class write path, a GC arena), at
> costs out of proportion to the measured wins. The unlanded milestones
> and their dispositions are listed in §5; there is no next experiment.

### 0. Working rules (2026-09-03)

- A milestone carries dated, measured facts only — no forward-looking
  numbers. Early `Expected:` estimates contradicted by the outcomes are
  deleted from the sections below, not annotated.
- One next step at a time; a completed experiment names the next single
  step in §5.
- A later measurement that contradicts an earlier note deletes the note.

### 1. Measured baseline (2026-09-02)

The full `--jit-bench` suite (12 rows) re-measured against node v24.12.0,
both JIT (default) and interpreter-only (`--jitless`, V8's Ignition), on
the same machine and session. Slag columns are best-of-3 `--jit-bench`
process runs; Node columns are the best (steady-state) round of
`tools/jit_bench/node_bench.js`. All four modes agree on every row's
completion value. Recorded in `.notes/perf.md` (measured 2026-09-02).

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

Gaps are slag ÷ node (ms). This table predates the `bench_once` steady-state
harness fix and the M2/M6/M10 slices; the slag cells superseded by later
dated measurements are corrected here (the milestone sections carry the
full records):

- `function calls` jit 1.9 → **0.71ms** (harness fix, 2026-09-02).
- `vector leaf call` → renamed `wide leaf call`; jit 29.7 → **1.6ms**,
  interp 45 → **21ms** (M2 fast-arg cap 32→64 gave jit 29.7→3.2ms; the
  harness fix then 3.2→1.6ms, both 2026-09-02).
- `apply leaf call` jit 18.3 → **7.0ms** (M10 slice 1, 2026-09-02; interp
  unchanged ~20.8).
- `arithmetic` jit 3.3 → **2.6ms** (harness fix, 2026-09-02).
- `string concat` interp 10.6 → **3.6ms** (harness fix — the old number was
  GC-polluted; the isolated steady probe confirms 4-5ms).
- `compound assign` jit 2.5 → **1.47–1.49ms** (re-measured 2026-09-03).

### 2. Goal

Focus on the wide rows (compound assign, typed-array write, vector leaf
call). "Closed" means the row's gap halves or better, measured per the
A/B protocol in §6 — not a single run.

### 3. The gap, decomposed

The interpreter gaps are per-op machinery cost. The JIT gaps are three
things V8 does that Slag's straight-line lowering does not — **callee
inlining**, **loop-invariant code motion (LICM)**, and **bounds-check
elision + register quality** — plus **FFI-helper calls per element** on
the shapes the JIT lowers through the shared machinery.

| row | interp bottleneck | jit bottleneck | lever |
|---|---|---|---|
| arithmetic | loop head | codegen quality | register quality (M7) |
| property read | `member_cell_get` double probe | V8 hoists invariant `o.a`/`o.b` | single probe (M3); LICM (M6) |
| string concat | rope node alloc + `Rc` bumps | concat helper round-trip | arena alloc (M8) |
| function calls | — (fine) | at the leaf-call protocol floor | at the floor (M4 measured); apply/call is M10 |
| global read | — (fine) | LICM hoists `g` | LICM (M6) |
| compound assign | read-modify-write machinery | keep `o` in a register | fused cell op (M3/M9); register quality (M7) |
| buildString shape | dense-append machinery (shared with JIT) | same shared machinery | dense elements (M1) |
| buildString full | concat + array machinery | concat helper | M1/M8 |
| typed-array write | per-write view checks | FFI helper per element + re-checked bounds | fused write (M3); inline store (M5) |
| typed-array length | — (fine) | LICM hoists `ta.length` | LICM (M6) |
| vector leaf call | fast-layout rebuild above the arg cap | same | fast-arg cap 32→64 (M2) |
| apply leaf call | arg-list copy + member read | member read + leaf frame setup | compiled intrinsic apply/call (M10) |

### 4. Milestones

#### M0 — Profiling pass (before any slice)

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

#### M1 — Dense array elements (both modes) — existing plan, highest absolute ROI

`.notes/array-store-plan.md` Item 2 (`ArraySlots`: keyless
`Vec<Option<Value>>` + `Cell<f64>` length, spill-on-miss). The
`buildString shape` row is the suite's biggest absolute time (interp
180ms); the append path (`array_element_write`: key + clone +
`SmallProps::push` + index-map insert + length write + generation bump +
RefCell borrows) is ~60ns/iter, and the JIT store helper calls the same
shared machinery — why the JIT row cannot go below it today. Phase A
(representation + the three hot paths: write/get/length) and Phase B
(exotic ops over the buffer) are the bulk; Phase C (buffer-direct ICs +
`offset_of!` inline store) is where the JIT row moves.

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

#### M2 — Vector-form calls without the rebuild (both modes)

`Step::Call`'s handler does `args.split_off(base)` (a `Vec` alloc) and
rebuilds the `[this, callee, args]` fast layout per call; the JIT's
`call_vector` mirrors it. The compiler knows the arg count at compile
time — emit the vector form's args **directly in fast layout** for plain
(non-spread) vector calls, so `do_call`/`call_vector` read them in place.
The `vector leaf call` row is the 33-arg case (the fast cap is 32, so it
always takes this path).

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

#### M3 — Single fused member-read probe (interp)

`member_cell_get` probes the map fast path (`member_cell_get_map`), then
`value_cell` — so a warm loop's read is a map read + in-fields access + a
per-read value-cell write. Fold them into one check (or reorder per M0's
fold them into one check (or reorder per M0's
measurement) and shave the per-read dispatch
on the register path (`GetMemberNameLocal`). Feeds property read,
compound assign, and every member-heavy loop.

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

#### M4 — JIT: statically-known leaf calls (premise superseded — the harness fix landed)

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

#### M5 — JIT: inline typed-array store + bounds elision

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
~99x vs node's 0.29ms, and closing it needs a real machine-code inline
(M5c), not a cheaper helper restructure (the FFI + checks floor is
~20ns/iter). M5c is the
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

#### M6 — JIT: loop-invariant code motion

The mechanism behind three wide rows (property read 21.8x, global read
12.1x, typed-array length 24.3x): V8 hoists `o.a`/`g`/`ta.length` out of
the loop because the body never writes them. Start with the safe subset:
hoist a `GetMemberName*`/`LoadGlobal`/`typed_array_length` read to a
pre-head temp when its operands are loop-invariant, the receiver is
never written in the loop, and the body contains no calls (no
alias/escape). Compose with the register-op machinery (a hoisted temp is
a frame-slot or machine-local).

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

**M6 slice 3 status (2026-09-02):** body-read hoisting landed. While
compiling a hoisted loop's FAST copy (the guard-probe-hit path), a
member read of the guard-probed receiver's `length` lowers to a plain
`LoadLocal` of the hidden hoist slot: a new `Compiler` field
(`hoisted_length: Option<(recv_slot, hoist_slot)>`, set around the fast
copy's `compile_for`, save/restored so a nested hoist re-establishes
its own redirect) and a `compile_member` hook
(`try_hoisted_length_read`) that matches the receiver by RESOLVED slot
— a shadowing declaration of the name (a block `let`/`var` rebinding)
resolves to a different slot and keeps the member path, and the
guard-miss fallback copy compiles with the field `None` (its hidden
slot is never initialized, so its reads re-run the member machinery).
The redirect is exact: the probe verified the IntegerIndexed /
no-own-`length` receiver and the slice-2 body purity (no calls, no RECV
writes) makes the length loop-invariant, so the per-iteration member
read and the slot agree. Measured (`--jit-bench`, min-of-3, 800K):
typed-array length 3.19→**2.02ms** jit (the plan's 2.0ms full-hoist
ceiling — the last 1.2ms) and 25.3→**16.4ms** interp (the step-level
transform is shared); the typed-array write row is unchanged (12.4ms —
its body has no length reads). Validation: clippy clean,
`cargo test --workspace` green, JIT + jitless language (23721/0/0/0)
and built-ins (23657/0/0/0) sweeps match baseline (one language-jitless
batch flake — a computed-property-name fixture with no loop that cannot
reach the hoist path — passed standalone and did not reproduce on
re-run); a 10-case differential (row/init reads, own-`length` shadow
fallback, nested conditionals, Float64, zero-length, alias receiver,
break body, update reads, element reads) agrees between JIT and the
interpreter.

#### M7 — Register quality (the acc-path register runs)

M7's original premise — manual loop unrolling to close `compound assign`'s
jit gap — was dropped: the register-run and raw-f64-counter work (Cut 35)
made the remaining jit rows codegen-quality issues, and the acc-path loop
counter already lives in a machine register across the back edge.

**Discovery (2026-09-03):** a braced loop body (`{ n += 1; }`) compiles to
`[ListBegin, body-steps, ListEnd]` inside the loop, so the block's
`ListEnd` follows the body's last statement. `lower_leaf_ops_segmented`
committed a run only at a `SetCompletion` or the slice end, so the body's
steps never closed a run — the whole braced body dispatched per step while
the unbraced equivalent ran as one `RunRegBody`. (The member-store shape
`a[l++] = i` did lower: its statement ends in a `SetCompletion` the run
absorbs, popping the assigned value the register form consumes.)

**M7 slice 1a LANDED (2026-09-03):** the commit rule also closes a run at a
self-balancing fused statement-terminal store (`FusedStoreLocal`/
`StoreLocal`/`InitLocal` — their step-path form pops their own value and
leaves nothing for a later step to pop), so a braced body whose last
statement is such a store closes a run before the `ListEnd`. A MEMBER store
is deliberately NOT such a boundary: its trailing `SetCompletion` pops the
assigned value and must stay absorbed with the run — ending a run at the
member store was the buildString corruption a first version of this rule
hit. The `SetCompletion`-absorbing and list-wrapper boundaries are
unchanged. Measured (interpreter, isolated 5M-iteration probe, both
orderings): braced `{ n += 1; }` 96→72ms and braced `{ n += i * 2; }`
110→78ms (~25-29%). JIT rows unchanged.

**M7 slice 1b LANDED (2026-09-03): the all-register block's list wrappers
are absorbed.** A run bracketed directly by a `ListBegin`/`ListEnd` pair —
a block whose EVERY statement lowered into the run — absorbs the pair into
its span (nested all-register blocks absorb outward). Over an all-register
block the wrappers are a completion no-op on the step path (`ListBegin`
saves, `ListEnd` restores only an untouched completion, and the register
ops never write it), and the run contains no abrupt control that could
skip the pop (breaks/jumps never lower), so dropping the pair — and its
per-iteration list push/pop — is unobservable. Braced bodies now compile
to the SAME single `RunRegBody` as the unbraced form. Measured
(interpreter, same probe): braced `{ n += 1; }` 72→**56.7ms** and braced
`{ n += i * 2; }` 78→**61ms** — equal to the unbraced rows (56/60ms);
JIT rows unchanged. Validation (both slices): clippy clean, workspace
tests green, all three sweeps at baseline, plus the bytecode-level
regression tests (the braced body lowers to one run with no list steps
left; the member-store body keeps its `SetCompletion` absorbed).

#### M8 — Arena allocation (interp)

`.notes/gc-plan.md`'s remaining lever: `Gc::new` heavier than `Rc::new`;
recovers the string-concat/construct-churn regressions. The rope
append allocates a box per append (100K for the string-concat row); a
bump arena + the small-string path (Cut 67) cuts the alloc + `Rc` bump.

#### Longer tail

- **M9 — fused compound member op** (`o.x += 1` as one register op:
  generation-validated cell read + add + store back).
- **M10 — apply arg-list copy**: `create_list_from_array_like`'s dense
  path still copies; inline the known `.apply` into a vector call (the
  M4 + M2 combination).

**M9 decomposition (2026-09-03, interp, the row's own shape):** the
`compound assign` row (`{ o.x += 1; s += o.x; }`, o/n locals, 100K iters)
is ~186ns/iter, of which ~146ns is the MEMBER WRITE. Isolated shapes:
slot-only control ~20ns/iter; two member reads (value-cell hits) add
~4ns each; `o.x = i` (plain warm own-property write) ~166ns/iter. The
write path (`assign_member` → `member_reference` + `put_value`) has no
single dumb bottleneck — `find_ecma_accessor` already short-circuits on
the own data property (one `get_own_property_key`, no chain walk), so
the cost is the broad sum of put_value's re-derivation (namespace
checks, receiver boxing) plus TWO property-vector lookups and the
`set_with_receiver_key` write. M9 therefore needs a STORE-side fast
path (an own-writable-data write validated like the read cells), whose
design must handle chain invalidation across objects (a setter added to
Object.prototype does not bump the receiver's generation) — not a
cheaper helper. Row gap vs node jitless stays ~12x until then.

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
leaf-inline probe to read the array's buffer directly.

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

### 5. Disposition (closed 2026-09-03)

The section-4 order was the original week plan; the work is executed one
single experiment at a time in measurement-dictated order. Landed as of
2026-09-03: M1 (dense elements + the inline append gate), M2 (fast-arg
cap), M3 slice 1 (value-cell-first probe), M5c (typed-array store
inline), M6 slices 1-3 (typed-array length read/host/body-read hoist),
M10 slice 1 (compiled intrinsic apply/call), M7 slices 1a-1b (braced
fused-store register runs + all-register list-wrapper absorption — the
braced and unbraced loop-body forms now compile identically).

**The plan is closed — no next experiment.** The remaining items were
assessed against the measurements and are not pursued:

- **M7 slice 2** (a loop-local accumulator carried across the back edge
  in a second register): a new VM register with JIT liveness and GC
  rooting across the back edge, to save the ~quarter of the ~4ns/iter
  register body that the accumulator's slot round trip still costs.
- **M1 C deep** (the machine-code RefCell/Vec dense-append inline): a
  UB-sensitive JIT slice for a row already at 42.9ms jit.
- **M2 residual** (65+ arg vector-form plain calls): no bench row
  exercises it.
- **M3 slice 2** (the remaining member-read dispatch): single-digit
  percent on one row.
- **M8** (the GC arena): a cross-cutting collector change for interp
  rows (string concat ~2.4x vs node jitless after the harness fix)
  already close to their floor.
- **M9** (the fused compound member op / store-side write path): the
  decomposition measured the warm member write at ~146ns/iter of the
  compound row's ~186; a store fast path must handle chain invalidation
  across objects (a setter added to a prototype does not bump the
  receiver's generation) — hidden-class-scale machinery.
- **M10 slice 2** (inlining the `.apply` member read): a JIT micro-slice
  for the residual ~35ns/call.

### 6. Tracking & methodology

- **Gate:** the §1 table in `.notes/perf.md` (measured 2026-09-02). After
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

### Archive B - the mechanism-based plan (2026-09-03)

## Performance plan: interpreter + JIT (2026-09-03)

> Supersedes the first, row-based plan (Archive A below, closed 2026-09-03). That plan was
> organized around benchmark rows and committed expected numbers before
> measuring them; this one is organized around the mechanisms that cost
> time, treats the two engines (interpreter, JIT) as one shared-machinery
> system, and uses the vendored V8 checkout (`v8/`, $V8 below) as the
> design reference — we borrow architecture, never code.

### 0. What the previous analysis got wrong

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

### 1. Ground rules

1. **Compliance is the constraint.** The engine passes ~99% of the
   runnable test262 corpus (language 23721/23724, built-ins 23657/23812
   incl. 155 skips, annexB 1086/1086). No performance landing proceeds
   without the three release sweeps at baseline, clippy clean, and the
   workspace tests green. A perf change that costs a fixture is reverted.
2. **A lever opens with its probe.** Before implementation, quantify the
   current cost of the mechanism being replaced with a dated measurement
   (recorded in `.notes/perf.md`). No expected numbers on a milestone; a
   milestone records what its probe showed and what its landing measured.
3. **One experiment at a time.** A landing names the next experiment.
4. **Both engines move together.** The interpreter and the JIT lower the
   same `Step`/register-op streams and share the object machinery; a
   mechanism change must state its effect on each and be measured on
   both (`--jit-bench` runs each row in both modes).
5. **$V8 is the reference.** Each lever cites the V8 mechanism it
   mirrors and why that mechanism is fast.

### 2. The two engines as they stand (measured 2026-09-02/03)

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

### 3. Why V8 is fast — the mechanisms, mapped to this engine

| V8 mechanism | $V8 source | What it buys | Slag's current analog |
|---|---|---|---|
| Maps (shapes) with descriptor offsets + in-object fields | `src/objects/map.h` | property load/store = shape identity check + direct field access; no name resolution per access | generation-validated cells (reads) + full `[[Set]]` re-resolution (writes) |
| Per-site inline caches / feedback vectors | `src/ic/ic.cc`, `src/objects/feedback-vector.h` | monomorphic fast path validated by one shape compare; exact fallback | global direct-mapped caches (thrash with many keys, Cut 35 slice 5) |
| Accumulator bytecode + specialized handlers | `src/interpreter/bytecodes.h` | low per-op interpreter cost | register runs (already match/beat this on certified straight-line bodies) |
| Baseline compilation of ALL code (Sparkplug) | `src/baseline/baseline-compiler.cc` | every body runs compiled, then hot bodies tier up | certified-subset-only JIT; the general path never compiles |
| Nursery (bump) allocation | `src/heap` | cheap per-object allocation | per-object `Gc::new`/Rc boxes on the hot paths |

### 4. Levers

#### L1 — Property machinery (the dominant lever; both engines)

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

#### L2 — Per-site feedback (after L1)

Per-call-site IC entries (shape/offset pairs on the L1c model) shared by
the interpreter (validate + direct access) and the JIT (monomorphic fast
path with exact slow path), replacing the global direct-mapped tables
that thrash at scale. V8's `ic.cc`/feedback-vector model. Defer until L1
establishes the shape representation; a per-site cache on the current
hash-based cells buys little.

#### L3 — JIT coverage: compile the general path

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

#### L4 — Allocation (bump arena)

The `buildString`/construct-churn rows allocate a box per element/object
(the rope append and per-object `Gc::new`). V8's nursery is a bump
allocator with a copying collector; Slag's per-object allocation is the
measured interp floor on those rows. A bump arena for the hot shapes
(ropes, fresh ordinary objects) with the existing collector sweeping the
arena is the M8 idea, resized to measured need: probe the allocation
share of the construct/buildString rows first (count boxes per
iteration), then build the smallest arena that covers them.

#### L5 — Call/construct breadth

The leaf-inline + certified-call machinery is strong within the
certified subset. After L1-L3 land, revisit the remaining call rows with
measurement: method-call inline paths (`o.m()` where `m` is a stable
slot/global), and the residual apply/call member-read cost. No design
work until L3 changes what is even reachable by the JIT.

### 5. Sequencing

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
A/B in both modes recorded in `.notes/perf.md`, and the measurement the
item's probe promised.

### 6. Measurement discipline

- Rows live in `.notes/perf.md` with their dates and harness; the machine
  swings ±15%, so deltas are judged on multi-run interleaved A/Bs, never
  single runs.
- The A/B harness (`bench_once`) measures steady state: definition
  evacuated once, args bound once, warmup before timing — no per-run
  recompile, no warmup-garbage skew.
- Mechanism probes (per-op costs) use isolated loops shaped like the
  real row, alternated in order; a probe that changes the goalposts is
  recorded as such, not silently re-baselined.

### Archive C - the merged task list (2026-09-04; superseded by the front-matter tables)

## Merged task list (the status view as of 2026-09-04)

> Merged, prioritized view of the remaining work in the active plan
> (mechanism-based, supersedes) and the closed historical plan (both
> archived above). Status reflects everything
> landed through `0d70d3e` (the L1c record-discipline landing), the
> write-cell capacity slice (1.3), and the certification-coverage slice
> (2.2): the gap-close milestones M1-M7/M10, the L1a/L1c register-path
> work, the typed-array no-alloc reads, the `UpdateReg`/`JumpIfEqImm`/
> `BinStoreReg` slices, and the GC fixes. Only remaining work is listed.
> One experiment at a time; a lever opens with its probe; every landing
> gates on clippy clean, workspace tests green, and the three release
> sweeps.

### P0 — Correctness (JIT)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 0.1 | JIT `Float16Array`/typed-array miscompile: compiled `makeArrayLike`-style loops read all-`NaN` from some iteration onward, then segfault (~200-fixture crash cluster, JIT-only, `--jitless` clean) | open; Linux-only; being debugged | Pre-existing at `d58caea`; not GC (reproduces with collections disabled). Lives in `crates/jit` lowering. Unblocks clean Linux JIT built-ins and the `-p jit` release test binary. |

### P1 — Structural property machinery (L1c → L2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 1.1 | L1c read/write end-state on maps/shapes: hot member paths serve via shape-compare + inline-field access instead of the generation/id/name value-cell probes; exotic receivers/accessors/index keys fall back to the exact machinery | partial; stones 1-3 (record discipline) and 1.3 (write-cell capacity) LANDED; the READ-end-state premise was probed and FALSIFIED on the interpreter (perf.md, 2026-09-04) | **Stones 1-3 (LANDED, 2026-09-04)**: the warm member write stopped bumping the generation (`write_data_property_slot`) after converting the three generation-stamped VALUE caches to the L1c oracle pattern (`construct_this_object` reads `prototype` via the shared member value cell; `member_chain_cells` cache the resolution and re-read live; the for-of verdict oracles AIP's `next`). **1.3 (LANDED, this tree)**: the L1a store cells moved to a SEPARATE 256-entry table (`MEMBER_WRITE_CELLS`) — see the 1.3 row. **Read probe (FALSIFIED the per-site read premise)**: warm register member reads are ~3.5ns and read 64 distinct same-map objects at ~4.2ns (+0.7ns — the map-cell layer absorbs value-cell misses at near-warm cost); a 16->256 `MEMBER_CELLS` experiment did NOT speed the cycling-object rows and bloated the Agent's inline tables ~25% slower on every warm row. So the interpreter read path is near its floor; per-site read ICs are NOT the next slice. **Next**: the write end-state — see 1.3's follow-on (per-site store cells only if >256-object working sets show up in a probe). |
| 1.2 | L2 per-site feedback: per-call-site IC entries (shape/offset) shared by the interpreter and the JIT, replacing the global direct-mapped tables | re-scoped by the 2026-09-04 probes | The interpreter read path does not need per-site ICs (1.1's probe: reads ~3.5-4.2ns across 64-object working sets). The remaining per-site argument is the WRITE side beyond the 256-entry capacity (1.3) and the JIT's compiled shape-compare end-state. Defer full L2 until a >256-object store-loop probe shows the capacity ceiling, or the JIT work needs the shape/offset representation. |
| 1.3 | L1a store cells on a separate, larger table (`MEMBER_WRITE_CELLS`) | **landed 2026-09-04** (this tree) | The 16-entry write cells alias across any >16-object store loop; a store-cell miss falls back to the full [[Set]] (~140ns). Interleaved A/B (parent `0d70d3e` + probe rows vs this tree): a 64-distinct-object cycling-store row (32M stores) drops ~4.4-4.5s -> ~0.42s (~10x, ~140ns -> ~13ns/store); the warm rows moved within the cross-build layout band (`arithmetic` — no property cells — moved ~±25%, recorded as noise). Read cells stay at 16 (1.1's probe); the store probe/record now index by `MEMBER_WRITE_CELLS` while the value-cell front keeps the read table's mask. Gates: clippy, workspace tests (new `warm_stores_across_many_distinct_objects_keep_separate_cells`), three sweeps at baseline. Follow-on: per-site store ICs if a probe finds >256-object hot store loops (the JIT's compiled stores are separate). |
| 1.4 | Primitive-string property reads box a String-exotic wrapper per access | **landed 2026-09-04** | Certified-body probe (200k `s.length` reads, `Gc::new` TLS counters): top-level eval, certified interp, AND the JIT all boxed 448B wrapper + 64B [[StringData]] per read. Fix in the shared `Vm::get_member_name`/`get_member_computed` helpers (mirroring the typed-array `length`/element shortcuts, so the step path, register ops, and JIT ABI all inherit): string-`.length` returns the code-unit count; in-range canonical numeric index returns the single code unit (StringGetOwnProperty — own, shadows the chain); OOB/non-index falls through (patched `%String.prototype%` numeric keys still found). Counts 400k -> 0; clean A/B on the 200k row: interp ~106-119ms -> ~4.2-4.8ms (~23x), jit ~108-308ms -> ~2.4-2.8ms (~40x). Gates: clippy, workspace tests (new `string_primitive_member_reads_serve_length_and_units_without_boxing`), three sweeps at baseline (perf.md record). |
| 1.5 | Primitive-string METHOD reads (chain data/accessor/symbol keys) resolve on `%String.prototype%` without boxing | **landed 2026-09-04** | Probe: `s.charAt`/`charCodeAt`/`indexOf` each boxed 448B wrapper + 64B [[StringData]] per CALL on top-level eval, certified interp, AND the JIT (200k calls = 400k boxes; the compiled call path has no primitive-receiver member fast path). Fix: `Vm::get_string_primitive` — after the 1.4 length/index shortcuts, a string-primitive fallback resolves the key against the realm's cached `%String.prototype%` with the PRIMITIVE as the [[Get]] receiver (exact: the wrapper's own props are only length/index, and the engine threads Receiver=primitive through OrdinaryGet — data props, accessors (strict getters see the primitive; sloppy this-coercion boxes), proxy links, symbol keys all match). Boxes 400k -> 0 (charAt retains its inherent result-string box). Clean A/B on 200k rows: charCodeAt interp ~413-430 -> ~246-250ms (~1.7x), charAt ~348-371 -> ~207-216 (~1.7x), indexOf ~550-578 -> ~394-409 (~1.4x); jit similar; the residual per-call cost is the intrinsic CALL dispatch (4.1/L5), not the read. Semantic battery byte-identical vs the boxed path (sloppy/strict getters, patched methods, proxy-in-chain receiver, numeric OOB). Gates: clippy, workspace tests (new `string_primitive_method_reads_resolve_on_the_prototype_chain`), three sweeps at baseline (perf.md record). |
| 1.6 | Non-string primitives (Number/Boolean/BigInt/Symbol) METHOD reads resolve on their %X.prototype% without boxing | **landed 2026-09-04** | The 1.4/1.5 helper was string-only; probe (200k, `Gc::new` TLS counters) showed Number/Boolean/Symbol/BigInt method reads/calls boxed a 448B wrapper PER READ on both engines (number/boolean wrapper creation also inserts an agent boxed-value-table entry): `n.toFixed`/`toString` reads 200k x 448B, call rows + the inherent result-string boxes, `sym.description` + result, bigint call + 48B box. Fix: generalized `Vm::get_string_primitive` -> `Vm::get_primitive_member` and routed every primitive (not just String) through it in `get_member_name`/`get_member_computed`; new `Intrinsics::primitive_prototypes` cache (Number/Boolean/BigInt/Symbol, array indexed like `function_prototypes`). Exactness is the same argument as 1.5 (these wrappers are ordinary objects with NO own properties — the direct chain read with the primitive receiver reproduces every read; the `description` accessor and proxy links see the primitive receiver). Boxes 200k -> 0 on reads. Clean A/B on Number method-read rows (200k): interp ~106-119ms -> ~13.3-14.1ms (~8.4x), jit ~97-104ms -> ~7.9-9.7ms (~11-13x); the residual ~67ns/call interp is the %Number.prototype% own-scan chain read (the 4.1 probe's chain-read primitive — L2). 18-line semantic battery byte-identical (methods, patches, strict/sloppy getters, proxy receiver, `Symbol().description`, boxed `new Number(5)` reads). Gates: clippy, workspace tests (new `non_string_primitive_method_reads_resolve_on_the_prototype_chain`), three sweeps at baseline (perf.md record). |
| 1.7 | Store-cell capacity: `MEMBER_WRITE_CELLS` 256 -> 4096 (boxed table, heap-direct init) | **landed 2026-09-04** | The (a) probe: a >256-distinct-object cycling store loop hits a real cliff — 1M stores across 1024 objects ~180ns/store interp (~165 jit) vs ~55ns/~40ns for 1-256-object sets, because the 256-entry direct-mapped write cells thrash and every store falls to the full [[Set]]; READ rows do NOT cliff (59-61ns at both 64 and 1024 — the read map/proto-cell layer absorbs). Such loops are realistic (per-frame entity/record updates over thousands of objects). Fix: grow the boxed `MEMBER_WRITE_CELLS` 256 -> 4096 and make `Agent::new` build it heap-direct (the `from_fn` array temporary sat on the stack — ~128KB at 4096 — and overflowed the 1MB-stack embed doctest). 1024-object stores drop ~180 -> ~55ns interp (~165 -> ~40 jit); warm rows and the suite move within the cross-build layout band; the charCodeAt control flat. Working sets >4096 still cliff (that residual is the L2 shape-keyed store slice, 1.8). Gates: clippy, workspace tests (4652/0 incl. the embed doctest), three sweeps at baseline (perf.md record). |
| 1.8 | Shape-keyed store cells + the direct own-data fallback (L2 slices b/c): remove the >4096-object store cliff for same-shape and vector-only hot stores | **landed 2026-09-04** (this tree) | The (id, name) `MEMBER_WRITE_CELLS` table thrashes once a store loop's object working set exceeds 4096, even when every object shares one shape — measured cliff (200k-call rows, fresh release build of `ebe30cc`): 8192-object inline-field stores ~181ns/store interp (~162 jit) and 16384 ~182ns vs ~56ns/~40ns at 64-1024 objects. Two mechanisms. (1) **Shape-keyed cells**: a second direct-mapped table `member_write_map_cells` keyed by (map id, name) — a map id pins the descriptor layout for every instance of the shape, so a hit needs no per-object identity or generation. Probed as the fallback when the (id, name) cell misses; recorded ONLY for map-described inline keys (a vector-only property's slot is per-object — two objects can share a live map yet hold different vectors after it), so the pinned inline mirror is always real. (2) **Direct own-data fallback**: the residual probe showed the same cliff for a key the map does NOT pin (a 5th+ field of a many-field shape is vector-only — 8192 ~214ns, 16384 ~223ns interp) and that no per-step IC can serve it (nothing shape-pins a vector-only slot). Instead the miss chain now resolves the object's OWN vector slot (`property_slot`) and writes in place when the property is already an own writable data property — exact (an own writable data property shadows the chain, spec 7.3.3; accessor/non-writable/absent fall to the full [[Set]]) — turning every warm in-place store into an O(1) resolve+write regardless of object count. A hit on either fallback re-keys the (id, name) cell so the same instance's next store keeps the cheaper primary probe, and fronts the read-side value cell under the L1c no-bump discipline. Inline rows: 8192/16384 ~181-182ns -> ~58-67ns/interp (~162 -> ~44 jit), now AT the 64/1024 warm level; vector-only rows ~214-223ns -> ~68ns (jit ~184-196 -> ~54ns); single-object rows unchanged (no warm regression). Gates: clippy, workspace tests (new `stores_over_many_same_shape_objects_stay_exact` + `stores_over_many_vector_field_objects_stay_exact` — 9000 same-shape / six-field instances with distinct values, interleaved map transitions, non-transitioning defineProperty, and deletes to dictionary mode, all read back), three sweeps at baseline (perf.md record). The remaining full-[[Set]] stores are true defines (a genuinely new key), which no IC can make faster without the L1c storage migration; the chain-member-read slice (a) still waits on L1c's shape end-state. |

### P2 — JIT coverage (L3)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 2.1 | Compile the general path (Sparkplug analog): emit every step in machine code for bodies the scope gate excludes, routing env/handler steps through the shared machinery | re-scoped by the 2026-09-04 probes | The scope-gate probe falsified the plan's premise for try/catch (those certify AND reach the JIT — per-iter try interp ~125ms / jit ~72ms) and 2.3 (this-capturing arrows) landed the dominant residual scope=None shape (~33x). The remaining scope=None hot shapes are narrow (with/eval/async-generator/super-constructors); their dispatch cost is not measured as a lever. Do NOT start the Sparkplug analog without a corpus probe showing uncertified hot bodies whose cost is dispatch (not the env path the certification fixes remove). |
| 2.2 | Certification over-rejection: nested NON-ARROW functions' own `this`/`arguments` bailed the enclosing body's scope certification | **landed 2026-09-04** | The closure walker (`closure_*_allows`) now threads an `own` flag: entering a nested non-arrow function sets it (its `this`/`arguments` are its OWN, bound at its own call); arrows propagate the caller's flag (an arrow in the analyzed body still observes its lexical `this` and bails). `super`/`class`/private/tagged/import stays rejected under `own`. Probe (perf.md, 2026-09-04): the construct-churn loop function-wrapped with a nested `function C(x){ this.x = x; }` ran ~117-129ms vs ~19-21ms with C a global; after the fix the nested form matches the control (~6x), because the body re-certifies and its `var`s leave the env path. Gates: clippy, workspace tests (new `nested_function_own_this_and_arguments_keep_the_body_certified`), three sweeps at baseline. |
| 2.3 | Certify `this`-capturing arrows: an arrow created in a certified non-arrow body that references `this` captures the body's this value (a synthetic context entry sourced from the this slot at creation); the arrow body reads it as a depth-0 context slot | **landed 2026-09-04** | The closure walker records a reserved marker (\u{1}captured-this) when an arrow references `this`; a NON-ARROW body allocates a marker context slot + forced this slot and `compile_body` emits an entry store copying this into it; an ARROW body certifies only when its outer chain carries the marker (its direct `this` compiles to a `LoadContextSlot` resolved through the chain; deeper this-arrows flow the same way). Env-path arrows (rest params etc.) inside a capturing body resolve lexical this through the capture context (`DeclarativeEnv::has_captured_this`/`captured_this_value` make the marker env a this-environment — the Object/keys/proxy-keys regression fix). Measurement (perf.md, 2026-09-04): the callback-in-method probe dropped ~1.4s -> ~42ms (~33x). Gates: clippy, workspace tests (new `this_capturing_arrows_certify`), three sweeps at baseline. |

### P3 — Allocation (L4 / M8 arena)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 3.1 | Bump arena for the hot shapes (ropes, fresh ordinary objects), swept by the existing collector | **closed by probe 2026-09-04 — no arena work indicated** | Counting probe (perf.md, 2026-09-04): `construct churn` = exactly 1 x 448B arena box per iteration (the `new C(i)` instance itself; no context/env/key extras); `buildString full` = 390 boxes TOTAL for the whole row (the ~1.1M dense element writes allocate zero). The bump arena the plan proposed ALREADY exists (A5.1: bump + size-classed free-list; GC-5 measured the free-list half net-neutral and registration ~11ns/alloc). No second hot shape to give a dedicated arena; the rows' residual cost is the certified-construct path and branchy step dispatch. The probe's side finding (primitive-string property reads boxing a wrapper per access) is tracked as 1.4. |

### P4 — Call/apply residual (L5 / M10 slice 2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 4.1 | Inline the `.apply`/`.call` member read on the compiled intrinsic path | probe done 2026-09-04 — target re-derived as diffuse; deferred to the L2 per-site IC and the L5 call-dispatch levers | Fresh A/B decomposition on the `apply leaf call` shape (200k, per call): apply-9 interp ~98-105ns / jit ~36-44ns; .call-9 interp ~100-107 / jit ~26-32; same-leaf direct 9-arg call interp ~58-62 / jit ~6.2 (the floor). Overhead vs the direct call: interp ~40-44ns, jit ~20-37ns — allocation-free (boxes 0) and the arg-array fill is NOT the term (interp .call ≈ .apply; jit apply-9 ≈ apply-1). The residual is spread across the per-iteration chain member read of the method, the intrinsic identity compare, and the CallApply dispatch; prototype-chain member reads cost ~4x own-data reads interp and ~10x jit (55 vs 16ns interp; ~28 vs ~2.8ns jit) across function/object/array receivers — the read IS a real primitive, but a narrow `.apply`-only inline has no clean target (the read is shared with every `o.m()`; an inline validation was measured slower in 2026-09-01). Defer the read-side fix to L2 (per-site shape/offset ICs) and the dispatch-side residual to L5. |
| 4.2 | Register agent-dependent builtin handlers in the O(1) per-function-id table so warm calls skip the module dispatch chains (the L5 intrinsic-call dispatch floor) | **landed 2026-09-04 — String + Number + Boolean + BigInt**; Object/Date/Keyed/etc. chains share the pattern | Probe (200k calls, certified rows, both engines): `s.charCodeAt` ~1.18µs/call interp and `a.push` ~1.7µs vs `Math.abs` ~150ns (a plain native closure, no agent chain) and a same-work JS leaf ~90ns. Mechanism: agent-dependent methods (ToString/@@-delegation need the agent, so they are placeholder-closure builtins dispatched by intrinsic identity) run the module's LINEAR `dispatch_call` chain on every warm call — each `intrinsics.get` arm allocates a JsString + hash-lookup, and only `array::handler_for`/`regexp::handler_for` register O(1) per-id handlers today. Fix: per-module `handler_for` maps (String ~39 non-HTML arms, Number 7, Boolean 3, BigInt 6 — each arm's `(agent, this, args)` handler, constructor arms via adapter closures) consulted by `Intrinsics::define`, registering each method by function id at install. charCodeAt interp ~1.18µs -> ~380ns (~3.1x); clean A/B per call on the primitive rows: `n.toFixed(1)` interp ~1170 -> ~680ns (~1.7x), `b.toString()` ~615-653 -> ~300-307 (~2.0-2.3x), `123n.toString()` ~926-963 -> ~346-361 (~2.6-2.8x); jit proportional. Residual is the primitive chain READ (~250ns) + native call (the L2 read lever). Gates: clippy, workspace tests (new `string_agent_builtins_dispatch_identically_via_registered_handlers` + `number_boolean_bigint_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Next: the same `handler_for` maps for the other agent-dependent modules if a corpus probe shows their methods hot. |
| 4.3 | Register the KEYED module (Map/Set/WeakMap/WeakSet + iterator nexts + statics/size/species) in the O(1) handler table — the (d)-landing follow-on (c) | **landed 2026-09-04** (this tree) | After the hash-index landing (d) the keyed rows' residual was the module's ~55-intrinsic `dispatch_call` chain (Set.has's arm ~40 `intrinsics.get` calls in — ~5µs/call; Map.get ~1.9µs at arm ~10). `keyed::handler_for` maps every `Intrinsics::define`'d keyed function (methods, groupBy, size/species getters, both iterator `next`s) to the named `(agent, this, args)` handler the chain already calls; the four constructors register their call-without-new TypeError (their `new` path keeps `dispatch_construct`). A/B vs parent `3cd5c9b` (the index landing, 200k-call rows): Map.get ~356 -> ~45.6ms (~7.8x, ~228ns/call), Map.set ~513 -> ~44.3ms (~11.6x), Set.has ~891 -> ~48.9ms (~18x — its late arm collapses to Map.get's cost), delete+set churn ~770 -> ~94ms (~8x); the JIT column matches. Gates: clippy, workspace tests (new `keyed_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Next: the (c)-probe's remaining chain-bound modules are Object (arms are INLINE closures — a named-handler refactor first) and DataView (~660ns) — extend per a corpus probe showing their methods hot. |
| 4.4 | Register the OBJECT and DataView modules in the O(1) handler table (candidate (c) completion) | **landed 2026-09-04** (this tree) | Object's `dispatch_call` arms were INLINE closures (the (c)-probe's reason registration was deferred): each closure is now extracted into a named `(agent, this, args)` handler (`prototype_has_own_property`, `object_create`, `object_define_property`, `object_entries`/`values`/`keys`, `object_get_own_property_descriptor(s)`, `object_has_own`, the integrity-level statics, ...), so the chain and the new `object::handler_for` share one implementation; DataView's get/set codecs register per element type and the buffer accessors directly. A/B vs the 4.3 parent (200k-call rows): Object.hasOwn ~1085 -> ~126ms (~8.6x, ~632ns/call), hasOwnProperty ~283 -> ~150ms (~1.9x — an early arm, work-bound), DataView.getUint8 ~630-720ns/call (the (c) probe) -> ~291ns (~2.2-2.5x); Object.keys' row is allocation-bound (64 fresh key strings per call), unchanged. Gates: clippy, workspace tests (new `object_and_dataview_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Candidate (c) is now CLOSED: every agent-dependent module whose methods a probe showed hot (String/Number/Boolean/BigInt/Keyed/Object/DataView) is registered. |

### P5 — Small interpreter micro-slices (probe first)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 5.1 | Drop the per-`if` `ResetCompletion` in certified loop bodies (one fewer dispatch/iteration on branchy bodies) | **landed 2026-09-04** | `buildString shape` ~99-104 -> ~94-97ms interp (~5%); completion battery + three sweeps at baseline (perf.md record). |
| 5.2 | Closed-plan residuals (assessed not-worth in gap-close §5, listed for completeness): M7 slice 2 (second register accumulator), M1-C-deep (machine dense append), M2 65+ args, M3 slice 2, general LICM of `o.a`/`g` reads | closed | No bench row exercises most; revisit only if a probe shows otherwise. |
| 5.3 | Fuse the statement-position local compound into one op when its RHS is in the accumulator (`BinStoreReg` — the `n += i*2`/`s += o.x` tails) | **landed 2026-09-04** (`83b7bea`) | arithmetic interp ~13.2-13.5 -> ~11.3-11.5ms (~15%), compound assign ~12%; JIT flat; three sweeps at baseline (perf.md record). |
| 5.4 | Direct-operand local compounds (`n += 1`, `s += t`) fused into one fat op | **closed by measurement 2026-09-04 (REVERTED)** | Interleaved A/B vs `83b7bea`: arithmetic +~1.2ms, bare loop +~0.4ms regression — the per-op match dispatch is cheaper than a fat arm with operand branches + a cold tail. The direct-right shapes stay three ops (perf.md record). |

### Recommended order

1. 0.1 in parallel (the Linux debug agent owns it) — a correctness blocker.
2. 5.1-5.4 are LANDED/CLOSED (2026-09-04): the register-run local-compound
   arc ended at the `BinStoreReg` fuse (5.3); the direct-right fat-op
   generalization measured as a regression (5.4) and the branchy micro-arc
   is at its 4-dispatch floor. 1.3 (write-cell capacity) is LANDED on this
   tree.
3. The read-end-state premise (per-site member reads) was probed and
   FALSIFIED (1.1): interpreter member reads are ~3.5-4.2ns across
   64-object working sets — the map-cell layer already absorbs the
   value-cell misses. 2.2 (certification over-rejection) and 2.3
   (this-capturing arrows, ~33x) are LANDED, and the scope-gate probe
   (2.1) closed the Sparkplug-analog premise for try/catch.
4. 3.1 (L4 arena) is CLOSED by its counting probe (2026-09-04): the arena
   already exists and both target rows measured 1 box/iter (construct) and
   ~390 boxes total (buildString full) — no arena to build. 1.4 (string
   `.length`/unit reads boxing a wrapper per access), 1.5 (string METHOD
   reads), and 1.6 (Number/Boolean/BigInt/Symbol METHOD reads) are LANDED
   on this tree (~23x/.length; ~8-13x on Number method-read rows; boxes
   200-600k -> 0 on every read). 4.1's fresh A/B is DONE (2026-09-04): the
   apply/.call residual (~40-44ns interp / ~20-37ns jit over a direct leaf
   call) is allocation-free, the fill is interp-free, and the cost spreads
   across the per-iteration chain method read + intrinsic compare +
   CallApply dispatch — no narrow .apply-only slice; the read side defers
   to L2 (per-site ICs), the dispatch side to L5. 4.2 (the intrinsic-CALL
   dispatch floor: warm agent-dependent builtin calls paid the module's
   linear identity chain) is LANDED for String + Number/Boolean/BigInt
   (charCodeAt ~3.1x, toFixed ~1.7x, bool/bigint toString ~2-2.8x; see the
   4.2 row). 1.7 (the write-side >256 capacity probe) is LANDED: the
   cliff was real (~3.3x at >256 objects) and the boxed `MEMBER_WRITE_CELLS`
   bump to 4096 removes it through ~4k-object sets at no measured warm-row
   cost. 1.8 (the >4096-object store ceiling) is LANDED (2026-09-04, this
   tree): a (map id, name)-keyed write table serves every instance of a
   shape at the map-pinned slot after the identity table thrashes, and a
   direct own-data resolve-and-write fallback serves vector-only (5th+)
   fields the map does not pin — the 8192/16384-object store rows drop
   ~181-223ns -> ~58-69ns/store interp (~162-196 -> ~44-54ns jit) and sit
   at the 64/1024 warm level, with the single-object rows unchanged. The
   remaining full-[[Set]] stores are true defines (a genuinely new key),
   which no IC can make faster without the L1c storage migration.
   Remaining candidates, in
   order: (a) the chain-member-read cost itself — probe DONE 2026-09-04:
   clean marginal warm chain reads (numeric values, bare-row-subtracted,
   both engines) are interp ~18ns / jit ~17ns vs own-data ~1-3ns, FLAT in
   link depth (1 vs 2 links identical) — the fixed member_chain_get
   validation dominates, not the walk; the JIT inline-probe experiment
   measured slower, and there is no shape-free slice, so the fix is L2's
   per-site shape/offset IC once the L1c representation lands; (b) the
   >4096-object store ceiling via L2 per-site store ICs once the shape
   representation exists; (c) extending 4.2's O(1) handler registration to
   the remaining agent-dependent modules — probe DONE 2026-09-04: the
   chain-bound residue is Object's (~40-arm chain: hasOwnProperty ~950ns,
   Object.hasOwn ~5µs — late arms pay ~35 `intrinsics.get`/call) and
   DataView's (~660ns) methods, but Object's dispatch arms are INLINE
   CLOSURES (registering them means refactoring to named fns — defer to L2
   or a dedicated mechanical pass); Map/Set/WeakMap are NOT chain-bound —
   they are O(n) per op (find_index/find_set_index linear-scans the
   entries Vec; Map.get ~2.9µs, Map.set ~3.7µs, Set.has ~5.5µs on a
   1024-entry map vs Math.abs ~155ns), a structural lever that registration
   cannot touch. So candidate (c) is superseded by (d): hash-index
   `map_data`/`set_data` (the entries Vec + a key index) — likely the
   largest remaining lever for Map/Set-heavy code, and O(n) is why no
   bench row exposes it. (d) is LANDED (2026-09-04, this tree): each
   Map/Set now carries a SameValue-consistent key-word index over its live
   entries (a `MapCollection`/`SetCollection` bundling the tombstoned
   entries List with the index), so get/has/set/delete probe O(1); a
   delete drops its row in O(1) and a word-absent probe is an
   authoritative miss (the exact scan runs only under a genuine 64-bit
   word collision, `collided`). A/B vs parent (fresh release builds,
   200k-call rows): the churn row (delete+set over a 1024-entry map, which
   tombstones and re-appends) drops ~34.5s -> ~0.9s (~38x); Map.get misses
   ~908 -> ~386ms; Map.get hits ~606 -> ~388ms and Set.has ~1245 -> ~1033ms
   with the row cost now FLAT in size (1024-entry == 16-entry rows), so
   the scans are gone. The per-call residual (~1.9µs Map.get / ~3-5µs
   Set.has) is the module's dispatch-chain arm, NOT a scan — so candidate
   (c) for the keyed module is the next slice: its dispatch arms are the
   named `(agent, this, args)` handlers 4.2's `handler_for` pattern wants
   (unlike Object's inline closures), and registration should collapse the
   Map.get/Set.has row floors toward the registered charCodeAt ~350ns
   floor. (c) is LANDED for the keyed module (4.3, 2026-09-04, this
   tree): Map.get/Map.set/Set.has rows drop to ~220-245ns/call
   (~7.8-18x vs the index-only rows), Set.has's late chain arm now equals
   Map.get, and the keyed row floor is the registered-call floor.
   WeakMap/WeakSet stay linear (their GC compaction renumbers
   slots, which a position index must clear at every sweep) — not in the
   measured rows. Remaining chain-bound modules from the (c) probe:
   Object (~40-arm; its `dispatch_call` arms are INLINE CLOSURES, so
   registration needs a named-handler refactor) and DataView (~660ns) —
   extend 4.2 only behind a corpus probe showing their methods hot; and
   the L2 per-site IC slices (a)/(b) stay behind the L1c shape
   representation. 4.4 (Object + DataView registration) is LANDED
   (2026-09-04, this tree): candidate (c) is CLOSED — every module whose
   methods a probe showed hot is now registered (String/Number/Boolean/
   BigInt/Keyed/Object/DataView); Object.hasOwn ~8.6x and DataView reads
   ~2.2-2.5x, and the only remaining linear chains are modules whose
   methods no probe has shown hot. The open structural items: the L2
   per-STEP store IC (for vector-only keys on shared maps and the JIT's
   shape-compare end-state) and the chain-member-read slice (a) both stay
   behind L1c's shape end-state; the JIT Float16Array/typed-array
   miscompile (0.1) is FIXED by the Linux work.

### Archive D - the L1c storage-options analysis (2026-09-05; superseded by the L1c decision section above)

Good question — this is the fork the thread had to settle, so let me lay out the actual trade space I found while reading the code, with names so we can talk precisely. The presize sub-question mostly dissolves once the storage options are clear, so I folded it in.

### The three storage shapes for "a mapped key with ordinal ≥ 4"

**Option 1 — Vector-slot pinning.** Keep the existing `SmallProps` insertion-ordered `(key, Property)` vector as the only store. A map descriptor at ordinal `o ≥ 4` means "this key lives at vector position `o`." Enforced by an append-alignment rule (transition a new key only when it lands at the descriptor boundary), with boilerplate presize capped at 4 (pre-described-but-unstored fields are only safe as in_fields holes below 4).

| | |
|---|---|
| **Pros** | Smallest change: no new per-object allocation, no new trace edge, no double write. Enumeration/delete/descriptor consumers keep working off the vector untouched. `map_field`/`map_set` extensions are ~30 lines. Matches the recorded B1 intent in `engine-redesign.md` ("`in_fields[offset]` **or the overflow store**"). Immediately makes the runtime read map cells (`MemberMapCell`) and shape write cells serve ordinals ≥ 4 — the map cell's `slot` becomes object-independent for every default key, so >4096-object 5th-field loops stop needing per-object slot resolution. |
| **Cons** | The ordinal==slot coincidence is an invariant that must hold forever — enforced at the three define sites, and fragile if a future path appends out of order (a missed gate silently misaddresses). The ≥4 read is still a `RefCell` borrow into `SmallProps` (not lock-free like `in_fields`), and — the big one — **it is not machine-addressable**: `SmallProps` can be inline or heap and the `Vec` reallocs, so the JIT can never emit a stable shape-compare + offset load for a >4 key from this. It pins the *logical* slot, not an address. |

**Option 2 — Dedicated out-of-line value array (mirror).** The object gains a per-object heap value array indexed by map ordinal ≥ 4 (V8's property backing store); `in_fields` + this array are the map-addressed value region. The vector stays authoritative for keys/attrs/enumeration; the array is a mirror, hole-capable (`Option<Value>` per slot), so boilerplate presize can pre-describe >4 fields and skipped fields read absent.

| | |
|---|---|
| **Pros** | Ordinal addressing is independent of vector order — **no alignment gate**, no scramble hazard. Presize can grow past 4 (a >4-field constructor starts on its full shape). Stable base pointer + fixed stride, so a future JIT inline load for >4 keys has a real target (this is the storage the "shape-compare + offset, JIT can emit inline" end-state actually wants). Read/write paths stay uniform (`map_field` is always an array access, lock-free). |
| **Cons** | Every mapped ≥4 value exists twice (vector + mirror): a second store per define and per warm write, a second trace path, and a new resize-on-transition path (the array must grow when an object's map transitions). Doubles memory for exactly the properties that are hottest at scale. Bigger diff than Option 1. It is a **middle state**: if the eventual end-state is field-authoritative (Option 3), this array later becomes the source of truth and the vector copy for mapped keys dies — so the mirror period is transitional overhead, not a step you'd keep. |

**Option 3 — Field-authoritative (the full V8 model).** Keys/attrs move entirely into the map's descriptors for map-described properties; the object holds only values (in_fields + an out-of-line array); the vector shrinks to overflow/dictionary/accessor/attr-drift cases. Enumeration, `own_property_keys`, `getOwnPropertyDescriptor`, `delete`, `property_slot`, the lazy index — everything currently reading the vector — becomes descriptor/field-aware.

| | |
|---|---|
| **Pros** | The clean end-state: one value copy, true shape semantics, natural JIT offsets, no mirror drift. |
| **Cons** | This is not a slice — it's the whole migration. `perf.md` L2846 (2026-09-03) *measured* the field-authoritative rewrite and rejected it as an interpreter lever (~5ns of a ~63ns row, "not justified") purely because every structural consumer must be reworked. Attr drift is the killer here: today `defineProperty` can make a mapped key non-enumerable/non-writable while the map keeps describing it (`sync_map_after_define` leaves it mapped); field-authoritative storage can't tolerate descriptor attrs that lie, so that path needs a fork-to-overflow or map-fork mechanism first. |

### What the evidence says

- **No current row moves under any option** — my probe (`scratch/l1c_cap_probe.js`) shows the 5th+ field define is ~same cost as the 1st-4th (~118ns/field construct churn), and warm 5th-field stores already equal warm 2nd-field stores (55ns interp) via the 1.8 direct fallback. So this slice is 100% downstream-unblocking, which makes **risk** the dominant selection criterion.
- **The presize sub-question dissolves**: even Option 2's full presize only removes a per-construct transition (a cached HashMap hit) — Options 1 and 2 both lose almost nothing by capping presize at 4. Don't let that drive the choice.
- **The end-state summary you attached says the point is "reads/writes via shape-compare + offset … which the JIT can emit inline."** Only Options 2/3 give the JIT an addressable offset for >4 keys. Option 1 cannot — its ordinals are logical slots into a movable, RefCell'd, sometimes-inline store.

### My recommendation

**Option 1 now, Option 3 later — skip Option 2 as a permanent state.**

Option 1 is the correct *first slice*: it is the smallest change that establishes "maps describe every default key with a real, shape-pinned offset," it immediately makes the interpreter's map cells exact for >4 keys (the only thing with any near-term effect), and — importantly — its alignment invariant (mapped keys are exactly the vector prefix, in descriptor order) is precisely the ordering a later field-authoritative migration needs, so it is not throwaway: Option 3 just splits that prefix's *values* out of the vector into an ordinal-indexed array and re-points enumeration at the descriptors. The recorded L2846 probe already told us Option 3's consumer rework isn't worth doing until a JIT/chain slice actually needs the addressable storage — and when that slice arrives, it should be gated on its own probe (a compiled member-read row with >4-key shapes), per the repo's one-lever-at-a-time discipline.

If you'd rather land the storage the JIT end-state needs *now* and eat the double-write/trace/memory cost in the interim, Option 2 is defensible — but I'd want a probe showing the >4-key JIT read row is actually hot before paying that.

Want me to proceed with Option 1 (alignment gate + presize-at-4 + `map_set`/`map_field`/`map_store_field` ≥4 + updated crux tests, gated on clippy/workspace tests), or did the thread specify something closer to Option 2?

_Resolution: Option 1 proceeded and landed as the map-describes-every-default-key slice; see the L1c decision section at the top of this file for the outcome and the remaining Option-3 gate._
