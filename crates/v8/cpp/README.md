# A0/A1: the `v8.h` facade

This is the C++ face of the `v8` crate. The Rust crate is the `v8` API bridge a
Rust host (Deno) compiles against; this directory is the source-and-binary
substrate a C++ host (Chromium) includes and links.

It is Route A's first milestones (`.notes/chromium-facade.md` §11):
V8's own `hello-world.cc` and `shell.cc` compile and run unchanged (A0), the
handles use the indirect-slot shape (A1), and a gin-shaped harness exercises the
template/context/script surface. Actual `gin` is a Chromium component and is not
standalone; see below.

## Layout

```
crates/slagc/              the Rust static library: values as u64, contexts, scripts
crates/v8/cpp/include/     the minimal headers the samples include
crates/v8/cpp/src/         the C++ implementation of those headers
crates/v8/cpp/samples/     V8's hello-world.cc and shell.cc, vendored
crates/v8/cpp/tests/       the gin-shaped harness
```

The headers are **not** a copy of V8's `include/`. They carry the subset the
samples and the harness use, with the same names and shapes. Copying V8's
headers and replacing `v8-internal.h` is the next step
(`.notes/chromium-facade.md` §4).

## Build

```
cargo build -p slagc --release
cmake -S crates/v8/cpp -B crates/v8/cpp/build -G "Visual Studio 17 2022" -A x64
cmake --build crates/v8/cpp/build --config Release
```

For a fast cycle, build the ABI in debug and point CMake at it:

```
cargo build -p slagc
cmake -S crates/v8/cpp -B crates/v8/cpp/build -G "Visual Studio 17 2022" -A x64 \
  -DSLAGC_LIB=C:/path/to/slag/target/debug/slagc.lib
cmake --build crates/v8/cpp/build --config Release
```

## Run

```
crates/v8/cpp/build/Release/hello-world.exe
crates/v8/cpp/build/Release/shell.exe -e "print(1 + 2)"
crates/v8/cpp/build/Release/shell.exe -e "print(version())"
crates/v8/cpp/build/Release/gin-shapes.exe
```

## A1: the indirect handle

`Local<T>` is now the word-sized indirect handle from V8's
`v8-handle-base.h`:

- a `Local<T>` is a `HandleSlot*` into a thread-local handle stack;
- an engine-value slot is retained in the C ABI's root registry, and a facade-pointer slot is not;
- a `HandleScope` records the stack's size and restores it on exit, so nested
  scopes release exactly the slots they allocated;
- `Global<T>` over an engine value retains it for the `Global`'s own lifetime,
  and is neither copyable nor assignable, matching V8's persistent handles;
- `Local<T>::As` reinterprets the same slot, so it does not copy the value;
- `FunctionCallbackInfo::operator[]` allocates a slot for each argument.

This is the shape V8 uses and the shape the design note argues for. A slot for
an engine value is now a real root: `Local<String>`, `Local<Value>`,
`Local<Number>`, and `Local<Object>` retain their value in `slagc`'s root
registry, and `HandleScope` releases it on exit. A slot for a facade pointer
(`Context`, `Script`, `ObjectTemplate`, `FunctionTemplate`) is not retained,
because those are C++ allocations, not engine values; registering them would
make the collector trace a pointer as a `Value`.

`gin-shapes` forces a collection (`slag_collect`) while a string is held only by
a handle slot and reads it back, so the root path is exercised through the real
boundary.

### Persistent handles

`Global<T>` is the same rule one level up: it outlives every `HandleScope`, so
the slot a scope would otherwise release cannot be what keeps its value alive.
It retains the value in `slagc`'s root registry on construction (and on
`Reset(isolate, local)`) and releases it on `Reset()` or destruction, so the
retention lives exactly as long as the `Global`.

Two consequences follow the V8 rules:

- a `Global` must be reset or destroyed before its isolate is disposed. The
  registry is thread-local, not owned by an isolate, so a leaked `Global` keeps
  naming a box that the freed heap will reuse;
- `Global<T>` is not copyable, because a copy would either release one
  retention twice or alias it. It is movable, and the move transfers the
  retention rather than taking a second one.

`Global<Context>` and `Global<Script>` hold facade pointers, not engine values,
and are not retained for the same reason those slots are not: tracing a C++
pointer as a `Value` would be unsound.

A collection cannot demonstrate this retention on its own. The conservative
stack scan reads the whole live stack region, including words left in dead C++
frames by the call that allocated the value, so a value under test can survive a
collection with nothing retaining it. The harness therefore asserts
`slag_root_count()`, a diagnostic that reports how many retentions the registry
holds: constructing the `Global` moves it by exactly one, the scope's own slot
by another, and `Reset()` back to the original count.

### Internal fields and wrappables

`ObjectTemplate::SetInternalFieldCount` and `ObjectTemplate::NewInstance` give an
object the shape gin wraps: N internal fields, the template's host functions, and
`IsApiWrapper` set. `Object::SetAlignedPointerInInternalField` and
`GetAlignedPointerFromInternalField` (in all three of V8's argument forms) store
and read the host's pointers, `Object::Wrappable<T>()` reads the wrappable from
the first field, and `FunctionCallbackInfo::This()` is what a wrappable method
eeds to recover its C++ object.

The storage is the object's own, not a table. `crux::object::JsObject` carries
one handle to a box of opaque pointers plus the API-wrapper bit, so the fields
die with the object. A table keyed by address would be unsound: the arena's free
list reuses swept slots, so a recycled address would hand a later object an
earlier object's pointer. The payload goes 424 to 432 bytes, which is the same
448-byte arena class, so an object that never carries host fields allocates
exactly as it did before.

`gin-shapes` builds the wrappable shape end to end: a template with one internal
field and a `bump` method, an instance holding a `HostCounter*`, a forced
collection with the pointer read back, and `counter.bump(10)` run from a script
so the method reaches the counter through `this`.

Two deviations, both deliberate. `CppHeapPointerTag` is accepted and not
inspected, because there is no pointer-tag check to make. `Wrappable<T>` has no
cppgc `GarbageCollectedMixin` constraint, because the facade has no cppgc.

### Traced references

`TracedReference<T>` is the root a host keeps in its own structures: the engine
holds the value for as long as the object exists, `Reset` or the destructor
releases it, and a copy is a second reference, so resetting one does not clear
the other. V8's version is traced through the embedder's collector; this facade
has no embedder-heap tracing, so the reference is a root the C++ side owns and
must release.

`gin-shapes` creates an object in a scope, marks it, takes a traced reference,
lets the scope pop, forces a collection and reads the mark back through the
reference, then releases the original and checks the copy still keeps the value
alive. The copy check was verified to fail when the copy is made empty instead.

## The gin target

Actual `gin` cannot be built standalone. `gin/BUILD.gn` has public deps
`//base`, `//base/allocator:buildflags`, and `//v8`, and deps
`//third_party/abseil-cpp:absl` and `//tools/v8_context_snapshot:buildflags`.
`gin/wrappable.h` also needs the part of `v8.h` this facade does not have yet:

- `v8::Object::Wrappable`, `v8::Object::WrapperTypeInfo`;
- `v8::CppHeapPointerTag` and the wrappable tag range from `v8-sandbox.h`;
- `v8::Object::Unwrap<T>`, `v8::Object::IsApiWrapper`;
- `v8::TracedReference<v8::Object>` and internal fields.

So "gin compiles" is one of two things:

1. a Chromium checkout with `//v8` swapped for this facade, then building
   `//gin`; or
2. a gin-shaped harness that reproduces the V8 usage patterns gin's
   arguments/converter/object_template_builder paths rely on.

A1 does (2): `tests/gin_shapes.cc` registers globals through
`ObjectTemplate`/`FunctionTemplate`, runs a script 100 times through nested
`HandleScope`s, checks `As` keeps the same slot, and converts values with
`Int32Value`/`Integer::New`. It is the compile/run gate for the handle change,
not a claim that gin itself links.

## What A0/A1 cover

- `Isolate`/`Context`/`HandleScope`/`Context::Scope` as the samples use them.
- `String::NewFromUtf8Literal`, `String::NewFromUtf8`, `String::Utf8Value`.
- `Number::New`, `Integer::New`.
- `Script::Compile` (parse only) and `Script::Run`.
- `TryCatch`, `Isolate::ThrowError`, a minimal `Message` (empty, so the sample
  prints the exception rather than a source line).
- `ObjectTemplate`/`FunctionTemplate` and host callbacks with arguments and a
  return value.
- `Global<T>`, `Maybe<T>`, `MaybeLocal<T>`.
- `ObjectTemplate::SetInternalFieldCount`/`NewInstance`, internal fields, and
  `Object::Wrappable<T>`, `IsApiWrapper`, `This()`.
- `Object::Get`/`Set` by a key value, through the engine's ToPropertyKey (so a
  numeric or object key names the property the language would), with the
  `uint32_t` index forms built on them. `Has` (own property plus the chain),
  `Delete`, and `GetRealNamedProperty` (own property only, no chain).
- `TracedReference<T>` as a host-owned root: copy is a second reference, `Reset`
  releases it, and the destructor releases it like any persistent.
- Handles and persistents as engine roots: a handle slot or a `Global` over an
  engine value retains it for that object's lifetime, and a facade pointer is
  never registered.
- WebAssembly through the engine's own `wasm` feature, which `hello-world`
  exercises.

## What it does not

- `TracedReference<T>` is a root the host owns, not an edge the embedder's
  collector traces: it never clears on its own and has no `SetWeak`.
- It is not source-compatible with the rest of `v8.h`. There are no interceptors,
  no `cppgc`, no platform tasks, no snapshots, no inspector.
- `CppHeapPointerTag` is accepted and ignored, and `Wrappable<T>()` does not
  require a cppgc mixin: the facade has no pointer tags and no cppgc.
- `Global<T>` keeps a value alive, but a `Global` leaked past `Isolate::Dispose`
  is not detected and leaves a dangling retention in the thread-local registry.
- `Message` is always empty and `StackTrace` is always empty, so the sample's
  rich error path (source line, caret) is compiled but not taken. An uncaught
  `Error` therefore prints the engine's generic `TypeError: Uncaught exception`
  rather than the thrown message; the ABI does not yet expose the thrown value.
- `PumpMessageLoop` always returns false; the engine drains jobs inline.
- `Isolate::Dispose` frees the contexts the facade created; templates are not
  freed.
- Host callbacks receive `this` now (`FunctionCallbackInfo::This`), but the
  `Object` surface is narrow: the other own-property predicates
  (`HasRealNamedProperty`, `HasRealIndexedProperty`, `HasRealNamedCallbackProperty`),
  the accessor/internal-field `SetAccessor`, and `GetIdentityHash`'s siblings are
  absent.
- `load()` is compiled but refused at run time. The sample re-enters through
  `ExecuteString`, and the safe engine path for a host callback is
  `FunctionCall::eval` (`slag_call_eval`), which the sample does not call. The
  ABI turns that re-entry into a pending error rather than aliasing the context
  (the `RefCell` in `crates/slagc`). `read()` works; it does not re-enter.

## Validated

Built with the Rust debug staticlib and MSVC 19.44 (Visual Studio 17 2022):

```
hello-world.exe
Hello, World!
3 + 4 = 7

gin-shapes.exe
30
(exit 0)

shell.exe -e "print(1 + 2)"
3

shell.exe -e "print(version())"
slag-a0

shell.exe -e "print('a', 'b')"
a b
```

The `3 + 4 = 7` line is the sample's WebAssembly half, running through the
engine's `wasm` feature.
