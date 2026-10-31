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
- `Global<T>` stores the tagged value itself and `Get` allocates a fresh slot;
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
boundary. The remaining persistence gap is `Global<T>` over a heap value: it
stores the tagged value and allocates a slot on `Get`, but it does not retain
the value for its own lifetime. That is the next ABI addition.

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
- The handle stack as an engine root source: engine-value slots are retained and released with their scope.
- WebAssembly through the engine's own `wasm` feature, which `hello-world`
  exercises.

## What it does not

- It is not source-compatible with the rest of `v8.h`. There are no interceptors,
  no internal fields, no `Wrappable`, no `TracedReference`, no platform tasks,
  no snapshots, no inspector.
- `Global<T>` over a heap value is not pinned; only handle slots are rooted.
- `Message` is always empty and `StackTrace` is always empty, so the sample's
  rich error path (source line, caret) is compiled but not taken. An uncaught
  `Error` therefore prints the engine's generic `TypeError: Uncaught exception`
  rather than the thrown message; the ABI does not yet expose the thrown value.
- `PumpMessageLoop` always returns false; the engine drains jobs inline.
- `Isolate::Dispose` frees the contexts the facade created; templates are not
  freed.
- Host callbacks do not receive `this` yet (`FunctionCallbackInfo` has no
  `This()`).
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
