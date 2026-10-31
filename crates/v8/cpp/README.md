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

### Named-property handlers

`ObjectTemplate::SetHandler` installs a `NamedPropertyHandlerConfiguration` on a
template, and `NewInstance` then makes an object whose property access is offered
to the handler first. All seven of V8's callbacks are carried: `getter`,
`setter`, `query`, `deleter`, `enumerator`, `descriptor`, and `definer`. Each
returns V8's `Intercepted`, and `kNo` runs the engine's own internal method, so a
handler can answer one name and let the rest through.

A template's handler makes its instances `crux::host::HostOps` objects, one
behaviour per template, with the configuration's `data` value pinned for as long
as an instance can call it. A handler is consulted *before* the object's own
lookup, which is V8's masking default; there is no non-masking mode.

The `enumerator` answers `[[OwnPropertyKeys]]` with the array of keys the host
owns, and the two configurations' answers are concatenated, the indexed one
first (V8's order: element indices before string keys). A number element becomes
an array-index key and a string or symbol is itself, which is the reading V8's
`KeepNumbers` describes. An enumerator that was never installed, or that answers
`kNo`, adds nothing.

A handler callback may allocate and may read. The operation that called it holds
the context, so the ordinary entry points refuse re-entry; instead the facade
publishes the callback on `PropertyCallbackInfo::token()`, and `Object::New`,
`Array::New`, `Object::Get`, `Object::Has`, `Object::Set`, `Object::Delete`,
`Object::GetRealNamedProperty`, and the real-property predicates route through it
while they are called on behalf of a running handler. That is what lets a getter
answer with an object it built, a descriptor answer with its own descriptor, an
enumerator answer with its array, and an accessor read another property of its
holder, including the own-property forms.

`SetHandler` also takes an `IndexedPropertyHandlerConfiguration`: a key that is
an array index goes to those callbacks when they are installed, and to the named
handler otherwise, which is V8's routing rule. It carries the same seven
callbacks.

`gin-shapes` installs a handler that owns three names through its query, answers
one through its getter and declines the rest, takes a set and a delete, and
answers `Object.getOwnPropertyDescriptor` from a `descriptor` callback that
builds the descriptor object inside the callback. A define reaches the `definer`,
which sees the descriptor marshalled back into `v8::PropertyDescriptor`. A getter
for `fresh` returns `Object::New` with a property set on it, and one for `list`
returns `Array::New` over two strings; both are built during the callback. The
same answers are repeated through C++ (`Get`/`Has`/`Set`/`Delete`) and through a
script, where `in` and a member read both reach the handler. `Object.keys`
walks the enumerator's keys, and the indexed handler's enumerator is read back
through `Reflect.ownKeys`.

### Accessors

`ObjectTemplate::SetAccessor` and `Object::SetAccessor` install an accessor whose
getter and setter are the host's callbacks (`v8::AccessorNameGetterCallback` and
`v8::AccessorNameSetterCallback`). Each callback receives the property's key and
a `PropertyCallbackInfo` carrying the accessor's `data`, and the getter answers
through `GetReturnValue()`. The engine installs the pair as an own accessor
property whose get and set are host functions, so the callbacks run with the
normal host-function agent and the allocation window; a template installs one on
every instance, and the object form installs one on the object it is called on.

The specification is kept for the process: a template is never freed, and
`Object::SetAccessor` allocates one for the object's lifetime. The name and data
are rooted in the ABI's registry, which is what a `Global` does for a host.

### Errors and object identity

`TryCatch::HasCaught` asks whether the last operation left an error, and
`TryCatch::Exception` returns the thrown language value when there is one: a
script's `throw` keeps its value across the boundary, so `catch`-style host code
sees the object that was thrown rather than a message rebuilt from it. A host
`ThrowError` records a message, so its text is what comes back. `Message` and
`StackTrace` are still empty (see the limits below).

`Object::GetIdentityHash` answers the engine's id for the object, mixed so
neighbouring objects do not collide and never zero. The Rust bridge's
`GetIdentityHash` uses the same function, and both agree with handle equality:
two handles that compare equal hash equal.

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
- `TryCatch`, `Isolate::ThrowError`, and a minimal `Message` (empty, so the
  sample prints the exception rather than a source line). `TryCatch::Exception`
  returns the thrown language value.
- `ObjectTemplate`/`FunctionTemplate` and host callbacks with arguments and a
  return value.
- `Global<T>`, `Maybe<T>`, `MaybeLocal<T>`.
- `ObjectTemplate::SetInternalFieldCount`/`NewInstance`, internal fields, and
  `Object::Wrappable<T>`, `IsApiWrapper`, `This()`.
- `Object::Get`/`Set` by a key value, through the engine's ToPropertyKey (so a
  numeric or object key names the property the language would), with the
  `uint32_t` index forms built on them. `Has` (own property plus the chain),
  `Delete`, `GetRealNamedProperty` (own property only, no chain), the
  own-property predicates `HasRealNamedProperty`,
  `HasRealIndexedProperty`, and `HasRealNamedCallbackProperty`, and
  `GetIdentityHash`.
- `ObjectTemplate::SetHandler` with the named and indexed property handlers:
  getter, setter, query, deleter, enumerator, descriptor, and definer, plus
  `Intercepted`, `PropertyCallbackInfo<T>`, and `PropertyDescriptor`.
- `ObjectTemplate::SetAccessor` and `Object::SetAccessor` with
  `AccessorNameGetterCallback`/`AccessorNameSetterCallback`, `Object::New`, and
  `Array::New` (length and element forms).
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
- `Message` is always empty and `StackTrace` is always empty. `TryCatch::Exception`
  does return the thrown value, though: a script's `throw` carries one, so the
  object comes back as it was thrown instead of being rebuilt from a message. A
  host `ThrowError` records only a message, so its text is the fallback. The
  sample's rich path (source line, caret) is still not taken, because the message
  and stack are what it reads.
- `PumpMessageLoop` always returns false; the engine drains jobs inline.
- `Isolate::Dispose` frees the contexts the facade created; templates are not
  freed.
- Host callbacks receive `this` now (`FunctionCallbackInfo::This`), and
  `Object::GetIdentityHash` answers the engine's id for the object, mixed so
  neighbouring objects do not collide (the Rust bridge's fold is the same
  function). The rest of the `Object` surface is what V8 declares beyond it. An
  ordinary entry point cannot be re-entered from a callback, because the
  operation that called the callback holds the context; every `Object` operation
  the facade offers routes through the window `PropertyCallbackInfo::token()`
  names while one is running, and uses the ordinary entry point otherwise.
- An accessor installed with `SetAccessor` keeps its specification for the
  process: a template owns its specifications and is never freed, and
  `Object::SetAccessor` allocates one and holds it for the object's lifetime. The
  name and data are roots in the ABI's registry rather than `Global`s, because
  `Global` is defined after `ObjectTemplate` in the header.
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
