# Browser integration: replacing V8 in Chromium (or not)

Research note. The question: Slag stands in for V8 for Deno through the `v8`
crate (`crates/v8`, `.notes/embedding.md` §6). Could it also stand in for V8 in a
browser, Brave or Chromium, with `cxx.rs` as the bridge? This note answers that,
separates the parts that are merely large from the parts that are structurally
different, and names the smallest experiments that would falsify the answer.

Status: research only. No code in this repository is changed by it. The
recommendation is in §8.

## 1. The short answer

`cxx.rs` is a good tool, but it does not bridge the boundary that a browser
needs. It does not parse or bind `v8.h`; it takes a hand-written
`#[cxx::bridge]` declaration and generates glue. So the work it removes is the
marshalling around a shim, not the shim itself. The shim is a hand-written C++
implementation of V8's C++ API, and that is the actual project.

More importantly, the language boundary is not the hard part. Chromium's Blink
does not consume a flat engine API. It compiles generated C++ bindings against
`v8.h`, and it shares object lifetime with V8 across two garbage-collected
heaps: Blink's Oilpan heap (C++) and V8's heap. Replacing V8 means replacing
that contract, not just exporting functions.

Verdict:

| Route | Feasibility | When |
|---|---|---|
| Replace V8 inside Chromium/Brave via a `v8.h` facade over Slag | possible, multi-year, security-critical, not a library substitution | research spike now, product later |
| Retarget an open C++ browser's bindings generator to Slag | large but bounded and reviewable | the realistic first browser |
| Build a Rust browser shell with Slag as its engine and no `v8.h` | tractable, reuses the Rust boundary Slag already has | start here |

Brave is Chromium plus a patch stack, so it inherits the Chromium problem and
adds a downstream maintenance burden. It is a consequence of a Chromium
solution, never a first target.

## 2. Three layers that "replace V8" can mean

The repo already distinguishes these for Deno (`.notes/embedding.md` §3, §4),
and the distinction decides everything here.

| Layer | What it is | Deno today | A browser |
|---|---|---|---|
| Source | the consumer compiles declarations, or an API it can call | the Rust `v8` crate API | Blink compiles `v8.h`, `v8-inspector.h`, and generated `V8Foo` classes |
| Link | symbols and ABI | one Rust crate, statically linked | a static `v8` library built by GN and linked into Blink |
| Semantics | GC, handles, contexts, scheduling, errors, security | finite, enumerable | huge, and load-bearing for correctness and security |

`.notes/embedding.md` §3 states the position this note has to argue with: C++ has
no interface, because the consumer compiles the source. That is exactly the
browser case. A `v8.h` facade is possible because we would write that source,
but it is a compatibility artifact, not an ABI, and every consumer build is a
compatibility test.

## 3. What `cxx.rs` actually provides

Verified from the cxx book (`extern "C++"`, built-in bindings):

- A bridge is written by hand, as `#[cxx::bridge]` with `extern "C++"`
  declarations and `include!("...")` lines. CXX does not parse arbitrary headers.
- C++ types cross as opaque types behind an indirection (`UniquePtr`, `&T`,
  `Pin<&mut T>`). Shared structs and functions use a fixed type table:
  primitives, `String`/`str`, slices, `Box`, `UniquePtr`, `SharedPtr`, arrays,
  `Vec`, `CxxVector`, raw pointers, function pointers, `Result`.
- There is no general template support. `Option<T>` and `Arc<T>` are listed as
  pending.
- The generated C++ glue must be compiled into the consumer's build
  (`cxx-build`/`cxxbridge`). It is not an ABI either.

None of that can express `v8::Local<T>`, `v8::MaybeLocal<T>`, `v8::Maybe<T>`,
`v8::FunctionCallbackInfo<Value>&`, the inline accessors on `Object`,
`ObjectTemplate`, `FunctionTemplate`, or `Value`. Those are the shapes Blink
writes against. To use CXX you would first write a flat C++ header of free
functions over V8's types, then declare those functions in a bridge. At that
point the bridge is a convenience, not the solution.

There is a second reason CXX cannot own this boundary even in principle: the
facade must be C++ that Blink includes. CXX-generated headers are for Rust
callers; Blink will never include them. So CXX, if used at all, lives *inside*
the facade, between the hand-written `v8.h` layer and Slag's C ABI. The public
artifact remains a hand-written `v8.h` plus an implementation, which is the same
shape as `rusty_v8`'s `binding.cc` run in the opposite direction.

What CXX is genuinely good for, and worth using:

- a typed, checked seam for the non-hot operations between `api.cc` and Rust;
- `UniquePtr`/`SharedPtr` ownership of facade-side objects;
- static assertions that signatures match, for a surface that will otherwise
  drift silently.

What it is not: a way to consume `v8.h`.

## 4. Where the cost is: Blink's actual surface

The measured Deno boundary is `binding.cc` at 5,110 lines and 321 distinct
`v8::` symbols (`.notes/engine-api-study.md` §1). That is the *small* number.
Blink is not a host on top of a Rust API; it is the generated-binding layer of a
browser, and the hand-written bindings core alone
(`third_party/blink/renderer/bindings/core/v8/`) is over a hundred source files
before the generated `V8Foo` class for every IDL interface.

A sample of what is in that directory, from the upstream tree:

| File (or family) | Size signal | Why it matters |
|---|---|---|
| `script_streamer.cc` | 105 KB | compiles scripts on a background thread while they download |
| `v8_compile_hints_producer.cc` | 87 KB | persistent code-cache hints |
| `native_value_traits_impl.h` | 73 KB | IDL conversion for every WebIDL type |
| `v8_script_runner.cc` | 49 KB | script evaluation, error reporting, cache integration |
| `v8_initializer.cc` | 47 KB | snapshot, flags, platform, fatal handlers |
| `v8_gc_controller.cc`, `trace_wrapper_v8_reference_test.cc` | | the V8/Oilpan GC bridge |
| `script_cache_consumer.cc`, `v8_code_cache.cc` | | code cache format and consumption |
| `binding_security.cc`, `world_safe_v8_reference.cc` | | cross-context access checks |
| `serialization/` | directory | structured clone for `postMessage` |
| `profiler_trace_builder.cc`, `v8_metrics.cc` | | CPU profiles and metrics |

Compiling those against a facade is the compatibility test. It is also a good
estimate of scope: every symbol they use must exist and behave.

### 4.1 What the bridge already has, and what it does not

Measured against this repository, not inferred.

| Requirement | Evidence in Blink | State in `crates/v8` / engine | Gap |
|---|---|---|---|
| Script identity, origin, code cache | `v8_code_cache.cc`, `v8_script_runner.cc` | `ScriptCompiler::Source` carries origin and offsets; `CachedData`, `compile_function`, and `create_code_cache` round-trip; `cached_data_version_tag` is `1` | small, except streaming and cache-format versioning |
| Streaming compile | `script_streamer.cc` | all work runs inline; no background parse/compile | large |
| Wrapper pointer storage | `SetAlignedPointerInInternalField` | `ObjectTemplate::set_internal_field_count` records a count only (`crates/v8/template.rs`) | large |
| Named/indexed interceptors | `ObjectTemplate::SetHandler` | `crates/v8/interceptor.rs` exists | partial |
| Access checks and signatures | `binding_security.cc`, `Signature`, `AccessorSignature` | no `Signature`; security tokens are "stored, and not consulted" (`crates/v8/context.rs`) | large, security |
| Promise hooks and microtasks | `v8_microtasks_scope.cc` | `set_promise_hooks` and the microtask policy landed | mostly |
| Structured errors and source maps | `Message`, `StackFrame`, `capture_source_location.cc` | `api::StackFrame` (function name, script name, line, column, eval/constructor/wasm/async flags) and `Isolate::capture_stack` landed; the simple path still renders text | medium: script ids, byte offsets, source-map URLs, and `get_script_source` are absent |
| Inspector | `v8-inspector.h`, `core/inspector` | only the CDP `Runtime` domain; everything else refuses (`crates/v8/inspector.rs`) | large |
| Profilers and heap snapshots | `profiler_trace_builder.cc`, `HeapProfiler` | none | large |
| Structured clone | `serialization/` | none | medium |
| Wasm | `v8_wasm_response_extensions.cc`, `WasmModuleObject` | an engine exists; the V8-specific shapes do not | medium |
| ArrayBuffer allocation | `ArrayBuffer::Allocator`, PartitionAlloc | blocks own their storage | medium |
| Snapshots | `v8_context_snapshot.cc`, `mksnapshot` | Slag's own format, with a documented restore limitation | medium |
| Threading and locker | workers, compositor workers sharing an isolate | thread-local agents | medium to large |

The Intl tail is also visible in the certification numbers: 3,205 of 3,357
`intl402` fixtures pass with 152 skips (`.notes/embedding.md` §5). A browser
cannot treat those skips as free.

## 5. The GC problem is the real wall

Blink keeps DOM objects in Oilpan, a C++ tracing GC. JS wrappers live in V8's
heap. The two heaps have to agree about liveness, and the agreement has
semantics a browser depends on:

1. **Wrapper identity.** For as long as the C++ DOM object is alive, the same JS
   wrapper is returned to JS in a given world. Blink implements this with
   `ScriptWrappable` for the main world and `DOMDataStore` for the others
   (`V8BindingDesign.md`, upstream).
2. **Worlds.** One isolate has N contexts (frames times worlds), and wrappers
   are not shared across worlds even though the underlying C++ objects are.
3. **Cross-heap cycles.** A JS object can reference a DOM object that references
   JS state. Neither collector may free the pair while the other holds it.
   V8 exposes this through `v8-embedder-heap.h` (`EmbedderRootsHandler`,
   `TracedReference`) and cppgc; Blink layers Oilpan on top.

Slag has the primitives for the Deno shape: traced host edges, deferred
finalizers, weak persistent handles, and pins (`.notes/host-object-gc.md`,
`.notes/embedding.md` §5, L2). What it does not have is the *protocol*: "the
embedder owns a heap with its own collector, ask it what is live, and let it ask
you". `TracedReference` plus `EmbedderRootsHandler` is exactly that protocol,
and it is absent.

Two further consequences, both specific to a browser:

- **Rooting across C++ frames.** V8's `Local` points at a handle-scope slot, so
  it survives arbitrary C++ calls. Slag's `Local` is a raw tagged value and
  relies on a conservative stack scan plus pins. A C++ compiler may keep a live
  value only in a callee-saved register across a call, where a stack scan cannot
  see it. A browser facade must therefore materialize every live `Local` into a
  stack slot (or use explicit handle scopes), or accept an unsound root. In
  Slag an unrooted handle does not crash, it silently aliases whatever reuses
  the slot (`crates/crux/src/heap.rs:16-18`). In a browser that is a
  use-after-free and a security bug.
- **Threading.** Blink's isolate-to-thread relationship is 1:1 except for
  compositor workers that share an isolate (`V8BindingDesign.md`). Slag's agents
  are thread-local. Without a locker equivalent, at least one Blink
  configuration cannot be hosted.

## 6. The non-GC hard parts

These are large but conventional; none is architecturally blocked.

- **Startup snapshots.** Chromium builds a V8 context snapshot and Blink loads
  it. Slag has its own snapshot format, so Blink's blob is not reusable; a
  browser port needs its own snapshot generation wired into the build and a
  stable external-reference table. The current restore limitation (a closure's
  captured scope is not rebuilt) is documented in `crates/v8/snapshot.rs`.
- **Streaming compilation.** `ScriptCompiler::StartStreaming` compiles on a
  background thread while bytes arrive. Slag has no off-thread compiler. This is
  an engine feature, not a facade detail, and it is also a startup-performance
  feature a browser will not give up.
- **Code cache.** Blink stores and consumes compiled code keyed by a version
  tag. The bridge round-trips a cache through `CachedData` and
  `create_code_cache`, but the tag is a bare constant (`1`) and the bytes are
  Slag's own representation, not V8's. A browser port needs a real format with
  invalidation, plus Blink's producer/consumer split (`v8_code_cache.cc`,
  `script_cache_consumer.cc`).
- **Inspector.** `v8-inspector.h` is a separate C++ API, not part of `v8.h`, and
  Blink's DevTools depends on `Debugger`, `Runtime`, `Profiler`, `HeapProfiler`
  and the protocol agents. Slag serves `Runtime` only. DevTools is table stakes
  for a usable browser.
- **Security hardening.** Chromium's renderer threat model leans on V8's
  sandbox, pointer compression, code-space isolation, and W^X. A Cranelift JIT
  has to fit Chromium's process sandbox and code-generation policy. This is a
  security-review program on its own, and it is the item most likely to stop a
  Chromium port after the functional work succeeds.
- **Build and policy.** Chromium now supports Rust targets in GN, so a
  `cxx`/`cargo` action is not impossible. It is still a build-system integration
  with vendoring, reproducibility, and OWNERS consequences.

## 7. Three routes, honestly costed

### Route A: a `v8.h` facade over Slag, inside Chromium

The artifact is `include/v8.h` (our source), `api.cc`, and a flat C ABI into
Slag. Blink's bindings compile against it. `v8-inspector.h` is a second facade.

What must be true for it to work: every symbol the bindings core uses exists and
behaves; `Local` rooting is sound; Oilpan and Slag reach a liveness fixpoint;
snapshots and streaming compile exist; the JIT fits the sandbox; DevTools works.

Cost: multi-year, several engineers, plus a permanent compatibility surface
tracking Blink's `v8.h` usage. This is the same structural objection that puts
Node out (`.notes/embedding.md` §2): a co-maintained source dependency, not a
library. Label: *inference from the measured surface, not a delivery estimate.*

`.notes/chromium-facade.md` is the design for this route. Its main finding is
that V8's indirect `Local` is a pointer to a word-sized handle slot and Slag's
`Value` is a word, so the handle headers can be copied nearly verbatim and only
`v8-internal.h` and `v8config.h` are really ours. The same note carries the
milestone ladder (`gin` first, Blink later), the two-heap GC mapping, and the
six experiments that decide whether to continue.

### Route B: retarget an open browser's bindings generator

Servo is the reference: Rust, open, with a bindings generator (`dom/bindings`)
and a `js`/`mozjs` layer wrapping SpiderMonkey. Retargeting the generator to
emit against Slag's Rust API removes the C++/GC wall entirely. The work is
large, but it is in-repo, reviewable, and does not require matching an opaque
C++ header. Ladybird's LibJS is C++ but its bindings are its own, so it is a
comparable, if less Rust-native, target.

This is the route that actually exercises "a browser running Slag" without
committing to `v8.h` compatibility forever.

### Route C: a Rust browser shell

`blitz` (HTML/CSS over Stylo, Taffy, and a GPU renderer) or Servo's components,
plus a Rust DOM and Slag, plus a WebIDL-driven bindings generator that targets
`slag::api` directly. No `v8.h`, no C++ facade, no Oilpan. The browser-specific
work is then the DOM, the event loop, modules, and the Web APIs, which is where
a new browser's cost lives anyway.

This is the strategically consistent path: `.notes/embedding.md` §3 says the
Rust boundary is the one to get right first, and a Deno-class host is a Rust
host. A Slag browser should be a Rust host too.

## 8. Recommendation

1. **Do not start with Chromium.** It is the largest possible compatibility
   surface, it is the one the plan already excludes for good reasons, and its
   hard part (Oilpan) is not the part a facade spike would test.
2. **Do start with the browser-shaped conformance harness.** A Rust shell with
   DOM-like host objects, multiple worlds, wrapper identity, cross-heap cycles,
   an event loop, modules, script origins, and structured stacks. That harness
   is the falsifiable definition of "browser-capable" and it is useful for every
   route.
3. **Then Route C, then Route B.** Both reuse the Rust boundary. Route A becomes
   worth doing only if a browser vendor funds it and accepts the security work.
4. **Keep the flat C ABI as the stable artifact.** `.notes/embedding.md` §4
   already names it. A C ABI is what any non-Rust host links; `v8.h` is one
   possible shape on top, and a deliberately optional one.

## 9. If we run the Chromium spike anyway

Make it small and make it falsify something. Suggested sequence:

1. **Facade shape.** Define `v8::Local<T>` as a word-sized tagged value (Slag's
   `Value` is a NaN-boxed `u64`, so predicates such as `IsString` can be inline
   in the header) and `v8::Global<T>` as a pinned slot. Only slow operations
   cross the C ABI. This preserves Blink's performance shape and keeps the
   boundary thin.
2. **Compile-only target.** Compile V8's own `samples/hello-world.cc` and
   `samples/shell.cc` against the facade. Then compile a slice of Blink:
   `generated_code_helper.cc`, `dictionary.cc`, and a `to_v8_traits.h`
   instantiation. Measure the number of symbols and the number of missing
   shapes. That number is the project size.
3. **Where `cxx.rs` goes.** Use it between `api.cc` and the Rust C ABI for the
   non-hot, structured calls (script compilation, module records, snapshots,
   serialization). Do not put it on `Local`/`Value` predicates; those should be
   header-inline.
4. **The GC experiment.** Build one cross-heap cycle: a C++ host object holding
   a JS object that holds the C++ host object. Free it under a forced
   collection. This tests the `TracedReference`/`EmbedderRootsHandler` protocol,
   which is the item most likely to invalidate the whole route.
5. **The rooting experiment.** Write a function that keeps a `Local` live only
   across an out-of-line call in optimized C++ (`-O2`), force a collection, and
   see whether a conservative scan keeps it. If it does not, the facade needs
   explicit handle scopes and the cost per call changes.

If experiments 4 and 5 fail, stop. They are cheap and they are the wall.

## 10. Open decisions

1. Which is the product: a browser (Route C/B), or `v8.h` compatibility (Route
   A)? They share almost no work beyond the engine.
2. Is Oilpan interop in scope, or does the browser route use a Rust DOM where
   Slag owns both heaps? The second is dramatically cheaper and is the reason
   Route C exists.
3. Does the browser need exact GC semantics (V8) or merely safe ones (Rust DOM)?
   Layout tests and web compatibility will answer that, not intuition.
4. Which inspector is required: CDP `Runtime` (Deno) or the full DevTools
   surface (browser)? The difference is a subsystem.
5. Is background/streaming compilation an engine feature we will build? A
   browser will not ship without it.

## 11. Sources and confidence

Verified locally or fetched during this note:

- `.notes/embedding.md` §2 (browser integration excluded), §3 (C++ has no
  interface), §4 (flat C ABI), §5 (L0-L3 and certification), §8 (parked C++
  face); `.notes/engine-api-study.md` §1, §3.1-3.9, §5; `.notes/host-object-gc.md`.
- `crates/v8/*.rs` (notably `script_compiler.rs`, `template.rs`, `context.rs`,
  `inspector.rs`, `snapshot.rs`), `crates/ffi/src/*.rs`,
  `crates/runtime/src/api/*.rs` (notably `stack_trace.rs`) for the state claims
  in the table.
- cxx book: `extern "C++"`, built-in bindings (fetched 2026-10-31).
- Chromium: `third_party/blink/renderer/bindings/README.md`,
  `bindings/core/v8/V8BindingDesign.md`, the `bindings/core/v8` directory
  listing and file sizes (fetched 2026-10-31).
- V8: `include/v8-embedder-heap.h` (fetched 2026-10-31).

Inferred, not measured: the effort ranges, the Chromium security/sandbox
interaction, and the claim that a conservative scan is insufficient for
register-resident `Local`s. The last one follows from how conservative scanning
works, but it is exactly what §9's experiment 5 exists to settle empirically.
