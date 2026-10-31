# Route A: a `v8.h` facade over Slag

Design note for replacing V8 inside Chromium (and therefore Brave) with Slag.
This is the focused version of `.notes/browser.md` §7 Route A. It assumes the
reader accepts the size of the project and wants to know what the artifact is,
where the hard edges are, and how to falsify the route cheaply.

Status: research and design. No code in this repository implements any of it.
All measurements below were taken on 2026-10-31 from `v8/v8` and
`chromium/chromium` at `main`.

## 1. What Route A is

Blink includes `v8.h` and compiles generated C++ bindings against it. Route A
provides our own `include/` tree with the same names, our own `libv8.a`, and a
flat C ABI into Slag. Nothing in `third_party/blink` changes.

It is **source compatibility at the level Blink uses**, not ABI compatibility.
That distinction matters:

- The consumer compiles our headers, so inline methods, templates, and macro
  layout are part of the artifact. This is `.notes/embedding.md` §3's "C++ has
  no interface" problem, and Route A is the deliberate choice to pay for it.
- We do not need V8's object layout, snapshot format, code cache format, or
  `v8::internal` implementation. We need the observable semantics of the public
  API, plus whatever `v8-internal.h` the headers themselves require.

The facade has three parts:

```
third_party/blink + gin
        |
        v
  include/*.h        our V8 headers: declarations, templates, inline methods
        |
        v
  api.cc / *.cc      our C++ implementations, one translation unit group per header
        |
        v
  slag_v8_abi.h      flat extern "C" ABI: opaque handles, u64 values, status codes
        |
        v
  libslag.a          crux + runtime + jit + wasm
```

`cxx.rs`, if used, sits between `api.cc` and `slag_v8_abi.h`. It never appears in
`include/`, because Blink cannot include CXX-generated headers.

## 2. The measured surface

V8's public `include/` at `main` is **66 headers, about 901 KB**, before the
`include/cppgc/` and `include/libplatform/` subdirectories. `v8.h` itself is
only 3.7 KB: it is an umbrella that includes the split headers.

The ten largest, which is also a decent priority order:

| Header | Size | What it carries |
|---|---:|---|
| `v8-internal.h` | 77.6 KB | `Address`, `ValueHelper`, `HandleHelper`, `Internals`, tagging, pointer compression, sandbox helpers |
| `v8-isolate.h` | 72.0 KB | isolate lifecycle, hooks, limits, GC callbacks, microtasks, stack capture |
| `v8-platform.h` | 58.7 KB | `Platform`, task runners, tracing, page allocator |
| `v8-profiler.h` | 50.8 KB | CPU and heap profilers, `EmbedderGraph`, snapshots |
| `v8-object.h` | 43.9 KB | property access, internal fields, interceptors, accessors, integrity |
| `v8-template.h` | 42.5 KB | `FunctionTemplate`, `ObjectTemplate`, signatures, call handlers |
| `v8-primitive.h` | 38.9 KB | strings, numbers, symbols, bigints |
| `v8-script.h` | 38.3 KB | compilation, origins, code cache, streaming |
| `v8config.h` | 38.1 KB | the macro matrix: export, inline, sandbox, compression, deprecations |
| `v8-fast-api-calls.h` | 30.3 KB | `CFunction` fast calls |

Then `v8-local-handle.h` (28.1 KB), `v8-context.h` (23.3 KB),
`v8-function-callback.h` (23.2 KB), `v8-array-buffer.h` (22.8 KB), and
`v8-callbacks.h` (21.3 KB).

Two of these are not really V8's API, and they are the two we must own:

- `v8-internal.h` is internal machinery that the handle headers include. It is
  not stable, but it is load-bearing.
- `v8config.h` is a build-configuration contract. Blink's `#if` branches depend
  on it.

The rest can, in principle, be copied and re-bodied. That is the single most
important finding below.

## 3. The unlock: indirect handles are already word-compatible

V8's `Local<T>` in the default (non-direct) configuration is
`LocalBase<T> : api_internal::IndirectHandleBase`, and
`IndirectHandleBase` is exactly:

```cpp
class IndirectHandleBase {
  // ...
  internal::Address* location_ = nullptr;   // pointer to a handle slot
};
```

`Local<T>::operator->` returns a `T*` obtained by reinterpreting the word in
`*location_`. The API types (`v8::Object`, `v8::String`, ...) are phantom types:
a `T*` is a tagged address, never a real C++ object with fields. V8 itself
relies on this.

Slag's `Value` is a `#[repr(transparent)]` NaN-boxed `u64`
(`.notes/embedding.md` §5). So:

| V8 concept | Facade representation |
|---|---|
| `internal::Address` | `uint64_t` (a Slag `Value`) |
| `Local<T>` | `IndirectHandleBase`: pointer to a `Value` slot |
| `T*` | `reinterpret_cast<T*>(value)`; phantom, never dereferenced |
| `HandleScope` | a root-registered segment of `Value` slots |
| `Global<T>` / `Persistent<T>` | a pinned `Value` (engine pin registry) |
| `Eternal<T>` | an immortal pinned slot (never released) |
| `TracedReference<T>` | a pinned `Value` with an embedder-edge marker |
| `MaybeLocal<T>` | a `Local<T>` plus the pending-exception state |

Consequences, and they are large:

1. **The handle headers can be copied nearly verbatim.** `v8-local-handle.h`,
   `v8-handle-base.h`, `v8-persistent-handle.h`, and `v8-maybe.h` are mostly
   templates over a word. We replace `v8-internal.h`'s helpers, not the handle
   types.
2. **Rooting becomes exact, not conservative.** A `Local` lives in a
   `HandleScope` segment, and the segment is registered with Slag as a root
   source. We do not need the C++ stack scan to see a `Local`, because the slot
   is the root. This removes the register-resident-`Local` hazard from
   `.notes/browser.md` §5 for facade-held values.
3. **No moving-GC update machinery.** Slag's arena keeps stable addresses, so a
   `HandleScope` slot is written once and never rewritten
   (`.notes/embedding.md` §5, the L1 argument). The slot's only job is to keep
   the value reachable.

This is a much better starting point than a flat C API where every value is an
opaque handle id: it keeps Blink's handle-heavy code close to V8's shape and
keeps `IsString`, `As<T>`, and friends inline.

### 3.1 Handle scope design

Per thread and isolate, keep the V8 `HandleScopeData` shape:

```
struct HandleScopeData { Address* next; Address* limit; int level; };
```

`CreateHandle` bumps `next`, writes the `Value`, returns the slot. `Extend`
allocates a new segment. A segment arena is registered with Slag once as a
`RootSource`; `~HandleScope` clears the slots it allocated (so they stop rooting
garbage) and restores `next`/`limit`. `EscapableHandleScope::Escape` copies the
value into the outer scope's slot. `SealHandleScope` raises the limit.

Cost: one 8-byte slot per `Local`, same order as V8. Blink allocates a lot of
locals; expect this to be visible in profiles, and expect it to be comparable to
V8 rather than free.

## 4. Header strategy

Three options, and one recommendation.

| Strategy | Shape | Verdict |
|---|---|---|
| Copy V8 headers, replace bodies | headers stay byte-close to upstream; `api.cc` implements out-of-line methods | **recommended**: templates, inline behavior, and deprecation macros stay correct |
| Hand-write a subset | only what compiles; discover it by linker errors | useful as a second pass, bad as the source of truth |
| Generate from V8 headers | parse and emit stubs | rejected: templates and inline methods are not mechanically stubbable |

The recommendation is to vendor V8's `include/` at the version Chromium pins,
keep the BSD notices, and treat two files as ours:

- **`v8-internal.h`**: provide `Address`, `ValueHelper`, `HandleHelper`,
  `Internals`, `HandleScopeData`, and the subset of tagging helpers the copied
  headers use. This is the most intricate file and the one to get reviewed
  hardest. Much of upstream is pointer-compression and sandbox machinery that
  we can compile out, so our version is smaller, not larger.
- **`v8config.h`**: choose the macro matrix deliberately (§9). This is a
  product decision disguised as a build header.

Everything else is a re-body. The out-of-line methods (`.cc`) are enumerated by
the linker: compile `gin`, then Blink, and the undefined symbols are the work
list. That is a better tracker than any inventory, because it measures actual
usage rather than declared API.

Version drift is a real cost. Blink's code contains
`#if V8_MAJOR_VERSION >= N` branches, and our `v8-version.h` must claim a
version whose API we actually implement. Pin to the V8 version in Chromium's
`DEPS`, track the delta per milestone, and treat a version bump as a porting
commit, not a routine update.

## 5. The boundary ABI

Keep it flat and boring. No Rust types, no C++ types with non-trivial layout,
no exceptions across it.

```c
typedef uint64_t slag_value;      // Slag's NaN-boxed Value
typedef uint64_t slag_pin;        // engine pin id
typedef struct slag_isolate slag_isolate;
typedef struct slag_realm slag_realm;

int32_t slag_eval(slag_realm*, const uint8_t* src, size_t len,
                  slag_value* out);           // 0 ok, 1 pending exception
void    slag_pin_new(slag_isolate*, slag_value, slag_pin* out);
void    slag_pin_release(slag_isolate*, slag_pin);
// ...
```

C++-side state (`Isolate`, `Context`, templates, scripts, modules, backings,
serializers) is owned by the facade and holds ABI pointers. A C++ object's
lifetime is the facade's business; the engine only knows realms, values, pins,
and buffers.

Rules that are not optional:

- **Panics do not cross.** Every ABI entry point catches unwinding and turns it
  into a fatal engine error or a pending exception, never into C++ unwinding.
  The existing `crates/ffi/src/guard.rs` behaviour of returning a plausible
  default is exactly what a browser must not do.
- **The pending exception is thread-local per isolate**, not a return value.
  V8's `MaybeLocal` is empty when an exception is pending; the facade reads the
  slot.
- **Strings are copied at the boundary**, or handed out as borrowed spans with
  an explicit lifetime. Blink calls `String::WriteUtf8` and
  `NewFromUtf8` heavily; do not design an allocation per call.
- **`cxx.rs` is for the non-hot, structured calls** (compile, module records,
  snapshots, serialization, inspector messages). Do not put it on `IsString` or
  `Object::Get`; those stay inline or direct.

## 6. GC: the two-heap contract

This is the wall. Everything above is work; this is the part that can invalidate
the route.

### 6.1 The V8 side of the contract

From the headers fetched on 2026-10-31:

- `TracedReference<T>` is an `IndirectHandleBase` whose slot points into V8's
  global-handle storage. The internal entry points are
  `GlobalizeTracedReference`, `CopyTracedReference`, `MoveTracedReference`, and
  `DisposeTracedReference` (`v8-traced-handle.h`).
- Its documented semantics are: tracing collections through `CppHeap`, and
  non-tracing collections consult `EmbedderRootsHandler` to decide whether the
  handle is a root (`v8-traced-handle.h`, `v8-embedder-heap.h`).
- Blink uses `v8::EmbedderGraph` for heap snapshots; `v8_gc_controller.cc`
  implements `EmbedderGraph::Node::Detachedness` for DOM wrappers.
- Blink's wrapper lifetime is `TraceWrapperV8Reference` over
  `v8::TracedReference`, with `ScriptWrappable` and `DOMDataStore` providing
  the per-world identity map (`V8BindingDesign.md`).
- The exact Oilpan-to-V8 tracing entry points in the current tree (whether
  through `CppHeap` or an embedder-tracing shim) must be mapped in the spike.
  Do not assume; the file layout moves.

### 6.2 The Slag side

Slag has the primitives: pins, a weak registry, traced host-object edges with a
write barrier, deferred finalizers, and `RootSource` registration
(`.notes/host-object-gc.md`, `.notes/embedding.md` §5). What it does not have is
the *protocol direction*: the embedder's collector driving, or being driven by,
the engine's mark phase.

Mapping:

| V8 | Slag primitive | Status |
|---|---|---|
| `Global<T>` / `Persistent<T>` | pin | exists |
| `Weak<T>` + `SetWeak` | weak registry + finalizer | exists, two-pass callback still missing in the bridge |
| `TracedReference<T>` | pin with an embedder-edge marker | marker does not exist |
| `EmbedderRootsHandler` | root source registration | direction inverted; no `ResetRoot` equivalent |
| `EmbedderGraph` | host object identity + edges | no graph API |
| `CppHeap` | none | absent |
| write barrier for host edges | `host_object_retain` / `release` | exists for host objects, not for a foreign heap |

### 6.3 The fixpoint

A correct unified mark is:

```
engine mark stack
  -> for each JS object, trace its edges
  -> for each JS -> C++ edge (TracedReference), ask the host collector to mark
host mark stack (Oilpan)
  -> for each C++ -> JS edge (TraceWrapperV8Reference), push the JS value back
repeat until both stacks are empty
```

Two failure modes:

- **Lost edge**: the host collector frees a C++ object whose JS reference is
  still live, or the engine frees a JS value the C++ heap still points at. This
  is a use-after-free, and in a browser it is a security bug.
- **False root**: a dead slot keeps a value alive. This is a leak, and at
  browser scale it is a memory-growth bug that will be blamed on the engine.

Slag's `Rc<dyn HostOps>` for host objects is also a constraint: it is not
`Send`, and the engine's agents are thread-local. Blink's isolate-to-thread
relationship is 1:1 except for compositor workers that share an isolate
(`V8BindingDesign.md`), so the facade needs an explicit answer for that
configuration before it can claim worker support.

### 6.4 The good news: `gin` avoids this

`gin` (Chromium's V8 glue layer) uses `Persistent`, `Global`, `WeakCallback`,
`External`, `ObjectTemplate`, and `FunctionTemplate`. It does **not** need
Oilpan. That makes `gin` the right first compile target: it exercises the C++
API and the weak-handle protocol without requiring the two-heap fixpoint. The
GC wall arrives at the first Blink bindings target, not at the first host.

## 7. Exceptions, `Maybe`, and termination

V8's error model is values, not exceptions:

- A method that can throw returns `Maybe<T>` or `MaybeLocal<T>` and takes a
  `Local<Context>`.
- A pending exception makes the `Maybe` empty; the host uses `TryCatch` to
  inspect it.
- `TerminateExecution` is a special pending state, not a normal error.

The facade needs a per-isolate `pending_exception: Value` and a `pending_termination: bool`,
plus a `TryCatch` stack that save/restores both. `Message` must be constructible
from the engine's error and stack capture. Slag already captures structured
frames (`api::StackFrame`: function name, script name, line, column, eval,
constructor, wasm, async flags; `Isolate::capture_stack`), but script ids,
byte offsets, source-map URLs, and `get_script_source` are absent. Those are
`v8-message.h` / `v8-script.h` work, not engine work.

Do not let a C++ exception cross the ABI. `TryCatch` is a value, not a C++
`catch` block.

## 8. Rooting and persistence, concretely

- **`Local<T>`**: slot in the current `HandleScope`. Exact root. Cleared on
  scope close.
- **`Global<T>` / `Persistent<T>`**: an engine pin. `Reset` releases it.
  `SetWeak` installs a weak callback; the two-pass protocol needs
  `SetSecondPassCallback` semantics, which the bridge does not have yet
  (`.notes/embedding.md` §5).
- **`Eternal<T>`**: a pin that is never released for the isolate's lifetime.
- **`TracedReference<T>`**: a pin marked as an embedder edge. In the first
  milestone it can be a strong root (safe, leaky); the fixpoint replaces that.
- **`EscapableHandleScope::Escape`**: copy the value into the outer scope's
  slot and return the outer slot.
- **`SealHandleScope`**: raise the limit so `CreateHandle` cannot allocate.

The `-O2` register experiment from `.notes/browser.md` §9 becomes a guard
against a shortcut, not a design constraint: if the facade ever lets a `Local`
be a bare register-resident word, that experiment fails. Keep the experiment in
the test suite as a regression.

## 9. Security and build configuration

`v8config.h` is a policy file. The first port should choose the simplest
configuration and say so:

| Macro | First port | Consequence |
|---|---|---|
| `V8_ENABLE_DIRECT_HANDLE` | off | indirect slots; exact rooting; no off-stack direct handles |
| `V8_COMPRESS_POINTERS` | off | 64-bit words; no pointer-compression assumptions in `v8-internal.h` |
| `V8_ENABLE_SANDBOX` | off | no heap sandbox; renderer hardening differs from Chrome |
| `V8_ENABLE_CHECKS` | off in release, on in debug | type checks in `Cast` |
| `V8_USE_EXTERNAL_STARTUP_DATA` | off initially | no `snapshot_blob.bin`; slower startup until snapshots land |
| `V8_ENABLE_WEBASSEMBLY` | on | Blink's wasm response extensions require it |

The sandbox choice is not a detail. Chromium's renderer threat model assumes
V8's heap sandbox and pointer compression; turning them off changes the security
properties of the browser, and some Chromium code and tests assume they are on.
A facade can answer these macros either way, but it must answer *truthfully*.
An `#if V8_ENABLE_SANDBOX` branch that compiles to a stub is a security bug
waiting for a fuzzer.

The other security surfaces:

- `SecurityToken`, `Signature`, `AccessorSignature`, and
  `ObjectTemplate::SetAccessCheckCallback` implement cross-context access
  checks. The bridge stores a token and does not consult it
  (`crates/v8/context.rs`). A browser cannot ship that state.
- `SetModifyCodeGenerationFromStringsCallback`,
  `AllowCodeGenerationFromStrings`, and CSP integration must gate the JIT.
- The Cranelift JIT must fit Chromium's W^X and code-space policy, and the
  renderer's process sandbox.

## 10. Build integration

The integration seam is the GN target name, not a large patch:

- Keep `//v8:v8` as the target. Replace its sources with the facade and a
  `rust_static_library` for Slag. `//v8:v8_inspector` is a second target with
  the same treatment.
- Produce `v8_build_config.json` (Chromium writes and reads it at build time).
- Decide `mksnapshot`: Chromium builds a snapshot at build time and Blink loads
  `v8_context_snapshot.bin`. The first milestones can skip it; a browser
  cannot, for startup. Slag's snapshot format is its own, with a documented
  restore limitation (`crates/v8/snapshot.rs`), so a `mksnapshot` replacement
  and a blob-loading path are part of the port.
- Keep Rust out of the C++ ABI: `libslag.a` is static, `panic = "abort"` or a
  catch at every entry point, and no Rust types in exported signatures.
- Reproducibility: vendored crates, a locked cargo build, and a
  deterministic `libslag.a`. Chromium's build farm will not accept a network
  fetch mid-build.

## 11. Milestone ladder

Each milestone has an exit criterion that is a command, not a feeling.

| # | Milestone | Exit criterion | What it proves |
|---|---|---|---|
| A0 | V8's own samples | `hello-world.cc` and `shell.cc` compile and run against the facade | the handle/value/exception core works |
| A1 | indirect handles, gin-shaped | `Local<T>` is an indirect slot; `tests/gin_shapes.cc` compiles and runs. Actual `gin` is a Chromium component, so its link gate needs a Chromium checkout (see the result) | handle/scope fidelity, templates, callbacks |
| A2 | `gin` tests | `gin_unittests` green | wrapper lifetime and callback semantics |
| A3 | Blink bindings core compiles | `bindings/core/v8` compiles against the facade | internal fields, interceptors, signatures, `TracedReference` |
| A4 | Blink bindings tests | the bindings unit tests green | wrapper identity, worlds, access checks |
| A5 | `content_shell` | `--dump-dom` on a static page with a script | DOM plus JS end to end |
| A6 | Web tests subset | a chosen `web_tests` subset green | real page behavior |
| A7 | Chrome smoke | a minimal browser session | the product claim |

A0 to A2 are worth doing regardless of the final decision: they are small, they
produce reusable compatibility tests, and they settle the C++ surface without
Oilpan.

*Result (2026-10-31).* `crates/v8/cpp/` compiles V8's `hello-world.cc` and
`shell.cc` unchanged against a minimal `v8.h` over `crates/slagc`.
`hello-world` prints `Hello, World!` and `3 + 4 = 7` (the WebAssembly half,
through the engine's `wasm` feature); `shell -e "print(1 + 2)"` prints `3`,
`version()` prints `slag-a0`, and a multi-argument `print` works. A0 used
value-sized `Local<T>`, not the indirect-slot design of §3; A1 replaced that
with the indirect slot, below. Two limits are
recorded rather than hidden: the sample's `load()` re-entry is refused at the
ABI (the safe path is `FunctionCall::eval`, which the sample does not use), and
an uncaught `Error` prints the engine's generic `TypeError: Uncaught exception`
because `JsError` does not expose the thrown value through the ABI yet.

*A1 result (2026-10-31).* `Local<T>` is now an indirect slot (`Address*` into a
thread-local handle stack), `HandleScope` restores the stack on exit, `Global<T>`
stores the tagged value and allocates a slot on `Get`, and
`FunctionCallbackInfo::operator[]` allocates per argument. `hello-world` and
`shell` still pass. The A1 gate as written, "gin compiles", is not reachable
standalone: `gin/BUILD.gn` public-deps `//base`,
`//base/allocator:buildflags` and `//v8`, and deps
`//third_party/abseil-cpp:absl` and
`//tools/v8_context_snapshot:buildflags`; `gin/wrappable.h` additionally needs
`v8::Object::Wrappable`, `v8::CppHeapPointerTag`, `Object::Unwrap`,
`IsApiWrapper`, `TracedReference`, and internal fields. A1 therefore lands the
handle change and a gin-shaped harness (`crates/v8/cpp/tests/gin_shapes.cc`,
prints `30`, exit 0). The real `gin` link gate belongs in a Chromium checkout
with `//v8` swapped, and it is the first place the wrappable/cppgc surface is
required. The handle stack is now a real root: `Local<T>` slots over engine
values are retained in `slagc`'s `RootSource` and released by `HandleScope`,
and `gin-shapes` forces a collection and reads a slot-held string back.

*Persistent handles (2026-10-31).* `Global<T>` now retains its value in the same registry for the `Global`'s own lifetime, which is the case a `HandleScope` slot cannot cover. Construction and `Reset(isolate, local)` retain, `Reset()` and the destructor release, copy construction and copy assignment are deleted, and a move transfers the retention instead of taking a second one. `Global<Context>` and `Global<Script>` name facade pointers, so they are not registered for the same reason a facade-pointer slot is not. A `Global` must still be reset or destroyed before `Isolate::Dispose`, because the registry is thread-local and a leaked `Global` leaves a retention naming a box the freed heap reuses.

The check is a retention count rather than a survival test, because survival cannot demonstrate the retention: the conservative stack scan reads the whole live stack region, including the words a dead C++ frame leaves behind, so a value can survive a collection with nothing retaining it. `gin-shapes` therefore asserts that `slag_root_count()` moves by exactly one when the `Global` is built and returns to its starting value after `Reset()`, and that assertion was verified to fail when the retain call is disabled. The wrappable, internal-field, and cppgc surface is the next piece.

## 12. Cost model

These are order-of-magnitude inferences, not commitments. Basis: 66 headers and
~900 KB of declarations, the `gin` surface, the Blink bindings core, and the
subsystems listed in `.notes/browser.md` §4.

| Phase | Rough shape | Dominant risk |
|---|---|---|
| A0 | weeks | handle/exception design mistakes that propagate |
| A1 | a few months, one engineer | template/inline fidelity |
| A2 | a few more months | weak-callback and wrapper lifetime |
| A3 | quarters | the two-heap GC protocol |
| A4 | quarters | access checks and worlds |
| A5 | year-plus | snapshots, streaming compile, inspector |
| A6 | multi-year | web-compat tail |
| A7 | program | security, performance, maintenance |

The schedule risk is concentrated in A3. If the cross-heap experiment fails, no
amount of header work saves the route.

## 13. Falsification experiments

Run these before committing to a schedule.

**E1: cross-heap cycle.** A C++ host object holds a JS object; the JS object
holds the C++ host object. Force a collection from both directions. It must be
collected, and finalizers must run once. If it leaks or frees early, stop.

*Result (2026-10-31).* `crux`'s
`a_host_rooted_cross_heap_cycle_collects_when_the_host_drops_its_root` models
the forward direction at the interface Slag offers: a foreign collector holds
the host object through a `RootSource`, the host object retains the JS half, and
dropping the host root collects both halves and finalizes the host object
exactly once. It passes (`cargo test -p crux`, 275 tests green; clippy clean).
This is one collector with a host-supplied root set plus host edges, not two
independent collectors. The reverse direction now has its primitives:
`crux::heap::TracedHandle` is a conditional root the host releases (the strong
half of `v8::TracedReference<T>`), `runtime::api::Traced` wraps it, and
`weak_set_second_pass_callback` queues the guaranteed pass, which
`Agent::run_host_finalizers` drains after every first pass. Tests:
`a_traced_reference_roots_until_the_host_resets_it`,
`a_second_pass_runs_after_the_first_and_even_if_the_handle_is_cleared`, and the
same two through the isolate. What is still missing is the engine's side of
`EmbedderRootsHandler::IsRoot`: the engine does not yet ask the host which
traced references are still roots, so today the host calls reset itself. The
`v8` bridge's `TracedReference` also remains weak by design until that host
tracer exists; switching it early would turn Deno's `cppgc` shim into a leak.

**E2: handle rooting.** Keep a `Local` live across an out-of-line allocation in
optimized C++ (`-O2`), force a collection. With indirect slots it must survive.
Then deliberately use a bare word and confirm the test fails, so the regression
is real.

**E3: `gin` link.** Compile `gin` against the facade and count undefined V8
symbols. The count is the A1 work list. If it is dominated by a subsystem you
had not planned (for example `v8-fast-api-calls.h` or `v8-platform.h`), that is
information.

**E4: snapshot.** Build a context with the facade, serialize it with Slag's
format, load it in a fresh process, and run a bootstrap function. If external
references cannot be made index-stable, the startup plan fails.

**E5: DevTools.** Serve `Debugger.enable`, a breakpoint, and a pause. The
current inspector refuses everything but `Runtime`; this experiment sizes that
subsystem honestly.

**E6: allocation.** Create an `ArrayBuffer` whose backing store is allocated by
the host (PartitionAlloc), wrap it, detach it, and prove the deleter runs
exactly once. Blink's buffers are not engine-owned.

## 14. Open decisions

1. **Version target.** Which V8 version does the facade claim? Pin to the
   Chromium milestone's `DEPS` and treat version bumps as ports.
2. **Configuration.** Sandbox and pointer compression off is the cheap first
   port; is that acceptable as a product, and if not, when do they come on?
3. **Oilpan protocol.** Does the facade implement `CppHeap` (V8 drives marking),
   or a host-driven tracing callback (the host drives marking)? The first is
   closer to V8's shape; the second is closer to Slag's roots-and-edges model.
4. **Threading.** What is the answer for compositor workers that share an
   isolate? Without a locker equivalent, that configuration is out.
5. **Maintenance.** Who tracks Blink's `v8.h` usage as it changes? A facade is
   a permanent compatibility surface, not a one-time port.
6. **Product.** Is the goal a browser that runs Slag, or V8 API compatibility as
   an artifact? Route A is the second; `.notes/browser.md` Routes B and C are
   the first.

## 15. Sources

Verified on 2026-10-31:

- `v8/v8` `include/` directory listing (66 headers, sizes summed to 901,312
  bytes) and the contents of `v8.h`, `v8-local-handle.h`, `v8-handle-base.h`,
  `v8-traced-handle.h`, `v8-cppgc.h`, `v8-embedder-heap.h`, `v8-cpp-heap-external.h`.
- `chromium/chromium` `third_party/blink/renderer/bindings/README.md`,
  `bindings/core/v8/V8BindingDesign.md`, `bindings/core/v8/v8_gc_controller.cc`,
  and the `bindings/core/v8` file inventory.
- This repository: `.notes/embedding.md`, `.notes/browser.md`,
  `.notes/engine-api-study.md`, `.notes/host-object-gc.md`, `crates/v8/*.rs`,
  `crates/runtime/src/api/stack_trace.rs`, `crates/ffi/src/tables.rs`.

Inferred, and to be settled by the experiments: the effort ranges, the exact
Oilpan tracing entry points in the current tree, the feasibility of the
`v8-fast-api-calls.h` surface, and the performance of one slot per `Local`.
