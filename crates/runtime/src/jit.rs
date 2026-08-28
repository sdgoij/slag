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
use syntax::ast::{BinaryOp, UpdateOp};

use crate::agent::Agent;
use crate::ir::Vm;
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
};

/// The slack (in slots) reserved above a compiled body's working area on the
/// value stack: the member helpers push their stored value once per call, and
/// the JIT's own usage is bounded by `JitCompiledInfo::stack_usage`.
pub const JIT_STACK_SLACK: usize = 16;

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
        Ok(()) => value.bits(),
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
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
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
    }
}
