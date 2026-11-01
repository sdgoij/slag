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
`ThrowError` records a message, so its text is what comes back.

`TryCatch::Message` answers the position a **compile** error was found at: the
script's name (the `ScriptOrigin` the host passed to `Script::Compile`), the
1-based line, the 0-based start column, the line's text, and the message text.
That is what makes the sample take its rich path for a syntax error. A runtime
error has no position, so its message is empty and the host prints the exception
alone: `JsError::span` indexes whichever source the failing code was parsed from
and does not say which, so recording it against the running script would put a
wrong position on the error. This is the narrowing the Rust bridge documents in
its own `position` module.

`TryCatch::StackTrace` returns the V8-style stack the engine captured for a
thrown `Error`, when it has one, as a string. `String::Utf8Value` runs ToString,
so a host that prints `Exception()` reads `Error: boom` rather than a tag. The
three reads (`Exception`, `Message`, `StackTrace`) do not consume the pending
error; the `TryCatch` destructor clears it, as V8's resets the isolate's.

`Object::GetIdentityHash` answers the engine's id for the object, mixed so
neighbouring objects do not collide and never zero. The Rust bridge's
`GetIdentityHash` uses the same function, and both agree with handle equality:
two handles that compare equal hash equal.

### Value predicates and conversions

`Value` carries V8's type predicates: `IsUndefined`/`IsNull` to
`IsArray`/`IsProxy`/`IsTypedArray`, plus `BooleanValue`, `TypeOf`, `ToString`,
`ToNumber`, `ToObject`, `StrictEquals`, and `SameValue`. `Object::IsCallable` and
`Object::IsConstructor` sit beside them.

The shape bits come from the value itself, so they answer anywhere. The brands
the engine records in per-object tables (`IsDate`, `IsRegExp`, `IsPromise`,
`IsMap`, `IsSet`, `IsNativeError`, `IsArrayBuffer`) need the agent, which the
entered `Context::Scope` owns, so V8's rule applies: a brand asked outside any
context answers false rather than crashing. `IsConstructor` also needs the
agent, because the `[[Construct]]` slots that make an arrow, a method, or an
async/generator function non-constructible live in the function record, not in
the value's tag.

`ToString`/`ToNumber`/`ToObject` run the object's own `toString`/`valueOf`, so
they take a `Local<Context>`; an empty result leaves the error pending. `TypeOf`
answers a string value (`typeof null` is `"object"`). `StrictEquals` is `===`
(NaN differs from itself) and `SameValue` is `Object.is` (NaN equals itself,
`+0` and `-0` differ).

`Int32Value`, `Uint32Value`, and `IntegerValue` convert with ToNumber first and
then wrap the number the way ToInt32/ToUint32/ToIntegerOrInfinity do, so
`4294967297` is `1`, `-1.9` is `-1`, and `Infinity` saturates at the `int64`
ends. Inside a host callback the engine is borrowed, so they take the same
route `Object::New` does: the callback's window runs the ToNumber (see the
README's `CallbackToken` note under accessors).

### Strings

String construction is `NewFromUtf8`, `NewFromUtf8Literal`, `NewFromOneByte`
(Latin-1: each byte is one code unit, so `0xE9` is U+00E9), and `NewFromTwoByte`.
The façade copies two-byte input; there is no external-string store to keep a
host buffer alive, unlike V8's `NewExternalTwoByte`.

`Length` is the UTF-16 code unit count, so an astral character counts two, and
`Utf8Length` is the UTF-8 byte count. `ContainsOnlyOneByte` says whether every
unit fits in one byte. The `Write`/`WriteOneByte`/`WriteUtf8` family copies a
window of the string into a host buffer: `start` is a code unit index (negative
counts from the end), `length` a unit or byte count (`-1` for the rest), and the
answer is the number of units written. A NUL is appended unless
`NO_NULL_TERMINATION` is passed, and `WriteUtf8` counts the terminator in its
answer the way the Rust bridge does, reporting the covered code units through
`nchars_ref`. `WriteUtf8` writes whole characters only, and without
`REPLACE_INVALID_UTF8` a lone surrogate is written as the three bytes of its own
code point (not valid UTF-8); `Utf8Length` counts it as three.

`String::Utf8Value` runs ToString, so it converts an object through its own
`toString` and reads the form that fits in one byte when it does.

### Promises

`Promise::State`, `Result`, `MarkAsHandled`, `Then`, `Then2`, and `Catch` are the
host side of a promise, and `Promise::Resolver::New`, `GetPromise`, `Resolve`, and
`Reject` are the settling side. `Then`/`Then2`/`Catch` run the engine's own
PerformPromiseThen and `Resolver::New` runs NewPromiseCapability over the realm's
`%Promise%`, so a script that replaced `Promise.prototype.then` or the `Promise`
binding cannot change what a host's call does. A handler a host did not give is
*undefined*, which the engine reads as "no handler".

`Resolve`/`Reject` answer whether *that* call settled the promise: the pair
shares one `[[AlreadyResolved]]` flag, so a second call is a no-op and answers
false. `State` and `Result` read the engine's Promise Record, and a pending
promise's `Result` is *undefined*. V8 gives those three no context, so the facade
reads the entered one, as V8 does.

The resolver handle *is* the pair's resolving function, as in V8, and the facade
keeps the rejecting one against it: the engine names the pair by function
identity but holds no reference to the rejecting value. A host callback can do
all of it (`Promise::Resolver::New` and `Resolve` from an accessor), because the
operations take the callback's window when one is running.

### Maps and sets

`Map` (`New`, `Set`, `Get`, `Has`, `Delete`, `Clear`, `Size`, `AsArray`) and `Set`
(`New`, `Add`, `Has`, `Delete`, `Clear`, `Size`, `AsArray`) run the engine's own
`%Map.prototype.*%` and `%Set.prototype.*%` intrinsics, so a script that replaced
`Map.prototype.set` cannot change what a host's call does. `Size` and `AsArray`
read the internal `[[MapData]]`/`[[SetData]]` lists directly: `Size` counts the
live slots, and `AsArray` answers V8's flat form, `[key, value, ...]` for a map
and the elements for a set. A host's map is an ordinary JS `Map`, so a script
sees the same entries (`hostMap.size`, `hostMap.get(k)`), which the harness
checks.

These take the ordinary route only: a `Map`/`Set` call runs an engine intrinsic,
which a host callback cannot do while it holds the engine, so the surface refuses
the re-entry rather than racing it. `Size` on a handle that is not a map answers
-1, as V8's does.

### Errors, private names, and data properties

`Exception::Error`, `TypeError`, `RangeError`, `ReferenceError`,
`SyntaxError`, `EvalError`, and `UriError` make the engine's own error objects
from a message, so a script catches an object with the usual `name`, `message`,
and stack. `Isolate::ThrowException(value)` makes any value the pending
exception, which is what `TryCatch::Exception` hands back.
`Exception::CreateMessage` answers an empty handle: a position exists only for a
compile error (see above), and no runtime factory produces one.

`Private::ForApi` mints one name per description for the isolate, `Private::New`
mints a fresh name per call, and `Private::Name` reads the description back.
`Object::GetPrivate`/`SetPrivate`/`HasPrivate`/`DeletePrivate` are the ordinary
key paths with the private value as the key. The engine models a private name as
a symbol, so the property is hidden from `Object.keys`, `for-in`, and `in` but
not from `Object.getOwnPropertySymbols`; that is the same half-step the Rust
bridge documents, and the harness pins it.

`ObjectTemplate::SetNativeDataProperty` and `SetLazyDataProperty` install the
accessor `SetAccessor` installs: the engine has no one-shot slot, so a lazy
getter runs on every read rather than once and the property is an accessor
rather than a data slot. `Object::SetAccessorProperty` installs an accessor
whose get and set are `FunctionTemplate`s; each runs with the receiver as `this`
and (for the setter) the assigned value as the first argument. The
`PropertyAttribute` bits are honoured: `DontEnum` and `DontDelete` reach the
descriptor, and `ReadOnly` installs no setter.

### Embedder data, security tokens, and creation context

`Context::SetEmbedderData`/`GetEmbedderData` and
`SetAlignedPointerInEmbedderData`/`GetAlignedPointerFromEmbedderData` are the
slots a host attaches to a realm, with `GetNumberOfEmbedderDataFields` and
`ClearAllSlots`. A value slot is a root, so replacing one releases the old root
and `ClearAllSlots` (or the context's destructor) releases them all; a pointer
slot is the host's address and is not traced. The two stores are separate, and
the Rust bridge keeps only the pointer form.

`Context::GetSecurityToken` answers the context's token, or the isolate's
default (one null-prototype object, minted on the first ask and shared by every
context without one) when none was set. `SetSecurityToken` replaces it and
`UseDefaultSecurityToken` goes back. The token is stored and not consulted: the
engine has no cross-context access check, which is the gap the Rust bridge
documents. `Object::GetCreationContext`/`GetCreationContextChecked` answer the
entered context, because the engine does not record a holder's realm on each
object and the facade has one realm per entered context.

### Weak handles, GC hooks, and heap statistics

`Global<T>::SetWeak` turns a persistent into a weak handle: the strong retention
is released, `Get` reads the live value from the engine and answers empty once a
collection has taken it, and the callback runs after that collection with a
`WeakCallbackInfo<P>` (the template parameter is the type of the pointer
`SetWeak` was given, as in V8). `GetParameter` is that pointer; `GetValue` is
empty because the value is already gone. `SetSecondPassCallback` registers a
guaranteed finalizer that runs after every first pass of the same drain, even if
the first pass released the handle. `ClearWeak` takes the value back under a
strong hold while it is alive, `IsWeak` and `SetWrapperClassId`
/`WrapperClassId` complete the persistent surface.

One detail is load-bearing: `SetWeak` clears the `Global`'s own value word as
well as releasing the root. A `Global` is usually a local, and the engine's
conservative stack scan would read that word as a root and keep the value alive,
so the weak handle would never empty. `Get` reads the engine's live bits
instead.

`AddGCPrologueCallback`/`AddGCEpilogueCallback` (and their removals) sit on the
engine's collection observer, so they run around every collection the agent
drives, including the ones its growth policy starts. The callback receives the
generation (`kGCTypeScavenge` or `kGCTypeMarkSweepCompact`) and
`kNoGCCallbackFlags`. `RequestGarbageCollectionForTesting` and
`LowMemoryNotification` collect now; the engine has one full collection, so
`kMinorGarbageCollection` runs that one. The engine also drains the death
callbacks the sweep queued, so a weak finalizer has run when the request
returns.

`Isolate::GetHeapStatistics` fills V8's struct from the arena walk and the live
buffer record: the four measured numbers are real, `heap_size_limit` is the
largest `size_t` (the arena has no limit), the executable/global-handle numbers
are zero because those stores do not exist, and `embedded_blob_size` is zero.

### Function templates and prototypes

`FunctionTemplate::New` takes V8's full option list (data, signature, length,
`ConstructorBehavior`, `SideEffectType`); the data reaches a callback as
`FunctionCallbackInfo::Data`, and `SetCallHandler` replaces it. `GetFunction`
materializes a function whose `new` builds an instance with the instance
template's internal-field count and API-wrapper bit, inheriting from the built
prototype object. `PrototypeTemplate`'s members land on that prototype, and
`Inherit(parent)` chains to the parent's built prototype, so an instance reaches
the parent's methods. `ConstructorBehavior::kThrow` really does materialize a
function with no `[[Construct]]`.

`Function::New`/`Call`/`NewInstance` are the direct function forms, `Signature`
and `AccessorSignature` are the declaration types an access check is written
over (accepted and not consulted, because the engine has no embedder access
checks), and `Object::GetPrototype`/`SetPrototype` are the engine's own internal
method, so a cycle is refused rather than installed. `Isolate::GetCurrent` and
`TryGetCurrent` answer the entered isolate. `MicrotasksScope` drains the queue
when its outermost `kRunMicrotasks` scope exits, and `Locker`/`Unlocker` are
accepted: the engine is single-threaded per isolate.

A host function callback runs with its window published, so `Object::Set` (and
the rest of the property surface) inside it takes the token route instead of
recording a re-entrant refusal. That was the one engine-side fix this rung
needed, and it is what the constructor in the harness exercises.

### Modules

`ScriptCompiler::CompileModule` parses a `Source` as a module in the entered
realm, with the source's resource name as the module's specifier and the name a
stack frame reports. The returned `Module` answers `GetStatus`, `GetException`,
`GetModuleNamespace`, `GetIdentityHash`, `GetModuleRequestsLength`,
`GetModuleRequest`, `IsSourceTextModule`/`IsSyntheticModule`, and
`SourceOffsetToLocation` (0-based, the same conversion the Rust bridge's
`Location` does).

`Module::InstantiateModule(context, resolve)` walks the module graph: it asks
the host's `ResolveModuleCallback` for each request, including the requests a
resolved dependency declares, registers each answer under its specifier in the
engine's module registry, and then links. A resolver that returns an empty
handle leaves the error pending. `Evaluate` answers the evaluation promise, and
the run drains the job queues so a synchronous body is `kEvaluated` when it
returns. Modules are pinned records and are never freed, like scripts and
templates.

### Snapshots

`SnapshotCreator` owns an isolate, the host builds the default context in it and
calls `SetDefaultContext`, and `CreateBlob` serializes the realm's globals into a
`StartupData` the caller frees with `delete[]`. An `Isolate::CreateParams`
`snapshot_blob` makes the new isolate's first `Context::New` restore that blob
onto its global, which is V8's default-context rule mapped onto this engine's
one-realm-per-context model.

A blob carries the realm's global bindings and nothing else, and the writer
refuses a realm that still holds host callbacks: `Context::New` installs
`console` and the timers, so a host that wants a snapshot deletes them first
(the reading context puts its own back). That is the same limitation the Rust
bridge's `write_snapshot` documents, and the harness exercises exactly that
sequence.

### WebAssembly

`WasmModuleObject::Compile(isolate, MemorySpan<const uint8_t>)` decodes and
validates wire bytes into the object `WebAssembly.Module` answers, so a script's
`new WebAssembly.Instance(module)` accepts what the host compiled.
`GetCompiledModule` answers the decoded module as a `CompiledWasmModule` (a
move-only owner of the engine's plain-data module, `is_null()` until read), and
`FromCompiledModule` makes another module object from it, so a host can carry a
compiled module between objects or isolates. `MemorySpan<T>` is the minimal
`v8::MemorySpan` shape.

`WasmInstanceObject` reads what a script's `WebAssembly.Instance` built:
`IsWasmInstanceObject` (the engine's per-instance exports record is the brand),
`GetExportedFunction` by name and by export order, `GetFunctionIndex` (the
engine's own index for the exported wrapper), and `GetMemoryObject` (the first
exported memory, or empty). `WasmMemoryObject` answers `IsWasmMemoryObject`,
`Buffer` (the engine's own `buffer` getter, so a grown memory materializes the
current buffer), and `Size`.

### Buffers and typed arrays

`BackingStore` is a handle to the engine's refcounted byte block, so a store
taken from a buffer keeps the bytes alive after the object naming them is gone.
`ArrayBuffer::New(Isolate*, size_t)` makes a zeroed buffer, `New(isolate, store)`
wraps a store, and `NewBackingStore` makes a fresh store or one over memory the
host owns with a `BackingStoreDeleterCallback`. `ByteLength`, `Data`,
`GetBackingStore`, `Detach`, `IsDetachable`, `WasDetached`, and `SetDetachKey`
are the buffer's own surface; `SharedArrayBuffer` repeats the creation and read
forms. The engine validates a typed array's geometry in `Uint8Array::New` and
its eleven siblings, so a misaligned offset or a range past the end leaves the
RangeError pending and answers an empty handle. `DataView::New` is the same
shape for the untyped view; its geometry arrives from the engine's DataView
record, which the view accessors read.

The bytes are shared, not copied: a host writing through `Data()` or a store is
what a script reading a view sees, and the reverse. `ArrayBufferView` gives
`ByteLength`, `ByteOffset`, `HasBuffer`, and `Buffer` (`HasBuffer` is false for
a view over a `SharedArrayBuffer`, matching V8), and `TypedArray::Length` is the
element count. `IsDataView`, `IsArrayBuffer`, and `IsSharedArrayBuffer` join the
value predicates.

`Isolate::AdjustAmountOfExternalAllocatedMemory` adds the host's account to the
engine's walk of live buffer bytes and answers the total. `ExternalMemoryAccounter`
is the scoped form. As the Rust bridge documents, the number is reported rather
than consulted: this engine's collections are paced by its own growth policy, so
an adjustment does not trigger one.

`ArrayBuffer::Allocator` is the host's own source for those bytes.
`Isolate::CreateParams::array_buffer_allocator` is kept by the isolate, and
`ArrayBuffer::New(isolate, length)` and `NewBackingStore(isolate, length)` draw a
zeroed block from `Allocate` and hand it back through `Free` once the last buffer
and store over it are gone, so a host that counts or partitions its memory sees
the bytes. `AllocateUninitialized` and `Reallocate` default to `Allocate` and
copy-and-free. An isolate without an allocator falls back to the engine's own
block, which is what V8 refuses to do; the facade is lenient there because the
engine can allocate. A store made by
`NewBackingStore(data, length, deleter, deleter_data)` is the host's own address
range: its deleter releases it, and its bytes stay out of the isolate's external
memory, which is V8's rule too.

### Signatures and access checks

`Signature::New(isolate, function_template)` declares the type a method may run
on, and `FunctionTemplate::New` takes it: a method installed with one only runs
when its receiver is an instance of that template, which the engine answers by
walking the receiver's prototype chain (starting at the receiver's prototype, so
a prototype is not an instance of itself). Any other receiver fails the call with
V8's `TypeError: Illegal invocation` and the handler does not run.

The check needs the template's *built* prototype, so `BuildPrototypeObject` now
builds once and keeps the object (rooted for the template's life): every instance
of one template shares a prototype, which is what makes the check an identity
walk, and it is also what V8's `FunctionTemplate` does. A signature over a
template whose function was never materialized admits nothing, since no instance
of it can exist.

The check reads the chain through the callback's window when a trampoline is
running, so it works inside a host callback. The same check is the only one a
receiver-type declaration makes: `AdmitsReceiver` in `facade.cc` is called by the
call trampoline, by both accessor trampolines and by the `SetAccessorProperty`
trampolines, so an accessor and a method cannot disagree about what an instance
is.

`AccessorSignature::New(isolate, template)` declares the same thing for an
accessor. `ObjectTemplate::SetAccessor` and `Object::SetAccessor` take it as an
overload (the signature sits before the attribute and has no default, so an
ordinary call cannot land on it), and a getter or setter whose `this` is not an
instance fails with `Illegal invocation` without running the host callback.
`Object::SetAccessorProperty` needs no signature argument: its getter and setter
are function templates, so a `Signature` declared on either is enforced through
the template's own `AllowsReceiver`.

### Per-object access checks

`ObjectTemplate::SetAccessCheckCallback(isolate, callback, data)` and
`SetAccessCheckCallbackAndHandler(isolate, callback, configuration)` install the
check Blink puts on a cross-origin wrapper. The callback is V8's shape,
`bool(Local<Context> accessing, Local<Object> accessed, Local<Value> data)`, and
it is consulted only for a **cross-realm** access, which is where V8 consults it
too: a same-realm read of the same object never reaches it. A refused access
fails with V8's `TypeError: Illegal access`.

An instance of a checked template is an engine **host object**, not an ordinary
one. That is the whole reason the check is enforced: the engine's compiled
member and element lanes serve plain objects straight from a cached value cell,
so an ordinary object reached from another realm could be read before any
runtime hook ran. A host object never takes those lanes (`is_plain_object` fails,
the interpreter's own fast path is `Ordinary | Array` only), so `[[Get]]`,
`[[Set]]`, `[[HasProperty]]`, `[[Delete]]`, `[[GetOwnProperty]]`,
`[[DefineOwnProperty]]` and `[[OwnPropertyKeys]]` all arrive at the handler, and
every one of them runs the check first.

The realm compared against is the engine's current one, which follows
`v8::Context::Scope` as well as the script that is running, so a host-side access
from C++ between scripts is checked against the context it was made in. The
engine side is `slag_value_realm` / `slag_value_same_realm` and
`slag_context_realm`, and every object records the realm it was created in (see
`.notes/chromium-facade.md` section 14.2).

What is still missing from the A4 section: the security token (stored, and now
comparable, but nothing consults it) and a signature's check moving behind the
handler rather than staying in the trampolines.

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
- `String::NewFromUtf8Literal`, `String::NewFromUtf8`, `String::NewFromOneByte`,
  `String::NewFromTwoByte`, `String::Utf8Value`, `Length`, `Utf8Length`,
  `ContainsOnlyOneByte`, and the `Write`/`WriteOneByte`/`WriteUtf8` forms.
- `Number::New`, `Integer::New`.
- `Script::Compile` (parse only) and `Script::Run`.
- `TryCatch`, `Isolate::ThrowError`, `Message` (a compile error's position), and
  `TryCatch::StackTrace` (the captured stack of a thrown `Error`).
  `TryCatch::Exception` returns the thrown language value.
- `Isolate::SetMicrotasksPolicy` / `PerformMicrotaskCheckpoint` and
  `platform::PumpMessageLoop`, over the engine's job queues.
- `ObjectTemplate`/`FunctionTemplate` and host callbacks with arguments and a
  return value. `SetCallAsFunctionHandler` makes the instance callable.
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
- The `Value` predicates and conversions: `IsUndefined` through `IsNativeError`,
  `Object::IsCallable`/`IsConstructor`, `BooleanValue`, `TypeOf`, `ToString`,
  `ToNumber`, `ToObject`, `Int32Value`, `Uint32Value`, `IntegerValue`,
  `StrictEquals`, `SameValue`, and `Local::operator==`.
- `Promise` (`State`, `Result`, `MarkAsHandled`, `Then`, `Then2`, `Catch`, static
  `Resolve` and `Reject`) and `Promise::Resolver` (`New`, `GetPromise`, `Resolve`,
  `Reject`).
- `Map` and `Set`: `New`, the keyed operations, `Clear`, `Size`, and `AsArray`.
- `ArrayBuffer` and `SharedArrayBuffer` (creation, `ByteLength`, `Data`,
  `GetBackingStore`, and the detach protocol), `BackingStore` (fresh and
  host-owned, with a deleter), `ArrayBuffer::Allocator` (the host's bytes, from
  `Isolate::CreateParams`), `DataView::New`, and the
  `ArrayBufferView`/`TypedArray` hierarchy with `New` for all twelve element
  kinds.
- `Isolate::AdjustAmountOfExternalAllocatedMemory` and
  `ExternalMemoryAccounter`, over the engine's live-buffer walk plus the host's
  account.
- `Exception::Error` and its typed siblings, `Isolate::ThrowException`, and
  `Exception::CreateMessage` (empty for a runtime value). `Private::ForApi`,
  `New`, and `Name`, with `Object::GetPrivate`/`SetPrivate`/`HasPrivate`/
  `DeletePrivate`.
- `PropertyAttribute`, `ObjectTemplate::SetNativeDataProperty` and
  `SetLazyDataProperty`, and `Object::SetAccessorProperty` over
  `FunctionTemplate` getters and setters.
- `Signature` on `FunctionTemplate::New`, enforced on the receiver (the method
  only runs on an instance of the named template).
- `Context::GetIsolate`, and `Isolate::GetEnteredContext` /
  `GetEnteredOrMicrotaskContext` (the entered `Context::Scope`).
- Embedder data (value and aligned-pointer slots, the field count, and
  `ClearAllSlots`), security tokens (get, set, and use-default), and
  `Object::GetCreationContext`/`GetCreationContextChecked`.
- `Global<T>::SetWeak` with `WeakCallbackInfo`, the second-pass callback, and
  `ClearWeak`/`IsWeak`; `AddGC*Callback` and their removals;
  `RequestGarbageCollectionForTesting` and `LowMemoryNotification`;
  `Isolate::GetHeapStatistics` and `HeapStatistics`.
- `Source`/`ScriptCompiler::CompileModule` and the `Module` surface: status,
  exception, namespace, requests, identity hash, kind predicates, the host
  resolver through `InstantiateModule`, `Evaluate`, and `SourceOffsetToLocation`.
  - `WasmModuleObject::Compile`, `GetCompiledModule`, `FromCompiledModule`, and
    `CompiledWasmModule`, over `MemorySpan<const uint8_t>` and the engine's
    WebAssembly decoder.
  - `SnapshotCreator` with `SetDefaultContext` and `CreateBlob`, `StartupData`,
    and `Isolate::CreateParams::snapshot_blob` restoring onto the first context.
  - `FunctionTemplate::New` with its full options, `SetCallHandler`,
    `SetClassName`, `Inherit`, `InstanceTemplate`, `PrototypeTemplate`, and
    `GetFunction`; `Function::New`/`Call`/`NewInstance`; `Signature` and
    `AccessorSignature`; `Object::GetPrototype`/`SetPrototype`;
    `Isolate::GetCurrent`/`TryGetCurrent`; `MicrotasksScope`; `Locker`/`Unlocker`.
- `WasmInstanceObject` (`IsWasmInstanceObject`, `GetExportedFunction` by name and
  by index, `GetFunctionIndex`, `GetMemoryObject`) and `WasmMemoryObject`
  (`IsWasmMemoryObject`, `Buffer`, `Size`).
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
- `Message` and `StackTrace` answer what the engine recorded, which is narrower
  than V8: a message exists only for a compile error (see above), and the stack
  is a string rather than an array of call sites. The sample's rich path is
  therefore taken for a syntax error and not for a runtime error.
- `String::Utf8Value` runs ToString through the engine, so an object's own
  `toString` runs; it falls back to a primitive rendering when a callback already
  holds the context (a property name is a string in that case anyway).
- The brand predicates (`IsDate`, `IsRegExp`, `IsPromise`, `IsMap`, `IsSet`,
  `IsNativeError`, `IsArrayBuffer`) and `Object::IsConstructor` need the entered
  context, which is where the agent lives: with no `Context::Scope` on the stack
  they answer false. V8 reads the isolate off the handle's location instead, so
  a host that keeps a value outside every scope and asks its brand gets a
  different answer here.
- `DataView` is `New` and the `ArrayBufferView` accessors: the geometry comes
  from the engine's DataView record, so the accessors need an entered context
  (a typed array's answer does not). `ArrayBuffer::New` needs an entered
  `Context::Scope` because the engine allocates in a realm; V8 takes the isolate
  alone. `BackingStore::Data` is the block's live base, so a caller must not hold
  the pointer across a resize. `SharedArrayBuffer::New` ignores the isolate's
  allocator (the engine allocates shared memory itself), and an isolate with no
  allocator still gets buffers from the engine, where V8 would refuse.
- A lazy data property's getter runs on every read (there is no one-shot slot),
  and a private name is a symbol, so `Object.getOwnPropertySymbols` can see a
  property V8's private names hide completely.
- The security token is stored and never consulted, so two contexts that set
different tokens can still reach each other's ordinary objects. The token is
now *comparable* (`slag_value_same_realm`, `slag_context_realm`), which is what
using it needs, but nothing compares it: a cross-origin wrapper is protected by
its own access check (`SetAccessCheckCallbackAndHandler`, above), and an
ordinary object has no check to run. Embedder value slots and aligned-pointer
slots are separate stores, so `GetEmbedderData` on a pointer slot answers
`undefined` rather than an `External`.
- `WeakCallbackInfo::GetValue` is always empty and every `WeakCallbackType`
  behaves as `kParameter`: the engine reports the death, not which kind of
  reference held the value. GC callbacks always receive `kNoGCCallbackFlags`,
  and `kMinorGarbageCollection` runs the one full collection the engine has.
- Heap statistics report `SIZE_MAX` for the limit and zeros for the executable,
  global-handle, and embedded-blob sizes, because this engine has none of those
  stores; the values are documented in the Rust bridge's `api::heap` module.
- A module is never freed and `CreateSyntheticModule` is absent: the engine has
  the record and the bridge has the synthetic form, but the facade's
  evaluation-steps callback has no C++ shape yet. `CompileOptions` and
  `NoCacheReason` are accepted and ignored (there is no code cache), and
  `Module::GetModuleRequests` as a `FixedArray` is not exposed; the length and
  index forms are.
- `WasmStreaming` is absent, and `CompiledWasmModule` carries no wire bytes, so
  `GetWireBytesRef` is absent rather than wrong. `WasmInstanceObject` reads what
  a module *exports*: a memory the module imports but does not export has no
  `GetMemoryObject` answer, and `GetFunctionIndex` answers -1 for an export that
  is not a wasm function wrapper.
- A snapshot carries the realm's globals only, and writing one refuses a realm
  that still holds host callbacks (delete `console` and the timers first).
  `SnapshotCreator::AddContext`, `SetDataBlob`, `AddData`, the external
  references, and the code cache are absent; a cached compile would need the
  engine to serialize its compiled program, which it does not yet do.
- `SetCallAsFunctionHandler` makes an instance callable: a builtin whose body is
  the handler, with the template's members and internal fields on the callee
  itself, so `typeof instance` is `"function"` and a call runs the host's code.
- `Signature` and `AccessorSignature` are enforced for function templates and
  for accessors (`SetAccessor`, `SetAccessorProperty`), by one receiver check:
  the template's built prototype must be on the receiver's chain. A per-object
  access check is enforced (`SetAccessCheckCallback` and
  `SetAccessCheckCallbackAndHandler`) for every property internal method of an
  instance, on a cross-realm access only. An instance of a checked template is a
  host object, so a template that also asks for a callable instance keeps the
  handler object instead (the engine's host objects have no `[[Call]]`).
- `Platform` is a handle plus a message pump: `PumpMessageLoop` runs the
  isolate's queued jobs (microtasks, promise reactions, due timers) and answers
  whether it ran any. There are no host-posted platform tasks or task runners
  yet, so `PostTask`/`CallOnForegroundThread` are absent rather than ignored.
- An `ObjectTemplate` with both a call handler and a property handler keeps the
  handler object, which is not callable: the engine's host objects have no
  `[[Call]]`. A callable instance also carries the builtin's `length` property
  and its `[[Prototype]]` is `%Object.prototype%`, as V8's callable instance's
  is. `Signature`/`AccessorSignature` are accepted without an access check, and
  `FunctionTemplate::InstanceTemplate`'s own accessors and members reach
  instances through the prototype object rather than as own properties.
- A `Map`/`Set` operation runs an engine intrinsic, so the container surface has
  no callback form yet: calling one from a host callback records a re-entrancy
  error instead of running. `WeakMap`/`WeakSet` and the `Map`/`Set` iterators as
  C++ objects are absent; a script has them, and `AsArray` covers the host side.
- A `Promise::Resolver` keeps its promise and rejecting function for the
  context's life, so a resolver that never settles is never released (V8
  collects the capability with the promise). `Resolver::New(context, promise)`,
  `Promise::WithResolvers`, and `Isolate::SetPromiseRejectCallback` with
  `PromiseRejectMessage` are absent; the engine reports unhandled rejections to
  the embedder callback instead.
- Microtasks follow `Isolate::SetMicrotasksPolicy`: `kAuto` (the default) drains
  after a successful `Script::Run`, `kExplicit` leaves the queues for
  `PerformMicrotaskCheckpoint` or `PumpMessageLoop`, and `kScoped` is accepted
  and behaves like `kExplicit` (there is no `MicrotasksScope` to close).
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

`gin-shapes` exits 0 only if every check passes. A failing step is **reported, not
fatal**: the shared `run`, `bind` and `number_at` helpers compile and run through
a `TryCatch`, print what threw (with the first 90 characters of the source) and
count a failure, so one broken block does not hide the rest of the run. The
remaining ad-hoc `ToLocalChecked()` calls in a block are still hard stops, which
is why the harness also switches the CRT's abort and assert dialogs off at
startup (a static initializer, so it is armed before `main`) and runs stdout
unbuffered: a dying run prints and exits instead of opening a modal box.
The predicate block asserts the
shape and brand bits for objects, arrays, functions, numbers, strings, symbols,
bigints, dates, regexps, promises, maps, sets, errors, typed arrays, array
buffers, proxies, and arguments objects, plus the `StrictEquals`/`SameValue`
differences on `NaN` and the two zeroes, `BooleanValue`, `TypeOf`, and the three
coercions. Its buffer block writes through an `ArrayBuffer`, a `BackingStore`,
and a `Uint8Array` and reads the bytes back from the script, then checks the
view geometry, the out-of-range RangeError, `GetBackingStore`, detach with and
without a key, `SharedArrayBuffer`, and the external-memory account. The
property block throws an `Exception::TypeError` and reads its name and message
back, round-trips a private property (and checks `Object.keys` does not list
it), reads a lazy and a native data property, checks `DontEnum` and `ReadOnly`,
and runs a `FunctionTemplate` accessor pair. The embedder block round-trips a
value and a pointer slot, checks that a value outlives the scope that made it,
that `ClearAllSlots` releases both roots, that the default security token is
one shared object and a set token reads back, and that an object reports the
entered context. The weak-handle block makes a value in a frame that returns
only the handle, scrubs the stack, collects, and checks that the callback and
its second pass ran, that the handle reads empty, and that `ClearWeak` keeps a
value alive without calling back; it then checks the GC hooks around requested
collections and their removal, and the heap statistics against a live buffer.
The module block compiles a dependency and a main module, resolves the import
through a file-scope callback, checks the request list, links, evaluates, and
reads the export from the namespace. The WebAssembly block compiles the sample's
`add` module, reads its decoded module back, makes a second module object from
it, binds it, and calls the export through a script's `WebAssembly.Instance`.
The snapshot block writes a realm with the console and timers removed, then
restores the blob in a fresh isolate and reads the global back. The template
block materializes a child template that inherits a parent's prototype method,
checks the data, argument, and instance checks, a `kThrow` template that cannot
be constructed, prototype get/set, and a `MicrotasksScope` draining on exit. The
string block builds a Latin-1 string from one-byte input and an astral string
from UTF-16 units, round-trips the three write forms (including a negative start,
a length window, `NO_NULL_TERMINATION`, and a buffer too small for the
character), checks `Utf8Length` and the lone surrogate's own three bytes against
`REPLACE_INVALID_UTF8`'s U+FFFD, and checks that `Int32Value`/`Uint32Value`/
`IntegerValue` wrap, truncate, run `valueOf`, saturate, and leave a throwing
conversion empty. The promise block settles and re-settles a resolver, reads the
state and result, rejects and marks handled, derives a promise through `Then2`
and one through `Catch` (draining the jobs between), and makes and settles one
from inside a host callback. The container block rounds a map and a set through
`Set`/`Get`/`Has`/`Delete`/`Clear`/`Add`, reads both `Size`s and both `AsArray`s,
checks that a script reads the same entries the host wrote, and checks that
`Size` refuses a handle that is not a map. The allocator block makes a buffer and
a store through a counting host allocator, checks that both drew their bytes from
it, that dropping the store called `Free` at once, and that the bytes are still
the isolate's external memory. The callable block builds a template with a call
handler, a method, and an internal field, checks that the instance is a function,
calls it through a script, and checks that a template with a property handler
keeps a handler object instead. The signature block declares a method over a
`Signature`, calls it on an instance (allowed), and calls it on a foreign object
(refused with a `TypeError` whose message is `Illegal invocation`). The accessor
signature block declares a getter over an `AccessorSignature` on a prototype
template, reads it from an instance (allowed, the getter ran once), calls it
through a descriptor with a foreign `this` (refused, the getter did not run, and
the message is `Illegal invocation`), and checks that an
`Object::SetAccessorProperty` getter whose template carries a `Signature` is
refused on a plain receiver. The access
check block installs a per-object check on a template, reads its instance from
its own realm (allowed, and the callback never runs), reads it from a second
context (refused with a `TypeError`, and the callback ran exactly once), and
reads an instance of a template whose callback allows the access from that same
second context. The promise
block checks the two statics, and the buffer block makes a `DataView` over the
host's buffer, writes through it, and checks that the bytes land in the buffer.
