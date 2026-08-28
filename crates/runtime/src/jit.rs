//! The Vm-side JIT integration: the hook the `jit` crate installs, the
//! slow-path helper table that routes the compiled code's fallbacks back
//! into the interpreter's machinery, and the per-call context those helpers
//! operate on.
//!
//! The dependency direction is one-way (`jit` depends on `runtime`), so the
//! hook is a runtime-owned registry of function pointers: the `jit` crate
//! installs a compiled-body cache (see `jit::JitCache`) and the runtime's
//! leaf-call path (`Vm::run_jit_leaf`, in `ir.rs`) consults it before
//! interpreting a certified body.
//!
//! # ABI contracts
//!
//! - **Entry**: `extern "C" fn(frame, stack, ctx) -> u64` — `frame` points
//!   at the body's frame slots, `stack` at one-past-the-frame (the compiled
//!   body pushes above it), `ctx` at the per-call [`JitCallContext`]. The
//!   return value is the completion value's bits.
//! - **Error signaling**: the context's first byte (`pending`, offset 0) is
//!   the JIT's error flag — after every slow-path helper call the compiled
//!   code loads it and, when set, jumps to its error exit (returning
//!   `undefined`); the runtime converts the pending [`JsError`] to an `Err`.
//!   This keeps the compiled body from executing any further side effect
//!   after a throwing slow path.
//! - **Slow paths**: [`JitSlowPaths`] is `#[repr(C)]` with the same field
//!   order as `jit::JitHelpers` (each field a function pointer), so the
//!   `jit` crate can convert the table without a runtime dependency.

use std::os::raw::c_void;

use crux::Value;
use syntax::ast::{AssignOp, BinaryOp, UpdateOp};

use crate::agent::Agent;
use crate::ir::{CompiledBody, Vm};
use crux::error::{ErrorKind, JsError};

/// The compiled entry ABI (mirrors `jit::JitEntry`; all arguments are
/// pointers, so the `*mut u64`/`*mut c_void` spelling difference is
/// ABI-invisible).
pub type JitEntry =
    unsafe extern "C" fn(frame: *mut c_void, stack: *mut c_void, ctx: *mut c_void) -> u64;

/// The per-body compiled-code metadata the cache returns on a hit (mirrors
/// `jit::JitCompiledInfo` — `#[repr(C)]`, layout-identical).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitCompiledInfo {
    /// The entry point (cast to `usize`).
    pub entry: usize,
    /// The body's maximum value-stack depth above the frame, in slots — the
    /// JIT's working area size.
    pub stack_usage: usize,
}

/// The registry the `jit` crate populates (see `jit::install`).
#[derive(Clone, Copy)]
pub struct JitHook {
    /// The installed cache (owned by the installer; freed by `drop_cache`).
    pub cache: *mut c_void,
    /// Look up (and compile on first use) a body. `body` points at the
    /// caller's `Rc<CompiledBody>`. Returns a `JitCompiledInfo` pointer, or
    /// null when the body is not JIT-compilable.
    pub lookup: unsafe extern "C" fn(cache: *mut c_void, body: *const c_void) -> *const c_void,
    /// Free the cache (called by the Agent's drop).
    pub drop_cache: unsafe extern "C" fn(cache: *mut c_void),
    /// The slow-path helper table.
    pub helpers: *const JitSlowPaths,
}

/// The per-call context the Vm passes to a compiled body as its `ctx`
/// argument. `pending` is offset 0 — the compiled code's error-check ABI.
#[repr(C)]
pub struct JitCallContext {
    /// Set by a slow-path helper that hit an interpreter error; the JIT
    /// checks this byte after every helper call.
    pub pending: bool,
    /// The pending error (valid when `pending`).
    pub error: Option<JsError>,
    /// The running Agent (helpers route interpreter machinery through it).
    pub agent: *mut Agent,
    /// The running Vm (member helpers need its member machinery).
    pub vm: *mut Vm,
}

/// The runtime's slow-path helper table (field order mirrors
/// `jit::JitHelpers`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitSlowPaths {
    /// Full binary-operator semantics (`apply_binary`); `op` is a
    /// `BinaryOp` discriminant.
    pub binary_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// JS relational semantics for a loop test on a non-Number; returns 1
    /// when the test holds.
    pub relational_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// The general `++`/`--` machinery on a non-Number; returns the new value.
    pub update_value_slow: extern "C" fn(ctx: *mut c_void, inc: u64, value: u64) -> u64,
    /// Full JS `ToBoolean` for a heap value; returns 1 when truthy.
    pub to_boolean_slow: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// Throws the TDZ ReferenceError (reported through the context).
    pub tdz_error: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Get(o, name)`; `name` is an `AtomId`.
    pub get_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64) -> u64,
    /// `Get(o, key)` with a computed key.
    pub get_member_computed: extern "C" fn(ctx: *mut c_void, object: u64, key: u64) -> u64,
    /// `Set(o, name, v)` (plain assignment); returns the stored value.
    pub set_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64,
    /// `Set(o, key, v)` with a computed key; returns the stored value.
    pub set_member_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64,
    /// The general `CallFast` (a body may contain calls — leaf bodies never
    /// do): `args` points at the JIT buffer's argument region (`argc`
    /// slots). Runs the interpreter's call machinery on the Vm's own stack.
    pub call_slow:
        extern "C" fn(ctx: *mut c_void, callee: u64, this: u64, argc: u64, args: *mut u64) -> u64,
    /// Read a declared top-level `var` off the global object (`name` is an
    /// `AtomId`); returns the value.
    pub get_global: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// Write a declared top-level `var`; returns the stored value.
    pub set_global: extern "C" fn(ctx: *mut c_void, name: u64, value: u64) -> u64,
    /// The identifier read a certified body uses for an outer/global binding
    /// (`resolve_binding` + `get_value`); `name` is an `AtomId`.
    pub load_ident: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// Resolve an identifier reference and push it onto the Vm's reference
    /// stack (the write path's `put_var_reference` pops it).
    pub resolve_var_ident: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// `PutValue` on the reference stack's top, popped with the stored value.
    pub put_var_reference: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// The identifier `++`/`--` (resolve, update, store, return the result).
    pub update_ident:
        extern "C" fn(ctx: *mut c_void, name: u64, op: u64, prefix: u64, old: u64) -> u64,
    /// The general named member assign (`o.x = v` and `o.x += v`): `op` is
    /// an `AssignOp` discriminant, `old` the cached GetValue for a compound
    /// op (ignored for `=`). Returns the stored value (the assignment's
    /// result).
    pub assign_member_name: extern "C" fn(
        ctx: *mut c_void,
        op: u64,
        object: u64,
        name: u64,
        old: u64,
        value: u64,
    ) -> u64,
    /// The general computed member assign (`o[k] = v` and `o[k] += v`);
    /// `old` as above. Returns the stored value.
    pub assign_member_computed: extern "C" fn(
        ctx: *mut c_void,
        op: u64,
        object: u64,
        key: u64,
        old: u64,
        value: u64,
    ) -> u64,
}

/// The runtime's slow-path table, installed into every `JitHook`.
pub static JIT_SLOW_PATHS: JitSlowPaths = JitSlowPaths {
    binary_slow,
    relational_slow,
    update_value_slow,
    to_boolean_slow,
    tdz_error,
    get_member_name,
    get_member_computed,
    set_member_name,
    set_member_computed,
    call_slow,
    get_global,
    set_global,
    load_ident,
    resolve_var_ident,
    put_var_reference,
    update_ident,
    assign_member_name,
    assign_member_computed,
};

/// The slack (in slots) reserved above a compiled body's working area on the
/// value stack: the member helpers push their stored value once per call, and
/// the JIT's own usage is bounded by `JitCompiledInfo::stack_usage`.
pub const JIT_STACK_SLACK: usize = 16;

/// The JIT's per-call frame/working buffer fits a stack array up to this
/// many slots (512 bytes); larger bodies spill to a per-call heap Vec. Most
/// certified bodies are far smaller, so the hot path avoids the allocation.
pub(crate) const INLINE_JIT_BUF: usize = 64;

/// The `BinaryOp` variants in declaration order (a fieldless enum's
/// discriminant is its index — guaranteed by the language).
const BINARY_OPS: [BinaryOp; 22] = [
    BinaryOp::Exp,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Rem,
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::LeftShift,
    BinaryOp::RightShift,
    BinaryOp::UnsignedRightShift,
    BinaryOp::LessThan,
    BinaryOp::GreaterThan,
    BinaryOp::LessEqual,
    BinaryOp::GreaterEqual,
    BinaryOp::In,
    BinaryOp::Instanceof,
    BinaryOp::Equal,
    BinaryOp::NotEqual,
    BinaryOp::StrictEqual,
    BinaryOp::StrictNotEqual,
    BinaryOp::BitAnd,
    BinaryOp::BitXor,
    BinaryOp::BitOr,
];

const UPDATE_OPS: [UpdateOp; 2] = [UpdateOp::Increment, UpdateOp::Decrement];

/// The `AssignOp` variants in declaration order (a fieldless enum's
/// discriminant is its index).
const ASSIGN_OPS: [AssignOp; 16] = [
    AssignOp::Assign,
    AssignOp::AddAssign,
    AssignOp::SubAssign,
    AssignOp::MulAssign,
    AssignOp::DivAssign,
    AssignOp::RemAssign,
    AssignOp::ExpAssign,
    AssignOp::LeftShiftAssign,
    AssignOp::RightShiftAssign,
    AssignOp::UnsignedRightShiftAssign,
    AssignOp::BitAndAssign,
    AssignOp::BitXorAssign,
    AssignOp::BitOrAssign,
    AssignOp::AndAssign,
    AssignOp::OrAssign,
    AssignOp::NullishAssign,
];

unsafe fn ctx_of(ctx: *mut c_void) -> &'static mut JitCallContext {
    // SAFETY: the Vm passes `&mut JitCallContext` on its stack for the
    // duration of the (synchronous) compiled call.
    unsafe { &mut *(ctx as *mut JitCallContext) }
}

/// Report an interpreter error through the per-call context and return the
/// placeholder value the compiled code discards.
fn slow_error(ctx: &mut JitCallContext, error: JsError) -> u64 {
    ctx.error = Some(error);
    ctx.pending = true;
    Value::Undefined.bits()
}

extern "C" fn binary_slow(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let op = BINARY_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(BinaryOp::Add);
    match crate::expr::apply_binary(agent, op, &Value::from_bits(a), &Value::from_bits(b)) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn relational_slow(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let op = BINARY_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(BinaryOp::LessThan);
    match crate::expr::apply_binary(agent, op, &Value::from_bits(a), &Value::from_bits(b)) {
        Ok(value) => crux::convert::to_boolean(&value) as u64,
        Err(error) => {
            slow_error(ctx, error);
            0
        }
    }
}

extern "C" fn update_value_slow(ctx: *mut c_void, inc: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let inc = UPDATE_OPS
        .get(inc as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::ir::update_value(agent, &inc, &Value::from_bits(value)) {
        Ok((_, new)) => new.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn to_boolean_slow(_ctx: *mut c_void, value: u64) -> u64 {
    // `ToBoolean` cannot throw.
    crux::convert::to_boolean(&Value::from_bits(value)) as u64
}

extern "C" fn tdz_error(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    slow_error(
        ctx,
        JsError::new(
            ErrorKind::ReferenceError,
            "Cannot access a binding before initialization".into(),
        ),
    )
}

extern "C" fn get_member_name(ctx: *mut c_void, object: u64, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    match vm.get_member_name(agent, object, name as crux::AtomId) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn get_member_computed(ctx: *mut c_void, object: u64, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    match vm.get_member_computed(agent, object, key) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_member_name(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    if object.is_undefined() || object.is_null() {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "Cannot set properties of null".into()),
        );
    }
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.assign_member(
        agent,
        object,
        crate::ir::PropertyKeyName::Name(name as crux::AtomId),
        None,
        value,
        syntax::ast::AssignOp::Assign,
    ) {
        // `assign_member` pushed the result (the assignment's value); pop it
        // back so the interpreter's value stack stays balanced across the
        // JIT body's helpers.
        Ok(()) => match vm.stack.pop() {
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_member_computed(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.assign_computed_plain(agent, object, key, value) {
        Ok(()) => match vm.stack.pop() {
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn call_slow(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    argc: u64,
    args: *mut u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let argc = argc as usize;
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let entry_len = vm.stack.len();
    vm.stack.push(Value::from_bits(this));
    vm.stack.push(Value::from_bits(callee));
    // The arguments are pushed straight from the JIT buffer (no copy): the
    // buffer is only written by the machine code, which is suspended for
    // the duration of this synchronous helper, so it is stable even while
    // `vm.stack` reallocates.
    for i in 0..argc {
        // SAFETY: the JIT passes a pointer into its own (live) stack buffer
        // with `argc` slots.
        vm.stack.push(Value::from_bits(unsafe { *args.add(i) }));
    }
    match vm.do_call_fast(agent, argc, false) {
        Ok(()) => {
            // `do_call_fast` replaced `[this, callee, args]` with the result.
            let result = match vm.stack.pop() {
                Some(value) => value,
                None => {
                    vm.stack.truncate(entry_len);
                    return slow_error(
                        ctx,
                        JsError::new(
                            ErrorKind::TypeError,
                            "the JIT call produced no result".into(),
                        ),
                    );
                }
            };
            debug_assert_eq!(vm.stack.len(), entry_len);
            result.bits()
        }
        Err(error) => {
            vm.stack.truncate(entry_len);
            slow_error(ctx, error)
        }
    }
}

extern "C" fn get_global(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.load_global_value(agent, name as crux::AtomId) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_global(ctx: *mut c_void, name: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    match vm.store_global_value(agent, name as crux::AtomId, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn load_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let reference = match crate::context::resolve_binding(
        agent,
        &crux::lookup(name as crux::AtomId),
        vm.strict,
    ) {
        Ok(reference) => reference,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::context::get_value(agent, &reference) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn resolve_var_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match crate::context::resolve_binding(agent, &crux::lookup(name as crux::AtomId), vm.strict) {
        Ok(reference) => {
            vm.var_ref_stack.push(reference);
            0
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn put_var_reference(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    let reference = match vm.var_ref_stack.pop() {
        Some(reference) => reference,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "PutVarReference without a resolution".into(),
                ),
            );
        }
    };
    match crate::context::put_value(agent, &reference, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn update_ident(ctx: *mut c_void, name: u64, op: u64, prefix: u64, old: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let old = Value::from_bits(old);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    let prefix = prefix != 0;
    let (old_numeric, new) = match crate::ir::update_value(agent, &op, &old) {
        Ok(result) => result,
        Err(error) => return slow_error(ctx, error),
    };
    let reference = match crate::context::resolve_binding(
        agent,
        &crux::lookup(name as crux::AtomId),
        vm.strict,
    ) {
        Ok(reference) => reference,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::context::put_value(agent, &reference, new) {
        Ok(()) => (if prefix { new } else { old_numeric }).bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn assign_member_name(
    ctx: *mut c_void,
    op: u64,
    object: u64,
    name: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    if crate::ir::is_nullish(&object) {
        return slow_error(
            ctx,
            crate::ir::nullish_error("Cannot set properties of null"),
        );
    }
    let old = if crate::ir::is_compound_assign(&op) {
        Some(Value::from_bits(old))
    } else {
        None
    };
    match vm.assign_member(
        agent,
        object,
        crate::ir::PropertyKeyName::Name(name as crux::AtomId),
        old,
        value,
        op,
    ) {
        Ok(()) => match vm.stack.pop() {
            // `assign_member` pushed the result (the assignment's value).
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn assign_member_computed(
    ctx: *mut c_void,
    op: u64,
    object: u64,
    key: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    if crate::ir::is_compound_assign(&op) {
        if crate::ir::is_nullish(&object) {
            return slow_error(
                ctx,
                crate::ir::nullish_error("Cannot set properties of null"),
            );
        }
        let key = match crate::context::to_property_key(agent, &key) {
            Ok(key) => key,
            Err(error) => return slow_error(ctx, error),
        };
        let old = Value::from_bits(old);
        match vm.assign_member(
            agent,
            object,
            crate::ir::PropertyKeyName::Key(key),
            Some(old),
            value,
            op,
        ) {
            Ok(()) => match vm.stack.pop() {
                Some(result) => result.bits(),
                None => value.bits(),
            },
            Err(error) => slow_error(ctx, error),
        }
    } else {
        match vm.assign_computed_plain(agent, object, key, value) {
            Ok(()) => match vm.stack.pop() {
                Some(result) => result.bits(),
                None => value.bits(),
            },
            Err(error) => slow_error(ctx, error),
        }
    }
}

/// The body's compiled-info pointer: the per-body fast cell when set (the
/// first successful lookup stores it — the cache never evicts a body it
/// holds alive, so it stays valid), else a consult of the installed hook
/// (compiling on first use). `1` marks a known non-compilable body, so the
/// hook is not reconsulted. Returns null when the body has no compiled
/// code.
pub(crate) fn lookup_info(
    hook: crate::jit::JitHook,
    ir: &std::rc::Rc<CompiledBody>,
) -> *const JitCompiledInfo {
    let known = ir.jit_info.get();
    if known > 1 {
        known as *const JitCompiledInfo
    } else if known == 0 {
        // SAFETY: `hook.cache` is the installed cache and `ir` is alive for
        // the call.
        let ptr = unsafe {
            (hook.lookup)(
                hook.cache,
                ir as *const std::rc::Rc<CompiledBody> as *const std::os::raw::c_void,
            )
        };
        if ptr.is_null() {
            ir.jit_info.set(1);
        } else {
            ir.jit_info.set(ptr as usize);
        }
        ptr as *const JitCompiledInfo
    } else {
        std::ptr::null()
    }
}

/// The general-path JIT run (the leaf path is `Vm::run_jit_leaf`): run
/// `ir`'s compiled machine code for a body running on its own Vm — one
/// that may contain calls, since leaf bodies never do (`steps_are_leaf`
/// excludes every call step). The frame is `vm.frame` (the caller set it
/// up: params, `var`s, TDZ slots, this slot); the working area is a
/// private buffer, rooted for the call's duration. Returns `Ok(None)` when
/// no hook is installed or the body has no compiled code — the caller
/// falls back to the interpreter.
pub(crate) fn run_jit_body(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &std::rc::Rc<CompiledBody>,
) -> Result<Option<Value>, JsError> {
    let Some(hook) = agent.jit_hook else {
        return Ok(None);
    };
    let info_ptr = lookup_info(hook, ir);
    if info_ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: the cache never evicts a body it holds alive; the pointer is
    // into the cache's own entry.
    let info = unsafe { &*info_ptr };
    // SAFETY: `info.entry` is a code pointer the cache owns; a fn pointer
    // is pointer-sized, so the integer cast is exact.
    let entry: JitEntry = unsafe { std::mem::transmute(info.entry) };
    // The frame lives in `vm.frame` (the caller filled it with `setup_frame`
    // plus the this slot); the working area is a private buffer — helpers
    // receive `&mut Vm` and may reallocate `vm.stack`, so the JIT's raw
    // pointers must never alias it. A small body fits a stack array (no
    // per-call heap allocation); larger bodies spill to a Vec.
    let (frame_ptr, _frame_len): (*mut Value, usize) = match &mut vm.frame {
        crate::ir::Frame::Inline(buf) => (buf.as_mut_ptr(), buf.len()),
        crate::ir::Frame::Heap(vec) => (vec.as_mut_ptr(), vec.len()),
    };
    let work_len = info.stack_usage + JIT_STACK_SLACK;
    let (mut inline_work, mut heap_work) =
        ([Value::Undefined; INLINE_JIT_BUF], Vec::<Value>::new());
    let work: &mut [Value] = if work_len <= INLINE_JIT_BUF {
        &mut inline_work[..work_len]
    } else {
        heap_work.resize(work_len, Value::Undefined);
        &mut heap_work[..]
    };
    let work_ptr = work.as_mut_ptr() as *mut c_void;
    let mut ctx = JitCallContext {
        pending: false,
        error: None,
        agent: agent as *mut Agent,
        vm: vm as *mut Vm,
    };
    // Register the vm (its trace covers `vm.frame`, `vm.stack`, and any
    // nested leaf jit roots) plus the working area for the call's duration:
    // a helper can allocate and trigger a collection, and a heap value only
    // those buffers reference must survive until the JIT stores or returns
    // it.
    let result = crate::ir::with_jit_run(vm, ir, work, || unsafe {
        (entry)(
            frame_ptr as *mut c_void,
            work_ptr,
            (&mut ctx as *mut JitCallContext) as *mut c_void,
        )
    });
    if ctx.pending {
        return Err(ctx.error.take().expect("a pending JIT error is present"));
    }
    Ok(Some(Value::from_bits(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slow_path_table_is_complete() {
        // Every helper is a real function pointer (the JIT bails on a None
        // helper, so a null here would silently drop bodies to the
        // interpreter).
        assert_ne!(JIT_SLOW_PATHS.binary_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.relational_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_value_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.to_boolean_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tdz_error as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.call_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_global as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_global as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.load_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.resolve_var_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.put_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_computed as usize, 0);
    }

    #[test]
    fn discriminant_tables_cover_the_enums() {
        assert_eq!(BINARY_OPS.len(), 22);
        assert_eq!(BINARY_OPS[BinaryOp::Add as usize], BinaryOp::Add);
        assert_eq!(BINARY_OPS[BinaryOp::In as usize], BinaryOp::In);
        assert_eq!(
            UPDATE_OPS[UpdateOp::Increment as usize],
            UpdateOp::Increment
        );
        assert_eq!(
            UPDATE_OPS[UpdateOp::Decrement as usize],
            UpdateOp::Decrement
        );
        assert_eq!(ASSIGN_OPS.len(), 16);
        assert_eq!(ASSIGN_OPS[AssignOp::Assign as usize], AssignOp::Assign);
        assert_eq!(
            ASSIGN_OPS[AssignOp::AddAssign as usize],
            AssignOp::AddAssign
        );
        assert_eq!(
            ASSIGN_OPS[AssignOp::NullishAssign as usize],
            AssignOp::NullishAssign
        );
    }
}
