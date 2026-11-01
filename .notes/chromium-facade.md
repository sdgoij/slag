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
an uncaught `Error` printed the engine's generic `TypeError: Uncaught exception`
because `JsError` did not expose the thrown value through the ABI yet (that and
the message/stack path landed later, above).

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

*Internal fields and wrappables (2026-10-31).* An object now carries its embedder's internal fields itself, in `crux::object::JsObject::internal_fields`: one `Cell` handle to an `InternalFields` box holding the opaque pointers and the `IsApiWrapper` bit. The field is 8 bytes, so the payload goes 424 to 432 bytes and stays in the same 448-byte arena class; an object that never carries host fields allocates exactly as before. The box traces nothing (the pointers are the host's, not values) but the object traces the box, so the fields die with the object. A side table keyed by address was rejected: the arena's free list reuses swept slots, so a recycled address would hand a later object an earlier object's pointer.

That storage forced a detail worth writing down: `JsObject` has two in-place initializers (`init_ordinary` for the arena fast path, and the one inside `array_create`) that `ptr::write` every field, and a new field must be added to both. A missed one leaves the field as whatever the reused arena slot held, and the first read dereferences garbage. The literal constructors are the visible ones; the `new_in_place` closures are the trap.

The facade side is the gin shape: `ObjectTemplate::SetInternalFieldCount` and `NewInstance` (an object with N fields, the template's host functions, `IsApiWrapper` set), `Object::InternalFieldCount`, `IsApiWrapper`, `SetAlignedPointerInInternalField`, `GetAlignedPointerFromInternalField` in V8's three argument forms, `Wrappable<T>()`, and `FunctionCallbackInfo::This()`, which is what a wrappable method needs to recover its C++ object. `gin-shapes` builds a template with one field and a `bump` method, stores a `HostCounter*`, forces a collection and reads the pointer back, then runs `counter.bump(10)` from a script; the check was verified to fail when `Wrappable` reads the wrong field. Two deviations are deliberate: `CppHeapPointerTag` is accepted and not inspected (there is no pointer-tag check), and `Wrappable<T>` has no cppgc `GarbageCollectedMixin` constraint (there is no cppgc).

*The same fields in both faces (2026-10-31).* Because the storage is the engine's, the Rust `v8` crate gets it too: a runtime `ObjectTemplate` instance is created with its internal-field count and the API-wrapper bit, and `Object::set_aligned_pointer_in_internal_field` and `get_aligned_pointer_from_internal_field` are the crate's accessors, so `set_internal_field_count` is no longer a count the engine records and ignores. The C++ face also gained `Object::Get`/`Set` by a key value, which run ToPropertyKey in the engine (a numeric key and the string it renders to name one property, and an object key reaches its `toString`), so `NewInstance`/method dispatch no longer needs the raw `slag_object_set` ABI. The `Object` surface is still only those two forms: the index-keyed overloads, `Has`, `Delete`, and `GetRealNamedProperty` remain, as do `TracedReference`, interceptors, and cppgc.

*Traced references (2026-10-31).* `TracedReference<T>` is now in the C++ face, built on the engine's `Traced` rather than on `Global`: the engine holds the value while the handle exists, `Reset` or the destructor releases it, and a copy is a second engine reference so resetting one does not clear the other. That distinction is the whole reason the earlier traced-root work exists, and it is why a copy is not just a second `SlagTraced` box pointing at the same entry. The ABI is six functions (`slag_traced_new`, `_get`, `_is_empty`, `_clone`, `_reset`, `_free`). V8 traces the reference through the embedder's collector; this facade has no embedder-heap tracing, so the reference is a root the host must release and it has no `SetWeak`. `gin-shapes` takes a reference to a marked object, lets the handle scope pop, forces a collection and reads the mark back, then releases the original and checks the copy still reads it; making the copy empty instead was verified to fail the check. Interceptors and cppgc remain.

*The rest of the Object surface (2026-10-31).* `Object` now has `Has` and `Delete` by key value (and by index), with `Has` walking the prototype chain and `Delete` answering true for a property that was never there, and `GetRealNamedProperty`, which is the own-property read with no interceptor and no chain walk and an own accessor's getter run on the receiver. The index forms are the key-value forms with the number built for the caller, so an index and the string it renders to are one property. The engine side is three embed helpers on `Context` (`has_property`, `delete_property`, `get_own_property`) over the existing `has_property_key`, `delete_key`, and `get_own_property_key`, plus ToPropertyKey, and three ABI functions. `Name` joins the facade's hierarchy as `Value`'s property-key subclass so `GetRealNamedProperty` takes the `Local<Name>` V8 takes. `gin-shapes` checks the own method and `Object.prototype.toString` are found, a key on neither is not, index write/read, the own-property read seeing `mark` but not the inherited `toString`, and delete-then-gone; forcing `Has` to always answer true was verified to fail two of those checks. `HasRealNamedProperty`, `HasRealIndexedProperty`, `HasRealNamedCallbackProperty`, `SetAccessor`, interceptors, and cppgc remain.

*The real-property predicates (2026-10-31).* `HasRealNamedProperty`, `HasRealIndexedProperty`, and `HasRealNamedCallbackProperty` are in, over one ABI call that reports which kind the own property is (absent, data named, data indexed, callback named, callback indexed). The named/indexed split is a divergence with a reason: V8 keeps a named dictionary and an elements store and the predicates ask one of them, while this engine has a single own-property store, so the split follows the key. An array index (`crux::object::array_index_of`, the spec's 0 to 2^32-2) is the indexed kind, and a canonical numeric index that is not an array index ("1.5") stays named, which is what V8's named store would answer. `gin-shapes` checks the own name is found, the inherited `toString` is not, the index key "0" is indexed and not named, and a data property is not a callback property; forcing the kind to always read "data named" was verified to fail the check. Interceptors are scoped in section 14.1, where the finding is that this is a design decision rather than plumbing: the engine's handler seam builds a `HostOps` object for the realm global, and a `NewInstance` object has no `HostOps`, so gin's and Blink's instance handlers have nowhere to run yet.

*Named-property handlers (2026-10-31).* The decision above was taken and the first slice landed. `ObjectTemplate::SetHandler` installs a `v8::NamedPropertyHandlerConfiguration`, and `NewInstance` on that template makes a crux host object (`Context::create_host_object`) whose getter, setter, query, and deleter are consulted before the engine's own internal method; `Intercepted::kNo` falls back, so masking is the only mode. The configuration's `data` value is pinned for as long as an instance can call it. The enumerator and the descriptor/definer pair were absent from the ABI and the C++ struct at that point, each for a stated reason (a callback could not yet allocate through the borrowed context; there was no descriptor ABI), so a host that needed one did not compile; both have since landed, below. Two engine-side findings came out of it and are fixed: `in` now delegates host objects to crux's [[HasProperty]] (`module.rs`), which it previously did only for proxies and typed arrays, and the real-property reads now use `ordinary_get_own_property` because V8 defines them to skip interceptors. `gin-shapes` checks the handler through `Get`/`Has`/`Set`/`Delete` and through a script, and the runtime test covers the `in` path plus its fallback to the prototype chain.

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

### 14.1 Interceptors: what the next milestone costs

Interceptors are not plumbing. The Rust crate's `interceptor.rs` already maps all
seven V8 callbacks onto `crux::host::HostOps` (`descriptor`, `getter`, `query`,
`setter`, `deleter`, `enumerator`, `definer`), with `Intercepted::kNo` meaning
"run the ordinary method", and it documents the divergences it accepts
(`NON_MASKING`, `ALL_CAN_READ`, `ONLY_INTERCEPT_STRINGS` accepted and not
honoured; `kThrow` refused by name; the query's attributes dropped; the two
enumerators asked and concatenated).

The structural constraint is where a handler can live. That bridge installs a
handler under the template's address and builds the realm's **global object** as
a `HostOps` object when the context is made from that template. A
`v8::ObjectTemplate::NewInstance` object is an ordinary object here, and an
ordinary object has no `HostOps`, so a host that sets a handler on an instance
template has nowhere for the engine to call it from. gin and Blink do exactly
that, and Blink's named properties on the global object depend on it too.

So the milestone needs a decision before code:

1. **Per-instance host objects (faithful).** Make `ObjectTemplate::NewInstance`
   create a `HostOps`-backed object whose methods dispatch to the template's
   handler plus a per-object data pointer. The engine change is a host object
   with interceptor state rather than a fixed realm-global hookup; the C++ side
   then gets `NamedPropertyHandlerConfiguration`, `PropertyCallbackInfo<Value>`,
   the seven callback typedefs, and `Intercepted`. This is the shape both gin and
   Blink assume.
2. **Global-template handlers only (cheap, wrong for gin).** Wire the C++
   `SetHandler` only where the engine already has a seam, which covers the
   global object and nothing an instance template promises. It would compile a
   host's code and silently not intercept what it expects, so it should only be
   taken with the limitation stated loudly.
3. **Defer until a real consumer exists.** Keep it out of the facade until the
   Blink bindings milestone reaches the named-property tests.

**Decided (2026-10-31): option 1, and the first slice is in.** `NewInstance` on a
template with a handler now makes a `crux::host::HostOps` object
(`Context::create_host_object`), one behaviour per template. All seven of V8's
callbacks are carried for the named handler (getter, setter, query, deleter,
enumerator, descriptor, definer), and the indexed one carries the same set. The
configuration's `data` value is pinned for as long as an instance can call it,
and `kNo` falls back to the engine's own internal method, so masking is the only
mode.

The work also exposed an engine gap worth recording: the `in` operator's
`has_property_with_deferred_trigger` delegated to crux only for proxies and
typed arrays, so a host object's `has_property` was never asked by script. It now
delegates `ObjectKind::Host` too (`crates/runtime/src/module.rs`), and a runtime
test covers both the host answer and the fallback to the prototype chain.

One more fidelity point the handler made visible: `GetRealNamedProperty` and the
real-property predicates are defined to *skip* interceptors, and crux's
`ordinary_get_own_property` is exactly that question, so the embed helpers use it
rather than the interceptor-consulting `get_own_property_key`.

*Indexed handlers, and why the enumerator waited (2026-10-31).* `SetHandler` now
takes an `IndexedPropertyHandlerConfiguration` too, with the same four callbacks
keyed by an array index, and the handler routes a key by its shape: an array
index goes to the indexed callback when one is installed, and to the named one
otherwise. That is V8's rule and the rule the crate's bridge already documents.

While wiring the enumerator the reason it cannot be useful yet became concrete:
`Object.keys` reads each key's own descriptor, and a host object whose
`get_own_property` is not implemented falls through to the ordinary method, which
reports the key absent — so the enumerator's keys would be dropped before a
caller sees them. The crate's interceptor answers `get_own_property` through its
`descriptor` callback for exactly this reason.

*Property descriptors (2026-10-31).* The descriptor ABI landed next, in the order
that finding implied. A `SlagPropertyDescriptor` carries value, get, set, and the
three attributes as present-or-absent fields, because a descriptor distinguishes
"not mentioned" from "mentioned and false". The named handler gains `descriptor`
(answering a descriptor object, which the Rust side reads with the engine's own
`to_property_descriptor`) and `definer` (receiving the marshalled descriptor);
`Object::defineProperty` and `Object.getOwnPropertyDescriptor` now reach a host.
The `gin-shapes` handler answers a descriptor it builds inside the callback, and
checks that a define arrives at the `definer` with value and writable set.

*Re-entrant allocation (2026-10-31).* The allocation path landed next.
`CallbackWindow` in `crates/runtime/src/embed.rs` is the property-callback form
of `FunctionCall`: opening it publishes the context's agent as a token, and
`Object::New`, `Array::New`, and `Object::Set` take the token's route while a
callback is running, so a getter can answer with an object or an array it built
and a descriptor can build its own descriptor. The token is only admitted while
its window is running, which keeps the ordinary entry points refusing re-entry
(the `load()` case stays refused) instead of making every re-entry legal.

The mechanism is the TLS shape the audit rejected for `NativeFn` (the pointer is
the context's boxed agent, an ancestor of the live `&mut Agent` reborrows), so it
is a stated divergence rather than the final form: threading the agent through
`crux`'s host seam is the sound version and is separate, larger work. The v8
crate's Rust handler already allocates this way (`api::Context::with_agent`
reaches the same boxed agent), so the facade matches the existing bridge. The
facade finds the context from `g_current_context`, so a host has to enter an
isolate and a context the way V8's samples do; `gin-shapes` did not before, which
is why its first allocation attempt returned an empty handle.

*The enumerator (2026-10-31).* With both prerequisites in place the enumerator
landed, and the handler set is complete: `[[OwnPropertyKeys]]` dispatches to
`HostOps::own_property_keys`, the indexed enumerator is asked first and the two
answers are concatenated (V8's order: element indices before string keys), and a
key answer is read back from the array the callback built. A number element is an
array index and a string or symbol is itself, the reading V8's `KeepNumbers`
describes; a hole or any other element is skipped. `gin-shapes` reads the named
enumerator through `Object.keys` (which also needs each key's own descriptor) and
the indexed one through `Reflect.ownKeys` (which asks only for the keys, so it
needs no descriptor callback).

*Accessors (2026-10-31).* `SetAccessor` landed last, on both a template and an
existing object. The engine has no separate accessor seam and does not need one:
the getter and setter are installed as an own accessor property whose get and set
are *host functions*, so they run with the dispatch's agent, its allocation
window, and the context's pending-error slot. The C++ side stores a specification
per accessor (the two callbacks, the name bits, the data bits) and passes its
address, which the Rust side keeps for the property's life; the name and data are
rooted in the ABI registry because `Global` is defined after `ObjectTemplate` in
the header. `gin-shapes` reads the accessor from script (the getter builds the
object it answers with), writes it from script, and checks the object form
through C++.

*The callback window reaches the own-property reads (2026-10-31).* The last gap in
the window was that a callback could allocate, read, write, and delete but not
ask the interceptor-free questions: `Object::GetRealNamedProperty` and the
`HasReal*Property` predicates took the ordinary entry points, which refuse
re-entry. They now route through the token as well, over
`CallbackWindow::get_own_property` and `own_property_kind`, so a callback can ask
what it owns without going through any handler. The shared bodies moved into free
`get_own_property_with`/`own_property_kind_with` so `Context` and the window run
the same code. `gin-shapes` reads and tests `tag` with both forms inside the
accessor getter, and the runtime window test sets, reads, has, deletes, and
re-checks a property through the window.

With that, every callback a `NamedPropertyHandlerConfiguration` or an accessor
carries is wired, and every `Object` operation the facade offers takes the window
while a callback runs. The one remaining absence on the surface is
`GetIdentityHash`'s siblings.

*The thrown value and the identity hash (2026-10-31).* Two remaining gaps closed.
The pending-error slot now holds the engine's `JsError` rather than its message,
so `TryCatch::Exception` can hand back the value a script threw (`slag_take_exception`)
instead of a string rebuilt from the text; a host `ThrowError` still records a
message only, and its text is the fallback. The same change makes a host function
or property callback that records an error propagate that error's value rather
than a fresh `Error`, because `host_fn` and `CxxHandler::callback` return the
stored error instead of rebuilding one.

`Object::GetIdentityHash` answers the engine's object id through
`crux::object::identity_hash`, a splitmix-style fold. The first attempt folded
the id the way the `v8` crate's bridge did (low bits, forced odd), and the
harness caught the consequence: consecutive ids collide, so every neighbouring
pair of objects hashed the same, which is a linear probe for a host's table. The
fold now lives in `crux` and both faces use it, so the facade and the Rust bridge
agree on the hash of an object; `gin-shapes` checks stability, non-zero, and that
eight objects give at least four distinct answers (a hash may collide, a table
should not be linear). `TryCatch` also gains a clean-run check and a `throw
{ code: 99 }` whose value comes back intact.

*The error message and stack (2026-10-31).* `TryCatch::Message` and
`TryCatch::StackTrace` were the last of the error path. A message now exists when
a **compile** recorded a position: `slag_compile` takes the host's `ScriptOrigin`
name, and on a parse error it records the 1-based line, the 0-based column, the
line's text, and the name (computed with `syntax::SourceText::line_column` and
the new `line_text`, which live in the syntax crate where the parser's spans
belong). A runtime error keeps no position, which is the Rust bridge's own rule:
`JsError::span` indexes whichever source the failing code came from, so recording
it against the running script would put a wrong position on the error. The
sample's rich path (name:line, source line, caret) is therefore taken for a
syntax error and not for a runtime one, and the caret is empty because the
parser's error span is zero-length.

`TryCatch::StackTrace` returns the V8-style stack the engine already captures for
an `Error` (`Agent::error_stack`, exposed as `Context::stack_trace_of`), as a
string. The sample only asks for it when it has a message, so a runtime throw
prints its exception alone, as V8's sample would for an error with no source
position.

The three reads (exception, message, stack) had to stop consuming the pending
error: `slag_pending_exception`, `slag_pending_error`, `slag_pending_stack`, and
`slag_pending_message` peek, and `slag_clear_error` is what the `TryCatch`
destructor calls (V8's TryCatch resets the isolate's pending exception the same
way). `String::Utf8Value` also had to start running ToString through the engine,
which is why the runner prints `Error: boom` instead of a tag; it falls back to
the primitive rendering while a callback holds the context, because no host code
can run there.

*Microtasks and the message pump (2026-10-31).* The platform rung was next, and it
started from a finding: `slag_script_run` reaches `Context::eval_script`, which
by design does not drain, and nothing else drained either, so a script's promise
reactions and due timers never ran in the facade at all (the sample printed
nothing for `Promise.resolve(1).then(print)`). Three pieces close that.
`Isolate::SetMicrotasksPolicy` (with `kAuto` the default, `kExplicit`, and
`kScoped` accepted as explicit because there is no `MicrotasksScope` to close)
drives whether `Script::Run` drains when it returns; `Isolate::PerformMicrotaskCheckpoint`
drains on demand; and `platform::PumpMessageLoop` drains the isolate's contexts
and reports whether it ran anything, which is what the sample's
`while (PumpMessageLoop(...)) continue;` loop asks. The engine gained
`Context::drain_jobs` and `has_pending_jobs` over the existing `run_jobs` and
`job_queues_empty`, plus one ABI call, and a runtime test pins the
"queued, drained, nothing left" sequence.

What is still missing on this surface is the host-task half: the facade's
`Platform` has no task runner, so `PostTask`/`CallOnForegroundThread` and
platform tasks do not exist, and the pump only ever runs engine work. `kScoped`
and `MicrotasksScope` are the other stated gap.

*Value predicates and conversions (2026-10-31).* The whole predicate surface is
in: `IsUndefined` through `IsNativeError`, `Object::IsCallable`/`IsConstructor`,
`BooleanValue`, `TypeOf`, `ToString`, `ToNumber`, `ToObject`, `StrictEquals`,
`SameValue`, and `Local::operator==`. The shape bits (undefined, null, boolean,
number plus the int32/uint32 ranges, bigint, symbol, string, object, function,
array, external, typed array, proxy, arguments) come from the value's tag, so a
single `Value`-bits mask answers all of them. The brands (date, regexp, promise,
map, set, native error, array buffer) live in the agent's per-object tables, and
so does the `[[Construct]]` slot that makes an arrow, a method, or an
async/generator function non-constructible.

That is where the rung turned up a real bug rather than plumbing. The first cut
read the brands off `crux::function::current_agent()`, the TLS window, which is
live inside a host callback but empty for the ordinary host call, so
`value->IsDate()` on a value a `Script::Run` returned would have answered false.
The mask now takes the entered context's own agent, and the facade passes the
`SlagContext` behind the thread-local `Context::Scope`, which is the same scope
V8 reads the isolate from. `Object::IsConstructor` had the same shape but a
second defect: it asked `crux::value::is_constructor`, which reports every
ECMAScript function as constructible, so an arrow answered true there while
`new (() => 1)` threw. It now uses the runtime's narrowed `is_constructor`, which
consults the function record. `gin-shapes` checks the shape and brand bits, the
`NaN`/`-0` differences between `===` and `Object.is`, `BooleanValue`, `TypeOf`
on an object, a function, and `null`, a custom `toString` and `valueOf` through
the coercions, and `ToObject` boxing a primitive; a runtime test pins the mask
and the arrow/method/generator narrowing. The known narrowing is that a brand
asked with no `Context::Scope` on the stack answers false, where V8 would read
the isolate off the handle location.

*Buffers and typed arrays (2026-10-31).* `ArrayBuffer`, `SharedArrayBuffer`,
`BackingStore`, and the `ArrayBufferView`/`TypedArray` hierarchy are in, so the
bytes side of the boundary is real rather than a stub. The engine already had
the storage: `SharedBuffer` is a refcounted block, `BufferState` holds the
length, the shared/detached/resizable flags, and the detach key, and
`typed_array_buffer_path` is the constructor the language's own `new
Uint8Array(buffer, offset, length)` runs. The facade's job was to expose them.

`BackingStore` is a C++ handle to a cloned `SharedBuffer`, so a store read off a
buffer outlives the object that named it and a host that writes through the
store writes what a script reads. A host-owned block goes through
`SharedBuffer::borrowed` with the C deleter boxed as its `FnOnce`, so the
release happens exactly once, when the last buffer and store are gone. Every
typed array `New` delegates to the language constructor, which means the
alignment and bounds checks are the engine's own RangeError rather than a second
implementation in the facade; an out-of-range range answers an empty handle with
the error pending, which is what a V8 host's `MaybeLocal` check expects.

The detach protocol is the engine's: `Detach()` runs
`detach_array_buffer_with_key` with `undefined`, so a buffer with a
[[ArrayBufferDetachKey]] refuses it and `Detach(key)` is the only way through;
`WasDetached`/`ByteLength`/`Data` read the live state, and a view over a
detached buffer reports length zero at offset zero.

External memory needed one refactor: `api::heap` computed the buffer walk inline
for the Rust bridge's `Isolate::GetHeapStatistics`, and the C++ isolate is a
different type, so the walk moved into `external_memory_of(agent, adjustment)`.
`Isolate::AdjustAmountOfExternalAllocatedMemory` keeps the host's account on the
`SlagIsolate` box and adds each context's buffer bytes, which is what makes the
number the same one the Rust bridge reports. As there, the number is reported
rather than consulted: this engine's collections are paced by its own growth
policy. What the rung leaves out is `DataView::New` (the predicate is there),
the resizable/growable creation forms, and `ArrayBuffer::Allocator` actually
backing the engine's allocations (it is still a type the samples delete).

*Errors, private names, and data properties (2026-10-31).* The error factories
are engine constructors rather than facade strings: `Exception::TypeError` and
its siblings call `builtins::error::to_throwable`, so the value carries the
realm's own `name`, `message`, and stack, and `Isolate::ThrowException` records
an arbitrary value as the pending exception (the same slot `TryCatch::Exception`
reads). `Exception::CreateMessage` answers empty, which is the narrowing the
error path already documents: a position exists only for a compile error.

Private names are the symbol model the Rust bridge already uses, and the
facade now has the same shape: `Private::ForApi` keeps a per-isolate registry
(description to retained symbol, released in `Dispose`), `Private::New` mints a
fresh symbol, and `Private::Name` reads the description back. The
divergence is the one `.notes/embedding.md` records for the bridge and the
harness pins: the property is invisible to `Object.keys` and `in` but visible to
`Object.getOwnPropertySymbols`, because the engine has no private-name kind.
Because the facade's private name is a symbol value, `Private` derives from
`Name` here rather than V8's `Data`, so the ordinary key paths reach it; the
four `Object::*Private` methods are those paths with the private as the key.

The data-property forms were almost free once attributes existed: `SetAccessor`
takes a `PropertyAttribute`, `define_accessor_key` maps `DontEnum`/`DontDelete`
onto the descriptor and drops the setter for `ReadOnly`, so
`SetNativeDataProperty` and `SetLazyDataProperty` are that call. The lazy form is
the honest half-step: the engine has no one-shot slot, so the getter runs on
every read and the property is an accessor rather than a data slot.
`Object::SetAccessorProperty` needed one new piece, a trampoline that builds a
`FunctionCallbackInfo` (receiver as `this`, the assigned value as argument zero)
and calls the `FunctionTemplate`'s callback, so a template-based accessor runs
the same code its function would.

`Signature` is the one item from this rung not landed: it exists for embedder
access checks, which are A4, and the Rust bridge models it as an opaque data
type for the same reason. The harness checks the thrown error's name and
message, the private name dedupe and round trip, the lazy and native getters
with `DontEnum`/`ReadOnly`, and the template accessor pair; two runtime tests
pin the error values and the attribute semantics.

*Embedder data and security tokens (2026-10-31).* The host's realm slots are the
facade's, not the engine's: `Context` keeps one vector of aligned pointers and
one of values, retains a value on write and releases the replaced one, and
releases the rest in `ClearAllSlots` and the destructor. Two stores rather than
V8's one tagged array is a stated narrowing, and it is why `GetEmbedderData` on
a pointer slot answers `undefined` instead of an `External`; the Rust bridge has
the same shape and keeps only the pointer half. The check is a delta on
`slag_root_count` around `ClearAllSlots` (it must release exactly the two slots
written) plus a value that outlives the handle scope that made it and a
collection.

The default security token is the bridge's construction exactly: a
null-prototype object minted on the first ask through `slag_security_token_new`
(`crux::object::JsObject::ordinary_object_create(None)`, which needs no agent)
and retained by the isolate until `Dispose`, so every context without a token of
its own answers the same object. A set token is retained on the context and
released when replaced or reset to the default. The token gates nothing, which
is the engine gap `get_security_token` in the Rust bridge already records.

`Object::GetCreationContext` is the entered context. V8 reads the object's
creation context out of its map; this engine does not record a realm on an
object, and the facade has one realm per context, so the entered one is
the truthful answer while a `Context::Scope` is active and the handle is empty
otherwise.

*Weak handles, GC hooks, and heap statistics (2026-10-31).* The weak rung began
with a finding rather than a port: the engine's `WeakHandle` already reports a
death, but its `second_pass` slot is filled by the *sweep*, so a callback
registered from inside a first pass (which is the only time
`SetSecondPassCallback` is called) had nowhere to go. `crux::heap` gained
`queue_late_second_pass`/`take_late_second_pass_callbacks`, and
`Agent::run_host_finalizers` drains them after every first pass, so the
guaranteed finalizer keeps its promise even when the first pass released the
handle.

The C++ shape is `Global<T>::SetWeak` over a `SlagWeak` box: the strong
retention is released, the handle holds only the engine's weak entry, and the
trampoline builds a `WeakCallbackInfo<P>` (the parameter type, as in V8) and
calls the host. `SetWeak` also clears the `Global`'s own value word, which is
the trap the harness caught: the engine's conservative stack scan read that word
as a root, so the value survived every collection and the callback never ran.
The harness now makes the value in a frame that returns only the handle and
scrubs the stack, mirroring the api test's `an_unrooted_weak_handle`.

GC callbacks took the better road once the pieces were in view: the agent
already has `add_collection_observer` (a `GcObserver` that runs around the mark
and sweep), so the facade registers one observer per context and forwards to
the host's prologue and epilogue lists. That makes the hooks fire for every
collection the engine starts, not just a requested one, and `RequestGarbage
CollectionForTesting`/`LowMemoryNotification` are thin over `slag_collect`, which
now also drains the death callbacks so a weak finalizer has run when it returns.
Heap statistics use the same `context`-side walk the Rust bridge's
`api::heap` documents; the constants (an unlimited `heap_size_limit`, zero for
the stores this engine does not have) are the bridge's, restated in V8's struct.

*Modules (2026-10-31).* The engine's module machinery was already complete and
only `api::Module` reached it; `embed` had nothing. So the rung added an
`embed::JsModule` that pins the record (a module is not a language value, so no
handle scope roots it, which is why the Rust bridge has `pin()`), plus the
compile/register/instantiate/evaluate/namespace entry points over the same
`crate::module` functions. `register_module` inserts into the realm's
`loaded_modules` with a write barrier, exactly as the bridge's `register` does,
so linking resolves imports without asking the host twice.

The C++ `Module` is a handle over the `SlagModule` box, and the module box is
never freed, like a script or a template. `InstantiateModule` is the one piece
with real logic in the facade: because the engine links from its registry, the
resolver has to be asked for every request in the graph, so the facade keeps a
worklist and pushes each resolved dependency after registering it. A resolver
that answers empty leaves the V8 `Nothing` contract intact (the callback is
expected to have thrown). The harness checks the dependency's kind and identity,
the request list, linking, the evaluation promise, and the export read from the
namespace; a runtime test does the same through `embed`.

What this leaves for the Wasm rung: `v8-wasm.h`'s `WasmModuleObject`,
`WasmInstanceObject`, `WasmMemoryObject`, and the streaming builder. The engine
has the compiler and the interpreter, and `hello-world` already runs WebAssembly
through `WebAssembly.instantiate`, so the work is the C++ shape over
`runtime::wasm`, not a new compiler.

*WebAssembly (2026-10-31).* The module half landed. The engine's
`builtins::wasm::compile_module_value` already answers the object
`WebAssembly.Module` constructs, so `WasmModuleObject::Compile` is a thin wrap
and the harness proves the result is the JS-API's object by instantiating it
from a script and calling `add(3, 4)`. `CompiledWasmModule` is a move-only owner
of a `SlagWasmModule` box; the box holds `api::CompiledWasmModule`, which is
plain data, so `GetCompiledModule` clones it out of the agent's table and
`FromCompiledModule` wraps it back into a fresh module object. `MemorySpan`
needed a local definition (`v8-sandbox.h`'s is a template the facade does not
otherwise need), and it is the shape Blink's `base::as_bytes(...)` produces.

The absence worth stating is the wire bytes: the engine decodes into structures
and keeps none, so `CompiledWasmModule` has no `GetWireBytesRef`, the same
divergence `.notes/embedding.md` §12 item 10 records for the Rust bridge.
`WasmInstanceObject`, `WasmMemoryObject`, and `WasmStreaming` are the rest of
`v8-wasm.h` and are not in this slice; a script instantiates through the JS API,
which is what the harness and `hello-world` both do.

*Snapshots (2026-10-31).* The blob format was already there: `embed::Context` has
`write_snapshot`/`read_snapshot` over the crate's snapshot codec, and the
facade needed the V8 shape around it. `SnapshotCreator` owns an isolate and
serializes its default context; `CreateBlob` asks for the length, allocates with
`new char[]`, and writes, so the host's `delete[]` matches. `CreateParams`'s
`snapshot_blob` is copied into the isolate at `New` (V8 lets the caller free its
`StartupData` as soon as `New` returns) and the first `Context::New` restores it,
which is V8's default-context rule in this engine's one-realm-per-context model.

The trap is the writer's refusal: a realm holding host callbacks cannot be
serialized, and `Context::New` installs `console` and the timers. The harness
had to delete them before the write, exactly as the embed test's comment says;
the first run of the snapshot block failed on that and is why the code is
written out rather than assumed. The reading context keeps its own callbacks
because it creates them after the restore.

What is not in this slice is the rest of `v8-snapshot.h` (`AddContext`,
`SetDataBlob`, `AddData`, the external-reference table) and the code cache. A
cached compile needs the engine to serialize its compiled program; the Rust
bridge's `CachedData` is a copy of the source and checks it rather than trusting
it, and that is the honest ceiling until the engine has a cache format.

*Function templates and prototypes (2026-10-31).* The A3 shape is in, and it
needed one engine addition and one engine fix. The addition is
`embed::create_template_constructor`: `Function::create_builtin` over a fixed
prototype, an instance shape (internal fields and the API-wrapper bit) and an
optional `[[Construct]]`, which is what `FunctionTemplate::GetFunction`
materializes. The parameter count tripped clippy, so the shape is a
`TemplateInstance` value rather than five arguments; that reads better anyway.

The fix is the one worth remembering. A host *function* callback used to run
without a published `CallbackWindow`, so the facade's `Object::Set` inside it
took the ordinary entry point and recorded "re-entrant evaluation is not
supported". The constructor in the harness sets two properties on `this`, which
is how it surfaced. `slagc`'s `host_fn` now wraps every callback in
`CallbackWindow::enter(context.agent)`, so the token names a live window and the
property surface works from a function callback exactly as it already did from a
property callback. The same fix covers allocation (`Object::New`, `Array::New`)
from a function callback.

`Inherit` chains the *built prototype objects*, not the instance templates: a
child's prototype inherits from the parent's built prototype, which is the
object carrying the parent's `PrototypeTemplate` members. The first version
chained the parent's instance object and the harness's inherited method was not
found. `ConstructorBehavior::kThrow` is enforced by passing `None` as the
construct closure, and the harness checks that `new` on such a function throws.
What is still absent is the call-as-function handler's behaviour (recorded, not
installed) and the access checks `Signature` exists for, which are A4.

The ABI shape is roughly a struct of up to seven C function pointers, the data
pointer, and the flags, handed over once per template, with the return codes
`Intercepted` already defines; the callbacks receive a `PropertyCallbackInfo`
over the same argument/this/return-value convention `FunctionCallbackInfo` uses.
The trap on the C++ side is the callback's `ReturnValue` and the value handles
it hands back: they must be rooted by the caller's handle scope, or the
interceptor's own temporaries die before the engine reads them.

*Strings and the integer conversions (2026-11-01).* The string surface landed
next: `NewFromOneByte` (Latin-1, one code unit per byte), `NewFromTwoByte`,
`ContainsOnlyOneByte`, `Utf8Length`, and the `Write`/`WriteOneByte`/`WriteUtf8`
family with `start` (negative counting from the end), `length` (`-1` for the
rest), `NO_NULL_TERMINATION`, and `REPLACE_INVALID_UTF8`. The encoding is the
one `crates/v8/primitives.rs` already pins: a lone surrogate is written as the
three bytes of its own code point unless replacement is asked for, `nchars_ref`
counts the code units covered, and the answer includes the terminator, so the
C++ side passes its buffer capacity straight through instead of re-deriving
V8's byte accounting. `String::Length` was wrong before this slice and is now
the code unit count V8 promises rather than the UTF-8 byte count it was measured
in. `Int32Value`/`Uint32Value` also changed meaning: they were a range check
that failed outside the type's bounds, and are now ToInt32/ToUint32 through
ToNumber, joined by `IntegerValue` (ToIntegerOrInfinity, saturating at the
`i64` ends).

Two traps surfaced while wiring them. A conversion inside a host callback cannot
borrow the engine, so it needs the same window route allocation does:
`CallbackWindow::to_number` and `slag_callback_value_to_{int32,uint32,integer}`
now carry it, and `Value::Int32Value` picks the token route while
`g_callback_token` is set. The second is a facade divergence fixed rather than
worked around: `Isolate::GetCurrentContext()` answered a *non-empty* handle over
address zero when no context was entered, so a host that converted with it
dereferenced null; it answers an empty handle now, which is what V8 does and
what makes `IsEmpty` usable. The harness then had to enter the context its
`bump` callback asks for, which is what a real host does anyway. Fixing that
exposed a third: the `SetAccessorProperty` `FunctionTemplate` trampolines never
published the facade's callback token (unlike `HostTrampoline`), so a conversion
inside such a getter or setter recorded a re-entrancy refusal; both now hold a
`CallbackToken` for the call.

*Promises (2026-11-01).* The `v8::Promise` surface landed next, on the `embed`
API rather than in the ABI: `promise_capability`, `promise_then`,
`promise_settle`, `promise_state`, `promise_result`, and `promise_mark_handled`
are free functions in `crates/runtime/src/embed.rs`, each with a method on
`Context` and a re-entrant method on `CallbackWindow`, so a binding can build and
settle a capability from inside an accessor. `Then`/`Then2`/`Catch` run the
engine's PerformPromiseThen and `Resolver::New` runs NewPromiseCapability over
`%Promise%`, not the `then`/`Promise` properties, so a script cannot change what
a host's call does.

The one piece of state the engine does not keep is the rejecting function: its
resolver record names the pair by function identity and holds only the promise,
so `slagc` keeps the promise and the rejecting function against the resolving
function's id, roots them, and drops the rejecting root on the first settlement
(the pair can never fire again). `GetPromise` and `Reject` then name them back.
The cost is a resolver that never settles: its pair lives until the context is
freed, where V8 collects it with the promise. `Resolver::New(context, promise)`
and the `Promise::Resolve`/`Reject` statics are left out for now.

*Maps and sets (2026-11-01).* `v8::Map` and `v8::Set` landed next, and the
interesting decision was where the semantics live: the engine already has the
`%Map.prototype.*%` and `%Set.prototype.*%` intrinsics and the internal
`[[MapData]]`/`[[SetData]]` lists, so the facade runs the intrinsics by name
(not the `Map.prototype` properties, which a script can replace) and reads the
lists directly for `Size` and `AsArray`. `agent.map_entries(id)` and
`agent.set_entries(id)` were already public for the set-methods, so no engine
change was needed. `AsArray` answers V8's flat form: `[key, value, ...]` for a
map.

The container surface is deliberately ordinary-route only. Every operation needs
`&mut Agent` and the engine's intrinsics, which means the callback window's route
would have to be threaded through as it was for promises; nothing in the gin
shape needs it, so the facade refuses a re-entry with the existing pending error
instead of growing twelve more entry points. That is the first thing to revisit
if a binding turns out to call `Map::Set` from a getter.

*The host's array-buffer allocator (2026-11-01).* `ArrayBuffer::Allocator` became
a real interface with `Allocate`/`AllocateUninitialized`/`Free`/`Reallocate`, and
`Isolate::CreateParams::array_buffer_allocator` is now kept by the isolate: a
backing store it makes is drawn from the host's allocator and released through
it. That is the hook Blink needs, because its bytes must come from the partition
allocator rather than from inside the engine's own arena.

The implementation reuses `ArrayBuffer::NewBackingStore(data, length, deleter,
  deleter_data)`: the allocator's bytes are lent to the engine exactly like a
host's own range, with `Free` as the deleter. That exposed one modelling
difference worth recording: an external-memory walk skips a borrowed block, on
the assumption that the host that lent it accounts for it. Allocator-provided
bytes are not that: V8 reports them as external memory. So `SharedBuffer` grew
`borrowed_counted`, the ABI's `slag_buffer_borrowed` grew a `counted` flag, and
the walk now skips a borrowed block only when it is *not* counted. The harness
proves the whole path with a counting allocator: both creation forms draw their
bytes from it, dropping a store calls `Free` immediately, and the bytes still
show up in `HeapStatistics::external_memory`.

`SharedArrayBuffer::New` still allocates its own block, as V8's shared memory
does, and an isolate with no allocator falls back to the engine's allocation
where V8 would refuse.

*Callable template instances (2026-11-01).* `SetCallAsFunctionHandler` used to be
recorded and ignored. Making it work needed no engine change after one
observation: a function in this engine already has an object part
(`Function.object`), so the instance can *be* a builtin whose body runs the host's
handler, with the template's members, internal fields, and API-wrapper bit on the
callee itself. `create_callable_instance` builds it, and `JsValue::as_object`
now maps a function to its object part (which is what `context::as_object` always
did, and what makes every `slag_object_*` entry point accept a callable).

Two narrowings are recorded rather than hidden: a template with both a call
handler and a property handler keeps the handler object, because the engine's
host objects have no `[[Call]]` and a callable cannot also be a `HostOps` object;
and the callable carries the builtin's `length` property, since `create_builtin`
always installs one. `info.This()` inside the handler is the call's receiver,
which for a plain `f()` is undefined, exactly as in V8.

*Signature access checks (2026-11-01).* A4 opens with the access check A3 left
accepted-and-ignored: `Signature::New(isolate, template)` now guards the method
it is declared on, and a receiver that is not an instance fails the call with
V8's `TypeError: Illegal invocation`. The check is a prototype-chain walk, and it
runs inside the trampoline, so it goes through the callback's window like every
other engine-reading entry point; the refusal itself has to be recorded rather
than constructed, since allocating an Error object inside a trampoline would need
the engine. `slag_throw_error_kind` exists for exactly that: it records a
`JsError` of any kind, which the call turns into the thrown value.

One prerequisite turned out to be a fidelity fix in its own right. The check
compares against the template's built prototype, and `BuildPrototypeObject` used
to build a fresh prototype object on every call, so two instances of one template
had different prototypes and the identity walk could never succeed. It now builds
once and keeps the object (rooted; templates are never freed), which is what V8
does and what `instanceof` through a materialized template now reports
consistently.

What A4 still needs is the rest of the section: `AccessorSignature`
enforcement, `SetAccessCheckCallback` and the per-object check, and making the
security token mean something (it is stored today and never consulted, which is
an engine gap rather than a facade decision).

*`DataView` and the promise statics (2026-11-01).* The last of the small compile
gaps landed: `v8::DataView::New` (both buffer kinds) and `Promise::Resolve` /
`Promise::Reject`. `DataView` exposed a real modelling difference rather than a
missing wrapper: `ArrayBufferView::ByteLength`/`ByteOffset` read a typed array's
own slots, which a DataView does not have, so both accessors answered 0 for one.
The engine keeps a DataView's geometry in its `dataview_data` table, which is
agent state, so the view accessors now go through `slag_array_view_byte_offset` /
`_byte_length` (typed array first, agent-free; DataView second, through the
context) and need an entered context for a DataView. The two promise statics are
`Promise.resolve`/`Promise.reject` faithfully: `Resolve` answers the value itself
when it is already a promise of the realm.

### 14.2 Per-object access checks: what the engine must record

The facade enforces a `Signature` for a function template today (a
prototype-chain check, above). What it cannot enforce is the *per-object* check
Blink configures with `Object::SetAccessCheckCallback` and
`ObjectTemplate::SetAccessCheckCallbackAndHandler`, which is what makes a
cross-origin `Location` readable or not, and the security token is stored and
never consulted for the same reason. Both want one fact the engine does not
have: **which realm an object came from**.

V8 keeps it in the object's map, and every access through a context whose token
differs from the object's goes through the check. This engine's `JsObject` has no
such field, and the two places that could approximate it are both wrong:

- a side table keyed by object id is unsafe, because the arena's free list reuses
  arena slots, so a swept object's id can name a later object (the same reason an
table keyed by address was rejected for internal fields);
- the facade keeping its own table has the same problem and cannot see objects a
  script created anyway.

So the first slice is engine work, in the shape the internal-fields work already
took:

1. A `realm: Cell<Option<Handle<Realm>>>` on `JsObject` (8 bytes, and the field
   two in-place initializers must both write it, which is the documented trap).
2. Stamped at creation, from the agent's current realm, everywhere an object is
   made for a realm (the arena fast path, `array_create`, host objects, function
   object parts). Objects with none read as "no realm", and the check treats that
   as the caller's own.
3. A `HostOps`-style hook, or a smaller per-object callback registered on the
   realm, consulted by `get`/`set`/`has` when the object's realm differs from the
   running one. `Signature`'s check would then move behind it rather than living
   in the trampoline.

Until that lands, the honest statement is the one the README makes: a signature
is enforced, a per-object access check and the security token are not.

*Worlds plumbing: the context-isolate link (2026-11-01).* The first A4 additions
are the ones every Blink bindings file assumes: `Context::GetIsolate`, which the
facade now records when it creates a context, and
`Isolate::GetEnteredContext`/`GetEnteredOrMicrotaskContext`, which answer the
entered `Context::Scope` (the facade has no separate microtask-context notion to
distinguish, and says so in the header). `gin-shapes` checks both against the
scope it entered.

The rest of A4 is blocked on an engine fact, and §14.2 records it: a per-object
access check (`SetAccessCheckCallback`) and the security token both need to know
which realm an object came from, and `JsObject` does not record one. A side table
is not an option for the reason internal fields already rejected one (arena slots
are reused), so the slice is a `realm` field stamped at creation plus a hook the
ordinary property paths consult on a cross-realm access. Until then a signature
is the only access check the facade enforces.

*The realm an object was created in (2026-11-01).* The first half of §14.2 is
in: `JsObject` carries `realm: Cell<u32>`, the realm record carries a
`signifier` (0 is "no realm", so the counter starts at one), and every object
constructor stamps it. Two sources decide the value, in that order: the realm
being built, which covers realm bootstrap (the global object and
`%Object.prototype%` are protoless and are created before the realm record
exists), and the realm of the prototype the object is created on, which covers
everything built while no realm is current (a host calling `Object::new`
between scripts). The execution-context stack keeps the first source live:
`ContextStack::push`/`pop` save and restore it, so an object made by running
code belongs to the realm that code runs in, and `run_job` sets it for a job.

Three decisions are worth recording. The field is a `u32`, and it fit in
padding the payload already had: `size_of::<JsObject>()` is still 432 bytes, so
the 448-byte arena class (and the guard test that pins it) is unchanged. The
realm is **not** read back through the map, which was the first attempt: an
object that drops to dictionary mode (`drop_map`) loses its map, and an
embedder wrapper drops its map on the first define whose attributes differ from
the shape, so the map is exactly the wrong place for the fact an access check
needs. And the prototype is consulted at *creation* rather than trusted later,
so a later `setPrototypeOf` cannot change which realm an object reports.

What is left of §14.2 is the hook: `get`/`set`/`has` consult the object's realm
on a cross-realm access, and `Signature`'s check moves behind it. The facade has
no way to ask yet (`slag_value_realm` / `slag_value_same_realm` come with the
hook), so the README's "a signature is enforced, a per-object access check and
the security token are not" still holds.

*Per-object access checks, and where the hook actually had to go (2026-11-01).*
§14.2's third item turned out not to be a hook on `get`/`set`/`has`. Those are
the interpreter's entry points, and the compiled lanes do not go through them:
a named read is served from a member-value cell validated by the object's id and
generation, and an element read from the dense buffer. A runtime hook would be
bypassed by any hot function, which in a browser is every function that matters.
So the check is enforced by the object's **shape** instead: an instance of a
template with `SetAccessCheckCallback` is an engine host object, and a host
object never takes a compiled fast lane (`is_plain_object` fails, the
interpreter's own fast path is `Ordinary | Array` only). Every property internal
method then arrives at the handler, and the handler runs the check first. This
costs nothing on the hot paths because the gate is a dispatch that already
existed, and it is what the notes already said host objects are for: the route a
V8 named-property handler rides.

The engine side is three pure queries, `slag_value_realm`,
`slag_value_same_realm` and `slag_context_realm`, plus `slag_context_enter` /
`slag_context_leave`, which `Context::Scope` uses to make the entered context's
realm current. That last pair matters: realm attribution is a thread-local, so
without it a host-side access between scripts would compare against whichever
realm was created last. With it, and with `realm_for` preferring the prototype
over the ambient (a realm's prototypes are its own, so `{}` from realm A belongs
to A even when B is running), the answer is right for a same-realm read, a
cross-realm read, and a host-side read.

Still open in A4: `AccessorSignature` enforcement, `Object::SetAccessor`'s
signature parameter, the security token (comparable now, consulted by nothing),
and moving `Signature`'s prototype-chain check behind the same callback.

*One receiver check, and two ways into it (2026-11-01).* The two gaps the
paragraph above names are closed, and they closed into one seam.
`AdmitsReceiver(template, receiver)` is now the only place the prototype-chain
walk lives; the call trampoline asks it through `FunctionTemplate::AllowsReceiver`,
and the ordinary-accessor and `SetAccessorProperty` trampolines ask it directly
before they build a callback info. So a method, an accessor declared over
`AccessorSignature`, and an accessor whose getter template carries a `Signature`
all agree about what an instance is, and a refusal is `Illegal invocation` in
every case.

The accessor signature is an overload rather than a new position in the existing
signature: `signature` sits before the attribute and has no default, so
`SetAccessor(name, getter, setter, data, None)` still binds the old form and
nothing that compiled before changes meaning. The signature itself is stored on
the accessor specification as the template pointer, because the trampolines
already read that specification and it outlives every instance.

One trap for the next person: a *template's* own signature is not a receiver
type. The first version of the `SetAccessorProperty` check compared the getter
template's built prototype against the receiver, and a never-materialized
template has no prototype, so every such accessor was refused. In a harness
whose scripts end in `ToLocalChecked()`, that surfaces as an `abort()` with exit
code 3 and no message, which is why the harness now sets stdout unbuffered and
switches the CRT's abort and assert dialogs off at startup: a failing run must
print and exit, never block on a modal box.

*The harness reports instead of aborting (2026-11-01).* The abort was reachable
because every block's `run`, `bind` and `number_at` ended in `ToLocalChecked` or
`ToChecked`, so a thrown script killed the process before anything could say
what it was. Those three now delegate to `RunScript`, `BindGlobal` and
`IntGlobal`, which go through a `TryCatch`, print what threw with the first 90
characters of the source, and count a failure, so one broken block no longer
hides the rest of the run. The check was made the hard way: a probe script that
throws prints and exits 1 with the rest of the harness still running, where it
used to abort with no output at all. A block's own ad-hoc `ToLocalChecked()` is
still a hard stop, and the dialog suppression (a static initializer now, so it is
armed before `main`, and `main` keeps the unbuffered stdout) is what keeps that
from blocking a caller.

*The security token means something (2026-11-01).* This was the last item of the
A4 section that was stored and never consulted. `Isolate::MayAccess(accessing,
accessed)` is the comparison itself: a value of the accessing realm is
reachable, a value of another realm is reachable exactly when the two contexts
answer the same token, and a realm no context of the isolate claims (a shadow
realm a script made) is the caller's. Contexts that never set a token answer the
isolate's shared default, so "every context in the test suite shares one token"
still admits everything and nothing regressed.

Two decisions. The check lives in the facade, not the engine: the engine records
realms and the facade is what knows which context is which origin, so the tokens
stay on the side that owns them. And an object is only checked when it asks to
be, which is V8's rule: `SetAccessCheckCallback(isolate, nullptr)` now means
"the default check" instead of "allowed", so a template can ask for the token
comparison without a callback. Ordinary property access is not gated by the
token, and the README says so rather than implying a security model the engine
does not have.

`Context::GetSecurityToken` used to mint the default on the *entered* isolate,
which answered empty outside every scope; it now uses the context's own isolate,
which `MayAccess` needs because a host may compare tokens between scripts.

*The last two A4 items, and the guard they produced (2026-11-01).*
`Object::GetCreationContext` answered the entered context because the engine had
no per-object realm to ask. It has one now, so the facade's
`Isolate::ContextForRealm(realm)` turns it back into the context and the answer
is the object's own (`gin-shapes` reads an object created in a second context
while the first is entered, which the old code could not have passed). The
entered-context answer survives only as the fallback for an object no realm
names, which is what a host-made value outside every context is.

The signature check moved to where the trampolines cannot forget it. Each one
used to publish the callback token and then ask the receiver check itself, two
steps that a new trampoline had to remember in the right order (the check reads
the engine, so the token must come first). `HostCall` is now that pair: it opens
the window, asks `AdmitsReceiver`, and records the refusal, and every trampoline
(function, callable instance, both accessor kinds, both template-accessor kinds)
starts with it. `FunctionTemplate::signature_type()` is the one unwrap of a
`Signature`, so no trampoline handles that type at all.

### 14.3 The inspector: what A5 needs, in the order it is needed

The milestone table makes A5 `content_shell --dump-dom`, so the exit criterion is
a Blink build and not the inspector. The inspector is the other half of A5's cost
row ("snapshots, streaming compile, inspector") and it is the part the facade
can start on before Blink exists, because everything it consumes is an engine
fact a host could also use. This section is the order.

1. **Message listeners.** `Isolate::AddMessageListener(WithErrorLevel)` and
   `RemoveMessageListener(WithErrorLevel)` are in (2026-11-01), delivering an
   uncaught error as a `v8::Message` and honouring `TryCatch` as V8 does. This is
   `Runtime.exceptionThrown` and half of `Log.entryAdded`.
2. **Console routing.** The engine has no `console`; `embed::Context` installs
   one for the Rust embedding, and the C facade's contexts have none. A browser's
   `console.log` is Blink's object calling its own message store, so what the
   inspector needs from this side is only that a console call can carry a
   `v8::Message` and a stack. That makes the stack the next item.
3. **Stacks as data.** `TryCatch::StackTrace` answers a string today. The
   inspector wants call frames (function name, script id, line, column), which
the engine has (`api::stack_trace` walks the execution contexts) and the facade
   discards. `V8StackTrace`/`V8StackTraceImpl` are the header shape for it.
4. **Script identity.** `Debugger.scriptParsed` needs a stable id per compiled
   script and its source, url and start line. `ScriptOrigin` already carries the
   name; the engine's `ScriptRecord` carries the source. What is missing is an
   id the host can hold, which is the same `SourceTextModule`-style handle the
   facade already uses for scripts.
5. **Pause and step.** The one genuinely large engine item: a `Debugger` domain
   needs the VM to consult a hook at source-position granularity, and the
   compiled tiers to either decline or emit the same check. This is the second
   instance of the trap the access check hit: a hook on the interpreter alone is
   bypassed by every compiled function. §14.4 fixes the protocol before any of
   it lands.
6. **`v8-inspector.h`.** `V8Inspector`, `V8InspectorSession`, `V8InspectorClient`,
   `StringView`, `StringBuffer`, `V8StackTrace` and the protocol dispatch on top
   of 1 to 5. The protocol itself is the inspector's own and does not belong in
   the facade.

Slices 1 to 4 are host-useful without an inspector and should land on their own;
slice 5 is a project, and slice 6 is glue once 1 to 5 exist.

### 14.4 Pause and step: the protocol the compiled tiers have to agree to

The `Debugger` domain needs four verbs (breakpoint at a position, step in, over
and out, pause on exception, pause now), and three data needs (the paused
position, the frame stack, and evaluation on a paused frame). This section fixes
the contract before any of it lands, because the cost lands in the VM and the
JIT, and a protocol fixed after the fact is a rewrite.

**What the engine has.** `Step` (`crates/runtime/src/ir.rs`) is the interpreter's
bytecode and `Vm::run_*` is its driver; `LeafOp` is the register tier with a
second `step` loop; `crates/jit` compiles a `CompiledBody` whose entry does not
return to the driver until the body finishes. `VmOutcome`
(`Completed`/`Suspended`/`TailCall`/`Call`) is already a *resumable* protocol:
await and yield suspend a `Vm` and resume it later, and the `Vm`'s `ip`,
`stack`, `frame`, try stack and for-of stacks are plain fields. The
execution-context stack already holds what a host would read
(`api::stack_trace` walks it). `Span`s exist in the AST, but **no `Step` carries
one** — that is the first thing to add.

**The trap, restated.** A hook consulted only by the interpreter driver is
skipped by every body the JIT compiled, and by the leaf tier if its loop does
not consult it too. The access check hit this and solved it by shape (a host
object takes no fast lane). Debugging cannot do that, because the object being
debugged is ordinary code by definition. So the protocol is written as three
rules the *compiler and the JIT* obey, not as a hook the interpreter happens to
call.

**R1. A debug session disables compilation.** `lookup_info` and
`lookup_info_leaf` answer null while a session is attached, and every
already-compiled body is evicted through the existing eviction path. This is not
a fidelity compromise: V8 disables optimization under the debugger for the same
reason (its stepping machinery is in the bytecode handler), and the cost is paid
only while debugging. It also makes the protocol testable: with the rule in
place a debug build can assert that no compiled body ran
(`jit_depth`-style, in the entry hook), so a future tier cannot quietly
reintroduce the bypass.

**R2. One debug point per statement, and one per loop back edge.** A new
`Step::DebugCheck { position: u32 }` precedes each statement's first step and
each back edge. It reads one `Cell<bool>` on the agent (`debug_attached`); when
false it is a predictable load and a not-taken branch, which is exactly what
V8's debug-break slot costs. The `position` indexes a per-body
`Vec<(u32 step_index, Span)>` the compiler builds while emitting, so the host
can ask "where am I" and "which statements are at line N" without decoding a
step, and a breakpoint is a set of positions in that table.

**R3. Exceptions are a debug point too.** `PauseOnExceptions` (`None`,
`Uncaught`, `All`) is consulted where the VM turns a `JsError` into
`Vm.thrown` — one place in the driver — rather than in the throwers, so a throw
from a builtin, a native and a `throw` statement all pause the same way.

**Pause and resume.** `VmOutcome` gains `Paused(PauseReason)`, and `run_inner`
returns it with the `Vm` intact. That is the architectural commitment to take
seriously: today a `Vm` is a local of the run, so a paused run cannot be
inspected or resumed. The driver must keep the paused `Vm` and its
`ExecutionContext` in a `PausedRun` record on the agent, which is what makes
resume possible — the same shape a generator uses, already proven in this
engine. A resume re-enters the same loop with the same `Vm`.

**Re-entrancy, the hardest part.** While paused, the host calls
`Debugger.evaluateOnCallFrame`, `callFunctionOn`, `getProperties`. Those run
code, and the pause hook itself is called from inside the borrowed agent. The
`CallbackWindow` seam already allows a host callback to allocate and call
through the operation that entered it, so evaluation goes through that window
while the paused `Vm` sits in the record rather than on the Rust stack. The
consequence for the facade is a rule worth writing down now: a session never
touches a paused frame directly, it hands the engine a frame id and gets values
back. Anything else means the paused `Vm` and a live `&mut Agent` overlap.

**Data the host sees.** `PauseInfo { reason, script_id, position, frames }` and
`DebugFrame { function_name, script_id, position, this, scope_chain }`. Scopes
are the one place the engine's environment model would leak: they are read
through the same `EnvRef` walk `stack_trace` uses and answered as key/value
pairs, so no `EnvRef` crosses the ABI.

**Cost, and how it is measured.** Two numbers, both with the same-binary A/B
protocol and both recorded in the perf archive: with `DebugCheck` emitted and
`debug_attached == false`, the benchmark rows must be within noise (one
predictable branch per statement is the budget); with a session attached, the
JIT is off entirely and the numbers are the interpreter's, which is expected and
not a regression. If the first number is outside noise, the fallback is to emit
`DebugCheck` only into bodies compiled while a session is attached — but that
changes the bytecode per compile, so it is the option to take only with a
measurement in hand.

**Order of landing.** (1) R1: the `debug_attached` flag, the compile gate, the
"no compiled body ran" assertion, and the ABI to set it. (2) R2's `DebugCheck`
and the position table. (3) `VmOutcome::Paused`, the `PausedRun` record, resume,
and frame reads. (4) Breakpoints against the position table. (5)
`evaluateOnCallFrame` through the callback window. (1) is the rule that makes
the rest honest and is the only one that needs nothing else in place.

**2026-11-01, R2's emission measured.** R1 landed (`Agent::debug_attached`, and
`lookup_info_for` refuses the compiled tiers and does not serve a cached entry
while a session is attached), and so did the point: `Step::DebugCheck { span }`
precedes each statement's first step, emitted by `compile_statement`. Two
corrections to this section from the code. **The span rides in the step**, not in
a per-body `(step_index, Span)` table: `Span` fits the step's existing payload,
and an index would be a second thing to keep in step with the bytecode. A body's
positions are therefore derived by scanning its steps when breakpoints land,
which is a debugging-time cost and not a per-statement one. The point is also
**not** a branch reading a `Cell<bool>` yet — that is the pause slice; the
emission is the bytecode change the pause can be attached to without a second
layout change under it.

The measurement answered the cost question, and it answered it by finding a bug
rather than a price. Emitting the step is **free**: full corpus, one process per
row, 7 interleaved rounds alternating order, per-row minima, `SLAG_DEBUG_POINTS`
as the same-binary seam: **1092.2 ms against 1093.8 ms (-0.14%)**, mean -0.17%,
no row outside the noise floor. What was not free was every lowering that had to
learn about the step, and each failure looked like the step being expensive:

- `lower_step`'s catch-all *refuses* an unknown step, so every leaf body gained
  a refusal and lost its leaf-call inline: **+93%** per row on `opcost`, with
  `obj_prop` 0.041 to 0.614 ms. The fix is an arm that emits nothing.
- the optimizing tier's lift refuses a body outright, so a body containing one
  demoted to the baseline tier: **+12.7%** on `opcost`+`control` as a directory
  run, up to **13x** on the small rows (`obj_prop` 0.043 to 0.604 ms). The fix is
  `0` in `stack_delta` and an empty arm in the optimizer's emitter.

The second one is the load-bearing result, and it changes the decision: a
session cannot survive a tier that refuses the step, so the emission was **on by
default** while the attach could not recompile the bytecode (the third
measurement below gates it). R1 alone does not make the compiled tiers harmless
here — the body a session runs is the bytecode built *before* the attach, which
is why the points have to be in the body a session runs rather than compiled in
on attach. What R1 buys is that the compiled tiers, which model the point as a
no-op, cannot run at all while a session is attached. The fallback in the cost
paragraph above is not taken for the statements, because the compiled tier is
free; the second measurement below takes it for the loop back edge.

**2026-11-01, second measurement: the interpreter, and the back edge.** The
number above is the JIT tier. `--jitless`, same seam, full corpus, 5 interleaved
rounds: **+1.11%** (3528.8 against 3490.1 ms), worst on the statement-dense rows
(`try_catch_loop` +8.4%, `switch_dispatch` +6.2%). A point is one extra step
dispatch in the interpreter, about 2.35 ns, not the "one predictable branch"
this section assumed; it is small enough per statement that the compiled tier,
which is the product, does not see it. The per-iteration case is different: one
extra point in a 2M-iteration bare loop costs **+6.9%** interpreted (73.06
against 68.36 ms; the switch-off control on the same pair is +0.33%).

That is why **the back-edge half of R2 is not emitted**, and why the fallback
this section rejected is the shape to take, only later and in full: a session
must get its own bytecode. On attach, invalidate the cached bodies so they are
recompiled with both points; on detach, invalidate again. That is what V8 does
(it flushes and recompiles bytecode under a debugger), it removes the +1.11% as
well, and it is safe only at a stop-the-world point, which R1's attach already
is. Until that slice lands, a loop with no statements (the shape the back-edge
point exists for) has no per-iteration place to stop, and a `continue` skips the
last statement's point; both are recorded gaps, not silently missing hooks.

**2026-11-01, the attach-time bytecode is not a cache clear.** Dropping the
cached bodies on attach and letting the next call recompile looks like a small
slice and is not, which is worth recording before someone tries it. A compiled
body is reachable from three places: `AgentRoots::compiled_bodies` (per site),
`AgentRoots::script_bodies` (per script text), and the `EcmaFunction` record's
own `ir`. The third is the one the call path reads (`ordinary_call` builds its
`FastCall` from the record), and it has **no recompile path**: `set_compiled`
runs only at function creation. So clearing the record's `ir` demotes that
function to the interpreted environment path for good, while clearing only the
site cache leaves the record's body in place, i.e. a session with no points in
exactly the functions that were already warm. Recompiling a record also cannot
be done from the record alone: `compile_body` takes the function's `name`, which
the record no longer carries (it was dropped on the way to 152 bytes), and the
verdicts `set_compiled` derives (`leaf_inline`, `construct_inline`) would have
to be recomputed. That record-level recompile is therefore step one of the
slice.

Until it lands, the point stays always-on and `Agent::set_debug_attached` only
flips the flag `lookup_info_for` reads. **It landed as the third measurement
below.**

**2026-11-01, the attach-time bytecode landed.** The record-level recompile that
paragraph said was missing now exists, so the emission is gated on the session
flag and a normal run pays nothing:

- `EcmaFunction` carries `compile_name: Option<AtomId>`, the interned form of the
  name `compile_body` was given at creation. It is a compile *input* (`Cut
  46`/`47` compare the body's own name against a tail call's callee), so a
  recompile without it would silently lose the self-tail-call fusion. Eight
  bytes on the record; the 200 to 152 shrink measured flat, so the record's size
  is not where its cost lives.
- `function::recompile_record` recompiles one record from its own state plus that
  name and installs it through `set_compiled`, which recomputes `leaf_inline` and
  `construct_inline`. It does not re-store a constructor's `this_writes`: the
  pattern is a function of the same source.
- `Agent::set_debug_attached` clears the two bytecode caches, then recompiles
  every registered record, and returns `Result` because a compile can fail. The
  caches recompile lazily on next use; a record has no such path, which is why
  that half is eager. The cost is one compile per registered function, on a
  transition a human triggered.
- An old body is never freed under the JIT cache: `Entry::body` is a strong `Rc`,
  so a body the cache still lists outlives the record's reference and the entry
  can still clear its fast cell on eviction.

Measured after the change with the same-binary seam (A = default, B =
`SLAG_DEBUG_POINTS=1`, which is what a session compiles), full corpus, one
process per row, per-row minima: compiled tier **1076.0 against 1078.8 ms
(-0.26%)**, no row outside the noise floor, and `--jitless` **3482.9 against
3531.8 ms (-1.38%)**. The +1.11% is gone from a normal run and comes back under
a session, which is the point.

One gap was still open at that point, and is closed by the next paragraph: the
per-loop-back-edge point, which now had the mechanism to be free when no session
is attached. The attach's own cost stays unmeasured beyond "one compile per
registered function".

**2026-11-01, the back-edge point landed.** R2 is now complete: `compile_statement`
emits one point per statement, and each loop form emits one at its back edge,
through a single `emit_debug_point` gate. The five shapes are the four statement
loops (`while` before its jump, `do`-`while` before its `jump_if_true`, `for`
before its jump, and `for`'s two fused paths before `FastLoopHead`) and the
certified `for`-in/`for`-of loop, whose back edge is not a jump at all: it re-runs
the protocol step at the bottom, so the point goes immediately before that fetch
(after the per-iteration env update, which keeps it the fetch's immediate
predecessor on both the fall-through and the `continue` path). `async` for-of has
the same bottom `Jump` shape and is covered by the same site.

The one trap this hit, and the reason it was not a two-line change: the fused
loop's planners (`plan_loop_num`, `plan_loop_int`, `plan_builder`) gate on
`head == body_start + 1`, i.e. the body is exactly one `RunRegBody`. A point
emitted before `FastLoopHead` sits inside that range, so every one of them would
have declined and the loop would have silently lost its int32/number/string-
builder lane *when a session was attached* (the one configuration where the
bytecode differs). They now ask `body_is_sole_run`, which accepts trailing steps
that emit nothing.

The cost is zero in the default configuration, because no point is emitted
without a session; the session path pays one dispatch per iteration and is a
debug build. `every_loop_back_edge_carries_a_debug_point` asserts the emission
structurally (the step before each backward branch, in each of the seven loop
shapes) rather than by count, and `a_loop_runs_correctly_with_a_session_attached`
runs a `continue`-heavy loop with a session to cover the interpreter path.

**2026-11-01, scoping the pause itself.** With both emission rules in place, the
next slice is `VmOutcome::Paused` and the `PausedRun` record, and reading the
driver first changed what that slice looks like. Two facts decide it:

- **A pause is an outcome, never an error.** Routing it through the step error
  path would run the enclosing `finally` blocks and `catch` handlers, which is a
  different program. So it has to propagate as a value from
  `run_inner_inner` the whole way out, and every layer that can swallow, convert
  or continue on a value must be told about it.
- **There are four places that own a `Vm`.** `eval_program` and three sites in
  `function.rs` (`ordinary_call`'s fast and slow paths, and the construct path)
  each do `take_vm` / run / `return_vm`. A pause in a nested call therefore has
  to unwind through each of those owners, and each has to park *its* `Vm`
  instead of returning it to the pool. So the record is a stack of paused runs,
  not one: `PausedRun` holds the innermost `Vm` and each enclosing frame's
  `Vm` above it, in order, until the host entry that the session is attached to.
  Resume re-enters the outermost correctly-sized layer and each parked `Vm`
  continues at the step that withdrew it.

The propagation surface is therefore the *return* of every `run` call inside a
call step (the in-place path and the withdrawn `VmOutcome::Call` path), the
leaf lane's `run_leaf`, and the JIT's compiled callee lane, which cannot return
a pause at all because a session refuses compilation. That last point is the one
piece of good news: `lookup_info_for` already guarantees no compiled body is
running under a session, so the pause protocol does not need a JIT-side deopt.

Build order, once the slice starts: (1) `PauseReason` and the outcome variant,
with every `match` on `VmOutcome` updated and a test that a `pause now` request
reaches a `try`-wrapped loop *without* running its `finally`; (2)
`PausedRun`/`Vm::paused` plumbing through the four owners; (3) resume and the
host re-entry through the `CallbackWindow`; (4) breakpoints against the step
spans the emission now carries.

**2026-11-01, step 1 landed.** `VmOutcome::Paused(PauseReason)` exists, the
`DebugCheck` arm observes a pending `Agent::pause_requested` and returns it, and
all ten `match` sites on `VmOutcome` handle the variant explicitly (script,
`ordinary_call`'s two paths, the construct path, module evaluation, generator and
async-generator drivers, the async-await driver's two sites, and `run_leaf_body`).
Nothing is parked yet, so every site reports `pause_without_run` instead of
letting the run continue; `Agent::request_pause` is `#[cfg(test)]` until step 2
gives that error somewhere real to put the state.

`a_pause_request_stops_the_run_without_running_the_enclosing_finally` is the test
that decides the shape: a host function asks for a pause, and the script
`try { pauseNow(); ran = true; } finally { fin = true; }` must leave *both*
assignments unrun. If the pause travelled as an error, `fin` would be true.

Two findings from wiring it. First, the `DebugCheck` arm comes before any step
that can run user code, but a pause requested *inside* a register run (a fused
loop body, where the point is transparent to the lowering) is not observed until
the run ends and the next step-path point is reached. That is a property of the
register lane, not a bug in the point, and it is worth a run-boundary check when
stepping needs an exact position. Second, the leaf lane `run_leaf_body` can now
see a paused outcome, which is why its arm is explicit: a leaf body carries
points like any other body.

**2026-11-01, step 2a: the run parks.** `PausedRun { vm, body, reason }` lives on
`AgentRoots` (traced with the rest: a parked `Vm` holds frame slots and stack
entries like a running one), `Agent::take_paused_run` hands it out, and the
script/eval boundary parks instead of returning the `Vm` to the pool. The entry
points still report an error (their signature is `Result<Value, JsError>`), but
that error now means "ask the agent for the run", which is why it has its own
`paused_run_error` and a message a host can rely on.

What made this step small is one deliberate restriction, and it is the shape of
the next step: **a pause is honoured only at the outermost activation**
(`Agent::vm_depth`, incremented by `Vm::run_inner`). A nested activation cannot
be parked without parking every frame above it, and the channel from a callee's
run back to its caller's call step is `Result<Value, JsError>`, which cannot
carry an outcome. The alternative, an error sentinel unwound past the `catch`
handlers, is what the termination flag does today (`execution terminated` is
catchable, unlike V8's) and is exactly the behaviour the pause exists to avoid.
So a request raised inside a nested call waits for the outermost body's next
point: `a_pause_requested_inside_a_nested_call_stops_at_the_outermost_point`
pins that (the nested body finishes; the statement after the call does not run).

Consequence to keep in mind for A5: a breakpoint inside a called function will
not stop until control returns to the outermost frame, which is not a usable
debugger. Lifting the restriction is the continuation-stack work (park each frame
on the way out and resume innermost-first, feeding each completion back into the
frame below), and it starts with the call-step channel in `ordinary_call`.

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
