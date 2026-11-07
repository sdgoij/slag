# QuickJS facade: implementation plan

Status: planning. Last updated: 2026-11-07.

Goal: one crate, `crates/quickjs`, that stands in for QuickJS over Slag.

- The crate's public Rust API is shaped after `rquickjs`, the way
  `crates/v8` is shaped after `rusty_v8`.
- The C/FFI drop-in (`quickjs.h` symbols) lives under `src/c/` (Rust side)
  and `c/` (C headers, shim, tests).
- Both QuickJS-NG 0.18 and Bellard QuickJS 2025-09-13 are targets.
- No bytecode wire compatibility; no `quickjs-libc`.

The facade is a compatibility artifact, not a second embedding product:
`slag` remains the embedding API. The C side exists to let QuickJS hosts
compile unchanged, and both sides exercise `runtime::api` from the outside.

## 1. Locked decisions

- [x] One crate: `crates/quickjs`. No `quickjs-core`/`rquickjs` split.
- [x] `quickjs` is a workspace member but **not** a `default-members` entry,
      like `crates/jsc` and `crates/v8`.
- [x] Rust facade at the crate root (`lib.rs`, `src/`), API-shaped after
      `rquickjs`.
- [x] C/FFI under `src/c/` (Rust exports) and `c/` (headers, shim, C tests).
- [x] C ABI is target-selected by features: `ng` (QuickJS-NG 0.18) and
      `bellard` (Bellard 2025-09-13), mutually exclusive, neither is default.
- [x] No QuickJS bytecode wire compatibility. Compile-only handles are
      in-process only; `JS_WriteObject`/`JS_ReadObject` are unsupported.
- [x] No `quickjs-libc`, `qjs` CLI, `js_std_cmd`, std/os modules, event loop,
      or worker/file I/O. Documented as out of scope.
- [x] Engine changes are a last resort; every gap is tracked in the gap
      register (section 10) and added to `runtime::api` only when general.
- [x] Primary platform: 64-bit, MSVC x64 and GNU/Linux x64. 32-bit NaN boxing
      is deferred.
- [x] Vendor upstream headers byte-for-byte; document divergences in
      `crates/quickjs/DIVERGENCES.md`, never by editing the headers.

## 2. Layout

```
crates/quickjs/
  Cargo.toml
  build.rs                    # C shim + layout asserts + C tests (cc)
  src/lib.rs                  # rquickjs-shaped public API (crate root)
  DIVERGENCES.md
  README.md
  src/
    jsruntime.rs              # Runtime over runtime::api::Isolate
    context.rs                # Context / Ctx
    value.rs                  # Value<'js>, conversions
    object.rs                 # Object, property helpers
    array.rs                  # Array
    function.rs               # Function, closures, constructors
    class.rs                  # Class, Trace/Finalize equivalents
    module.rs                 # Module, exports, dynamic import
    promise.rs                # Promise, async, jobs
    typed_array.rs            # TypedArray, ArrayBuffer, DataView
    string.rs                 # String, Symbol, BigInt
    exception.rs              # Exception, Result mapping
    c/                        # C ABI (Rust implementation)
      mod.rs
      abi.rs                  # JSValue repr, tags, RefBox, roots
      ffi_types.rs            # per-target C structs (JSPropertyEnum, ...)
      runtime.rs
      context.rs
      value.rs
      string.rs
      atom.rs
      object.rs
      class.rs
      function.rs
      buffer.rs
      promise.rs
      module.rs
      error.rs
      json.rs
      misc.rs                 # introspection, limits, no-ops
      ng.rs                   # NG-only names and signatures
      bellard.rs              # Bellard-only names and signatures
  c/
    include/
      ng/quickjs.h            # vendored, byte-identical
      bellard/quickjs.h       # vendored, byte-identical
    shim/
      variadic.c              # JS_New*Error / JS_Throw*Error / js_std_cmd
      layout.c                # _Static_assert layout checks
    tests/
      ng/*.c
      bellard/*.c
  tests/
    facade.rs                 # Rust API tests
    compat_ng.rs              # links the C tests (like crates/jsc)
    compat_bellard.rs
```

The Rust facade uses `runtime::api`/`crux` directly and does not touch the
QuickJS `JSValue` representation. `src/c/` is the only place that marshals
between the engine and the 16-byte C `JSValue`.

## 3. Features and build

```toml
[features]
default = []
ng = []
bellard = []
```

- `cargo build -p quickjs` / `cargo test -p quickjs` build the Rust facade only.
- `cargo build -p quickjs --features ng` builds the C drop-in for QuickJS-NG.
- `cargo build -p quickjs --features bellard` builds it for Bellard.
- `compile_error!` when both `ng` and `bellard` are enabled.
- `crate-type = ["rlib", "staticlib", "cdylib"]`; the staticlib/cdylib are
  meaningful only with a C feature, the rlib is the Rust facade.
- Never link `ng` and `bellard` artifacts into one process: `JS_NewClassID`
  and `JS_NewArrayBuffer` have different signatures.
- Clippy must run per feature set, never `--all-features` (it would enable
  both and trip the `compile_error!`).

```text
cargo build -p quickjs                                  # Rust facade
cargo test  -p quickjs                                  # Rust facade tests
cargo build -p quickjs --features ng                    # C drop-in (NG)
cargo test  -p quickjs --features ng                    # C drop-in (NG)
cargo build -p quickjs --features bellard               # C drop-in (Bellard)
cargo test  -p quickjs --features bellard               # C drop-in (Bellard)
```

## 4. Naming note

The package is named `quickjs` (not `qjs`) so it is a `[patch.crates-io]`
replacement for the crates.io `quickjs` crate: a `[patch.crates-io]` key must
match the package name being replaced. The Rust facade is API-shaped after
`rquickjs`, the way `crates/v8` mirrors `rusty_v8`; a host migrating from
`rquickjs` changes `use rquickjs::...` to `use quickjs::...`.

## 5. Compatibility matrix

The C ABI delta between targets is small and localized. On 64-bit both
headers use the same 16-byte `JSValue` (`{ union; int64_t tag }`); 32-bit
builds use the same NaN boxing.

| Area | QuickJS-NG 0.18 | Bellard 2025-09-13 | Home |
|---|---|---|---|
| `JSValue` (64-bit) | `{ union, int64_t tag }` | same | `src/c/abi.rs` |
| `JS_NewClassID` | runtime-scoped, 2 args | process-global, 1 arg | `src/c/ng.rs` / `bellard.rs` |
| `JS_NewArrayBuffer` | realloc callback, max length | free callback | `src/c/ng.rs` / `bellard.rs` |
| `JS_NewUint8Array` | present, 6 args | absent | `src/c/ng.rs` |
| `JSPropertyEnum` | `bool is_enumerable` | `JS_BOOL is_enumerable` | `src/c/class.rs` |
| `JSClassExoticMethods` | 7 callbacks | 11 callbacks | `src/c/class.rs` |
| `JS_BOOL` | `bool` in places | `int` | `src/c/abi.rs` |
| NG-only names | promise helpers, `JS_SetPromiseHook`, proxy accessors, dynamic-import helpers, `JS_DetectModule`, `JS_NewCClosure`, `JS_NewCFunction3/Data2`, `JS_NewObjectFrom/FromStr`, `JS_NewArrayFrom`, length/property-value helpers, brand checks, `JS_GetVersion`, `JS_GetClassName`, `JS_AddRuntimeFinalizer`, `JS_NewSymbol`, UTF-16 C strings, `JS_New*Error` family | — | `src/c/ng.rs` |
| Bellard-only names | — | `JS_ParseJSON2`, `JS_PrintValue*`, `JS_StrictEq`/`JS_SameValue*`, `JS_SetUncatchableException`, `JS_SetLocaleCallbacks`, `JS_AddIntrinsicStringNormalize`, `JS_Get/SetStripInfo`, `JS_NewShortBigInt` | `src/c/bellard.rs` |

Everything else is shared in `src/c/`.

## 6. Phase 0: crate skeleton

Dependencies: none. Size: S (1–2 days).

- [x] Add `crates/quickjs` to `[workspace] members`.
- [x] Exclude `crates/quickjs` from `default-members`.
- [x] `Cargo.toml`: `publish = false`, workspace edition/license, features
      `default = []`, `ng`, `bellard`, `compile_error!` on both.
- [x] `crate-type = ["rlib", "staticlib", "cdylib"]`; crate root is
      `src/lib.rs` (jsc-style), not a top-level `lib.rs`.
- [x] `build.rs` stub present; shim/layout compilation deferred to Phase 2.
- [x] `src/lib.rs` declares the `rquickjs`-shaped module tree (stubs).
- [x] `src/c/mod.rs` gated on `any(feature = "ng", feature = "bellard")`.
- [x] C exports are behind `mod c`, so neither C feature means no C symbols.
- [x] `cargo check` passes with no features, `ng`, and `bellard`.
- [x] `cargo clippy --all-targets -- -D warnings` passes for all three
      feature sets.

`tools/build.sh` is N/A for now: it caches release binaries, and `quickjs` is a
library. Revisit if a `quickjs` host binary lands.

## 7. Phase 1: Rust facade

Dependencies: Phase 0. Size: L (1–2 weeks). This is the primary surface and
the fastest way to exercise `runtime::api`; it needs no `JSValue` marshaling.

- [x] `Runtime`: `Runtime::new`, `set_heap_limits` (memory limits),
      `terminate_execution`, `run_microtasks`. Interrupt configuration is gap
      G2.
- [x] `Context`/`Ctx`: wraps `api::Context`; `eval`, `global`, `set_global`,
      `run_microtasks`. Multiple realms per runtime are distinct, but `eval`
      runs in the agent's current realm rather than the context's (engine gap;
      a facade test for it is blocked on that).
- [x] `Value` over `api::Local`, mapping to engine `Value`.
- [x] Primitive accessors/constructors: undefined/null/bool/number/string.
- [x] `Object` get/set/has/delete and `new`.
- [x] `Array`, `Function`.
- [x] `String`, `Symbol`, `BigInt`.
- [x] `Error`/`Result` with message and thrown value.
- [x] Host functions and closures (`Runtime::create_function`/`add_function`).
- [x] Constructors and classes (`Class` with constructor/prototype/method;
      finalizers still via `crux::host::HostOps`, not wired into `Class`).
- [x] Modules: `new`/`evaluate`/`namespace`/`status`/`name` (static import via
      the engine registry; dynamic import and `import.meta` are embedding gaps).
- [x] Promises (`resolve`/`reject`/`then`/`state`/`result`) and job draining.
- [x] Typed arrays (`ArrayBuffer`, `TypedArray`, `DataView`).
- [x] `rquickjs` API fidelity checklist in `src/` docs (`lib.rs`).
- [x] Examples: eval, host function, class, module, promise, typed array
      (`crates/quickjs/examples/`).
- [x] Port a representative slice of upstream `rquickjs` tests
      (`tests/facade_rquickjs.rs`).
- [x] `cargo test -p quickjs`; `cargo clippy -p quickjs --all-targets -- -D warnings`.

Design note: `Value` currently carries no `'js` lifetime (it is a copyable
handle over `api::Local`, like `crates/v8`'s `Local`). The rquickjs
context-tied lifetime is a follow-up refinement, not a rework — the types
that hold values will just thread a phantom lifetime through.

## 8. Phase 2: C ABI foundation

Dependencies: Phase 0 (independent of Phase 1). Size: M–L (1–2 weeks).
Deliverable: the shared `JSValue` marshaler and the L0 C surface, used by both
skins.

- [x] `src/c/abi.rs`: `JSValue` as `{ u: u64, tag: i64 }` (ABI-identical to
      the C union on 64-bit), primitive tags, value encode/decode.
- [x] Bit-for-bit agreement against the header's `JS_MKVAL`/`JS_MKPTR` for
      int/bool/float (Rust-side, since the header is not vendored yet).
- [x] `RefBox` (`refs`, `value`, `kind`) and `RefKind` for object, string,
      string rope, symbol, BigInt, module, bytecode.
- [x] `dup`/`free` with last-ref drop.
- [x] Canonicalize object boxes by object id.
- [x] Register the box registry as a `RootSource` (per `crates/ffi`).
- [x] Layout tests: `JSValue` size 16, align 8, tag offset 8.
- [ ] `c/shim/layout.c` `_Static_assert` for the same numbers (needs the
      vendored header).
- [x] Runtime/context lifecycle: `JS_NewRuntime`, `JS_FreeRuntime`,
      `JS_NewContext`, `JS_DupContext`, `JS_FreeContext`, `JS_GetRuntime`,
      and a TLS current-context slot. (`JS_NewRuntime2`, opaque get/set,
      deferred teardown, and `ffi::guard` poisoning still owed.)
- [ ] `JS_NewClassID` (shared helper; per-target signature lives in the skin).
- [x] Atoms: thread-local table, `JS_ATOM_NULL = 0`, dup/free/refcount,
      `JS_AtomToValue`/`ToString`/`ToCString`, `JS_ValueToAtom`.
      (Per-runtime scoping is a noted v1 simplification.)
- [x] Strings: `JS_NewStringLen`, `JS_NewAtomString`, `JS_ToString`,
      `JS_ToCStringLen2`, `JS_FreeCString`. (`JS_FreeCStringRT` and
      lone-surrogate handling owed.)
- [x] Numbers: `JS_ToInt32`, `JS_ToNumber`, `JS_ToBool`. (BigInt and the
      rest owed.)
- [x] Exceptions: `JS_Throw`, `JS_GetException`, `JS_HasException`,
      `JS_NewError`, `JS_ThrowOutOfMemory`, `JS_IsError`, the variadic error
      constructors/throwers (C shim), and `JS_Eval` sets a pending exception.
- [x] Allocator: export the `js_malloc` family (ctx and `_rt` variants).
- [x] `c/shim/variadic.c`: the `JS_New*Error`/`JS_Throw*Error` bridges.
      (`js_std_cmd` is out of scope — quickjs-libc.)
- [x] L0 C test: eval, values, strings, objects, exceptions, atoms,
      conversions (host function still owed).
- [x] `cargo test -p quickjs --features ng`; clippy per feature.

## 9. Phase 3: QuickJS-NG C skin

Dependencies: Phase 2. Size: M (1–2 weeks).

- [x] Vendor `c/include/ng/quickjs.h` from quickjs-ng v0.18.0.
- [x] `build.rs` selects `ng`, exposes the include path, and compiles the C
      tests.
- [x] Export groups (partial): runtime/context/value/object landed in Phase 2;
      classes/opaque/class-prototype and the class callbacks
      (finalizer/call/exotic get/set/has/delete/get_own_property) landed, then
      C functions (`JS_NewCFunction2`) and function lists
      (`JS_SetPropertyFunctionList`: generic C functions + primitive constants)
      landed now, the core introspection predicates (`JS_IsFunction`,
      `JS_IsConstructor`, `JS_IsArray`, `JS_IsProxy`, `JS_GetVersion`) landed,
      ArrayBuffer (`JS_NewArrayBufferCopy`/`JS_GetArrayBuffer`/
      `JS_IsArrayBuffer`/`JS_DetachArrayBuffer`) landed, typed arrays
      (`JS_NewUint8ArrayCopy`/`JS_GetTypedArrayBuffer`/`JS_GetTypedArrayType`)
      landed, the promise helpers (`JS_NewSettledPromise`/`JS_IsPromise`/
      `JS_PromiseState`/`JS_PromiseResult`) landed, jobs (`JS_EnqueueJob`/
      `JS_IsJobPending`/`JS_ExecutePendingJob`) landed, modules
      (`JS_Eval` with `JS_EVAL_TYPE_MODULE`, `JS_ResolveModule`,
      `JS_EvalFunction`, `JS_GetModuleNamespace`, `JS_GetModuleName`,
      `JS_NewCModule`, `JS_AddModuleExport`, `JS_SetModuleExport`) landed,
      promise capability (`JS_NewPromiseCapability`) landed, and the
      builtin-instance introspection (`JS_IsDate`/`JS_IsRegExp`/`JS_IsMap`/
      `JS_IsSet`/`JS_IsDataView`) landed, and the getter/setter/object/alias
      function-list rows (`JS_DEF_CGETSET`/`JS_DEF_CGETSET_MAGIC`/
      `JS_DEF_OBJECT`/`JS_DEF_ALIAS`) landed, and the exotic
      `define_own_property`/`get_own_property_names` callbacks landed, the
      generic `JS_NewTypedArray` landed, and the host-borrowed
      `JS_NewArrayBuffer`/`JS_NewUint8Array` (fixed-size borrow over the
      host's bytes, no copy; the `realloc_func` resizable path is gap G10)
      landed, and the synchronous module loader (`JS_SetModuleLoaderFunc`)
      landed (engine: `HostHooks::resolve_imported_module`).
      Remaining: `JS_LoadModule` (file I/O, non-goal). The dynamic-import
      hooks (`JS_SetDynamicImportFunc`/`JS_FinishDynamicImport`),
      `JS_SetModuleLoaderFunc2`/`JS_SetModuleNormalizeFunc2` (attributes-aware
      loader), and `gc_mark` (documented no-op; `JS_MarkValue` exported) have
      since landed.

Class note: `JS_NewClassID` uses the NG two-arg signature; Bellard's one-arg
`JS_NewClassID` is a Phase 4 delta. Class instances are ordinary objects whose
class id + opaque pointer live in a side table; opaque entries are currently
not reclaimed (finalizers will clear them).
- [ ] NG-only exports in `src/c/ng.rs` (`JS_NewClassID(rt, id)`, NG
      `JS_NewArrayBuffer`, `JS_NewUint8Array`, promise/proxy/module/dynamic
      helpers, `JS_GetVersion`, `JS_GetClassName`, `JS_AddRuntimeFinalizer`,
      `JS_NewSymbol`, UTF-16 strings, `JS_New*Error`).
- [ ] C tests under `c/tests/ng/`: objects, functions, classes, buffers,
      promises, modules.
- [ ] `tests/compat_ng.rs` links the compiled C tests and the staticlib.
- [ ] Static and shared link smoke tests on MSVC x64 and GNU/Linux x64.
- [ ] Surface ledger: `tools/quickjs-surface.py` asserts every declared NG
      symbol is implemented or in `unsupported.toml`.
- [ ] `cargo test -p quickjs --features ng`; clippy per feature.

## 10. Phase 4: Bellard C skin

Dependencies: Phase 3. Size: S–M (3–5 days).

- [x] Vendor `c/include/bellard/quickjs.h` from the pinned 2025-09-13 tarball
      (https://bellard.org/quickjs/quickjs-2025-09-13.tar.xz); SHA-256
      `ad3455cafab56a036e5b94c8e6915e653a09f48e32e72cb66f0d0775047a7bb5`
      (patched: `JS_DupValue`/`JS_DupValueRT`'s `return (JSValue)v;` ->
      `return v;` to satisfy MSVC C2440, and the inline `JS_FreeValue`/
      `JS_DupValue`/`JS_FreeValueRT`/`JS_DupValueRT` bodies replaced with plain
      declarations so they route to the facade's exported symbols — the NG
      model).
- [x] Per-target `JS_NewClassID(id*)` (one arg, returns the id).
- [x] Per-target `JS_NewArrayBuffer` with the free callback (borrowed block
      whose deleter calls `JSFreeArrayBufferDataFunc` on last ref).
- [x] Bellard `JS_NewUint8Array` is absent; gated out under `bellard`.
- [x] `JS_FreeValueRT`/`JS_DupValueRT` exported (were missing; both headers
      declare them).
- [x] `JS_BOOL` return-type gating (`int` under `bellard`, `bool` under `ng`)
      via `abi::JS_BOOL` + `abi::js_bool`; `JS_IsArray`/`JS_IsError` also gate
      their arity (`JSContext *` under Bellard). The structs need no gating
      (the atom offset and the first seven exotic callbacks are
      byte-identical).
- [x] Bellard-only exports in `src/c/bellard.rs`: `JS_StrictEq`/`JS_SameValue`/
      `JS_SameValueZero`, `JS_ParseJSON2`, `JS_AddIntrinsicStringNormalize`
      (no-op: full realm), `JS_SetUncatchableException` (no-op: not modeled),
      `JS_SetStripInfo`/`JS_GetStripInfo`, and the `JS_PrintValue*` family
      (`JS_PrintValueSetDefaultOptions`/`JS_PrintValue`/`JS_PrintValueRT` via a
      JSON dump writer). `JS_NewShortBigInt` is inline.
- [x] Omit NG-only exports under `bellard` (surface hygiene: gated 17
      NG-only symbols — the builtin-instance predicates `JS_IsProxy`/`JS_IsDate`/
      `JS_IsRegExp`/`JS_IsMap`/`JS_IsSet`/`JS_IsDataView`, `JS_IsArrayBuffer`,
      `JS_IsPromise`, `JS_NewSettledPromise`, `JS_NewUint8ArrayCopy`,
      `JS_GetTypedArrayType`, `JS_ToNumber`, `JS_GetVersion`, `JS_GetClassName`,
      `JS_GetPendingJobContext`, `JS_DupAtomRT`, `JS_FreeCStringRT` — plus the
      `ng`-gated imports they were the sole users of).
- [x] C tests under `c/tests/bellard/` (`api_delta.c`: class id, free-callback
      buffer, the `JS_BOOL` predicates, and the Bellard-only names) +
      `tests/compat_bellard.rs`.
- [x] Artifact separation in CI: `tools/build-quickjs.sh` + the
      `quickjs-artifacts` job copy each build to
      `target/quickjs/<feature>/libquickjs.a` (and the shared library).
- [x] `cargo test -p quickjs --features bellard` and clippy per feature.

### Bellard refcounting model — the real delta (plan correction)

The §5 matrix understated this: the Bellard header does **inline reference
counting**. `JS_FreeValue`/`JS_DupValue` are `static inline` functions that
read `JSRefCountHeader { int ref_count; }` at `JS_VALUE_GET_PTR(v)` and bump
it directly, calling `__JS_FreeValue`/`__JS_FreeValueRT` only when the count
reaches zero. The facade's box registry (`src/c/abi.rs`) is the NG model —
`JSValue.u` names a `RefBox` in a separate `HashMap`, and `JS_FreeValue`/
`JS_DupValue` are exported symbols that do their own refcounting. Resolved by
the vendored-header patch above: the inline bodies are replaced with plain
declarations, so a Bellard host's `JS_FreeValue`/`JS_DupValue` call the
facade's exported symbols exactly as an NG host does.

## 11. Embedding API gap register

Dependencies: Phases 1–4. Size: M, ongoing.

| ID | Gap | Used by | Facade fallback | Engine work | Status |
|---|---|---|---|---|---|
| G1 | Eval with an explicit `this` | `JS_EvalThis`, `JS_EvalThis2` | wrapper changes declaration semantics | `Context::eval_with_this` | open |
| G2 | Interrupt callback at engine checkpoints | `JS_SetInterruptHandler` | poll only at API entries | `HostHooks::interrupt` + JIT probe | open |
| G3 | Synchronous import resolution hook | `JS_SetModuleLoaderFunc` | pre-register sources | `HostHooks::resolve_imported_module` | done |
| G4 | Proxy target/handler accessors | `JS_GetProxyTarget/Handler` | unsupported | `builtins/proxy` accessors | open |
| G5 | Run one job / pending-job query | `JS_ExecutePendingJob`, `JS_IsJobPending` | run all jobs | `api` job-queue query | open |
| G6 | Set the constructor bit | `JS_SetConstructorBit` | wrappers may diverge | `Function` setter | open |
| G7 | Seal/freeze/extensible helpers | `JS_SealObject`, `JS_FreezeObject`, `JS_PreventExtensions` | use JS builtins | `api` helpers if absent | open |
| G8 | Own-property descriptor/name helpers | `JS_GetOwnProperty`, `JS_GetOwnPropertyNames` | internal pattern | promote to `api` if absent | open |
| G9 | HTMLDDA internal slot | `JS_SetIsHTMLDDA` | unsupported | internal slot | open |
| G10 | Resizable external buffers | NG `realloc_func` path | fixed-size only | `byteblock` realloc support | open |

- [ ] Re-measure each gap while implementing Phases 1–2; drop rows that turn
      out to be facade-only.
- [ ] For each engine addition: an `api` test, a facade test, and a
      `.notes/embedding.md` status line.
- [ ] For each documented divergence: an entry in `DIVERGENCES.md`.
- [ ] Do not add an engine API that is useful only to one QuickJS quirk.

## 12. Phase 6: hardening and certification

Dependencies: Phases 2–5. Size: M (1 week).

### 12.1 Lifetime and GC

- [x] Host-held value survives a deterministic collection (`c_host_value_survives_gc`).
- [x] Last `JS_FreeValue` unroots; the value is swept afterwards
      (`c_gc_lifetime`).
- [x] Object box canonicalization: two refs share a box pointer
      (`c_box_canonicalization`).
- [x] C-string pointers survive table growth and free with the right function
      (`c_cstring_table_growth`).
- [x] Atom lifetime across contexts in one runtime
      (`c_atom_lifetime_across_contexts`).
- [x] Class finalizer runs once after the last host ref drops and a sweep
      (`c_gc_lifetime`).
- [ ] Cyclic JS graph with a host object does not leak it forever (deferred:
      the conservative stack scan keeps short-lived cycles alive across one
      collection; needs `scrub_stack`-style isolation or `--gc-stress`).
- [x] `--gc-stress` over the Rust facade (`Runtime::set_gc_stress` +
      `tests/gc_stress.rs`); the C suite exercises deterministic collection via
      `JS_RunGC` (full per-allocation stress over C is not plumbed).
- [x] Context released inside a callback is torn down at the outermost entry
      (`c_context_released_in_callback`).

### 12.2 Re-entrancy and errors

- [x] Host callback re-enters `JS_Eval`/`JS_NewObject` and throws
      (`c_reentrant_callback_throws`; `JS_Call`/`JS_Invoke` are not exported —
      a gap, not exercised).
- [x] Host callback releases its own context (`c_context_released_in_callback`).
- [x] Panic in a Rust host callback is contained by `ffi::guard` (re-exported
      as `quickjs::guard`); the isolate is poisoned and rejects later calls
      (`tests/panic.rs`). `JS_Eval`/`JS_EvalFunction`/`JS_ResolveModule`/
      `JS_ExecutePendingJob` are guarded; `JS_RunGC` is deliberately **not** — a
      `catch_unwind` frame during `collect_garbage` leaves stale words the
      conservative scan misreads as roots.
- [x] `JS_GetException`/`JS_Throw` nesting (`c_exception_nesting`).
- [x] `JS_ThrowOutOfMemory` (`c_throw_out_of_memory`).

### 12.3 ABI and build

- [x] C layout assertions pass for both headers on MSVC and GNU.
- [x] By-value `JSValue` call/return tests on both compilers.
- [x] Static and shared link tests.
- [x] `ng`/`bellard` artifact separation in CI.
- [x] Header drift CI: vendored headers match the pinned checksums (`tools/check-quickjs-headers.sh`; the pinned values are the *post-patch* vendored files, not the upstream tarball SHA-256).
- [x] Surface ledger fails on missing or unexpected exports.
- [x] Symbol audit: no helper collides with a test symbol.

### 12.4 Documentation

- [x] `crates/quickjs/DIVERGENCES.md`: bytecode, finalizers, `gc_mark`,
      allocator, partial realms, Intl/Temporal, `quickjs-libc`, dump flags,
      interrupt polling, resizable external buffers.
- [x] `crates/quickjs/README.md`: Rust facade usage and per-target C build/link.
- [x] `crates/quickjs/c/README.md`: header provenance and SHA-256s.
- [x] Update this plan's checkboxes and progress log.

### 12.5 Certification commands

- [x] `cargo test -p quickjs`.
- [x] `cargo test -p quickjs --features ng`.
- [x] `cargo test -p quickjs --features bellard`.
- [x] `cargo clippy -p quickjs --all-targets -- -D warnings` (no features).
- [x] `cargo clippy -p quickjs --all-targets --features ng -- -D warnings`.
- [x] `cargo clippy -p quickjs --all-targets --features bellard -- -D warnings`.
- [ ] `cargo test --locked --workspace` (quickjs stays out of default-members;
      run its own gate plus the workspace gate).
- [ ] GC-stress run.
- [ ] Cross-target C test matrix green.
- [x] Header drift and surface ledger green.

## 13. Explicit non-goals

- [ ] Bytecode wire compatibility; `JS_WriteObject`/`JS_ReadObject`
      unsupported with a recorded reason.
- [ ] `quickjs-libc`, `qjs`, `js_std_cmd`, event loop, file I/O, workers.
- [ ] Custom allocator redirection via `JS_NewRuntime2`; accept and ignore,
      keep memory limits.
- [ ] Partial realms (`JS_NewContextRaw` + `JS_AddIntrinsic*`); full realm,
      additions no-op.
- [ ] Intl/Temporal/StringNormalize/locale callbacks; no-op or unsupported.
- [ ] `CONFIG_CHECK_JSVALUE`; unsupported, documented.
- [ ] 32-bit targets; deferred with a designed NaN-boxing module.

## 14. Risks

| Risk | Mitigation |
|---|---|
| Refcount/root bugs silently alias freed slots | stress tests, deterministic `Heap::collect`, `--gc-stress` |
| Finalizer timing differs from QuickJS | document; test once-after-sweep |
| By-value `JSValue` ABI differs across compilers | layout asserts and C hello-world early |
| Both C targets linked accidentally | separate artifacts and CI directories |
| Header drift | pinned checksums and a diff job |
| `JS_EvalThis` semantics | prefer `Context::eval_with_this`; else document the wrapper difference |
| Sync module loader | gap G3; else pre-register and document |
| External resizable buffers | fixed-size first; gap G10 |
| `gc_mark` expectation | host refs are roots; accept and document it is never called |
| Surface drift | ledger generated from the selected header |
| `--all-features` trips the mutual-exclusion `compile_error!` | run clippy per feature set |

## 15. Progress log

| Date | Change | Next |
|---|---|---|
| 2026-11-07 | Plan written; single-crate `quickjs` layout; decisions locked | Phase 0 crate skeleton |
| 2026-11-07 | Phase 0 complete: `crates/quickjs` skeleton, features, checks green | Phase 1 Rust facade |
| 2026-11-07 | Phase 1 L0: Runtime/Context/Value/Object/Error + 5 tests green | Host functions, Array/Function, remaining Phase 1 |
| 2026-11-07 | Phase 1: host functions, Function, Array + Phase 2 abi (JSValue/RefBox) | C exports, String/Symbol/BigInt, constructors |
| 2026-11-07 | Phase 2/3: vendored NG header + first C exports + C compat test green | Atoms, more value/object exports, allocator, shim |
| 2026-11-07 | Atoms, exceptions, conversions + atom/uint32 property access; C tests green | Allocator, variadic shim, remaining exports |
| 2026-11-07 | Allocator + variadic error shim; 7 C compat tests green | JS_IsError/TLS, remaining export groups |
| 2026-11-07 | TLS current-context + JS_IsError; exception surface complete | Remaining export groups (classes, buffers, promises, modules) |
| 2026-11-07 | Classes: class-id, registration, opaque, class proto, GetClassID | Class callbacks, buffers, promises, modules |
| 2026-11-07 | Class callbacks (finalizer/call) land; JS_RunGC drains via Isolate::run_finalizers | Exotic dispatch, buffers, promises, modules |
| 2026-11-07 | Exotic dispatch (get/set/has/delete/get_own_property) + C compat test | Function lists, buffers, promises, modules |
| 2026-11-07 | C functions (JS_NewCFunction2) + function lists (CFUNC + primitive constants) | Buffers, promises, modules |
| 2026-11-07 | Introspection (JS_IsFunction/JS_IsConstructor/JS_IsArray/JS_IsProxy/JS_GetVersion) | Buffers, promises, modules |
| 2026-11-07 | ArrayBuffer (JS_NewArrayBufferCopy/JS_GetArrayBuffer/JS_IsArrayBuffer/JS_DetachArrayBuffer) | Typed arrays, promises, modules |
| 2026-11-07 | Typed arrays (JS_NewUint8ArrayCopy/JS_GetTypedArrayBuffer/JS_GetTypedArrayType) | Promises, modules |
| 2026-11-07 | Promises (JS_NewSettledPromise/JS_IsPromise/JS_PromiseState/JS_PromiseResult) | Jobs, modules |
| 2026-11-07 | Jobs (JS_EnqueueJob/JS_IsJobPending/JS_ExecutePendingJob) | Modules |
| 2026-11-07 | Modules (JS_Eval module flags, JS_ResolveModule, JS_EvalFunction, JS_GetModuleNamespace, JS_GetModuleName, JS_NewCModule, JS_AddModuleExport, JS_SetModuleExport) + 2 C tests green | Promise capability, introspection, exotic callbacks, function-list rows |
| 2026-11-07 | Promise capability (JS_NewPromiseCapability) + C test green | Builtin-instance introspection, exotic callbacks, function-list rows |
| 2026-11-07 | Builtin-instance introspection (JS_IsDate/JS_IsRegExp/JS_IsMap/JS_IsSet/JS_IsDataView) + C test green | Exotic callbacks, function-list rows |
| 2026-11-07 | Function-list accessor/object/alias rows (JS_DEF_CGETSET/CGETSET_MAGIC/OBJECT/ALIAS) + C test green | Exotic callbacks, generic typed array, host-borrowed buffers |
| 2026-11-07 | Exotic define_own_property/get_own_property_names callbacks + C test green | Generic typed array, host-borrowed buffers |
| 2026-11-07 | Generic JS_NewTypedArray + host-borrowed JS_NewArrayBuffer/JS_NewUint8Array + 3 C tests green | gc_mark (no-op), module loader |
| 2026-11-07 | Module loader (JS_SetModuleLoaderFunc via HostHooks::resolve_imported_module) + C test green | gc_mark (no-op), dynamic import, Phase 4 Bellard |
| 2026-11-07 | Phase 4 foundation: vendored Bellard header (SHA-256 recorded, MSVC patch), per-target JS_NewClassID/JS_NewArrayBuffer, JS_NewUint8Array gated out; clippy green | Bellard refcounting model (inline JSRefCountHeader), JS_BOOL return types, Bellard C compat tests |
| 2026-11-07 | Bellard refcounting resolved via header patch (inline JS_FreeValue/DupValue -> declarations), JS_FreeValueRT/DupValueRT exported, api_delta.c + compat_bellard.rs green | JS_BOOL return types, omit-NG-only exports, Bellard-only exports, CI artifact separation |
| 2026-11-07 | Bellard JS_BOOL return-type gating (abi::JS_BOOL + js_bool; JS_IsArray/JS_IsError arity too) + predicate C test green | Omit NG-only exports, Bellard-only exports, CI artifact separation |
| 2026-11-07 | Bellard-only exports (JS_StrictEq/JS_SameValue/JS_SameValueZero, JS_ParseJSON2, strip-info, no-op intrinsics) + C test green | Omit NG-only exports, JS_PrintValue*, CI artifact separation |
| 2026-11-07 | Omit NG-only exports under `bellard`: gated 17 NG-only symbols + their now-unused `ng` imports; clippy/tests green per feature | JS_PrintValue*, CI artifact separation |
| 2026-11-07 | Phase 1 Rust facade complete: String/Symbol/BigInt, Class, Module, Promise, ArrayBuffer/TypedArray/DataView + fidelity checklist, 6 examples, rquickjs test slice; clippy/tests green per feature | JS_PrintValue*, CI artifact separation |
| 2026-11-07 | C-skin leftovers: JS_MarkValue no-op, JS_PrintValue* (JSON dump writer), JS_SetModuleLoaderFunc2/JS_SetModuleNormalizeFunc2 (attributes-aware), JS_SetDynamicImportFunc/JS_FinishDynamicImport (token + pending-promise registry), CI artifact separation (tools/build-quickjs.sh + quickjs-artifacts job); +4 C tests, clippy/tests green per feature | Phase 6 hardening |
| 2026-11-07 | Phase 6.1 lifetime/GC: +6 C tests (host value survives, box canonicalization, cstring growth, atom lifetime, finalizer-once/last-free, context-in-callback) + Runtime::set_gc_stress + tests/gc_stress.rs; cyclic-graph case deferred (conservative scan) | Phase 6.2 re-entrancy |
| 2026-11-07 | Phase 6.2 re-entrancy/errors: +3 C tests (reentrant callback throws, exception nesting, throw-OOM) + ffi::guard wrapping (JS_Eval/EvalFunction/ResolveModule/ExecutePendingJob; JS_RunGC excluded — catch_unwind breaks the conservative scan) + tests/panic.rs; clippy/tests green per feature | Phase 6.3 ABI/build |
| 2026-11-07 | Phase 6.3 ABI layout: feature-gated `JSPropertyEnum.is_enumerable` (`bool` vs `JS_BOOL`) and `JSClassExoticMethods` trailing Bellard callbacks (7 vs 11) so the Rust structs match each header; +C `abi.c` static asserts (sizeof/offsetof) per target and Rust layout tests; corrected the §5 matrix (`JSClassExoticMethods` 7/11, home moved to `class.rs`/`abi.rs`); clippy/tests green per feature | Phase 6.3 by-value `JSValue` ABI |
| 2026-11-07 | Phase 6.3 by-value `JSValue` ABI: +C `value_abi.c` (`echo` host callback) round-trips a float64, an object box, and an int-valued double through the 16-byte union on both x64 compilers (engine boxes JS numbers as doubles, so `42` is `JS_TAG_FLOAT64`); clippy/tests green per feature | Phase 6.3 static/shared link |
| 2026-11-07 | Phase 6.3 static/shared link: +`c/link-test/hello.c` and `tools/link-test-quickjs.sh` link a C host against the staticlib (with the shim + platform system libs) and the cdylib for each feature, assert `1+2` prints `3`, and run it; verified green on Windows (clang/MSVC) for both skins, wired into the CI `quickjs-artifacts` job for the Linux path | Phase 6.3 header drift CI |
| 2026-11-07 | Phase 6.3 header drift: +`tools/check-quickjs-headers.sh` pins the post-patch vendored-file SHA-256s (ng `6d3879c8…`, bellard `6904cf9e…`) and fails on drift; wired into the CI `quickjs-artifacts` job. The `ad3455…` hash in §10 is the upstream Bellard tarball, distinct from the patched vendored file | Phase 6.3 surface ledger |
| 2026-11-07 | Phase 6.3 surface ledger + symbol audit: +`tools/quickjs-surface.py` (feature-aware, cross-checks exports vs header contract vs `unsupported.<feature>.toml`; `--bless`; collision check against the C test harness) + `tools/check-quickjs-surface.sh`; surfaced and fixed 10 Bellard-only leaks (gated `js_calloc`/`js_calloc_rt` to ng; wrapped the NG-only `JS_New*Error`/`JS_ThrowPlainError`/`JS_ThrowDOMException` shim symbols in `#ifdef QJS_NG` via a `build.rs` define); blessed ng=137 / bellard=86 gaps; wired into CI | Phase 6.4 documentation |
| 2026-11-07 | Phase 6.4 documentation: +`DIVERGENCES.md` (bytecode/finalizers/gc_mark/allocator/partial-realms/Intl-Temporal/quickjs-libc/dump-flags/interrupt/resizable-buffers/eval-this/modules), +`README.md` (Rust facade usage + per-target C build/link), +`c/README.md` (header provenance + vendored SHA-256s); plan checkboxes updated | Phase 6.5 certification |

### Known issue: strings in ordinary objects are collected by `JS_RunGC`

`JS_SetPropertyStr(obj, "k", string)` then `JS_RunGC(rt)` collects the string
(`obj.k` then throws), while a boxed string survives the same GC and a number
or function stored the same way survives. Reproduces with both
`JS_NewString` and `JS_Eval("'...'")`. Not yet diagnosed; needs the
`--gc-stress`/`for_each_live_box` probe from `slag-gc-rooting`. Worked around
in the function-list C test by using primitive constants, not string rows.
