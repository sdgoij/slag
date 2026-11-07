# Divergences from QuickJS

This crate is a facade over Slag's engine, not a port of QuickJS. It reproduces
the QuickJS C ABI (both QuickJS-NG and Bellard skins) and an `rquickjs`-shaped
Rust API, but the underlying engine is Slag. This file records the places where
behaviour differs in a way an embedder can observe.

The C skin's supported surface is machine-checked: every header link-symbol is
either exported or listed in `c/unsupported.<feature>.toml` (see
`tools/quickjs-surface.py`). The gaps below are the *semantic* divergences
behind that list, not a duplicate of it.

## Bytecode and object serialization

`JS_WriteObject` / `JS_ReadObject` (and `JS_WriteObject2` / `JS_ReadObject2`)
are not implemented. There is no QuickJS bytecode wire format: compiled
functions are in-process Slag bytecode with no serialized form. Any embedder
that persists `JS_WriteObject` output will not be able to read it back here.

## Finalizer timing

`JS_NewClass` finalizers are C callbacks the facade stores on the class; they
run when Slag finalizes the host object, which happens on a collection sweep,
not synchronously at `JS_FreeValue`. `JS_RunGC` drains them (it performs a
collection and then `run_finalizers`); a following entry such as `JS_Eval`
also drains them. An embedder must not rely on the finalizer running during
the `JS_FreeValue` call that drops the last reference.

## `gc_mark` is never called

`JSClassDef.gc_mark` is accepted but never invoked. The facade keeps host-held
`JSValue`s alive by rooting their boxes in a GC root source, so there is no
`gc_mark` callback to trace them. Classes that do their own traversal in
`gc_mark` must instead root every referenced value through the C API (or hold
them in a box) for them to survive a collection.

## Allocator redirection

`JS_NewRuntime2` (custom `JSMallocFunctions`) is not exported; the facade
allocates with Slag's allocator and there is no per-runtime allocator override.
`JS_SetMemoryLimit` / `JS_SetGCThreshold` / `JS_GetGCThreshold` are likewise
not exported. Heap limits are available on the Rust facade
(`Runtime::set_heap_limits`), not through the C ABI.

## Partial realms

`JS_NewContextRaw` and the `JS_AddIntrinsic*` family are not exported. A realm
is always the full default realm; there is no way to build one up intrinsic by
intrinsic, and `JS_AddIntrinsicBaseObjects`-style calls have no C equivalent
here.

## Intl, Temporal, StringNormalize

No Intl, Temporal, or `String.prototype.normalize` support. The corresponding
intrinsics do not exist in the engine, so a program that reaches for them gets
the usual missing-global/undefined behaviour rather than a QuickJS build with
those features enabled.

## `quickjs-libc`

`js_std_cmd`, `js_std_*`, the `qjs` CLI, std/os modules, an event loop,
workers, and file I/O are out of scope. This is the bare `quickjs.h` embedding
surface, not the standalone interpreter's host library.

## Dump flags

`JS_SetDumpFlags` / `JS_GetDumpFlags` are not exported. There is no bytecode
or GC dump output to configure; the dump flag bitmask is meaningless here.

## Interrupt polling

`JS_SetInterruptHandler` is not exported. There is no interrupt callback
invoked at engine checkpoints; execution can only be stopped between API
entries (the Rust facade exposes `Runtime::terminate_execution`, and the
guard rails on a panicking host callback poison the isolate).

## Resizable external buffers

The QuickJS-NG `JS_NewArrayBuffer` `realloc_func` path (which lets QuickJS
grow a host-provided buffer in place) is not implemented; buffers are
fixed-size. The Bellard free-callback path and the `JS_NewArrayBufferCopy` /
`JS_NewUint8ArrayCopy` copy paths work.

## `JS_EvalThis`

`JS_EvalThis` / `JS_EvalThis2` are not exported. The facade has
`Context::eval_with_this` on the Rust side; the C form (which changes the
`this` binding of the evaluated script) is a wrapper-level gap.

## Module loading

Static imports resolve through the engine's module registry, exposed to the C
skin as `JS_SetModuleLoaderFunc` (and the attributes-aware
`JS_SetModuleLoaderFunc2` / `JS_SetModuleNormalizeFunc2`). Dynamic `import()`
is supported via `JS_SetDynamicImportFunc` / `JS_FinishDynamicImport`, but
`import.meta` and synchronous resolution semantics differ from QuickJS's
`JS_DetectModule`-driven model. There is no `JS_DetectModule`.

## Multiple contexts

Several contexts can exist on one runtime (realms are distinct), but `eval`
runs in the agent's current realm rather than the context the `Context`
handle names. This is an engine gap, not a facade one, and is shared with the
Rust facade.
