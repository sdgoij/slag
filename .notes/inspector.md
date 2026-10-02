# The inspector: scoping the Runtime domain the REPL needs

**Status:** the Runtime subset the REPL drives is **landed** in
`crates/v8/inspector.rs` — `Runtime.enable`, `Runtime.evaluate` and
`Runtime.callFunctionOn`, evaluated on the running isolate. `deno repl` works on
a Deno built with `tools/build-deno.py` (verified 2026-10-02). Everything past
the Runtime domain is still refused.

## 1. The failure

`deno repl` dies on its first protocol message:

```
thread 'main' panicked at crates/v8/inspector.rs:444:
v8::inspector::V8InspectorSession::dispatch_protocol_message: Slag has no
inspector: no breakpoints, no stepping, no debug protocol, and no way to stop a
running script
```

That is `crates/v8/inspector.rs`'s `NO_INSPECTOR` (line 55) doing what its
module doc says it does: creation, the context lifecycle and a connection are
**inert** (`create`, `connect`, `context_created`, `context_destroyed`,
`create_stack_trace`, `exception_thrown` all succeed and record nothing), and
every **protocol** method refuses — `can_dispatch_method` answers `false` and
`dispatch_protocol_message` / `schedule_pause_on_next_statement` /
`cancel_pause_on_next_statement` panic. The refusal is deliberate: a host that
boots an inspector for every runtime (deno_core does) must be able to *connect*,
so the loud failure is at the first message a debugger sends.

This was not a regression — nothing was implemented then. It is now: see §7.

## 2. Where the message goes

`deno_core` owns part of CDP in Rust and forwards the rest to V8:

- `libs/core/inspector.rs:1626` `SessionContainer::dispatch_message_from_frontend`
  tries `dispatch_custom_domain` (`:287`) first. That handles `Network.*`,
  `DOMStorage.enable/disable`, and `Debugger.enable/disable` /
  `Page.waitForDebugger` (the last two at `:1640-1655`). Those are answered in
  Rust, no V8 involved.
- Anything else falls through to `InspectorSession::dispatch_message`
  (`:1923`), which calls `self.v8_session.dispatch_protocol_message(msg)` — the
  bridge, and the panic.
- The REPL reaches it through `LocalInspectorSession` (`:2311`), whose
  `dispatch` (`:2324`) calls `dispatch_message_from_frontend` directly. That call
  is **synchronous on the isolate thread**, from inside the REPL's
  `post_message_with_event_loop`, with `SessionContainer`/`InspectorSession`
  holding no isolate handle.

So the Runtime domain lands in the one place that has no way to run JS.

## 3. What the REPL actually drives (the minimal set)

`cli/tools/repl/session.rs` uses exactly this much of CDP:

| Message | Site | Notes |
|---|---|---|
| `Runtime.enable` | `:318` | waits for the `Runtime.executionContextCreated` notification to learn the context id |
| `Runtime.evaluate` | `:968` | `replMode: true`; the REPL wraps the line itself and reads `cdp::EvaluateResponse { result, exceptionDetails? }` |
| `Runtime.callFunctionOn` | `:638`, `:665`, `:697`, `:732` | drives `$`, member access, and the object-printing path; args are `cdp::RemoteObject` |
| `Runtime.executionContextCreated` (notification) | `:329` | `context.id` + `context.auxData.isDefault` are read |
| `Runtime.exceptionThrown` (notification) | `:476` | async throws surfaced on the event loop |

Notably **absent**: the REPL never sends `Runtime.getProperties` or
`Runtime.releaseObject` (they were looked for, no sites). So the common
object-inspection protocol is not on the critical path — but a `RemoteObject`
returned by `evaluate`/`callFunctionOn` still needs an `objectId` the front end
can pass back as a receiver.

## 4. Options

**A — implement the Runtime domain in the bridge (`crates/v8/inspector.rs`).**
V8's inspector lives inside the engine and evaluates on the isolate; the bridge
can do the same. `V8Inspector::create(isolate, client)` already receives
`&mut crate::Isolate` and discards it (`:312`, `let _ = isolate`); it can hold
the raw handle — `Isolate::as_raw_isolate_ptr` exists (`crates/v8/isolate.rs:983`),
exactly the pattern `deno_core`'s own `JsRuntimeInspectorState.isolate_ptr`
uses. `connect` then threads it into the session, and `dispatch_protocol_message`
parses CDP, enters a scope, and evaluates. The bridge already has the raw
material: `test_support::eval` (`:22`), `module.rs:424`, `script.rs`,
`script_compiler.rs`, and the object/property types in `object.rs` /
`property.rs` / `json.rs`.

*Pros:* faithful (one place owns the protocol), no `deno_core` change, no new
host API. *Cons:* the bridge grows a real CDP agent.

**B — intercept `Runtime.*` in `deno_core`'s `dispatch_custom_domain`.**
`Runtime.enable` and the `executionContextCreated` notification could be served
here cheaply. `Runtime.evaluate`/`callFunctionOn` cannot: the dispatcher has no
isolate and no eval entry, so this needs a *new* host hook (a
`Fn(&mut PinScope, script) -> RemoteObject` the runtime supplies) — more moving
parts across `deno_core` + `cli`, and it splits the protocol across two owners.

**Recommendation: A**, scoped to the Runtime subset the REPL uses.

## 5. What "minimal" excludes

Everything the panic message lists and more: breakpoints, stepping, the
`Debugger` domain (beyond deno_core's existing enable/disable bookkeeping),
pause/resume and `schedule_pause_on_next_statement`, `Profiler`, heap and
sampling snapshots, the console API bridge, and `Runtime.getProperties` /
`releaseObject` unless a later need appears. Those stay refused, but with the
same sentence pointing at what *is* missing once the REPL subset is served.

## 6. The hard parts

- **Reentrant eval.** Dispatch happens on the isolate thread, inside the REPL's
  synchronous `post_message`. Entering a scope and evaluating there is what V8
  does, but it means the bridge's eval must tolerate being called from inside
  `JsRuntime`'s event-loop tick (the inspector is polled from
  `poll_event_loop_inner`, `jsruntime.rs:2428`).
- **`RemoteObject` serialization.** `type`, `value`, `objectId`, `description`,
  `unserializableValue` (bigint / -0 / NaN / ±Infinity), and `preview` for
  objects/arrays/functions. This is most of the surface area.
- **Object handles.** A `RemoteObjectId` ↔ `Global` table per session, released
  on context teardown and (if added later) `Runtime.releaseObject`. Must not
  dangle across a GC — the handles are roots, so they keep the objects alive,
  which is V8's semantics too.
- **`replMode` semantics.** Deno's REPL wraps input (`cli/tools/repl/session.rs`
  around `:448`) and expects V8's `replMode` behavior for top-level `let`/`await`
  and completion values; this has to match or the REPL's own parsing double- or
  mis-transforms lines.
- **Exceptions.** `exceptionDetails` on the response *and* a
  `Runtime.exceptionThrown` notification for async throws, with the shape
  `cli`'s `cdp` types deserialize.
- **The context-id contract.** The REPL reads `context.id` and
  `auxData.isDefault` from `executionContextCreated` and passes the id back as
  `contextId` (it notes "our inspector does not support a default context (0 is
  an invalid context id)", `session.rs:321-322`).

## 7. Phased plan

**Landed (phases 1-3).**

1. **The domain boots.** `Runtime.enable` answers `{}` and emits one
   `Runtime.executionContextCreated` (`id: 1`, `auxData.isDefault: true`).
2. **`Runtime.evaluate`.** Compiles and runs the expression as a script in the
   realm `context_created` named, then serializes the value as a `RemoteObject`:
   primitives by value, `NaN`/`±Infinity`/`-0`/bigint as `unserializableValue`,
   objects/arrays/functions by `objectId` from a pinning handle table.
3. **`Runtime.callFunctionOn`.** Receiver by `objectId` (else the realm's
   global), arguments by value/`objectId`/`unserializableValue`; this is the
   path the REPL's `$`, member access and printer use.

Verified from `deno repl` on a Deno built by `tools/build-deno.py`: `1 + 1` →
`2`, `let x = 21` → `undefined` then `x * 2` → `42` (top-level `let` persists),
`[1, 2, 3]` and `({ a: 1, b: 2 })` print via `callFunctionOn`, `1 / 0` →
`Infinity`, `foo.bar` → `Uncaught ReferenceError: "foo" is not defined`.

**Remaining.**

4. **The async-throw notification — landed; `preview` — deliberately not.**
   `Runtime.exceptionThrown` is now broadcast to every connected session from
   `V8Inspector::exception_thrown`, which `deno_core` calls for an uncaught
   exception (unit-tested: a connected recorder receives the event). A
   structured `preview` is *not* emitted: Deno's own `cdp::RemoteObject`
   (`deno/cli/cdp.rs`) has no `preview` field, and the REPL prints through its
   own `callFunctionOn` calls, so a preview here would be dead bytes for every
   consumer in this tree. Add it only when a front end that reads it (Chrome
   DevTools) becomes a target.

   **Blocker found while testing, and fixed.** Driving an uncaught throw
   through the REPL panicked inside the bridge:
   `bridge bug: a handle was made with no handle scope open`
   (`crates/v8/store.rs:218`), reached from
   `Isolate::perform_microtask_checkpoint` -> a snapshot host callback ->
   `Local::from_engine` with no region open. No `inspector` frame was on the
   stack, and `deno eval` was fine with the same input. The cause: the bridge's
   `run_microtasks` / `perform_microtask_checkpoint` are host entry points that
   drain jobs, but `deno_core` calls them on the bare isolate with no handle
   scope open, so a job that invokes a host callback had no region. Each now
   opens a scope for the drain (`crates/v8/isolate.rs`). Verified: the microtask
   throw now prints `Uncaught Error: micro boom` through
   `Runtime.exceptionThrown`, with no panic.
5. **Re-release the refusal.** The `Runtime` methods above are answered and
   `can_dispatch_method` says so; every other method (the `Debugger`/`Profiler`
   domains, `getProperties`, stepping, pause) still aborts with `NO_INSPECTOR`,
   which now reads as "everything past the Runtime domain", not "no inspector".
   The module doc is updated to match; the tests that pinned the blanket refusal
   were narrowed to the debugger domain.

## 8. Risks

- The reentrancy contract (6, first bullet) is the one that can bite: if the
  bridge's eval cannot run from inside the event-loop tick, the whole approach
  moves back to option B.
- Protocol drift: matching V8's `RemoteObject` shapes exactly is
  what `cli`'s `cdp` types demand, and they deserialize strictly.
- Handles are roots; without `releaseObject` the table grows for the life of a
  session. Acceptable for a REPL, worth bounding before any `--inspect` claim.

## 9. The host-entry-point scope scan

Fixing the microtask drain (§7) prompted a scan for the same shape elsewhere: a
bridge entry point the engine or `deno_core` invokes *outside* any handle scope,
which then runs a job or a host callback that builds handles — a panic in
`store::cell` ("a handle was made with no handle scope open"). Found and fixed:

- `Isolate::run_microtasks` / `perform_microtask_checkpoint` (`crates/v8/isolate.rs`) — the original, exercised from `deno repl`.
- `MicrotaskQueue::perform_checkpoint` (`crates/v8/microtask.rs`) — the host-owned queue deno's `vm` drains; the same drain, with no scope.
- `BridgeHooks::promise_rejection_tracker` (`crates/v8/isolate.rs`) — invoked the host's reject callback with no scope, unlike its sibling `prepare_stack_trace`, which opens a `callback_scope!`.

Already correct, checked: the other `HostHooks` callbacks (`prepare_stack_trace`, `wasm_streaming`, `import_module_dynamically`, `initialize_import_meta_object`, `allow_wasm_code_generation`), the module resolver/loader callbacks, the interceptor callbacks, `Function::call`, and `Platform::run_idle_tasks` all open a scope. `IsolateGcObserver::run` (`crates/v8/heap.rs`) invokes GC callbacks with no scope, but V8's contract is that a GC callback allocates no JS, so it is left as is.

The stray `Error:` line above a report is not this class: it is the REPL's own
`closing()` probe (`deno/cli/tools/repl/mod.rs:67`), whose `Runtime.callFunctionOn`
returns no value because the uncaught throw terminated execution. The same throw
is still reported once through `Runtime.exceptionThrown`, and the REPL recovers
on the next line.
