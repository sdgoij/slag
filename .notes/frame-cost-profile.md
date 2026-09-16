# Frame-cost profile — Slag Goat on Slag

Where a frame's time actually goes, measured in the running client, and what that
says about optimising the engine. Two engine revisions were compared because the
game lost ~7 fps when it moved to the second one; §6 shows that the loss was the
build rather than the revision, which is itself the most actionable finding here.

## 0. TL;DR

- **The frame is CPU-bound in the scene, not GPU-bound.** The buffer swap
  (`boundary`) costs 0.07–0.08 ms. There is no hidden GPU wait to find.
- **The biggest single cost is the herd: 5.4 ms/frame for 7 goats.** It is
  per-bot CPU skinning (`updateModelAnimation` deforms the mesh) plus one model
  draw per bot. No other phase is close.
- **The grass field is second: 4.2 ms** for ~370 immediate-mode cube draws per
  frame (plus 0.9 ms more in the shadow pass, which draws the same grass again).
- **Mods cost ~2.0 ms/frame** of that budget (`mods_upd` + `mods_draw3d`) with
  four mods loaded.
- **Engine `8a4209fa` is not shown to be slower.** It measured 17% slower, and
  then the *same source* — verified byte-identical — measured as fast as the old
  engine, depending only on how cargo built it. See §6.
- **A ±20% build-to-build spread is larger than anything else found here**, and it
  lives in the engine's compiled code: the pure-JS interpreter loop is identical
  between those two binaries, while every path that enters native code is 13–29%
  slower in the slow one. **`codegen-units = 1` + `lto = "thin"` removes it** and
  lands on the fast side, for ~7× the build time (§6).
- **The scene's loops are compiled; their global reads are not
  cell-accelerated.** The gate is not compilation — a body that names a true
  global (`Math`, `rl`, `TUNING`) compiles like any other. What costs is the
  *read*: the JIT's direct-mapped value cell serves global OBJECT-record data
  properties, and it is bypassed for (a) any read in a body whose env chain is
  not the bare global record, which is every helper nested inside a mod wrapper
  (`clean_chain`), and (b) the global env's DECLARATIVE record — any top-level
  `const`/`let`/`class`, even in a top-level body. A mod helper's `Math.abs`
  costs ~163 ns/iter where the same loop with the builtin frozen into a captured
  `const` costs ~27 ns (§4b, corrected). Calls, member accesses, captured
  bindings, nested loops and `continue` are all fine. Our scene hangs off `rl.*`
  and `TUNING`, so its kernels pay the resolve path on every read. **(b) landed
  2026-09-16** — a declarative read now hits the cell (52.5 ms → 2.5 ms in the
  corpus); **(a) landed 2026-09-16 too** — the gate is per name now, so a nested
  helper reads a global at top-level speed (130 ms → 2.5 ms per 1M reads). The
  freeze-into-a-captured-`const` workarounds below still pay (a context slot is
  cheaper than any cell), but they are no longer the difference between 24 and
  163 ns a read.
- **Removing global reads from three hot bodies recovered 2.1–2.3 ms/frame**: the
  grass grid 2.88 → 1.78 ms (`shadow_grass` 0.53 → 0.36), the goat collision
  resolve 0.49 → 0.22 ms, and the birds mod (0.70 → 0.47–0.53 update, 0.68 →
  0.31–0.37 draw) (§4b, §5b). The arithmetic, the 380 draw calls and the flock's
  behaviour are unchanged; only the reads moved.
- **The frame is at vsync, so these wins show up as headroom, not as fps.** The
  sum of the phases went 14.3 → 12.0–12.2 ms while `boundary` (the swap wait) grew
  by the same 2.1–2.3 ms and fps stayed 59–60. The budget is what changes; fps only
  will when the *sum* drops under ~16 ms with margin, or on hardware slower than
  this.
- **The lever for the engine is the value cell's coverage, then the
  crossing.** A frame here is also thousands of `rl.*` calls, but the microsecond
  figures §4 read off the probe are mostly resolve-path time, not the crossing
  (§4b): widen the cell and the scene's own kernels stop paying it — without any
  scene change.
- **The "once per frame" penalty of §5b was the JIT cache, and it is fixed.**
  In the same process the same body measured ~9 µs/call in a burst and ~226 µs/call
  reached once per frame: the app's body set exceeds the cache's capacity, so the
  cache cleared its LRU tail and `lookup_info` recompiled each cleared body on its
  very next call — a frame's whole budget at one call per frame. Landed in the
  engine (2026-09-16): the cache holds 1024 bodies before it evicts, and an
  evicted body re-earns the compile threshold instead of recompiling on the next
  call (~16× fewer recompiles under pressure).

## 1. What was measured, and how

| | |
| --- | --- |
| Binary | `cargo build --release -p goats`, the real client (window, vsync, audio) |
| Scene | the game as it stands, 7 bots, 4 mods, default settings, lit shader + shadow map on |
| Run | ~90 s each: load, `perf probe` ×3 after warm-up, `perf on`, 60–70 s of frames |
| Machine | one Windows box, same shell, same protocol for every run |
| Weather | offline and seeded, so the runs follow the same weather timeline and the windows line up |
| Resolution | per phase, ms/frame, averaged over 240-frame windows |

Engine revisions compared:

| revision | what it is |
| --- | --- |
| `2dba2c5e` | what `main` pinned before this work ("move the engine to 2dba2c5e") |
| `8a4209fa` | `feat(raylib): complete the rl surface's blending, textures and images` — the commit M19 needs (M19a), and the one the drop arrived with |

A third configuration was used to isolate the engine from the scene: the M19
scene code on the **old** engine. That runs at the old engine's speed, so the
scene-side changes are not the cause.

| configuration | fps |
| --- | --- |
| `main` (old engine `2dba2c5e`, git dep, no M19) | 56–62 |
| M19 scene, old engine, git dep | 55–61 |
| M19 scene, old engine, **path** dep | 55–60 |
| M19 scene, new engine `8a4209f`, **path** dep | 52–60 |
| M19 scene, new engine `8a4209f`, git dep | 45–49 |
| M19 scene, new engine, git dep, `tune explosions.enabled 0` | 46–54 |

## 2. Instrumentation

A `perf` verb was added to the console (this is a benchmark, not a feature):

```text
perf on          log the phase breakdown every 240 frames
perf             print the breakdown now and reset the window
perf probe       run the micro-probe: 4 loops of 20 000 iterations each
```

The phase marks live in `sceneFrame` (`crates/goats/src/game/goat.js`), in
`renderShadowMap` (`lighting.js`) and in the grass loop (`weather.js`); the block
itself is at the end of `goat.js`. Each mark is two `rl.getTime()` calls, so
turning the probe off leaves one boolean test per mark.

Two things to read the numbers with:

- **A phase time is submission plus whatever is synchronous inside the call.**
  The frame is vsynced, so a healthy frame is quantised to 16.7 ms; the
  breakdown decomposes the *budget*, and `boundary` is where the leftover waits.
- **`boundary` being 0.07 ms is the headline fact about the renderer**: the GPU
  keeps up, and the frame is long because our own CPU work is.

## 3. The frame, phase by phase

Representative steady-state window (n=240), ms/frame. Both columns are cargo
**git-dependency** builds — read §6 before reading them as a comparison of
revisions: the same source built as a path dependency lands on the *old*
column's numbers.

| phase | old `2dba2c5e` | new `8a4209fa` | Δ |
| --- | ---: | ---: | ---: |
| food (nearest tuft, each frame) | 0.08 | 0.10 | +0.02 |
| weather (state machine + wind) | 0.17 | 0.23 | +0.06 |
| clouds_upd (volumetric cloud state) | 0.99 | 1.27 | **+0.28** |
| audio | 0.19 | 0.22 | +0.03 |
| food_upd | 0.09 | 0.12 | +0.03 |
| fx_upd (mines + effects) | 0.18 | 0.22 | +0.04 |
| light (ambient + light + shadow state) | 0.20 | 0.23 | +0.03 |
| input (keys, mouse, camera) | 0.16 | 0.23 | +0.07 |
| goat_sim (gait state machine + movement) | 0.35 | 0.50 | **+0.15** |
| goat_pose (player skinning) | 0.78 | 0.80 | +0.02 |
| bots_ai (7 bots) | 0.20 | 0.28 | +0.08 |
| collide (goat vs herd, 2 passes) | 0.60 | 0.80 | **+0.20** |
| mods_upd (mod `update` hooks) | 0.85 | 1.00 | **+0.15** |
| *update subtotal* | *4.93* | *6.11* | *+1.18* |
| sky2d (sky shader or gradient) | 0.23 | 0.32 | +0.09 |
| shadow_grass (grass in the shadow pass) | 0.64 | 0.87 | **+0.23** |
| shadow_goat | 0.05 | 0.07 | +0.02 |
| shadow_bots (7 bots in the shadow pass) | 0.85 | 0.90 | +0.05 |
| stars | 0.07 | 0.10 | +0.03 |
| **tufts (grass, main pass)** | **3.48** | **4.16** | **+0.68** |
| goat (model + eyes) | 0.04 | 0.05 | +0.01 |
| **bots (7 skinned models)** | **5.37** | **5.38** | +0.01 |
| mods_draw3d (mod `draw3d` hooks) | 0.81 | 1.13 | **+0.32** |
| endmode3d | 0.08 | 0.09 | +0.01 |
| hud / mods_hud / ui | 0.32 | 0.40 | +0.08 |
| *draw subtotal* | *12.34* | *13.95* | *+1.60* |
| boundary (buffer swap) | 0.07 | 0.08 | +0.01 |
| **total** | **17.3** | **20.1** | **+2.8** |
| fps | 55–58 | 46–51 | −7 |

Phase times not listed individually (`rain_upd`, `rain2d`, `terrain`, `goat`,
`peers`, `fx`, `clouds`, `mod2d`, `shadow_tail`) are each ≤0.17 ms and weather-
dependent: `rain_upd` and `rain2d` rise to ~1.6 and ~1.0 ms while it rains.

Measured draw volume: **368–380 cube draws per frame** (grass, main and shadow
passes together).

## 4. The engine crossing (micro-probe)

Four loops, 20 000 iterations each, in one function, three consecutive probes
after the JIT has settled. Nanoseconds per iteration *as reported by the probe*.

| loop | old | new | Δ |
| --- | --- | --- | --- |
| `sink += i * 3` (nothing native) | 648 / 769 / 699 | 676 / 676 / 647 | **unchanged** |
| `sink += rl.getFPS()` (a native call) | 3131 / 3138 / 3184 | 3604 / 4141 / 3419 | **+12…+31%** |
| `sink += rl.WHITE` (a property read) | 5639 / 5553 / 5603 | 7713 / 6151 / 6935 | **+10…+37%** |
| `sink += Math.sin(i)` | 3244 / 3192 / 3226 | 3942 / 3679 / 4339 | **+14…+34%** |

**Read the rows relatively, not absolutely.** The arithmetic loop reports ~0.7 µs
per iteration, which no JIT'd loop would; the whole probe function contains
native calls, so it is executed by the interpreter and the absolute numbers
carry that overhead. What survives the caveat is the comparison:

- The **pure-JS floor is identical** on both revisions (0.65–0.77 µs).
- Everything that **touches the engine** — a call, a property read, even
  `Math.sin` — got **10–40% slower**, subtracting the floor: a crossing went from
  ~2.4 µs to ~2.9–3.4 µs, and a property read from ~4.9 µs to ~5.5–7.0 µs.
- A property read on `rl` measuring *more* than a call suggests those constants
  are not plain data. If they are native accessors, every `rl.KEY_W`-style read
  in the scene is a crossing.

**§4b supersedes the reading above.** `Math.sin` is not slow and `rl.WHITE` is not
an accessor: the *global reads* in that loop are not served by the value cell,
because the function holding it is nested and names `rl` and `Math`. The same
loop in a top-level function that reads a builtin runs ~7× faster (§4b,
corrected). The relative result (a native-touching loop got slower between the
two builds) still stands — that is a build effect on the interpreted path as
well.

## 4b. What the engine actually compiles

The `perf probe` loop that started this was the interpreter, not the crossing, and
the rule behind it is narrow enough to state, measure and code around.

`perf probe` reports `pureNs` (added during this work): the identical
`sink += i * 3` loop, in a function of its own with no engine call and no global
in it. Measured on the current binary it reports **9.7–56.6 ns** per iteration
(56.6 on the first probe, ~9.7 once the engine has settled) against **646–707 ns**
for the same loop beside the engine calls — a ~60× gap, which is the interpreter's
own per-step cost (~90–200 ns for the ~4 steps a loop iteration takes).

To find the *rule* rather than guess at it, `mods/jitprobe` (a throwaway
measurement mod, `jitprobe` verb) times one body per shape over 200 000
iterations, warm, after the JIT has been consulted. Nanoseconds per iteration;
four runs, same binary, same order as listed:

| body | ns/iter | verdict |
| --- | ---: | --- |
| `sink += i * 3`, parameters only | 9.2–15.4 | compiled |
| `+` a wrapper-scope `const` | 12.8–20.9 | compiled |
| `+` a two-level member chain on that const | 17.3–28.8 | compiled |
| `+` a call to a wrapper-scope function | 55.6–56.8 | compiled |
| `+` a call to a parameter | 13.6–13.9 | compiled |
| `+` `(i + 1) ** 0.5` | 32.1–35.4 | compiled |
| `+` a `continue` | 24.2–25.5 | compiled |
| `+` an `if`/`else` | 20.8–21.7 | compiled |
| nested loops with member reads *and writes* | 30.6–34.4 | compiled |
| **`+` a true global read (`Math.PI`)** | **2830–2913** | **interpreted** |
| **`+` a global object property read** | **3030–3051** | **interpreted** |
| **`+` a call to a global function** | **3030–3032** | **interpreted** |
| **`+` `Math.sqrt(i)`** | **2829–3286** | **interpreted** |

So the gate is: **a body that names a true global is not compiled.** A global read,
not a global *call*, is enough on its own; note the last four rows are ~100× the
rows above them and that swapping `Math.sqrt(i)` for `(i + 1) ** 0.5` moves a row
from the interpreted group to the compiled one. Calls are fine — including calls
to functions the body does not know statically — and so are member reads and
writes on objects reached through a local, a parameter or a captured binding,
nested loops, `continue`, and declarations inside blocks. The engine does have
`load_ident`/`get_global` slow paths for compiled bodies, so the cost is not the
lowering — and, corrected below, not certification either: **the scene is written
the way JS is normally written, and the read path normal JS uses is the one the
value cell does not cover.**

> **Correction (2026-09-16, measured on `main` `2dba2c5`).** The rule above is
> not what the engine does, and these magnitudes do not reproduce on `main`.
> Instrumenting `JitEngine::compile` (every bail prints its `Unsupported` step)
> and `lookup_info` (every consult prints step count, `scope`, `loop`) shows all
> four "interpreted" shapes **are** consulted, certified (`scope=true`, so the
> scope analysis accepts an undeclared name — `declared_depth.get(name)` missing
> is the accept case) and compiled: nothing bails and the cache stops consulting.
> What differs is the *read inside the compiled loop*:
>
> | body (2M iterations, warm, ns/iter) | jit | `--jitless` |
> | --- | ---: | ---: |
> | top-level declaration, `Math.abs(i - 50000)` | **24.0** | 57.0 |
> | top-level function expression, same body | 24.5 | 57.5 |
> | nested declaration (a mod helper), same body | **163.5** | 176.0 |
> | nested arrow, same body | 168.0 | 167.5 |
> | nested helper, `TUNING.step` | 185.5 | 173.5 |
> | nested helper, `const ABS = Math.abs` in the wrapper | 27.0 | 45.5 |
>
> Two mechanisms, one per row group:
>
> - **Nested bodies — `clean_chain`.** `crates/runtime/src/jit.rs` computes it as
>   `matches!(env, EnvRecord::Global(_)) && env.outer().is_none()`, and
>   `LoadIdent`'s cell probe is gated on it. The gate is sound — an intervening
>   env could shadow a name the cell records — but it is about the *body's env
>   shape*, not about the name, and a helper nested inside a wrapper fails it, so
>   every global read in its loop takes the `load_ident` resolve. The frozen
>   `const` removes the read entirely (a context-slot load), which is why the
>   workaround works.
> - **Declarative bindings — the cell cannot represent them.** A top-level
>   `const`/`let`/`class` lives in the global env's declarative record, not as a
>   property of the global object, so the cell (keyed by name, validated against
>   the global object's identity/generation) is never warmed for it — even in a
>   top-level body. The corpus's `globals` family times all three binding kinds on
>   one loop: `declarative_read` 52.5 ms, `hoisted_local` 2.5 ms,
>   `object_read` 2.5 ms, all compiled — 21×.
>
> The practical guidance below stands unchanged; the engine lever is not
> certification but **the value cell's coverage**. Letting a statically-unbound
> name use the cell from any body (the proof the scope analysis already has), and
> giving the declarative record a cell of its own, are both local changes, and
> neither needs a scene to change. The declarative half is analysed in its own
> note — `.notes/global-read-cells.md` — and **landed on 2026-09-16**: the cell
> is warmed for a declarative binding too (load-only, validated by a generation
> the global environment bumps on every declarative mutation), which takes the
> corpus row from 52.5 ms to 2.5 ms and closes the 21x gap. The nested-body half
> (`clean_chain`) landed the same day: the probe's gate is now per name, walked
> once per run against the body's `LoadIdent` names, so a mod helper's global
> read costs what a top-level one does (130 ms → 2.5 ms per 1M reads). That
> change also exposed and fixed a store-side gap — an in-place member write to a
> global object does not bump the generation the read cell validates, so the
> write now refreshes the cell directly. `.notes/perf.md`'s 2026-09-16 entries
> have both, including the hazard that forced the per-name verdict.
>
> Re-measure before sizing the win: the rows above are from a pin that is not
> `main`, built at cargo's default profile, and the `LoadIdent` cell probe is
> itself a later cut — that is the likeliest source of the ~15× gap between
> those figures and these.

This is not a fixture-only effect. All three of this session’s frame-level wins
are the rule in the scene:

| change | body | phase (ms/frame) |
| --- | --- | ---: |
| three `Math.imul`, one `Math.sin` and a `terrainHeight()` call moved out of the grass grid into parameters | `tuftField` | `tufts` 2.88 → 1.78, `shadow_grass` 0.53 → 0.36 |
| two `TUNING` reads and a `Math.sqrt` removed from the pair loop | `collidePairs` | `collide` 0.49 → 0.22 |
| every `Math.*` member frozen into a module-scope const (same functions, read as locals) | the birds mod’s step, pose and quaternion helpers | `mods_upd` 0.70 → 0.47–0.53, `mods_draw3d` 0.68 → 0.31–0.37 |

The grass change is the cleanest evidence: the grid walks the same 600 cells, does
the same hash, and issues the same ~380 draws — only the reads moved.

There are two forms of the same fix, and which one applies is a judgement about
the call graph. The grass grid and the collision resolve take **everything as a
parameter** (`out, cellH, eaten, imul, sin, groundY, …`); the birds mod instead
**freezes the members at module scope** and reads them as captured bindings,
which needs no change to a signature anywhere. A module-scope `const` in a mod
lives in the mod wrapper’s scope, and reading it from a nested function is a
context-slot read, not a global one — measured at 13–18 ns/iter in the table
above. Prefer freezing when many small helpers each need the same two or three
builtins; prefer parameters when a leaf body needs one or two.

**Open, and the next thing to chase:** the absolute cost of a *single* call to one
of these kernels does not match its cost in a burst. In the same process, the same
global-free body costs **~9 µs/call** called 2000 times in a row and **~226 µs/call**
called once per frame from the scene’s own update path (or from a console command),
stable across runs, with the measurement floor at ~2.9 µs. `collide`’s 0.22 ms per
frame matches the per-frame figure almost exactly, which suggests the scene’s
kernels are **still interpreted** and that what this session recovered is the cost
of the global reads that were removed rather than interpreter overhead in general.
What makes the per-call cost depend on the calling pattern — an eviction between
calls, the leaf-inline room check, a first-consult path — is the number to explain
in the engine: 226 µs for a body that measures 9 µs in a burst.
<br>(A second, smaller oddity in the same fixture: within one run the *same* body
measured 4147 ns/iter early and 93 ns/iter later, which is the same
pattern-dependence seen from the other side. Treat any single measurement of a
newly-called body as provisional until it is repeated.)

> **Resolved (2026-09-16, engine fix landed).** The dependence is the JIT cache,
> not the call itself. The scene's body set exceeds `MAX_CACHE_ENTRIES`, so a
> cache overflow cleared the LRU tail's per-body fast pointer, and `lookup_info`
> then recompiled each cleared body on its very next call — a loop body with no
> threshold at all, a straight-line body because its consult count was already
> past the threshold. A small body compiles in ~0.4 ms, which is the 226 µs/call
> figure once the compile is amortised over a frame with one call to the body in
> it, and it is the whole of the "still interpreted" reading: these bodies were
> compiled, then evicted, then compiled again, every frame.
>
> The engine's cache policy changed, not the scene's calling pattern: the cache
> holds 1024 bodies (evicting to 512) and an evicted body re-earns the 16-consult
> compile threshold. `.notes/perf.md` (2026-09-16) has the numbers and the
> verification; a client should confirm the scene-level win with `rl.getTime()`,
> which the engine cannot do for itself (no sub-millisecond timer).

## 5. Where our usage spends the frame

Top items, the slow git build of §3 beside the profile this repo now ships (§6):

| # | item | slow build | shipped | what it is |
| --- | --- | ---: | ---: | --- |
| 1 | `bots` | 5.38 | 5.37 | 7 bots: pose (CPU skinning) + model draw each |
| 2 | `tufts` | 4.16 | 2.77 | the grass field, ~370 immediate-mode cubes/frame |
| 3 | `clouds_upd` | 1.27 | 0.75 | volumetric cloud *state*, in JS, every frame |
| 4 | `mods_upd` | 1.00 | 0.72 | mods' `update` hooks |
| 5 | `mods_draw3d` | 1.13 | 0.68 | mods' `draw3d` hooks (the flock is the big one) |
| 6 | `shadow_bots` | 0.90 | 0.09–2.3 | the herd again, in the shadow pass (very variable) |
| 7 | `goat_pose` | 0.80 | 0.78 | the player's own CPU skinning |
| 8 | `collide` | 0.80 | 0.49 | goat-vs-herd resolution, two passes |
| 9 | `shadow_grass` | 0.87 | 0.49 | the grass again, in the shadow pass |
| 10 | `goat_sim` | 0.50 | 0.28 | gait state machine + movement |
| | *everything else* | ~2.0 | ~1.3 | weather, audio, food, input, hud, sky, stars, fx, terrain |

The order is the same either way; only the scale moves — except `bots`, which does
not move at all. That is the shape of the remaining work: one item is real per-call
work (CPU skinning of the herd) and everything else is the price of crossing into
the engine.

Two things stand out for the near term:

- **The grass is paid for twice.** `tufts` (4.16) + `shadow_grass` (0.87) = 5.0 ms
  for one visual idea, and 5.4 ms for the herd is the same order again.
- **Mods cost 2.0 ms/frame** with four mods, which is more than the shadow pass
  and more than the cloud state. Worth attributing per mod before adding more.

`shadow_bots` is worth a separate look: it ranged from 0.14 to 2.40 ms across
windows in the same run, which is a 17× spread on what should be a constant
amount of work.

## 5b. This session: the compile gate, worked around

Same machine, same protocol, same binary flags — only the bodies named in §4b
changed. A mod change needs no rebuild at all (the client loads `mods/` at
startup), so the birds row is measured in the same binary as the two before it.
All runs are `perf on` windows from a settled client, 7 bots, 4 mods, dry.
ms/frame:

| phase | before | + scene kernels | + birds |
| --- | ---: | ---: | ---: |
| `tufts` | 2.88 | 1.78 | 1.76–1.77 |
| `shadow_grass` | 0.53 | 0.35–0.36 | 0.28–0.36 |
| `collide` | 0.49 | 0.22 | 0.21–0.22 |
| `mods_upd` | 0.69–0.77 | 0.70–0.79 | **0.47–0.53** |
| `mods_draw3d` | 0.68–0.69 | 0.68 | **0.31–0.37** |
| `shadow_tail` | 0.14–0.15 | 0.14 | 0.03–0.13 |
| `endmode3d` | 0.08 | 0.08 | 0.02–0.09 |
| **sum of phases** | **14.31** | **12.78** | **11.99–12.21** |
| `boundary` | 2.22 | 3.55 | 4.24–4.32 |
| frame total | 16.53 | 16.33 | 16.31–16.45 |
| fps | 59 | 60 | 59–60 |

Ranges are run-to-run, and one of them has a known cause: the grass cull follows the
goat, so `tufts`, `shadow_grass` and the two tail spans move with it — the runs
above drew 347–381 cubes/frame. The `mods_*` rows are the ones to read: they are
the flock, they do not depend on where the goat is, and they fell by 0.24 and 0.34
ms/frame.

Two things to read from this. The JS phases fall by 2.1–2.3 ms and `boundary`
(the swap wait) takes exactly that back: the frame was already vsync-limited at
59–60, so the win appears as **headroom** — about a third of the 16.7 ms budget
returned — not as fps. And the phases that did not change are the ones the rule
predicts: `bots` 5.45–5.5 (the herd’s CPU skinning and model draw, inside a single
engine call), `goat_pose` 0.77–0.83 (the same for the player), and the rest of the
scene, which still reads globals.

One earlier attempt in this same phase **did not** pay: replacing the birds’
`update` loop’s four `Number.isFinite` global reads with one call changed
`mods_upd` by nothing measurable. That was the right shape for the wrong body —
the cost was never in the loop, it was in the O(n²) neighbour loop inside
`stepFly` and in the ~10 small helpers each bird calls, every one of which named
`Math` in its own body. Freezing the members (§4b) is what moved it.

## 6. The `8a4209f` "regression" is a build artifact

The first conclusion drawn from §3 was that engine `8a4209f` costs 17% of the
frame. **That does not survive checking.**

The commit touches exactly one code file, `crates/runtime/src/raylib.rs`
(+600/−68); no `Cargo.toml`, no new dependency. Its source is byte-identical to
what cargo's git checkout holds for the same revision — `diff -rq
--strip-trailing-cr` over the whole `crates/` tree reports no difference — so the
only source difference between the two builds is line endings (the git checkout
is LF, a Windows checkout is CRLF).

Building that one source two ways, and alternating the two binaries in a single
session:

| binary (both built from `8a4209f`) | `tufts` | `mods_draw3d` | fps |
| --- | ---: | ---: | ---: |
| cargo **path** dependency | 3.47–3.49 | 0.82–0.84 | 52–55 |
| cargo **git** dependency | 4.48–4.81 | 1.07–1.15 | 45–49 |
| *path again* | 3.48–3.49 | 0.83 | 52–55 |
| *git again* | 4.48–4.81 | 1.07–1.10 | 45–49 |

The micro-probe on those two binaries:

| loop | path build | git build | Δ |
| --- | ---: | ---: | ---: |
| `sink += i * 3` (nothing native) | 654–795 | 611–718 | **unchanged** |
| `sink += rl.getFPS()` | 3154–3167 | 3532–3881 | +13…+23% |
| `sink += rl.WHITE` | 5495–5632 | 6626–6982 | +18…+27% |
| `sink += Math.sin(i)` | 3188–3353 | 4039–4107 | +22…+29% |

And the full matrix for the engine revisions:

| engine source | dependency kind | fps | `tufts` |
| --- | --- | ---: | ---: |
| `2dba2c5e` (old) | git | 55–61 | 3.48 |
| `2dba2c5e` (old) | path | 55–60 | 3.52 |
| `8a4209f` (new) | path | 52–60 | 3.46–3.49 |
| `8a4209f` (new) | git | 45–49 | 4.48–4.81 |

What this says:

- The slow binary is **not** explained by its source: the same source built the
  other way is as fast as the old engine.
- Being a git dependency is **not** the cause either: the *old* source built as a
  git dependency is fast.
- The difference is in what the source was **compiled into**, not in what it does:
  the pure-JS interpreter loop is identical in both binaries, while every path
  that enters native code is 13–29% slower.
- Three of four builds are fast, which is what an unlucky code layout looks like,
  and there is a known mechanism for it: cargo passes a crate hash
  (`-C metadata`) that includes the package's **source id**, and that hash feeds
  codegen-unit partitioning. At the default `codegen-units = 16`, the same source
  is therefore split into different units, inlined differently and laid out
  differently. The two binaries differ by 1,536 bytes — the size of a few embedded
  path strings at these path lengths.

So the earlier attribution is withdrawn: `8a4209f` is not shown to be slower. The
honest statement is that **this engine currently has a build-to-build spread on
native dispatch that is larger than any source change we can measure** — which is
worth acting on in its own right, because it makes a real regression and an
unlucky build indistinguishable from the outside.

Two observations from trying to pin it:

- Adding an inert item to the engine (`fn layout_pad_a() -> u64`) did **not** move
  the fast build (57–60 fps). The sensitivity is real but not triggered by every
  change, which is exactly what makes it hard to control from outside.
- The one build that is consistently slow is reproducible: rebuilding the git
  dependency reproduces 45–49 fps and `tufts ≈ 4.0–4.8`, so it is not noise
  between runs — it is that binary.

### The experiment, run

Adding `codegen-units = 1` to `[profile.release]` and rebuilding the *same* source
(git dependency, `8a4209f`) recovers the whole spread:

| | slow build (16 CGUs) | fast build (path dep) | `codegen-units = 1` |
| --- | ---: | ---: | ---: |
| fps | 45–49 | 52–60 | **51–56** |
| `tufts` | 4.01–4.81 | 3.46–3.49 | **3.49–3.54** |
| `collide` | 0.80 | 0.60 | **0.59–0.60** |
| `goat_sim` | 0.50 | 0.34 | **0.34–0.35** |
| `clouds_upd` | 1.20–1.28 | 0.99 | **0.95–0.96** |
| `mods_draw3d` | 1.07–1.20 | 0.81 | **0.87–0.89** |
| probe `rl.getFPS()` | 3532–3881 | 3154–3167 | **3113–3161** |
| probe `rl.WHITE` | 6626–6982 | 5495–5632 | **5447–5592** |
| probe `Math.sin(i)` | 4039–4107 | 3188–3353 | **3013–3267** |
| probe `i * 3` | 611–718 | 654–795 | **662–714** |

So the mechanism is confirmed in effect even if the exact code movement is not
visible: putting the engine in one codegen unit removes the freedom the crate hash
was exploiting, and the build lands on the fast side, reproducibly.

Both LTO modes were then measured against one codegen unit alone:

| | 16 CGUs | 1 CGU | **1 CGU + thin LTO** | 1 CGU + fat LTO |
| --- | ---: | ---: | ---: | ---: |
| fps | 45–49 | 51–56 | **55–62** | 57–61 |
| `tufts` | 4.01–4.81 | 3.49–3.54 | **2.77–2.82** | 2.85–2.92 |
| `collide` | 0.80 | 0.59–0.60 | **0.49–0.51** | 0.51–0.53 |
| `goat_sim` | 0.50 | 0.34–0.35 | **0.27–0.29** | 0.29–0.30 |
| `clouds_upd` | 1.20–1.28 | 0.95–0.96 | **0.75–0.77** | 0.76–0.82 |
| `bots` | 5.36–5.40 | 5.45–5.48 | **5.37–5.45** | 5.52–5.66 |
| `boundary` | 0.08 | 0.22 | **0.07–0.15** | 0.19–0.81 |
| build | ~30 s | 2 m 35 s | **3 m 27 s** | 6 m 22 s |

The frame lands at ~16.3 ms either way, so the two LTO modes are a tie on frame
time. Thin is kept because its phases are consistently a little lower, it does not
regress `bots`, and it builds in half the time. Note `boundary` in the fat build:
at 0.19–0.81 ms the frame has come close enough to the 16.7 ms budget that the
swap starts absorbing the slack — a different problem from the one this document
opened with.

The profile removes the *disqualifying* spread (the 13–29% one); it does not make
the measurement perfectly stable. Re-measuring the thin build later gave `bots`
6.04–6.09 with `boundary` 0.41–1.08 and 59–63 fps — one phase still varies by
~12% between builds, but the frame is now at the vsync cap, so it no longer shows
in the frame rate.

One thing that did **not** move in any of the four builds: `bots` (5.36–5.66
throughout). The herd's cost is inside `updateModelAnimation` and `drawModelEx` —
real work per call, not crossing overhead — so no build flag can touch it. At
5.4 ms of a 16.3 ms frame it is now the whole game.

## 6b. What survives from the original attribution

Everything in §3–§5: the phase profile is of a *binary*, not of a source change,
and it was measured on a build whose input was unchanged between the phases. The
per-phase shape (herd 5.4 ms, grass 4.2 ms, mods 2.0 ms, shadow pass ~1.9 ms) is
unchanged, and `boundary ≈ 0.08 ms` is unaffected.

What does not survive is the claim that the *engine commit* is the cause. The
table in §3 should be read as "this build is faster than that build", not as
"this revision is faster than that revision".

## 7. Recommendations

### 7.1 Engine

0. **Keep the release profile as it is: `codegen-units = 1` + `lto = "thin"`**
   (§6). Together they recovered the whole build spread and then some: the same
   engine source and the same scene went from 45–49 fps to 55–62, with every phase
   improving except the herd's own work. Cost: ~3 m 27 s per release build.
   `lto = "fat"` is a tie on frame time and twice the build.
1. **Widen the value cell's coverage before attacking the crossing.** The JIT
already has the `load_ident`/`get_global` slow paths, so nothing needs
relaxing in certification (§4b, corrected): a body that names a global
compiles today. Two local gaps, both measured — one closed, one open:
- **Landed (2026-09-16): the global env's declarative record.** The cell is
  warmed for a top-level `let`/`const`/`class` too now — load-only, validated
  by a generation the global environment bumps on every declarative mutation
  — so the corpus `globals` row goes **52.5 ms → 2.5 ms** and the family gap
  30.9x → 2.8x. No scene change needed (our `TUNING` is a mod-wrapper
  `const`, i.e. already a context slot), but every top-level binding in a
  bundled script now reads like a local. `.notes/perf.md` (2026-09-16).
- **Landed (2026-09-16): the `clean_chain` gate**, which skipped
  `LoadIdent`'s cell probe for any body whose env is not the bare global
  record — i.e. every helper nested inside a mod wrapper, where §4b measured
  **163 ns/iter vs ~24**. The gate is per name now (a once-per-run walk over
  the names the body reads), so the same read measures 2.5 ms per 1M in the
  corpus for both shapes. This is the half the scene paid on `rl.*`/`Math`
  reads inside mod functions. One caveat to expect: the relaxation also had to
  fix a store-side gap it exposed (an in-place member write to a global
  left the read cell stale) — see `.notes/perf.md` (2026-09-16).
2. **The calling-pattern dependence (§4b) — resolved (2026-09-16): it was the
JIT cache.** A ~9 µs/call burst body costing ~226 µs/call once per frame was
the cache recompiling what it had just evicted: the app's body set exceeds
`MAX_CACHE_ENTRIES`, and every cleared body recompiled on its next call, so a
body reached once per frame paid a ~0.4 ms compile per frame. Landed: the cap
is 1024 (evicting to 512) and an evicted body re-earns the compile threshold,
which cuts the recompile rate by the threshold factor under pressure. The
scene-level win wants a client-side `rl.getTime()` re-measure — the engine has
no sub-millisecond timer — see `.notes/perf.md` (2026-09-16).
3. **Attack the per-crossing cost.** A frame here is thousands of `rl.*` reads
   and calls. Concrete things to check: whether each call builds
   an arguments array or formats anything on the success path; whether the texture
   registry's `Mutex` is taken per draw.
4. **GPU skinning.** CPU skinning is what makes the herd cost 5.4 ms and what
   forces one model per goat (`updateModelAnimation` deforms the mesh itself, so
   two goats cannot share one). Bone matrices as uniforms would collapse both the
   cost and the memory.
5. **A batched immediate-mode path.** ~370 `drawCube` crossings per frame for the
   grass; anything that lets the scene submit N quads in one crossing (or a mesh
   it can rebuild) removes them. Note that the JS *round* the draws is already
   compiled-eligible after this session, so the remaining crossing cost is what is
   left to attack.

### 7.2 Scene

1. **Write hot bodies the way the gate wants them (§4b).** Two forms, both in
   use now: **parameters** (`collidePairs`, `tuftField` — the members of `TUNING`
   it needs, the caches it touches, and any builtin it calls are all passed in)
   and **frozen module-scope consts** (`mods/birds`, where `PI`/`SIN`/`COS`/… are
   captured bindings instead of `Math.*`, needing no signature change anywhere).
   Let the caller, which is interpreted anyway, do the global reads and the
   `rl.*` calls. Use `** 0.5` for a square root and `x < 0 ? -x : x` for
   `Math.abs`; both keep a body compiled where the `Math` call would not.
2. ~~The birds mod is the next candidate~~ **done** (§5b): `mods_upd` 0.70 → 0.47–0.53,
   `mods_draw3d` 0.68 → 0.31–0.37. What worked was freezing `Math`’s members; what
   did not was replacing the update loop’s own `Number.isFinite` reads, because the
   cost was never in that loop. `mods/birds` is the template for the rest: the
   helpers are untouched behaviourally, they just stop naming a global.
3. **Draw the grass once.** Build the visible field as a mesh instead of ~370
   immediate-mode cubes, and/or cut the shadow pass's grass (`shadow_grass`,
   0.35 ms after this session, draws the same grass the main pass already drew).
4. **Attribute the mods.** ~0.9 ms/frame for four mods, down from 1.4; measure
   per mod and per hook before adding a fifth.
5. Keep the herd honest: `bots_ai` is 0.16 ms while `bots` (pose + draw) is
   5.5 ms. The AI is not the problem; the rendering of it is, and it is one
   engine call per bot.

## 8. Caveats

- One machine, one scene, one spot in the meadow, vsync on. The numbers are a
  CPU-budget decomposition, not a GPU profile.
- Phase times are the scene's own view: submission plus synchronous work. GPU
  time would appear in `boundary`, and `boundary` is 0.07 ms.
- Weather-dependent phases (`rain_upd`, `rain2d`, cloud state) vary by design;
  the windows quoted here are dry ones, and the runs are seeded so they line up.
- The micro-probe’s absolute magnitudes include interpreter overhead (§4); use
  it for the comparison, not the absolute cost of a call. §4b is the reason: the
  probe’s own loops are interpreted.
- **A kernel’s cost depends on how it is called (§4b).** The same global-free body
  measures ~9 µs/call in a burst and ~226 µs/call once per frame. Any conclusion
  from a single call pattern must be repeated in the other before it is trusted;
  the four-run shape table in §4b was stable, the absolute per-call figure was not.
- The `collide` and `tufts` rewrites were validated with the scene harness
  (`cargo test -p harness --test scene_logic -- --ignored`, 158 checks), which
  asserts the collision invariants and that the grass field is derived rather than
  stored.
- **The engine comparison in §3/§6 is between builds, not between revisions.**
  The same source built two ways differs by 13–29% on native dispatch; treat any
  single-revision comparison as provisional until the build is stabilised.
- **The absolute per-iteration figures in §4b are pin-specific.** They come from
  a revision that is not `main` and a build at cargo's default profile; on `main`
  the same nested shape measures 163 ns/iter compiled and 176 interpreted, not
  ~2900. The *grouping* and the workarounds survive; the magnitude does not.
- `perf on` itself costs two `rl.getTime()` calls per mark (~36 marks/frame);
  it is a measurement overhead of the same order as one crossing per mark.

## Appendix A — raw windows

New engine (`8a4209fa`), `perf on` windows in order (three shown at the start of
the run, then the steady state):

```text
[...] clouds_upd 1.28 collide 0.81 shadow_grass 0.87 tufts 4.20 bots 5.40 mods_draw3d 1.11 cubes/frame 368 fps 48
[...] clouds_upd 1.27 collide 0.80 shadow_grass 0.86 tufts 4.17 bots 5.37 mods_draw3d 1.13 cubes/frame 364 fps 47
[...] clouds_upd 1.20 collide 0.81 shadow_grass 0.89 tufts 4.12 bots 5.43 mods_draw3d 1.12 cubes/frame 371 fps 45
```

Old engine (`2dba2c5e`), same protocol:

```text
[...] clouds_upd 0.99 collide 0.60 shadow_grass 0.63 tufts 3.48 bots 5.37 mods_draw3d 0.81 cubes/frame 369 fps 57
[...] clouds_upd 1.02 collide 0.60 shadow_grass 0.63 tufts 3.47 bots 5.37 mods_draw3d 0.81 cubes/frame 369 fps 57
[...] clouds_upd 0.98 collide 0.59 shadow_grass 0.62 tufts 3.47 bots 5.37 mods_draw3d 0.81 cubes/frame 369 fps 57
```

Probes, three consecutive each:

```text
old  {"callNs":3130.86,"propNs":5639.07,"jsNs":3244.21,"arithNs":648.43,"fps":53}
old  {"callNs":3137.59,"propNs":5552.95,"jsNs":3192.19,"arithNs":768.51,"fps":53}
old  {"callNs":3183.88,"propNs":5603.14,"jsNs":3225.85,"arithNs":698.78,"fps":53}
new  {"callNs":3603.81,"propNs":7713.00,"jsNs":3941.80,"arithNs":675.56,"fps":54}
new  {"callNs":4140.84,"propNs":6151.41,"jsNs":3679.09,"arithNs":676.07,"fps":54}
new  {"callNs":3418.88,"propNs":6934.52,"jsNs":4339.37,"arithNs":646.99,"fps":55}
```

Fps measurements (80 s runs, `frame … fps` log lines):

```text
main, old engine, no M19     56 62 58 58 58 61 62 61 59 59 59 60 61 60 59 59 58
M19 scene, old engine        60 57 61 61 61 61 61 61 60 61 60 61 60 61 59 56 55
M19 scene, new engine        53 50 49 50 50 52 54 53 51 55 56 56 49 48 48 50
```

This session’s three stages, `perf on` for 64 s (7 bots, 4 mods, dry), before and
after the §4b rewrites — first and last window of each run, to show the spread:

```text
before  [...] collide 0.50 tufts 2.88 shadow_grass 0.53 bots 5.46 mods_upd 0.72 cubes/frame 381 fps 59
before  [...] collide 0.49 tufts 2.87 shadow_grass 0.53 bots 5.49 mods_upd 0.69 cubes/frame 380 fps 59
after   [...] collide 0.22 tufts 1.78 shadow_grass 0.36 bots 5.48 mods_upd 0.70 cubes/frame 381 fps 60
after   [...] collide 0.22 tufts 1.75 shadow_grass 0.35 bots 5.47 mods_upd 0.79 cubes/frame 375 fps 60
birds   [...] collide 0.22 tufts 1.76 shadow_grass 0.35 bots 5.54 mods_upd 0.51 mods_draw3d 0.37 cubes/frame 372 fps 59
birds   [...] collide 0.22 tufts 1.76 shadow_grass 0.36 bots 5.53 mods_upd 0.53 mods_draw3d 0.36 cubes/frame 366 fps 59
birds2  [...] collide 0.21 tufts 1.77 shadow_grass 0.28 bots 5.45 mods_upd 0.53 mods_draw3d 0.31 cubes/frame 347 fps 60
birds2  [...] collide 0.21 tufts 1.77 shadow_grass 0.28 bots 5.45 mods_upd 0.47 mods_draw3d 0.31 cubes/frame 347 fps 60
```

The `birds` runs also sanity-check the flock, since the change was supposed to be
behaviour-neutral: `birds` reported all five flight states in use across the runs
(`{"idle":1,"fly":3,"perch":1,"land":1}` then
`{"fly":2,"takeoff":1,"walk":1,"idle":2}`), with `near`/`far` of 3.5–9.1 and
15.3–23.7 m from whichever anchor the flock had picked — the same ranges as before
the change.

The full shape table from §4b, one run verbatim (`jitprobe` and
`jitprobe-globals` from `mods/jitprobe`, which is a measurement fixture and not
part of the game):

```text
jitprobe N=200000 (10 ns/iter = compiled, 150+ = interpreter)
arith                  9.5 ns/iter
outer-read            13.3 ns/iter
outer-chain           18.1 ns/iter
global-read         2885.3 ns/iter
call-outer            56.3 ns/iter
call-param            13.8 ns/iter
sqrt-global         3286.1 ns/iter
pow-half              35.4 ns/iter
continue              25.5 ns/iter
if-else               21.6 ns/iter
nested-7              34.4 ns/iter
collide-current     4983.3 ns/iter
collide-leaf        4148.3 ns/iter
jitprobe-globals N=200000
outer-fn-call         17.9 ns/iter
global-fn-call      3030.2 ns/iter
global-value        3051.1 ns/iter
```

The next run of the same binary put `arith` at 15.4 and `nested-7` at 31.2 — the
ranges in §4b are the envelope of four runs, and the *grouping* was identical in
all four.
