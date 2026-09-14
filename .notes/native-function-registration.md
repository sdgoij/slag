# Registering native functions through the public embedding API — proposal

> **Status:** all four slices landed — `HostFn`, `FunctionCall` (including the
> re-entrant `call`/`construct`/`eval`, `is_construct`, and `new_target`),
> `Context::{create_function, create_constructor, create_object, register_fn,
> define_accessor}`, the `ErrorKind` re-export, and `JsValue::thrown`, with tests
> in `embed.rs` and the `examples/embed.rs` walkthrough.

## 1. Summary

The public embedding surface (`crates/slag`, re-exporting `runtime::embed`) can
evaluate scripts, call and construct script-defined functions, expose host
*values* as globals, and exchange objects/arrays/buffers/JSON. It cannot
register a Rust closure as a callable function that JavaScript can invoke.

This proposal adds native-function registration to `embed`, with three public
entry points (`Context::create_function`, `Context::create_object`,
`Context::register_fn`), a callback type that receives `this`/arguments and a
re-entrant handle to the engine, and the error affordances a host needs to throw
from Rust. The internal machinery already exists and is exercised by several
in-tree consumers; the work is a facade plus its contracts, not a new
mechanism.

## 2. Current state (evidence)

### 2.1 What the public API exposes today

`crates/slag/src/lib.rs` re-exports:

- `Context`, `HostCallbacks`, `JsObject`, `JsValue`, `OutputFn`, `RandomFn`
- `HostHooks`, `JsError`, `runtime::dump`, and (feature `jit`) `install_jit`

`runtime::embed::Context` (`crates/runtime/src/embed.rs`) provides `new`, `eval`,
`eval_script`, `eval_jsx`, `call`, `construct`, `run_jobs`, `global`,
`set_global`, the coercion/JSON/buffer helpers, and the host-module installers
(`install_console`/`install_timers`/`install_fs`/`install_rlx`/`install_raylib`).

None of these turns a Rust closure into a JS-callable value. `JsValue` has
constructors for `undefined`/`null`/`boolean`/`number`/`string` only. `JsObject`
can `set`/`define` properties, but there is no way to produce a function value
to put in them. An embedder's only workaround is to write a JS shim and route
through `set_global` + `eval`, which cannot see host state except through
globals.

### 2.2 The machinery exists internally

| Consumer | Call shape |
|---|---|
| `context.rs` (host globals) | `Function::create_builtin(name, len, call, None, None)` then `create_data_property_or_throw` |
| `embed.rs` `install_fs`/`install_timers`/`install_console` | same; `current_agent_mut()` (TLS) inside the closure to reach the agent |
| `runtime::api::FunctionTemplate` (`api/template.rs`) | host functions + accessors + constructors, but this is the **V8-shaped** API, not part of the `slag` facade |
| `crates/jsc/src/object.rs` `JSObjectMakeFunctionWithCallback` | the same `Function::create_builtin`, proving the capability is not runtime-internal |
| `crates/slag/examples/wasm_binding/dom.rs` | registers `el.*`/`dom.*` native methods — but only by depending on `crux` directly, which the facade deliberately hides |

The core primitive is `crux::function::Function::create_builtin`
(`crates/crux/src/function.rs`):

```rust
pub fn create_builtin(
    name: Option<JsString>,
    length: u64,
    call: NativeFn,
    construct: Option<NativeCtor>,
    prototype: Option<Handle<JsObject>>,
) -> Result<Handle<Function>, JsError>;

pub type NativeFn = Box<dyn Fn(&Value, &[Value]) -> Result<Value, JsError>>;
```

It is agent-free (safe to call with `&self`), and the closure it stores is `Fn`
(no `&mut` capture). An error returned from a native call is converted to a real
thrown Error object (`builtins::error::to_throwable`, e.g. `runtime/src/eval.rs`,
`function.rs::ordinary_call`), so `Result` already models `throw` correctly.

### 2.3 The three concrete blockers

1. **No facade method** to build a callable `JsValue` and no plumbing of the
   realm's `%Function.prototype%` (a global function created with a `None`
   prototype reproduces the historical conformance bug recorded in
   `.notes/conformance.md`).
2. **`ErrorKind` is not public.** `JsError` is re-exported but hosts cannot call
   `JsError::new(kind, message)` because `crux::error::ErrorKind` is not.
3. **No way to throw an arbitrary value.** `JsError::with_value` takes
   `crux::value::Value`, which the facade does not expose, so a host cannot
   `throw "some string"` or `throw { code: 7 }`.

## 3. Goals and non-goals

**Goals**

- Register, from Rust, a non-constructible callable value usable anywhere in JS.
- Attach native functions to the global object or to any host-held object
  (namespaces).
- Let a host function synchronously call back into JS and coerce values.
- Throw engine errors and arbitrary JS values from Rust.
- Stay representation-independent: do not expose `crux::value::Value` (PLAN.md
  §4.1 keeps the public API stable across the NaN-boxing change).

**Non-goals (this proposal)**

- The V8-shaped `runtime::api` surface — it already covers templates/accessors
  and is not the facade.
- Host *classes* / `new`-able constructors and accessor properties on
  `JsObject`. Both are natural Phase B/C follow-ups (sections 9.3, 9.4), out of
  scope for the first cut.
- WASM/JSC drop-ins — they have their own bindings.

## 4. Proposed API

### 4.1 Types

```rust
/// A native (host) function body.
pub type HostFn = Box<dyn Fn(&FunctionCall<'_>) -> Result<JsValue, JsError> + 'static>;

/// The invocation a host function receives: `this`, the arguments, and a
/// re-entrant handle to the running engine.
pub struct FunctionCall<'a> { /* this, args, construct flag, agent pointer */ }

impl FunctionCall<'_> {
    pub fn this(&self) -> JsValue;
    pub fn length(&self) -> usize;
    pub fn arg(&self, index: usize) -> Option<JsValue>;
    /// The arguments as a slice (zero-copy; see §6.2).
    pub fn args(&self) -> &[JsValue];

    // Re-entrancy (§5).
    pub fn call(&self, function: &JsValue, this: &JsValue, args: &[JsValue])
        -> Result<JsValue, JsError>;
    pub fn construct(&self, constructor: &JsValue, args: &[JsValue])
        -> Result<JsValue, JsError>;
    pub fn eval(&self, source: &str) -> Result<JsValue, JsError>;

    // Coercions a host function commonly needs (agent-backed).
    pub fn to_number(&self, value: &JsValue) -> Result<f64, JsError>;
    pub fn to_string(&self, value: &JsValue) -> Result<String, JsError>;
}
```

`this()`/`arg()`/`args()` return `JsValue`, so the callback never sees `Value`.

### 4.2 `Context` methods

```rust
impl Context {
    /// CreateBuiltinFunction (spec 10.2.3) over `callback`, linked to this
    /// realm's %Function.prototype%. Non-constructible.
    pub fn create_function(
        &self,
        name: &str,
        length: u32,
        callback: HostFn,
    ) -> Result<JsValue, JsError>;

    /// A plain object inheriting %Object.prototype% — a namespace to hang
    /// functions on, without the `eval("({})")` workaround.
    pub fn create_object(&self) -> Result<JsObject, JsError>;

    /// `create_function` + define on the global object (matches `set_global`'s
    /// attribute choice). Roots the function immediately.
    pub fn register_fn(
        &mut self,
        name: &str,
        length: u32,
        callback: HostFn,
    ) -> Result<JsValue, JsError>;
}
```

Namespaces then compose with the existing object API:

```rust
let math = context.create_object()?;
math.set("hypot", context.create_function("hypot", 2, Box::new(|call| {
    let a = call.arg(0).and_then(|v| v.as_number()).unwrap_or(0.0);
    let b = call.arg(1).and_then(|v| v.as_number()).unwrap_or(0.0);
    Ok(JsValue::number(a.hypot(b)))
}))?)?;
context.set_global("host", math.as_value())?;
// JS: host.hypot(3, 4) === 5
```

### 4.3 Errors

Re-export `ErrorKind` from `slag`, and add one helper for value throws:

```rust
pub use crux::error::{ErrorKind, JsError};

impl JsValue {
    /// A `JsError` that throws this value verbatim (spec `throw`), for
    /// `Err(arg.thrown())` from a host function.
    pub fn thrown(&self) -> JsError;
}
```

`to_throwable` returns `error.value` directly when present, so `thrown()`
produces an exact-value throw while `JsError::new(ErrorKind::TypeError, …)`
produces a real `TypeError` object. Optional ergonomics: `JsError::type_error`,
`range_error`, `syntax_error` constructors — cheap, but deferrable.

### 4.4 Worked example

```rust
use slag::{Context, ErrorKind, HostFn, JsError, JsValue};

fn main() -> Result<(), JsError> {
    let mut context = Context::new()?;

    // Simple host function.
    context.register_fn("now_ms", 0, Box::new(|_call| {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        Ok(JsValue::number(ms as f64))
    }))?;

    // A host function that calls back into JS: apply(f, x) === f(x).
    let apply: HostFn = Box::new(|call| {
        let f = call.arg(0).ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "apply: expected a function".into())
        })?;
        let x = call.arg(1).unwrap_or_else(JsValue::undefined);
        call.call(&f, &JsValue::undefined(), &[x])
    });
    context.register_fn("apply", 2, apply)?;

    assert_eq!(context.eval("typeof now_ms")?.as_string().as_deref(), Some("function"));
    assert_eq!(context.eval("apply(x => x + 1, 41)")?.as_number(), Some(42.0));
    Ok(())
}
```

## 5. Re-entrancy: how a host function calls back into JS

This is the load-bearing design decision. `Agent` is owned by value inside
`Context`, and `Context::eval`/`call` borrow it `&mut` for the duration of the
call, so a callback cannot be handed `&mut Context` (aliasing).

The engine already solves this everywhere: native closures run inside a
`crux::function::with_agent` window and recover the agent through the TLS
pointer (`crux::function::current_agent()`; see `embed::current_agent_mut`,
`FunctionCallbackInfo` in `api/template.rs`, and the `runtime::api`
`Context::with_agent`). `FunctionCall` resolves that same pointer when it needs
the agent (its coercions today; `call`/`construct`/`eval` in slice 2), each
doing the same `unsafe { &mut *agent }` dance under the same invariant
(`with_agent` guarantees the pointer is live for the whole synchronous call).
Nested `crux::function::with_agent` re-entry is already supported (it skips the
swap when the agent is already current).

Alternatives considered:

| Option | Verdict |
|---|---|
| Callback gets `&JsValue, &[JsValue]` only; re-entrancy via a separate `Context::current()` TLS handle | Two types, discoverability cost; the info-struct already exists internally |
| Make `embed::Context` methods take `&self` (V8-style) and pass `&Context` | Larger rework of a delivered API; `Context` would still alias the agent |
| Queue calls instead of calling synchronously | Wrong semantics for listeners/comparators; deferred work belongs in jobs |
| Callback returns a thunk | Cannot express "call this JS function and use its result now" (e.g. `sort` comparator) |

**Recommendation:** the `FunctionCall` info-struct. It mirrors the internal
`FunctionCallbackInfo` and V8, keeps one callback signature, and leaves room for
`new_target`/construct later without a breaking change.

## 6. Contracts to document and test

### 6.1 GC rooting

Handles are `Gc<T>`; a live handle must be reachable from a root at sweep
(`crates/crux/src/heap.rs` module docs). `register_fn`/`set_global` root the
value through the global object. `create_function` returns a value rooted only
by the host's own stack frame (conservative stack scan) — a `JsValue` stashed in
native heap (e.g. a `Vec` field) across an `eval` may be swept. The rustdoc must
say so and point at "define it on a reachable object" as the rooting rule;
`create_function` is the low-level escape hatch, `register_fn` the safe default.

### 6.2 Zero-copy arguments

`FunctionCall::args()` should be `&[JsValue]` without allocating. `Value` is
`#[repr(transparent)]` over `u64` (`crates/crux/src/value.rs`), and `JsValue` is
a single-field newtype over `Value`; marking `JsValue` `#[repr(transparent)]`
makes `&[Value]` and `&[JsValue]` layout-identical, so one documented `unsafe`
slice cast at the boundary is sound. Guard it with a static assertion
(`size_of`/`align_of` equal) plus a test. Fallback if we want zero unsafe: build
a `Vec<JsValue>` per call (the existing `Context::call` already allocates a
`Vec<Value>` per call, so this is not a regression, merely avoidable).

### 6.3 `Fn`, not `FnMut`

`create_builtin` stores `Box<dyn Fn>` and both existing consumers rely on it.
`HostFn` stays `Fn`; hosts use `Cell`/`RefCell` for mutable state (the timers and
`wasm_binding/dom.rs` already do). Wrapping an `FnMut` in a `RefCell` internally
was rejected: a re-entrant call into the same native function would panic on
`borrow_mut`, and re-entrancy is explicitly supported.

### 6.4 Non-constructible by default

`create_builtin(…, None, …)` yields `FunctionKind::Builtin { construct: None }`;
`new hostFn()` throws, as expected for a plain method. Constructors are Phase B.

## 7. Implementation plan

Files:

- `crates/runtime/src/embed.rs` — `HostFn`, `FunctionCall`, `JsValue::thrown`,
  `Context::create_function`/`create_object`/`register_fn`; reuse
  `current_agent_mut`; wire `%Function.prototype%` via `realm.intrinsics`.
- `crates/slag/src/lib.rs` — re-export `HostFn`, `FunctionCall`, `ErrorKind`.
- `crates/slag/examples/embed.rs` — show registration, a global, a namespace, a
  re-entrant callback, and a value throw.
- `README.md` (§Embedding) and `PLAN.md` (§Phase 18 embedding bullet) — one
  paragraph and a minimal snippet.

Suggested slices, each independently landable and testable:

1. **Facade core** — `HostFn` with the `FunctionCall` shape, `create_function`,
   `create_object`, `register_fn`, `ErrorKind` re-export, `JsValue::thrown`,
   `FunctionCall::{this,length,arg,args,to_number,to_string}`. Rooting + error
   rustdoc.
2. **Re-entrancy** — `FunctionCall::{call,construct,eval}` via the TLS agent;
   tests that a host function calls a JS callback synchronously and that a JS
   callback throws through it.
3. **Constructors (Phase B)** — `Context::create_constructor(name, length,
   construct_cb)` mapping to a `NativeCtor`, with a `.prototype` object and
   `is_construct` in `FunctionCall`.
4. **Accessors (Phase C)** — `JsObject::define_accessor(get, set)` for host
   objects (today only data properties exist on the facade), which would also
   let `wasm_binding/dom.rs` drop its direct `crux` dependency.

## 8. Testing

- Doctest on `embed`'s module docs and on each new `Context` method.
- Unit tests in `embed.rs::tests`, alongside the existing
  `host_function_is_callable_from_js`-style coverage in `api/mod.rs`:
  - a registered global is `typeof "function"`, has the right `name`/`length`,
    and is not constructible;
  - `this` is the receiver and args/`arg`/`length` agree;
  - a returned `Err(JsError)` is catchable in JS as the right Error kind;
  - `Err(value.thrown())` catches the exact value (string and object);
  - re-entrancy: a host function calls a JS arrow passed as an argument and
    returns its result; nesting is two deep;
  - a namespace object built with `create_object` + `set`;
  - the `%Function.prototype%` link (`f.call`, `f.hasOwnProperty`) resolves.
- `cargo test -p slag` (doctests included) and `cargo run -p slag --example
  embed`.
- `cargo clippy -- -D warnings` on the touched crates, per `.rules`.

## 9. Risks and open questions

1. **`unsafe` at the args boundary** (§6.2). Mitigated by `repr(transparent)` +
   static assertions + a single cast site. If reviewers prefer zero unsafe,
   allocate the `Vec` in slice 1 and revisit.
2. **Rooting footgun** (§6.1) is inherent to exposing `create_function`; the
   rustdoc and `register_fn` default are the mitigation.
3. **Aliasing `&mut Agent` in re-entrant methods** repeats a pattern the engine
   already relies on globally. If we want to remove the `unsafe`, the internal
   `with_agent` API would need to hand out a scoped `&mut Agent` — a larger
   change to `crux` with broad blast radius; not proposed here.
4. **Naming**: `FunctionCall` vs `HostCall`/`CallInfo`; `register_fn` vs
   `register_function`. Bikeshed; pick one before slice 1 to avoid churn.
5. **Should `create_function` take `&self` or `&mut self`?** `&self` is accurate
   (no agent mutation) and composes better, but diverges from `set_global`'s
   `&mut self`. Recommendation: `&self` for `create_*`, `&mut self` for
   `register_fn`.

## 10. Suggested `.rules` additions

None — this is a feature proposal, not a discovered trap. If the `unsafe` args
cast or the rooting rule surprise a future session more than once, they should
graduate to a rule then.
