# Engine API study: what a real V8 host demands

Slag's "embeddable" claim is currently a README adjective. This note derives the
actual contract from a demanding, real-world host instead of from intuition:
**Deno**, because it is Rust integrating V8 through a Rust API, which makes its
engine boundary enumerable, checkable, and directly comparable to ours.

Sources, and how each was used:

| Source | Kind | Used for |
|---|---|---|
| `v8/docs/embed.md`, `v8/include/APIDesign.md` (local checkout at `v8/`) | primary, V8's own | the embedder obligations V8 states in prose |
| `denoland/rusty_v8` at `main` (`src/*.rs`, `src/binding.cc`) | primary, fetched | the engine boundary as a finite Rust API |
| `denoland/deno_core` at `main` (`core/**`) | primary, fetched | the host seams built above that boundary |
| `oven-sh/bun` (local checkout) | secondary, not read in this pass | the Node-ecosystem-on-a-foreign-engine precedent |

Everything below is verified against those sources except where marked
*inferred*. Fetches were against `main` (2026-09-20); the numbers will drift.

## 1. Why Deno is the reference host

- **The boundary is Rust, not C++.** Deno consumes V8 as the `v8` crate. There is
  no FFI language gap between that boundary and Slag, so "Deno on Slag" reduces to
  "implement the `v8` crate's Rust API over Slag's engine" — bounded and testable.
- **It already solved our layering problem.** `rusty_v8` is the engine boundary;
  `deno_core` is everything a host builds above it. That is the `slag-api` → host
  split, done against a real engine for years.
- **The size is finite and measurable.** `src/binding.cc` is **5,110 lines**
  referencing **321 distinct `v8::` symbols** (nested names included); the literal
  `extern "C"` appears only 30 times, so the entry points are macro-generated and
  the symbol count is the better size signal. The crate's `src/` is ~68 entries.
- **The frequency histogram is a priority order**, from `binding.cc` symbol counts:
  `Value` 247, `Isolate` 220, `Context` 130, `String` 89, `Object` 76, `Data` 59,
  `ScriptCompiler` 47, `Local` 41, `ArrayBuffer` 31, `Module` 28,
  `ValueSerializer` 27, `PropertyDescriptor` 27, `BackingStore` 26, `platform` 24,
  `Function` 23, `ValueDeserializer` 22, `Promise` 18, `Platform` 18, `Name` 17,
  `FunctionTemplate` 17, `Message` 16, `TryCatch` 15, `ResourceConstraints` 14,
  `ObjectTemplate` 14, `StackFrame` 13.

  Read that as: the core value API dominates (Slag has it), then **script identity
  and code cache** (`ScriptCompiler`, larger than any other non-core type), then
  host memory, module machinery, structured serialization, platform, and
  structured errors. None of those are facade details.

**Caveat — the trap of learning from Deno.** Deno inherits the hard half of GC
integration from V8 (`TracedReference`, `cppgc`). It teaches us the *requirement*
but not the *implementation*; for that the local `v8/include/cppgc/`,
`v8-embedder-heap.h` and `v8-traced-handle.h` are the reference. Also
`serde_v8`, `fast_string.rs` and the `v8_use_custom_libcxx` /
`v8_enable_pointer_compression` build features are V8-specific by construction:
the architecture transfers, those modules do not.

## 2. What V8 states as the embedder's obligations

From `v8/include/APIDesign.md` and `v8/docs/embed.md`, in V8's own words:

- **Platform.** "V8 requires access to certain OS-level primitives such as the
  ability to schedule work on threads, or allocate memory. The embedder can define
  how to access those primitives via the `v8::Platform` interface… embedders are
  *highly encouraged* to implement `v8::Platform` themselves."
- **Buffer memory.** "the `v8::ArrayBuffer::Allocator` is passed to the
  `v8::Isolate` factory method… all instances of V8 should share one allocator."
- **Global handles.** "If the embedder wishes to free all memory associated with
  the `v8::Isolate`, it has to first clear all global handles associated with that
  `v8::Isolate`." Clearing persistent handles is the *host's* job.
- **Exceptions are values.** "V8's C++ code doesn't use C++ exceptions. Therefore,
  all API methods that can throw should indicate so by returning a `Maybe` /
  `MaybeLocal`… and by taking a `Local<Context>` parameter."
- **Inspector.** "All debugging capabilities of V8 should be exposed via the
  inspector protocol."
- **Handles come in flavors** because V8's collector *moves* objects and rewrites
  handles: `Local` (handle scope, stack-embedded, cannot be `new`ed) versus
  `Persistent` / `UniquePersistent` / `Eternal`, with `SetWeak` for a GC callback.
- **Context cost + snapshot.** "the first context you create is somewhat
  expensive, subsequent contexts are much cheaper… With the V8 snapshot feature…
  the time spent creating the first context will be highly optimized."

The obligations above map one-to-one onto the gaps found by reading Slag's source
(`crates/crux/src/heap.rs:16-18` soundness invariant and slot reuse;
`crates/crux/src/host.rs:19-111` behavioral-only host objects;
`crates/runtime/src/embed.rs:769-773` string stack traces). The study confirms the
L0-L3 ladder; it does not replace it.

## 3. The engine boundary, as `rusty_v8` requires it

Each row is a requirement with the API that expresses it. "Slag" is the status
found in this repository.

### 3.1 Handles, roots and lifetimes

| Requirement | rusty_v8 evidence | Slag status |
|---|---|---|
| Scoped local handles tied to a lifetime | `Local<'s, T>(NonNull<T>, PhantomData<&'s ()>)`, `SealedLocal` | `api::Local` is a bare `Value`, documented as needing no rooting (`api/handle.rs:9-14`) — stale since the arena GC |
| Scope objects that own handle storage | `HandleScope<'s, C>`, `EscapableHandleScope::escape`, `PinnedRef`, `ScopeStorage` | markers only (`api/handle.rs:196-219`) |
| Persistent handles across calls | `Global<T>::new(isolate, handle)`, `into_raw`, `Handle` trait | `Global` holds a `Value` with no collector registration |
| **Weak** handles with GC callbacks | `Weak<T>::new`, `with_finalizer`, `with_guaranteed_finalizer`, `clone_with_finalizer`, `to_global`, `WeakData<T>` | none. Host-object finalization is `Rc`-drop driven (`crates/jsc/src/lib.rs:21-23`), so cycles never finalize |
| **Traced** references (host object references a JS value) | `TracedReference<T>::new/get/reset`, `Traced` | none; host-held references are invisible to the collector |
| Never-collected handles | `Eternal<T>::set/get/clear` | none |
| Per-scope typed slots | `PinnedRef::get_slot/set_slot/remove_slot` | none |

### 3.2 Isolate lifecycle and limits

`CreateParams` (`isolate_create_params.rs`) is itself the requirements list:
`snapshot_blob(StartupData)`, `array_buffer_allocator`, `external_references`,
`allow_atomics_wait`, **`heap_limits(initial, max)`**, `max_old_generation_size`,
`max_young_generation_size`, `code_range_size`, **`stack_limit(*mut u32)`**,
`cpp_heap(UniqueRef<Heap>)`, `counter_lookup_callback`,
`configure_defaults_from_heap_size`.

`Isolate` adds: `new_with_group`, per-isolate host data (`set_data`/`get_data`,
`get_number_of_data_slots`, typed `get_slot`/`set_slot`), `set_default_context`,
`add_context`, `add_isolate_data` / `add_context_data` (**values restored from a
snapshot**), `get_heap_statistics`, `set_idle`, `memory_pressure_notification`,
`low_memory_notification`, `add_near_heap_limit_callback`, `set_oom_error_handler`,
`add_gc_prologue_callback` / `add_gc_epilogue_callback`,
`adjust_amount_of_external_allocated_memory`, `set_microtasks_policy` /
`perform_microtask_checkpoint`, `set_promise_hook`, `set_promise_reject_callback`,
`request_garbage_collection_for_testing`, `take_heap_snapshot`.

Slag has GC knobs (`set_nursery_threshold`, `set_gc_stress`, `set_gc_trace` —
`crates/runtime/src/embed.rs:289-321`) but no heap cap, no near-limit callback, no
memory-pressure signal, no external-memory accounting, and no host data slots.

### 3.3 Execution control

| Requirement | rusty_v8 evidence | Slag status |
|---|---|---|
| Terminate a runaway script | `IsolateHandle::terminate_execution` / `cancel_terminate_execution` / `is_execution_terminating`, `request_interrupt` | none |
| Detect termination in a handler | `TryCatch::has_terminated`, `can_continue` | none |
| Forbid JS execution during a host callback | `DisallowJavascriptExecutionScope`, `AllowJavascriptExecutionScope` | none |
| Refuse string compilation | `set_modify_code_generation_from_strings_callback` | `HostHooks::ensure_can_compile_strings` (`crates/runtime/src/host.rs:23-31`) |

A server-style host needs termination for timeouts, not just for `eval` policy.

### 3.4 Platform, tasks and microtasks

- `PlatformImpl: Send + Sync` with `new_custom_platform` — the host implements it;
  plus `new_default_platform`, `new_single_threaded_default_platform`,
  `pump_message_loop`, `run_idle_tasks`, `Task::run`, `IdleTask::run`,
  `Isolate::has_pending_background_tasks`. Slag has nothing: all work runs inline
  on the calling thread.
- `MicrotaskQueue::new/enqueue_microtask/perform_checkpoint/is_running_microtasks`
  plus the isolate-wide policy. Slag's `api::Context` requires an explicit
  `run_microtasks` and `eval` does not drain at all
  (`crates/runtime/src/api/context.rs:65-84`).

### 3.5 Host memory

`ArrayBuffer::Allocator` (a host vtable), `BackingStoreDeleterCallback`,
`BackingStore` (`data`, `byte_length`, `is_shared`,
`is_resizable_by_user_javascript`), constructors
`new_backing_store`, `new_backing_store_from_boxed_slice`,
`new_backing_store_from_vec`, `new_backing_store_from_bytes`, and
`ArrayBuffer::new` / `with_backing_store` / `detach` / `set_detach_key` /
`was_detached` / `is_detachable`.

Slag's blocks own their storage (`Rc<RefCell<Vec<u8>>>`, `crates/byteblock/src/lib.rs:147-157`).
Aliasing *out* works (`SharedBuffer::data_ptr`, used for wasm linear memory);
wrapping host memory and detach semantics do not exist.

### 3.6 Compilation, script identity and code cache

`ScriptCompiler::Source`, `CachedData` (with `rejected()` — the host can reject a
stale cache), `NoCacheReason`, `CompileOptions`, `compile`, `compile_module`,
`compile_module2`, `compile_function`, `compile_unbound_script`, and a
`cached_data_version_tag()`. `UnboundScript` and `UnboundModuleScript` then bind to
a context: `bind_to_current_context`, `script_id`, `create_code_cache`,
`get_source_url`, `get_source_mapping_url`.

This is why `ScriptCompiler` is the largest non-core type in the shim: a host needs
to *name* a script (URL, source-map URL, id) and to cache its compilation, not just
evaluate a string. Slag's `api::Script::compile(context, source)` has no origin
(`crates/runtime/src/api/script.rs:8-22`) and there is no code cache.

### 3.7 Modules

`Module`: `get_status`, `get_exception`, `get_module_requests`,
`instantiate_module` / `instantiate_module2`, `evaluate`,
`evaluate_for_import_defer`, `get_module_namespace` (+ `_with_phase`),
`has_top_level_await`, `is_graph_async`, `is_source_text_module` /
`is_synthetic_module`, `create_synthetic_module` + `set_synthetic_module_export`,
`get_unbound_module_script`, `get_stalled_top_level_await_message`,
`source_offset_to_location`, `script_id`, `get_identity_hash`; plus
`ResolveModuleCallback`, `ResolveSourceCallback`, `StalledTopLevelAwaitMessage`,
`Location`.

Host hooks on the isolate: `set_host_import_module_dynamically_callback`,
`set_host_initialize_import_meta_object_callback`,
`set_host_import_module_with_phase_dynamically_callback`,
`set_host_create_shadow_realm_context_callback`.

Slag's engine has the module machinery; the host-facing half is missing (no
resolver, no `import.meta` properties, no synthetic modules, no
`source_offset_to_location`).

### 3.8 Promises, errors and positions

- `Promise`: `state`, `has_handler`, `mark_as_handled`, `result`, `then`, `then2`,
  `catch`, `new`, `resolve`, `reject`; `PromiseRejectEvent`,
  `PromiseRejectMessage::get_promise/get_event/get_value`.
- `Message`: `get`, `get_stack_trace`, `get_source_line`,
  `get_script_resource_name`, `get_start_position` / `get_end_position`,
  `get_wasm_function_index`, `error_level`, `is_shared_cross_origin`, `is_opaque`.
- `StackFrame`: `get_frame_count` / `get_frame`, `get_line_number`, `get_column`,
  `get_script_id`, `get_script_name`, `get_script_name_or_source_url`,
  `get_script_source`, `get_script_source_mapping_url`, `get_function_name`,
  `is_eval`, `is_constructor`, `is_wasm`, `is_user_javascript`.
- `Exception::current_stack_trace`, `current_script_name_or_source_url`;
  `set_prepare_stack_trace_callback`, `add_message_listener`,
  `set_capture_stack_trace_for_uncaught_exceptions`.

Slag captures stacks as rendered text (`Agent::error_stack:
HashMap<u64, JsString>`, `crates/runtime/src/agent.rs:769-773`) with no frames,
positions, script ids, or source-map URLs. Devtools and source-mapped errors are
both impossible on that representation.

### 3.9 Serialization, cppgc, fast calls, locking

- **Structured clone:** `ValueSerializer` / `ValueDeserializer` (~49 references).
  Slag has none; workers and `postMessage` need it.
- **cppgc:** `initialize_process(platform)`, `Heap::create` / `terminate`,
  `Visitor::trace`, `Traced`, `GarbageCollected`, `Member` / `Ref` / `UnsafePtr` /
  `GcCell`, `HeapCreateParams`, `EmbedderStackState`, `MarkingType`, `SweepingType`.
  This is the host *participating* in GC, and Deno uses it (`core/cppgc.rs`).
- **Fast calls:** `fast_api.rs` (`v8::CFunction`) — the op fast path. Performance
  only, not correctness.
- **Locking:** `SharedIsolate::lock` → `Locker`, `thread_safe_handle` →
  `IsolateHandle` (cross-thread terminate/interrupt).

## 4. The host seams above the boundary (deno_core)

| Seam | Evidence in `deno_core/core/` | Slag implication |
|---|---|---|
| Runtime + realm lifecycle | `runtime/jsruntime.rs`, `runtime/jsrealm.rs`, `runtime/setup.rs`, `runtime/snapshot.rs`, `runtime/exception_state.rs` | needs isolates/realms we can create, snapshot and tear down |
| Module loading | `modules/loaders.rs`: `trait ModuleLoader { resolve, load, load_external_source_map, source_map_source_exists }`, `ModuleLoadResponse`, `ModuleLoadOptions`, `ModuleLoadReferrer`, `ExtCodeCache`; `modules/map.rs`, `modules/recursive_load.rs` | the resolver must be a **host trait**, not a pre-registered map |
| Ops | `ops.rs` (`OpId = u16`, `PromiseId = i32`, `OpCtx`, `OpState`, `OpMetadata`, `ReentrancyGuard`, `ExternalOpsTracker`), `runtime/ops.rs`, `runtime/ops_rust_to_v8.rs`, `runtime/op_driver/` | host functions at scale; fast-call layout is the perf story |
| Event loop | `event_loop.rs`, `reactor.rs`, `reactor_tokio.rs`, `tasks.rs`, `web_timeout.rs`, plus the JS bootstrap `00_primordials.js` / `01_core.js` | the host owns scheduling; the engine must hand work over |
| Inspector + source maps | `inspector.rs` (`JsRuntimeInspector`, `InspectorSessionProxy`, `V8InspectorClient` impl), `source_map.rs` (`SourceMapper`, `SourceMapApplication`, `get_source_line`) | protocol/DTO work is portable; the engine hooks are ours to build |
| cppgc | `cppgc.rs` (`make_cppgc_object`, `wrap_object`, `try_unwrap_cppgc_object`, `Ref`/`Member`, `FunctionTemplateData`, `FunctionTemplateSnapshotData`) | wrapping a native object requires traced host objects |
| Snapshots | `runtime/snapshot.rs`, feature `include_js_files_for_snapshotting`, `PinnedRef::get_isolate_data_from_snapshot_once` | the host's own JS bootstrap is snapshotted, then per-isolate data is recovered by index |
| Intl data | feature `include_icu_data`, `deno_core_icudata` | Slag ships Intl tables in-tree; the plumbing differs |

## 5. Build order this implies for Slag

1. **Roots and handles** — handle scopes, `Global`, `Weak` (+ finalizer), `Traced`.
   This is `L1`/`L2` and everything else is untestable without it.
2. **Platform + microtask policy + task runner** — the host must own scheduling.
3. **Snapshot + external references + per-isolate/context data slots** — without
   these, every isolate re-parses and re-runs the host's JS bootstrap.
4. **Module resolver as a host trait**, plus unbound scripts, code cache and
   script origins (`ScriptCompiler` / `Source` / `CachedData` equivalents).
5. **Structured frames** (`Message`/`StackFrame` shape) and **termination /
   interrupt**, which a server host needs for timeouts.
6. **Host memory**: an allocator hook, `BackingStore`-style external blocks with
   detach semantics.
7. **Inspector + source maps.**
8. **Traced host objects** (a cppgc-equivalent or a simpler traced-handle model).
9. **Structured clone** (`ValueSerializer` equivalent).
10. **Fast-call ops** — performance, last.

One asymmetry worth banking: V8's handle scopes exist largely because its collector
*moves* objects and must rewrite handles. Slag's arena keeps stable addresses and
reuses swept slots, so a handle scope for Slag needs **rooting but no update
machinery** — `L1` is materially cheaper for us than it was for V8. The corollary
is that slot reuse makes an unrooted handle *silently alias* a different object
rather than crash, which is why (1) must land before any host-facing surface ships.

## 6. The acceptance test

Stated so it can be falsified:

1. **Small:** a shape-compatible `v8`-crate subset over Slag's engine, sufficient
   for a hand-written host that creates an isolate, runs a script with a script
   origin, registers one host op, loads one module from disk through a
   `ModuleLoader`-shaped trait, drains microtasks, and surfaces a stack with
   frames.
2. **Real:** `deno_core` compiles against that crate and its smallest host example
   runs. This is the goal that makes the claim checkable, because `deno_core`
   depends on the `v8` crate's API rather than on V8 internals.
3. **Regression net:** run test262 through the new API, not only through the Rust
   engine, so the boundary itself is covered by the corpus we already own.

## 7. What we deliberately do not copy

- V8's handle-update machinery (a consequence of a moving collector).
- `fast_api` / `v8::CFunction` (perf only; revisit if ops get hot).
- V8 build features (`use_custom_libcxx`, pointer compression, the V8 sandbox).
- `serde_v8` and `fast_string` internals (V8 value-layout specific).
- ShadowRealm plumbing (`set_host_create_shadow_realm_context_callback`), if we
  keep ShadowRealm out of scope.

## 8. Risks and open questions

1. **`Weak` + `Traced` require GC-visible host objects.** Today host objects are
   `Rc`-backed with "no GC heap edges" (`crates/crux/src/object.rs:752-762`) and
   finalize on the last `Rc` drop. Whether Slag grows a cppgc-equivalent or a
   simpler traced-handle model is a design decision that gates `L2`.
2. **The conservative stack scan is not a substitute for traced handles.** It saves
   Rust-stack locals; it cannot see host heap buffers, which is precisely where a
   host keeps its references.
3. **Snapshot format is a compatibility surface.** External references must be
   index-stable across builds, and the format needs a version tag (V8 has
   `cached_data_version_tag()` and `StartupData::can_be_rehashed` for the same
   reason).
4. **Error model at the boundary.** `ffi/src/guard.rs:4-19` converts a panic into
   `R::default()`. A host-facing ABI must not do that; V8's answer is `Maybe` /
   `MaybeLocal` plus a context parameter, with aborts reserved for invariant
   violations.
5. **Priority risk.** The shim histogram says `ScriptCompiler`, `ArrayBuffer` /
   `BackingStore` and `Module` outrank the inspector. Inspector is the most
   expensive item and should not lead.

## 9. Verification status

- Read locally: `v8/docs/embed.md`, `v8/include/APIDesign.md`,
  `v8/docs/node-integration.md`, `v8/include/` (header inventory).
- Fetched from `main`: `rusty_v8/src/{binding.cc,handle,scope,isolate_create_params,platform,microtask,snapshot,external_references,locker,isolate,script_compiler,unbound_script,unbound_module_script,module,promise,exception,array_buffer,cppgc}.rs`;
  `deno_core/core/{ops,event_loop,inspector,source_map,cppgc}.rs`,
  `core/modules/{mod,loaders}.rs`, `core/Cargo.toml`, plus the `core/` and
  `core/runtime/` inventories.
- Measured: `binding.cc` line count, distinct-`v8::`-symbol count, and the
  type-frequency histogram.
- Not done: Node's own `src/` was not examined (V8 has a dedicated Node-CI bot and
  maintains a Node fork to keep it building against V8's main branch, per
  `v8/docs/node-integration.md`, so Node is a *co-maintained source* dependency
  rather than an ABI to satisfy). Bun's N-API layer was not read in this pass.
