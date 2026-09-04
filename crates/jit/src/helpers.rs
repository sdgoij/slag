//! Slow-path entry points the JIT bakes into compiled machine code.
//!
//! The JIT inlines the number fast paths; anything it cannot handle calls
//! one of these `extern "C"` functions (their addresses are baked into the
//! code at compile time, so the table does not need to stay alive). The
//! runtime integration fills in real helpers that route to the interpreter's
//! machinery (`apply_binary`, `get_member_name`, `update_value`, the TDZ
//! ReferenceError, `to_boolean`). The scaffold's tests provide test doubles.
//!
//! Every helper takes `vm` (the opaque pointer the caller passed to the JIT
//! entry point) so the runtime implementation can reach the `Vm`/`Agent`.
//! `op`/`inc`/`name` arguments are the raw discriminants of
//! [`syntax::ast::BinaryOp`]/[`syntax::ast::UpdateOp`] / the `AtomId`, passed
//! as `u64`.

use std::os::raw::c_void;

use crux::Value;

/// Identifies one slow-path helper; used to look the entry point up and to
/// name it in bail diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    BinarySlow,
    ConcatStrings,
    RelationalSlow,
    UpdateValueSlow,
    ToBooleanSlow,
    TdzError,
    GcSafepoint,
    GetMemberName,
    GetMemberComputed,
    SetMemberName,
    SetMemberComputed,
    /// The register fused computed-member compound (`o[k] op= v`): converts
    /// the key once, reads the old value, applies the compound to the RHS
    /// value, and stores through the same key. `op` is an `AssignOp`
    /// discriminant.
    RmwCompoundComputed,
    /// The register fused computed-member update (`o[k]++` / `o[k]--`):
    /// converts the key once, applies the ToNumeric update, and stores.
    /// `op` is an `UpdateOp` discriminant.
    RmwUpdateComputed,
    CallSlow,
    LeafCallProbe,
    GetGlobal,
    SetGlobal,
    SetGlobalSlot,
    LoadIdent,
    ResolveVarIdent,
    PutVarReference,
    UpdateIdent,
    AssignMemberName,
    AssignMemberComputed,
    FastArrayElementWrite,
    DenseArrayAppend,
    SetMemberSlot,
    LoadContext,
    StoreContext,
    InitContext,
    UpdateContext,
    LoadPerIter,
    StorePerIter,
    UpdatePerIter,
    GetVarReference,
    UpdateVarReference,
    PutVarReferenceOp,
    PopVarReference,
    CreateFunction,
    CreateArrow,
    CreateFunctionDecl,
    NewTarget,
    RegExpLiteral,
    TailCall,
    ArgsBase,
    ArgsPush,
    ArgsSpread,
    CallVector,
    /// The compiled `Step::CallApply` (a member call whose property name is
    /// `apply`/`call`): runs the interpreter's `do_call_apply` — the
    /// intrinsic check and, on a match, the direct call of the receiver with
    /// the `this` argument and the `CreateListFromArrayLike` element reads.
    CallApply,
    /// M10: the compiled `CallApply` fast path's dense-`argArray` element
    /// fill — copies the array's elements into the JIT buffer at the passed
    /// address and returns the element count (`u64::MAX` = not fast).
    ApplyArgsFill,
    TailCallVector,
    TailCallSelfVector,
    ArrayBegin,
    ArrayElement,
    ArraySpread,
    ArrayHole,
    ArrayEnd,
    ObjectBegin,
    ObjectInitName,
    ObjectInitComputed,
    ObjectKeyToPropertyKey,
    ObjectMethodName,
    ObjectMethodComputed,
    ObjectAccessorName,
    ObjectAccessorComputed,
    ObjectSpread,
    PushStr,
    ConcatStr,
    ConcatStrConst,
    PushConst,
    LoadConst,
    EnterBlock,
    LeaveBlock,
    EnterTry,
    ExitTry,
    ReturnControl,
    BreakControl,
    ContinueControl,
    ThrowControl,
    FinallyEnd,
    CatchBind,
    DispatchError,
    SwitchDisc,
    SwitchTest,
    ForInBegin,
    ForInNext,
    ForOfBegin,
    ForOfNext,
    ForOfNextBindLocal,
    ForOfClose,
    ForOfCloseAll,
    EnterPerIteration,
    PerIteration,
    YieldSuspend,
    AwaitSuspend,
    DestructureBegin,
    DestructureNext,
    DestructureRest,
    DestructureObjCoercible,
    DestructureObjKey,
    DestructureObjKeyComputed,
    DestructureObjKeyStore,
    DestructureObjKeyGet,
    DestructureObjRest,
    DestructureClose,
    DestructureObjEnd,
    DestructureCloseAll,
    CreateArguments,
    TypeofTop,
    TypedArrayLength,
    GetSuperBase,
    ThisValue,
    GetSuperName,
    GetSuperComputed,
    GetSuperComputedKeep,
    AssignSuperName,
    AssignSuperComputed,
    UpdateSuperName,
    UpdateSuperComputed,
    DeleteSuper,
    ResolveSuperRefName,
    ResolveSuperRefComputed,
}

impl Helper {
    pub fn name(self) -> &'static str {
        match self {
            Helper::BinarySlow => "binary_slow",
            Helper::ConcatStrings => "concat_strings",
            Helper::RelationalSlow => "relational_slow",
            Helper::UpdateValueSlow => "update_value_slow",
            Helper::ToBooleanSlow => "to_boolean_slow",
            Helper::TdzError => "tdz_error",
            Helper::GcSafepoint => "gc_safepoint",
            Helper::GetMemberName => "get_member_name",
            Helper::GetMemberComputed => "get_member_computed",
            Helper::SetMemberName => "set_member_name",
            Helper::SetMemberComputed => "set_member_computed",
            Helper::RmwCompoundComputed => "rmw_compound_computed",
            Helper::RmwUpdateComputed => "rmw_update_computed",
            Helper::CallSlow => "call_slow",
            Helper::LeafCallProbe => "leaf_call_probe",
            Helper::GetGlobal => "get_global",
            Helper::SetGlobal => "set_global",
            Helper::SetGlobalSlot => "set_global_slot",
            Helper::LoadIdent => "load_ident",
            Helper::ResolveVarIdent => "resolve_var_ident",
            Helper::PutVarReference => "put_var_reference",
            Helper::UpdateIdent => "update_ident",
            Helper::AssignMemberName => "assign_member_name",
            Helper::AssignMemberComputed => "assign_member_computed",
            Helper::FastArrayElementWrite => "fast_array_element_write",
            Helper::DenseArrayAppend => "dense_array_append",
            Helper::SetMemberSlot => "set_member_slot",
            Helper::LoadContext => "load_context",
            Helper::StoreContext => "store_context",
            Helper::InitContext => "init_context",
            Helper::UpdateContext => "update_context",
            Helper::LoadPerIter => "load_per_iter",
            Helper::StorePerIter => "store_per_iter",
            Helper::UpdatePerIter => "update_per_iter",
            Helper::GetVarReference => "get_var_reference",
            Helper::UpdateVarReference => "update_var_reference",
            Helper::PutVarReferenceOp => "put_var_reference_op",
            Helper::PopVarReference => "pop_var_reference",
            Helper::CreateFunction => "create_function",
            Helper::CreateArrow => "create_arrow",
            Helper::CreateFunctionDecl => "create_function_decl",
            Helper::NewTarget => "new_target",
            Helper::RegExpLiteral => "regexp_literal",
            Helper::TailCall => "tail_call",
            Helper::ArgsBase => "args_base",
            Helper::ArgsPush => "args_push",
            Helper::ArgsSpread => "args_spread",
            Helper::CallVector => "call_vector",
            Helper::CallApply => "call_apply",
            Helper::ApplyArgsFill => "apply_args_fill",
            Helper::TailCallVector => "tail_call_vector",
            Helper::TailCallSelfVector => "tail_call_self_vector",
            Helper::ArrayBegin => "array_begin",
            Helper::ArrayElement => "array_element",
            Helper::ArraySpread => "array_spread",
            Helper::ArrayHole => "array_hole",
            Helper::ArrayEnd => "array_end",
            Helper::ObjectBegin => "object_begin",
            Helper::ObjectInitName => "object_init_name",
            Helper::ObjectInitComputed => "object_init_computed",
            Helper::ObjectKeyToPropertyKey => "object_key_to_property_key",
            Helper::ObjectMethodName => "object_method_name",
            Helper::ObjectMethodComputed => "object_method_computed",
            Helper::ObjectAccessorName => "object_accessor_name",
            Helper::ObjectAccessorComputed => "object_accessor_computed",
            Helper::ObjectSpread => "object_spread",
            Helper::PushStr => "push_str",
            Helper::ConcatStr => "concat_str",
            Helper::ConcatStrConst => "concat_str_const",
            Helper::PushConst => "push_const",
            Helper::LoadConst => "load_const",
            Helper::EnterBlock => "enter_block",
            Helper::LeaveBlock => "leave_block",
            Helper::EnterTry => "enter_try",
            Helper::ExitTry => "exit_try",
            Helper::ReturnControl => "return_control",
            Helper::BreakControl => "break_control",
            Helper::ContinueControl => "continue_control",
            Helper::ThrowControl => "throw_control",
            Helper::FinallyEnd => "finally_end",
            Helper::CatchBind => "catch_bind",
            Helper::DispatchError => "dispatch_error",
            Helper::SwitchDisc => "switch_disc",
            Helper::SwitchTest => "switch_test",
            Helper::ForInBegin => "for_in_begin",
            Helper::ForInNext => "for_in_next",
            Helper::ForOfBegin => "for_of_begin",
            Helper::ForOfNext => "for_of_next",
            Helper::ForOfNextBindLocal => "for_of_next_bind_local",
            Helper::ForOfClose => "for_of_close",
            Helper::ForOfCloseAll => "for_of_close_all",
            Helper::EnterPerIteration => "enter_per_iteration",
            Helper::PerIteration => "per_iteration",
            Helper::YieldSuspend => "yield_suspend",
            Helper::AwaitSuspend => "await_suspend",
            Helper::DestructureBegin => "destructure_begin",
            Helper::DestructureNext => "destructure_next",
            Helper::DestructureRest => "destructure_rest",
            Helper::DestructureObjCoercible => "destructure_obj_coercible",
            Helper::DestructureObjKey => "destructure_obj_key",
            Helper::DestructureObjKeyComputed => "destructure_obj_key_computed",
            Helper::DestructureObjKeyStore => "destructure_obj_key_store",
            Helper::DestructureObjKeyGet => "destructure_obj_key_get",
            Helper::DestructureObjRest => "destructure_obj_rest",
            Helper::DestructureClose => "destructure_close",
            Helper::DestructureObjEnd => "destructure_obj_end",
            Helper::DestructureCloseAll => "destructure_close_all",
            Helper::CreateArguments => "create_arguments",
            Helper::TypeofTop => "typeof_top",
            Helper::TypedArrayLength => "typed_array_length",
            Helper::GetSuperBase => "get_super_base",
            Helper::ThisValue => "this_value",
            Helper::GetSuperName => "get_super_name",
            Helper::GetSuperComputed => "get_super_computed",
            Helper::GetSuperComputedKeep => "get_super_computed_keep",
            Helper::AssignSuperName => "assign_super_name",
            Helper::AssignSuperComputed => "assign_super_computed",
            Helper::UpdateSuperName => "update_super_name",
            Helper::UpdateSuperComputed => "update_super_computed",
            Helper::DeleteSuper => "delete_super",
            Helper::ResolveSuperRefName => "resolve_super_ref_name",
            Helper::ResolveSuperRefComputed => "resolve_super_ref_computed",
        }
    }

    /// Whether calling this helper can re-enter the interpreter (a getter,
    /// setter, `valueOf`/`toString`, or nested call), which is the only way
    /// the Vm stacks and realm count the leaf-call probe's eligibility
    /// checks can change mid-run. The compiled code bumps the ctx's
    /// leaf-eligibility epoch after such helpers so a cached leaf verdict is
    /// not reused across the disturbance. The excluded helpers are pure
    /// Whether calling this helper can re-enter the interpreter (a getter,
    /// setter, `valueOf`/`toString`, or nested call), which is the only way
    /// the Vm stacks and realm count the leaf-call probe's eligibility
    /// checks can change mid-run. The compiled code bumps the ctx's
    /// leaf-eligibility epoch after such helpers so a cached leaf verdict is
    /// not reused across the disturbance. The excluded helpers are pure
    /// slot/descriptor reads and writes, the string concat, or immediate
    /// throws (the probe itself is excluded — it only validates and fills).
    pub fn disturbs_leaf_eligibility(self) -> bool {
        !matches!(
            self,
            Helper::TdzError
                | Helper::LeafCallProbe
                | Helper::ApplyArgsFill
                | Helper::ToBooleanSlow
                | Helper::ConcatStrings
                | Helper::LoadContext
                | Helper::StoreContext
                | Helper::InitContext
                | Helper::LoadPerIter
                | Helper::StorePerIter
                | Helper::PopVarReference
                | Helper::TypeofTop
                | Helper::TypedArrayLength
        )
    }
}

/// The slow-path helper table (see the module docs).
///
/// `#[repr(C)]` so the offsets are stable for future code that reads the
/// table from the compiled code side; the scaffold bakes the addresses in at
/// compile time instead.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitHelpers {
    /// Full binary-operator semantics (`apply_binary`): `op` is a
    /// `BinaryOp` discriminant. Returns the result value.
    pub binary_slow: Option<extern "C" fn(vm: *mut c_void, op: u64, a: u64, b: u64) -> u64>,
    /// Cut 41: the string-string `Add` fast path — the compiled `Add`
    /// checked both operands' string tags, so the rope concat runs directly.
    /// Returns the concatenated value (0 when either operand is not a
    /// string).
    pub concat_strings: Option<extern "C" fn(vm: *mut c_void, a: u64, b: u64) -> u64>,
    /// JS relational semantics for a loop test on a non-Number: `op` is a
    /// `BinaryOp` discriminant; returns 1 when the test holds, else 0.
    pub relational_slow: Option<extern "C" fn(vm: *mut c_void, op: u64, a: u64, b: u64) -> u64>,
    /// The general `++`/`--` machinery on a non-Number: `inc` is an
    /// `UpdateOp` discriminant; returns the NEW value.
    pub update_value_slow: Option<extern "C" fn(vm: *mut c_void, inc: u64, value: u64) -> u64>,
    /// Full JS `ToBoolean` for a heap value (empty-string, object, ...):
    /// returns 1 when truthy, else 0.
    pub to_boolean_slow: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    /// Throws the TDZ ReferenceError. Never returns normally (the JIT emits
    /// `unreachable` after the call).
    pub tdz_error: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// The compiled-loop safe point (see `JitCallContext::gc_ticks`): runs
    /// the runtime's collection trigger when the allocation budget is
    /// exceeded. Returns 0; never sets the pending error.
    pub gc_safepoint: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// `Get(o, name)`: `name` is an `AtomId`; returns the value.
    pub get_member_name: Option<extern "C" fn(vm: *mut c_void, object: u64, name: u64) -> u64>,
    /// `Get(o, key)` with a computed key value.
    pub get_member_computed: Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64) -> u64>,
    /// `Set(o, name, v)` (plain assignment); returns the stored value.
    pub set_member_name:
        Option<extern "C" fn(vm: *mut c_void, object: u64, name: u64, value: u64) -> u64>,
    /// `Set(o, key, v)` with a computed key; returns the stored value.
    pub set_member_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, value: u64) -> u64>,
    /// The fused register computed-member compound (`o[k] op= v`): `op` is
    /// an `AssignOp` discriminant; returns nothing meaningful (the run
    /// discards the result).
    pub rmw_compound_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, value: u64, op: u64) -> u64>,
    /// The fused register computed-member update (`o[k]++` / `o[k]--`): `op`
    /// is an `UpdateOp` discriminant; returns nothing meaningful.
    pub rmw_update_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, op: u64) -> u64>,
    /// The general `CallFast` (a body may contain calls): `args` points at
    /// the JIT buffer's argument region (`argc` slots); returns the call's
    /// result value. `direct_eval` (Cut 62) routes a direct-eval callee
    /// through `perform_eval` with the caller's environment intact.
    pub call_slow: Option<
        extern "C" fn(
            vm: *mut c_void,
            callee: u64,
            this: u64,
            argc: u64,
            args: *mut u64,
            direct_eval: u64,
        ) -> u64,
    >,
    /// The compiled leaf-call probe (Cut 37): validates the callee is an
    /// inlineable leaf and returns its JIT entry (0 = fall back to
    /// `call_slow`). `site` is the call site's step index (Cut 39 — the
    /// probe records the cache identity so repeat visits skip it).
    pub leaf_call_probe: Option<
        extern "C" fn(vm: *mut c_void, callee: u64, args: *mut u64, argc: u64, site: u64) -> u64,
    >,
    /// Read a declared top-level `var` off the global object (`name` is an
    /// `AtomId`); returns the value.
    pub get_global: Option<extern "C" fn(vm: *mut c_void, name: u64) -> u64>,
    /// Write a declared top-level `var`; returns the stored value.
    pub set_global: Option<extern "C" fn(vm: *mut c_void, name: u64, value: u64) -> u64>,
    /// The compiled `StoreGlobal` fast path's property-vector write: `slot`
    /// is the binding's property-vector slot (the compiled code validated
    /// the cell against the live global). Returns the stored value.
    pub set_global_slot:
        Option<extern "C" fn(vm: *mut c_void, name: u64, slot: u64, value: u64) -> u64>,
    /// The compiled `AssignMemberName` fast path's in-place property write
    /// (Cut 40): the compiled code validated the member value cell and
    /// computed the compound's new value, so the vector entry is written
    /// directly (with the inline-field mirror); falls back to the full Set
    /// machinery on any doubt. Returns the stored value.
    pub set_member_slot:
        Option<extern "C" fn(vm: *mut c_void, object: u64, name: u64, value: u64) -> u64>,
    /// The identifier read a certified body uses for an outer/global binding
    /// (`resolve_binding` + `get_value`); `name` is an `AtomId`.
    pub load_ident: Option<extern "C" fn(vm: *mut c_void, name: u64) -> u64>,
    /// Resolve an identifier reference and push it onto the Vm's reference
    /// stack (the write path's `put_var_reference` pops it).
    pub resolve_var_ident: Option<extern "C" fn(vm: *mut c_void, name: u64) -> u64>,
    /// `PutValue` on the reference stack's top, popped with the stored value.
    pub put_var_reference: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    /// The identifier `++`/`--` (resolve, update, store, return the result).
    pub update_ident:
        Option<extern "C" fn(vm: *mut c_void, name: u64, op: u64, prefix: u64, old: u64) -> u64>,
    /// The general named member assign (`o.x = v` and `o.x += v`): `op` is
    /// an `AssignOp` discriminant, `old` the cached GetValue for a compound
    /// op (ignored for `=`). Returns the stored value.
    pub assign_member_name: Option<
        extern "C" fn(
            vm: *mut c_void,
            op: u64,
            object: u64,
            name: u64,
            old: u64,
            value: u64,
        ) -> u64,
    >,
    /// The general computed member assign (`o[k] = v` and `o[k] += v`);
    /// `old` as above. Returns the stored value.
    pub assign_member_computed: Option<
        extern "C" fn(vm: *mut c_void, op: u64, object: u64, key: u64, old: u64, value: u64) -> u64,
    >,
    /// The JIT's inline dense-array element write: 1 when the element was
    /// stored through `array_element_write` (plain Array + canonical index
    /// Number key), 0 for the `assign_member_computed` fallback. Never
    /// errors.
    pub fast_array_element_write:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, value: u64) -> u64>,
    /// The JIT-inline dense-array append (gap-close M1 C): the compiled
    /// gate verified the receiver/key/append shape; the helper runs the
    /// stateful append and returns 1/0. Never errors.
    pub dense_array_append:
        Option<extern "C" fn(vm: *mut c_void, object: u64, index: u64, value: u64) -> u64>,
    /// The capture-context read (`LoadContextSlot`): `depth` is the static
    /// context-chain depth, `index` the binding's context slot. Returns the
    /// value.
    pub load_context: Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64) -> u64>,
    /// The capture-context write (`StoreContextSlot`): the TDZ and const
    /// checks, then the slot write. Returns the stored value.
    pub store_context:
        Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64, value: u64) -> u64>,
    /// The first-write context store (`InitContextSlot`, no checks).
    pub init_context: Option<extern "C" fn(vm: *mut c_void, index: u64, value: u64) -> u64>,
    /// The capture-context `++`/`--` (`UpdateContextSlot`); returns the old
    /// (postfix) or new (prefix) value.
    pub update_context:
        Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64, op: u64, prefix: u64) -> u64>,
    /// The per-iteration read (`LoadPerIteration`): `depth` walks out
    /// through the enclosing per-iteration envs (0 = this loop's env),
    /// `index` the head's slot. Returns the value.
    pub load_per_iter: Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64) -> u64>,
    /// The per-iteration write (`StorePerIteration`): no TDZ/const checks.
    pub store_per_iter:
        Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64, value: u64) -> u64>,
    /// The per-iteration `++`/`--` (`UpdatePerIteration`); returns the old
    /// (postfix) or new (prefix) value.
    pub update_per_iter:
        Option<extern "C" fn(vm: *mut c_void, depth: u64, index: u64, op: u64, prefix: u64) -> u64>,
    /// `GetValue` of the reference stack's top (`GetVarReference`); the
    /// reference stays for the write path.
    pub get_var_reference: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// The identifier `++`/`--` through the reference machinery
    /// (`UpdateVarReference`); returns the old (postfix) or new (prefix)
    /// value.
    pub update_var_reference:
        Option<extern "C" fn(vm: *mut c_void, op: u64, prefix: u64, old: u64) -> u64>,
    /// The compound assign through the reference machinery
    /// (`PutVarReferenceOp`); returns the new value.
    pub put_var_reference_op:
        Option<extern "C" fn(vm: *mut c_void, op: u64, old: u64, value: u64) -> u64>,
    /// Drop the reference stack's top (`PopVarReference`).
    pub pop_var_reference: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// Create a function expression's closure (`Step::CreateFunction`):
    /// `step` is the step index into the running body (the payload — the
    /// function AST and the enclosing-chain layouts — is read back out of
    /// the body's step stream, not marshalled across the boundary). Returns
    /// the created function value.
    pub create_function: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// Create an arrow function's closure (`Step::CreateArrow`). Returns the
    /// created function value.
    pub create_arrow: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// Instantiate a hoisted top-level function declaration
    /// (`Step::FunctionDeclInit`) and store it into its frame or
    /// capture-context slot. Returns the created function value (the step
    /// completes with no value).
    pub create_function_decl: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// `new.target` (`Step::NewTarget`): the active constructor, or
    /// *undefined* at the script level.
    pub new_target: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// A `RegExp` literal (`Step::RegExpLiteral`): construct a fresh RegExp
    /// object; `step` is the step index into the running body (the
    /// pattern/flags strings live in the step).
    pub regexp_literal: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// A proper tail call (`Step::TailCallFast` and the fused global/slot
    /// forms): an ordinary certified callee replaces the current frame on
    /// the Vm; anything else is a normal call whose result completes the
    /// calling body's return. `args` points at `argc` slots in the JIT
    /// buffer; `direct_eval` is the step's direct-eval flag.
    pub tail_call: Option<
        extern "C" fn(
            vm: *mut c_void,
            callee: u64,
            this: u64,
            argc: u64,
            args: *mut u64,
            direct_eval: u64,
        ) -> u64,
    >,
    /// The vector call form (Cut 49): `Step::ArgsBase` records the argument
    /// boundary; `ArgsPush`/`ArgsSpread` append to the Vm's argument vector;
    /// the vector `Call`/`TailCall` steps run with those arguments.
    pub args_base: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub args_push: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    pub args_spread: Option<extern "C" fn(vm: *mut c_void, iterable: u64) -> u64>,
    pub call_vector:
        Option<extern "C" fn(vm: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64>,
    /// The compiled `Step::CallApply` (perf.md "remaining apply floor"):
    /// `args` points at the JIT buffer's argument region (`argc` slots, the
    /// `thisArg` first); `kind` is 0 for `apply`, 1 for `call`. Runs the
    /// interpreter's `do_call_apply` — the intrinsic check, the direct call
    /// of the receiver on this Vm (leaf-inline included), or the general
    /// fallback call of the resolved function.
    pub call_apply: Option<
        extern "C" fn(
            vm: *mut c_void,
            resolved: u64,
            callee: u64,
            argc: u64,
            args: *mut u64,
            kind: u64,
        ) -> u64,
    >,
    /// M10: the compiled `CallApply` fast path's dense-`argArray` element
    /// fill (see `Helper::ApplyArgsFill`): copies the element bits into the
    /// JIT buffer at `dest` and returns the element count, or `u64::MAX`
    /// when the array is not fast (nothing written).
    pub apply_args_fill: Option<extern "C" fn(vm: *mut c_void, arg_array: u64, dest: u64) -> u64>,
    pub tail_call_vector:
        Option<extern "C" fn(vm: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64>,
    /// Cut 51: the vector-form self-tail-call (`Step::TailCallSelfVector`,
    /// and `TailCallSelfCheckVector`'s identity-match path): rebind the
    /// frame in place from the Vm's argument vector; returns 1 when the
    /// machine code should jump back to the body's re-entry.
    pub tail_call_self_vector: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// Array literal steps (Cut 52): `ArrayBegin` creates the array and
    /// opens an index; `ArrayElement`/`ArraySpread` define elements;
    /// `ArrayHole` skips an index; `ArrayEnd` closes it and sets `length`.
    pub array_begin: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub array_element: Option<extern "C" fn(vm: *mut c_void, array: u64, value: u64) -> u64>,
    pub array_spread: Option<extern "C" fn(vm: *mut c_void, array: u64, iterable: u64) -> u64>,
    pub array_hole: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub array_end: Option<extern "C" fn(vm: *mut c_void, array: u64) -> u64>,
    /// Object literal steps (Cut 53): `ObjectBegin` creates the plain
    /// object; the init/method/accessor steps define the properties;
    /// `ObjectKeyToPropertyKey` converts a computed key;
    /// `ObjectSpread` copies a source's own enumerable properties.
    pub object_begin: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub object_init_name: Option<
        extern "C" fn(
            vm: *mut c_void,
            object: u64,
            name: u64,
            set_name: u64,
            shorthand: u64,
            value: u64,
        ) -> u64,
    >,
    pub object_init_computed: Option<
        extern "C" fn(vm: *mut c_void, object: u64, key: u64, set_name: u64, value: u64) -> u64,
    >,
    pub object_key_to_property_key: Option<extern "C" fn(vm: *mut c_void, key: u64) -> u64>,
    pub object_method_name: Option<extern "C" fn(vm: *mut c_void, object: u64, step: u64) -> u64>,
    pub object_method_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, step: u64) -> u64>,
    pub object_accessor_name: Option<extern "C" fn(vm: *mut c_void, object: u64, step: u64) -> u64>,
    pub object_accessor_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, step: u64) -> u64>,
    pub object_spread: Option<extern "C" fn(vm: *mut c_void, object: u64, from: u64) -> u64>,
    /// String literal steps (Cut 54): `PushStr` pushes a literal (the
    /// `JsString` payload is read back from the running body);
    /// `ConcatStr`/`ConcatStrConst` run the template flatten concat.
    pub push_str: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub concat_str: Option<extern "C" fn(vm: *mut c_void, value: u64, acc: u64) -> u64>,
    pub concat_str_const: Option<extern "C" fn(vm: *mut c_void, acc: u64, step: u64) -> u64>,
    /// `Step::Push` with a heap constant (a plain string/bigint literal):
    /// the payload is read back from the running body at `step`.
    pub push_const: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// A register body's heap constant (`LoadConst`/`BinConst`/member
    /// `RegOperand::Const`): read the value from the running body's register
    /// op at `(step, op)`, `field` selecting the const-bearing field.
    pub load_const: Option<extern "C" fn(vm: *mut c_void, step: u64, op: u64, field: u64) -> u64>,
    /// try/catch/finally and control-transfer steps (Cut 55): `EnterBlock`
    /// pushes a block env; `LeaveBlock` pops it; `EnterTry` pushes a
    /// `TryFrame`; `Exit`/`ReturnControl`/`BreakControl`/`ContinueControl`/
    /// `ThrowControl`/`FinallyEnd` run the control-transfer machinery and
    /// return the target step (or a completion sentinel); `CatchBind` binds
    /// the catch parameter; `DispatchError` routes a pending engine error
    /// through the handler table.
    pub enter_block: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub leave_block: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub enter_try: Option<extern "C" fn(vm: *mut c_void, handler: u64) -> u64>,
    pub exit_try: Option<extern "C" fn(vm: *mut c_void, ip: u64, after: u64) -> u64>,
    pub return_control: Option<extern "C" fn(vm: *mut c_void, ip: u64, value: u64) -> u64>,
    pub break_control: Option<extern "C" fn(vm: *mut c_void, ip: u64, target: u64) -> u64>,
    pub continue_control: Option<extern "C" fn(vm: *mut c_void, ip: u64, target: u64) -> u64>,
    pub throw_control: Option<extern "C" fn(vm: *mut c_void, ip: u64, value: u64) -> u64>,
    pub finally_end: Option<extern "C" fn(vm: *mut c_void, ip: u64) -> u64>,
    pub catch_bind: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub dispatch_error: Option<extern "C" fn(vm: *mut c_void, ip: u64) -> u64>,
    /// Switch steps (Cut 56): `SwitchDisc` stores the discriminant;
    /// `SwitchTest` strictly-equals a case test against it (1 = match).
    pub switch_disc: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    pub switch_test: Option<extern "C" fn(vm: *mut c_void, case: u64, test: u64) -> u64>,
    /// for-in/for-of iterator steps (Cut 57): `ForInBegin`/`ForOfBegin` open
    /// the enumeration/iteration, `ForInNext`/`ForOfNext` advance it (the
    /// element lands at the passed working-stack pointer; the return is 1 =
    /// element, 0 = done), `ForOfNextBindLocal` lands the element in a frame
    /// slot directly, `ForOfClose` closes a generic iterator, and
    /// `EnterPerIteration`/`PerIteration` create the certified loop's
    /// per-iteration envs (step-index helpers).
    pub for_in_begin: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    pub for_in_next: Option<extern "C" fn(vm: *mut c_void, stack: u64) -> u64>,
    pub for_of_begin: Option<extern "C" fn(vm: *mut c_void, step: u64, value: u64) -> u64>,
    pub for_of_next: Option<extern "C" fn(vm: *mut c_void, stack: u64) -> u64>,
    pub for_of_next_bind_local: Option<extern "C" fn(vm: *mut c_void, slot: u64) -> u64>,
    pub for_of_close: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub for_of_close_all: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub enter_per_iteration: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub per_iteration: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// Suspension steps (Cut 58): `YieldSuspend`/`AwaitSuspend` record the
    /// suspension (value + working-sp + continuation step) and return the
    /// `DISPATCH_SUSPEND` sentinel so the machine code ends the segment.
    pub yield_suspend:
        Option<extern "C" fn(vm: *mut c_void, sp: u64, value: u64, delegate: u64, ip: u64) -> u64>,
    pub await_suspend: Option<extern "C" fn(vm: *mut c_void, sp: u64, value: u64, ip: u64) -> u64>,
    /// Destructuring steps (Cut 59): `DestructureBegin`/`DestructureNext`/
    /// `DestructureRest`/`DestructureClose` drive an array pattern's iterator;
    /// `DestructureObjCoercible` opens an object pattern (the key reads via
    /// `DestructureObjKey`/`DestructureObjKeyComputed` and the store/get pair
    /// `DestructureObjKeyStore`/`DestructureObjKeyGet`, the rest via
    /// `DestructureObjRest`, the close via `DestructureObjEnd`);
    /// `DestructureCloseAll` is the engine-error close.
    pub destructure_begin: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    pub destructure_next: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub destructure_rest: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub destructure_obj_coercible: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    pub destructure_obj_key: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub destructure_obj_key_computed: Option<extern "C" fn(vm: *mut c_void, key: u64) -> u64>,
    pub destructure_obj_key_store: Option<extern "C" fn(vm: *mut c_void, key: u64) -> u64>,
    pub destructure_obj_key_get: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub destructure_obj_rest: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    pub destructure_close: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub destructure_obj_end: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub destructure_close_all: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// `Step::CreateArguments` (Cut 60): the body's `arguments` object —
    /// sloppy mapped (aliasing the formals through the capture context) or
    /// strict unmapped — stored into the frame slot; a step-index helper
    /// reading the `slot`/`mapped` payload.
    pub create_arguments: Option<extern "C" fn(vm: *mut c_void, step: u64) -> u64>,
    /// `Step::TypeofTop` (Cut 60): the `typeof` string of a value operand;
    /// never errors.
    pub typeof_top: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    /// A compiled `GetMemberName` with the `length` atom on an IntegerIndexed
    /// receiver: the slots length, or a NaN sentinel when the receiver is not
    /// a typed array (the machine code falls back to the member-cell probe /
    /// `get_member_name`). Never errors, never disturbs the leaf-eligibility
    /// state.
    pub typed_array_length: Option<extern "C" fn(vm: *mut c_void, object: u64) -> u64>,
    /// Super property steps (Cut 61): `GetSuperBase`/`ThisValue` (the base
    /// and receiver), `GetSuperName`/`GetSuperComputed`/`GetSuperComputedKeep`
    /// (reads), `AssignSuperName`/`AssignSuperComputed`/`UpdateSuperName`/
    /// `UpdateSuperComputed` (writes/updates), `DeleteSuper` (always the
    /// ReferenceError), and `ResolveSuperRefName`/`ResolveSuperRefComputed`
    /// (the reference for the update/logical-assign paths).
    pub get_super_base: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub this_value: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub get_super_name: Option<extern "C" fn(vm: *mut c_void, base: u64, name: u64) -> u64>,
    pub get_super_computed: Option<extern "C" fn(vm: *mut c_void, base: u64, key: u64) -> u64>,
    pub get_super_computed_keep:
        Option<extern "C" fn(vm: *mut c_void, stack: u64, base: u64, key: u64) -> u64>,
    pub assign_super_name: Option<
        extern "C" fn(vm: *mut c_void, op: u64, base: u64, name: u64, old: u64, value: u64) -> u64,
    >,
    pub assign_super_computed: Option<
        extern "C" fn(vm: *mut c_void, op: u64, base: u64, key: u64, old: u64, value: u64) -> u64,
    >,
    pub update_super_name: Option<
        extern "C" fn(vm: *mut c_void, op: u64, prefix: u64, base: u64, name: u64, old: u64) -> u64,
    >,
    pub update_super_computed: Option<
        extern "C" fn(vm: *mut c_void, op: u64, prefix: u64, base: u64, key: u64, old: u64) -> u64,
    >,
    pub delete_super: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    pub resolve_super_ref_name: Option<extern "C" fn(vm: *mut c_void, name: u64) -> u64>,
    pub resolve_super_ref_computed:
        Option<extern "C" fn(vm: *mut c_void, base: u64, key: u64) -> u64>,
}

impl JitHelpers {
    /// An empty table: any body that needs a slow path bails.
    pub fn none() -> Self {
        Self {
            binary_slow: None,
            concat_strings: None,
            relational_slow: None,
            update_value_slow: None,
            to_boolean_slow: None,
            tdz_error: None,
            gc_safepoint: None,
            get_member_name: None,
            get_member_computed: None,
            set_member_name: None,
            set_member_computed: None,
            rmw_compound_computed: None,
            rmw_update_computed: None,
            call_slow: None,
            leaf_call_probe: None,
            get_global: None,
            set_global: None,
            set_global_slot: None,
            load_ident: None,
            resolve_var_ident: None,
            put_var_reference: None,
            update_ident: None,
            assign_member_name: None,
            assign_member_computed: None,
            fast_array_element_write: None,
            dense_array_append: None,
            set_member_slot: None,
            load_context: None,
            store_context: None,
            init_context: None,
            update_context: None,
            load_per_iter: None,
            store_per_iter: None,
            update_per_iter: None,
            get_var_reference: None,
            update_var_reference: None,
            put_var_reference_op: None,
            pop_var_reference: None,
            create_function: None,
            create_arrow: None,
            create_function_decl: None,
            new_target: None,
            regexp_literal: None,
            tail_call: None,
            args_base: None,
            args_push: None,
            args_spread: None,
            call_vector: None,
            call_apply: None,
            apply_args_fill: None,
            tail_call_vector: None,
            tail_call_self_vector: None,
            array_begin: None,
            array_element: None,
            array_spread: None,
            array_hole: None,
            array_end: None,
            object_begin: None,
            object_init_name: None,
            object_init_computed: None,
            object_key_to_property_key: None,
            object_method_name: None,
            object_method_computed: None,
            object_accessor_name: None,
            object_accessor_computed: None,
            object_spread: None,
            push_str: None,
            concat_str: None,
            concat_str_const: None,
            push_const: None,
            load_const: None,
            enter_block: None,
            leave_block: None,
            enter_try: None,
            exit_try: None,
            return_control: None,
            break_control: None,
            continue_control: None,
            throw_control: None,
            finally_end: None,
            catch_bind: None,
            dispatch_error: None,
            switch_disc: None,
            switch_test: None,
            for_in_begin: None,
            for_in_next: None,
            for_of_begin: None,
            for_of_next: None,
            for_of_next_bind_local: None,
            for_of_close: None,
            for_of_close_all: None,
            enter_per_iteration: None,
            per_iteration: None,
            yield_suspend: None,
            await_suspend: None,
            destructure_begin: None,
            destructure_next: None,
            destructure_rest: None,
            destructure_obj_coercible: None,
            destructure_obj_key: None,
            destructure_obj_key_computed: None,
            destructure_obj_key_store: None,
            destructure_obj_key_get: None,
            destructure_obj_rest: None,
            destructure_close: None,
            destructure_obj_end: None,
            destructure_close_all: None,
            create_arguments: None,
            typeof_top: None,
            typed_array_length: None,
            get_super_base: None,
            this_value: None,
            get_super_name: None,
            get_super_computed: None,
            get_super_computed_keep: None,
            assign_super_name: None,
            assign_super_computed: None,
            update_super_name: None,
            update_super_computed: None,
            delete_super: None,
            resolve_super_ref_name: None,
            resolve_super_ref_computed: None,
        }
    }

    /// The address of a helper, when present.
    pub fn get(&self, helper: Helper) -> Option<u64> {
        match helper {
            Helper::BinarySlow => self.binary_slow.map(|f| f as usize as u64),
            Helper::ConcatStrings => self.concat_strings.map(|f| f as usize as u64),
            Helper::RelationalSlow => self.relational_slow.map(|f| f as usize as u64),
            Helper::UpdateValueSlow => self.update_value_slow.map(|f| f as usize as u64),
            Helper::ToBooleanSlow => self.to_boolean_slow.map(|f| f as usize as u64),
            Helper::TdzError => self.tdz_error.map(|f| f as usize as u64),
            Helper::GcSafepoint => self.gc_safepoint.map(|f| f as usize as u64),
            Helper::GetMemberName => self.get_member_name.map(|f| f as usize as u64),
            Helper::GetMemberComputed => self.get_member_computed.map(|f| f as usize as u64),
            Helper::SetMemberName => self.set_member_name.map(|f| f as usize as u64),
            Helper::SetMemberComputed => self.set_member_computed.map(|f| f as usize as u64),
            Helper::RmwCompoundComputed => self.rmw_compound_computed.map(|f| f as usize as u64),
            Helper::RmwUpdateComputed => self.rmw_update_computed.map(|f| f as usize as u64),
            Helper::CallSlow => self.call_slow.map(|f| f as usize as u64),
            Helper::LeafCallProbe => self.leaf_call_probe.map(|f| f as usize as u64),
            Helper::GetGlobal => self.get_global.map(|f| f as usize as u64),
            Helper::SetGlobal => self.set_global.map(|f| f as usize as u64),
            Helper::SetGlobalSlot => self.set_global_slot.map(|f| f as usize as u64),
            Helper::LoadIdent => self.load_ident.map(|f| f as usize as u64),
            Helper::ResolveVarIdent => self.resolve_var_ident.map(|f| f as usize as u64),
            Helper::PutVarReference => self.put_var_reference.map(|f| f as usize as u64),
            Helper::UpdateIdent => self.update_ident.map(|f| f as usize as u64),
            Helper::AssignMemberName => self.assign_member_name.map(|f| f as usize as u64),
            Helper::AssignMemberComputed => self.assign_member_computed.map(|f| f as usize as u64),
            Helper::FastArrayElementWrite => {
                self.fast_array_element_write.map(|f| f as usize as u64)
            }
            Helper::DenseArrayAppend => self.dense_array_append.map(|f| f as usize as u64),
            Helper::SetMemberSlot => self.set_member_slot.map(|f| f as usize as u64),
            Helper::LoadContext => self.load_context.map(|f| f as usize as u64),
            Helper::StoreContext => self.store_context.map(|f| f as usize as u64),
            Helper::InitContext => self.init_context.map(|f| f as usize as u64),
            Helper::UpdateContext => self.update_context.map(|f| f as usize as u64),
            Helper::LoadPerIter => self.load_per_iter.map(|f| f as usize as u64),
            Helper::StorePerIter => self.store_per_iter.map(|f| f as usize as u64),
            Helper::UpdatePerIter => self.update_per_iter.map(|f| f as usize as u64),
            Helper::GetVarReference => self.get_var_reference.map(|f| f as usize as u64),
            Helper::UpdateVarReference => self.update_var_reference.map(|f| f as usize as u64),
            Helper::PutVarReferenceOp => self.put_var_reference_op.map(|f| f as usize as u64),
            Helper::PopVarReference => self.pop_var_reference.map(|f| f as usize as u64),
            Helper::CreateFunction => self.create_function.map(|f| f as usize as u64),
            Helper::CreateArrow => self.create_arrow.map(|f| f as usize as u64),
            Helper::CreateFunctionDecl => self.create_function_decl.map(|f| f as usize as u64),
            Helper::NewTarget => self.new_target.map(|f| f as usize as u64),
            Helper::RegExpLiteral => self.regexp_literal.map(|f| f as usize as u64),
            Helper::TailCall => self.tail_call.map(|f| f as usize as u64),
            Helper::ArgsBase => self.args_base.map(|f| f as usize as u64),
            Helper::ArgsPush => self.args_push.map(|f| f as usize as u64),
            Helper::ArgsSpread => self.args_spread.map(|f| f as usize as u64),
            Helper::CallVector => self.call_vector.map(|f| f as usize as u64),
            Helper::CallApply => self.call_apply.map(|f| f as usize as u64),
            Helper::ApplyArgsFill => self.apply_args_fill.map(|f| f as usize as u64),
            Helper::TailCallVector => self.tail_call_vector.map(|f| f as usize as u64),
            Helper::TailCallSelfVector => self.tail_call_self_vector.map(|f| f as usize as u64),
            Helper::ArrayBegin => self.array_begin.map(|f| f as usize as u64),
            Helper::ArrayElement => self.array_element.map(|f| f as usize as u64),
            Helper::ArraySpread => self.array_spread.map(|f| f as usize as u64),
            Helper::ArrayHole => self.array_hole.map(|f| f as usize as u64),
            Helper::ArrayEnd => self.array_end.map(|f| f as usize as u64),
            Helper::ObjectBegin => self.object_begin.map(|f| f as usize as u64),
            Helper::ObjectInitName => self.object_init_name.map(|f| f as usize as u64),
            Helper::ObjectInitComputed => self.object_init_computed.map(|f| f as usize as u64),
            Helper::ObjectKeyToPropertyKey => {
                self.object_key_to_property_key.map(|f| f as usize as u64)
            }
            Helper::ObjectMethodName => self.object_method_name.map(|f| f as usize as u64),
            Helper::ObjectMethodComputed => self.object_method_computed.map(|f| f as usize as u64),
            Helper::ObjectAccessorName => self.object_accessor_name.map(|f| f as usize as u64),
            Helper::ObjectAccessorComputed => {
                self.object_accessor_computed.map(|f| f as usize as u64)
            }
            Helper::ObjectSpread => self.object_spread.map(|f| f as usize as u64),
            Helper::PushStr => self.push_str.map(|f| f as usize as u64),
            Helper::ConcatStr => self.concat_str.map(|f| f as usize as u64),
            Helper::ConcatStrConst => self.concat_str_const.map(|f| f as usize as u64),
            Helper::PushConst => self.push_const.map(|f| f as usize as u64),
            Helper::LoadConst => self.load_const.map(|f| f as usize as u64),
            Helper::EnterBlock => self.enter_block.map(|f| f as usize as u64),
            Helper::LeaveBlock => self.leave_block.map(|f| f as usize as u64),
            Helper::EnterTry => self.enter_try.map(|f| f as usize as u64),
            Helper::ExitTry => self.exit_try.map(|f| f as usize as u64),
            Helper::ReturnControl => self.return_control.map(|f| f as usize as u64),
            Helper::BreakControl => self.break_control.map(|f| f as usize as u64),
            Helper::ContinueControl => self.continue_control.map(|f| f as usize as u64),
            Helper::ThrowControl => self.throw_control.map(|f| f as usize as u64),
            Helper::FinallyEnd => self.finally_end.map(|f| f as usize as u64),
            Helper::CatchBind => self.catch_bind.map(|f| f as usize as u64),
            Helper::DispatchError => self.dispatch_error.map(|f| f as usize as u64),
            Helper::SwitchDisc => self.switch_disc.map(|f| f as usize as u64),
            Helper::SwitchTest => self.switch_test.map(|f| f as usize as u64),
            Helper::ForInBegin => self.for_in_begin.map(|f| f as usize as u64),
            Helper::ForInNext => self.for_in_next.map(|f| f as usize as u64),
            Helper::ForOfBegin => self.for_of_begin.map(|f| f as usize as u64),
            Helper::ForOfNext => self.for_of_next.map(|f| f as usize as u64),
            Helper::ForOfNextBindLocal => self.for_of_next_bind_local.map(|f| f as usize as u64),
            Helper::ForOfClose => self.for_of_close.map(|f| f as usize as u64),
            Helper::ForOfCloseAll => self.for_of_close_all.map(|f| f as usize as u64),
            Helper::EnterPerIteration => self.enter_per_iteration.map(|f| f as usize as u64),
            Helper::PerIteration => self.per_iteration.map(|f| f as usize as u64),
            Helper::YieldSuspend => self.yield_suspend.map(|f| f as usize as u64),
            Helper::AwaitSuspend => self.await_suspend.map(|f| f as usize as u64),
            Helper::DestructureBegin => self.destructure_begin.map(|f| f as usize as u64),
            Helper::DestructureNext => self.destructure_next.map(|f| f as usize as u64),
            Helper::DestructureRest => self.destructure_rest.map(|f| f as usize as u64),
            Helper::DestructureObjCoercible => {
                self.destructure_obj_coercible.map(|f| f as usize as u64)
            }
            Helper::DestructureObjKey => self.destructure_obj_key.map(|f| f as usize as u64),
            Helper::DestructureObjKeyComputed => {
                self.destructure_obj_key_computed.map(|f| f as usize as u64)
            }
            Helper::DestructureObjKeyStore => {
                self.destructure_obj_key_store.map(|f| f as usize as u64)
            }
            Helper::DestructureObjKeyGet => self.destructure_obj_key_get.map(|f| f as usize as u64),
            Helper::DestructureObjRest => self.destructure_obj_rest.map(|f| f as usize as u64),
            Helper::DestructureClose => self.destructure_close.map(|f| f as usize as u64),
            Helper::DestructureObjEnd => self.destructure_obj_end.map(|f| f as usize as u64),
            Helper::DestructureCloseAll => self.destructure_close_all.map(|f| f as usize as u64),
            Helper::CreateArguments => self.create_arguments.map(|f| f as usize as u64),
            Helper::TypeofTop => self.typeof_top.map(|f| f as usize as u64),
            Helper::TypedArrayLength => self.typed_array_length.map(|f| f as usize as u64),
            Helper::GetSuperBase => self.get_super_base.map(|f| f as usize as u64),
            Helper::ThisValue => self.this_value.map(|f| f as usize as u64),
            Helper::GetSuperName => self.get_super_name.map(|f| f as usize as u64),
            Helper::GetSuperComputed => self.get_super_computed.map(|f| f as usize as u64),
            Helper::GetSuperComputedKeep => self.get_super_computed_keep.map(|f| f as usize as u64),
            Helper::AssignSuperName => self.assign_super_name.map(|f| f as usize as u64),
            Helper::AssignSuperComputed => self.assign_super_computed.map(|f| f as usize as u64),
            Helper::UpdateSuperName => self.update_super_name.map(|f| f as usize as u64),
            Helper::UpdateSuperComputed => self.update_super_computed.map(|f| f as usize as u64),
            Helper::DeleteSuper => self.delete_super.map(|f| f as usize as u64),
            Helper::ResolveSuperRefName => self.resolve_super_ref_name.map(|f| f as usize as u64),
            Helper::ResolveSuperRefComputed => {
                self.resolve_super_ref_computed.map(|f| f as usize as u64)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test doubles. The scaffold tests never hit these except to prove the call
// ABI works end to end; each returns a fixed marker value. They are `extern
// "C"` because the compiled code calls them through the platform ABI.
// ---------------------------------------------------------------------------

/// Returns `42` — proves `binary_slow` was called with the right ABI.
pub extern "C" fn test_binary_slow(_vm: *mut c_void, _op: u64, _a: u64, _b: u64) -> u64 {
    Value::Number(42.0).bits()
}

/// Returns the left operand unchanged — proves `concat_strings` was called
/// with the right ABI.
pub extern "C" fn test_concat_strings(_vm: *mut c_void, a: u64, _b: u64) -> u64 {
    a
}

pub extern "C" fn test_relational_slow(_vm: *mut c_void, _op: u64, _a: u64, _b: u64) -> u64 {
    1
}

pub extern "C" fn test_update_value_slow(_vm: *mut c_void, _inc: u64, _value: u64) -> u64 {
    Value::Number(7.0).bits()
}

pub extern "C" fn test_to_boolean_slow(_vm: *mut c_void, _value: u64) -> u64 {
    1
}

pub extern "C" fn test_tdz_error(_vm: *mut c_void) -> u64 {
    panic!("the TDZ error slow path ran in a test (a lexical slot was read before init)")
}

pub extern "C" fn test_gc_safepoint(_vm: *mut c_void) -> u64 {
    0
}

pub extern "C" fn test_get_member_name(_vm: *mut c_void, _object: u64, _name: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_get_member_computed(_vm: *mut c_void, _object: u64, _key: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_set_member_name(
    _vm: *mut c_void,
    _object: u64,
    _name: u64,
    value: u64,
) -> u64 {
    value
}

pub extern "C" fn test_set_member_computed(
    _vm: *mut c_void,
    _object: u64,
    _key: u64,
    value: u64,
) -> u64 {
    value
}

pub extern "C" fn test_rmw_compound_computed(
    _vm: *mut c_void,
    _object: u64,
    _key: u64,
    value: u64,
    _op: u64,
) -> u64 {
    value
}

pub extern "C" fn test_rmw_update_computed(
    _vm: *mut c_void,
    _object: u64,
    _key: u64,
    _op: u64,
) -> u64 {
    0
}

/// Returns 42 — proves `get_global` was called with the right ABI.
pub extern "C" fn test_get_global(_vm: *mut c_void, _name: u64) -> u64 {
    Value::Number(42.0).bits()
}

pub extern "C" fn test_set_global(_vm: *mut c_void, _name: u64, value: u64) -> u64 {
    value
}

pub extern "C" fn test_set_global_slot(
    _vm: *mut c_void,
    _name: u64,
    _slot: u64,
    value: u64,
) -> u64 {
    value
}

/// Returns 42 — proves `load_ident` was called with the right ABI.
pub extern "C" fn test_load_ident(_vm: *mut c_void, _name: u64) -> u64 {
    Value::Number(42.0).bits()
}

pub extern "C" fn test_resolve_var_ident(_vm: *mut c_void, _name: u64) -> u64 {
    0
}

pub extern "C" fn test_put_var_reference(_vm: *mut c_void, value: u64) -> u64 {
    value
}

/// `old + 1` — proves the `update_ident` arguments arrive in order.
pub extern "C" fn test_update_ident(
    _vm: *mut c_void,
    _name: u64,
    _op: u64,
    _prefix: u64,
    old: u64,
) -> u64 {
    let old = Value::from_bits(old).as_number().unwrap_or(0.0);
    Value::Number(old + 1.0).bits()
}

/// `old + value` for a compound op (any op but `=`), else `value` — proves
/// the assign arguments arrive in order.
pub extern "C" fn test_assign_member_name(
    _vm: *mut c_void,
    op: u64,
    _object: u64,
    _name: u64,
    old: u64,
    value: u64,
) -> u64 {
    let value_num = Value::from_bits(value).as_number().unwrap_or(0.0);
    if op == 0 {
        Value::Number(value_num).bits()
    } else {
        let old_num = Value::from_bits(old).as_number().unwrap_or(0.0);
        Value::Number(old_num + value_num).bits()
    }
}

pub extern "C" fn test_assign_member_computed(
    _vm: *mut c_void,
    op: u64,
    _object: u64,
    _key: u64,
    old: u64,
    value: u64,
) -> u64 {
    let value_num = Value::from_bits(value).as_number().unwrap_or(0.0);
    if op == 0 {
        Value::Number(value_num).bits()
    } else {
        let old_num = Value::from_bits(old).as_number().unwrap_or(0.0);
        Value::Number(old_num + value_num).bits()
    }
}

/// Returns 1 — proves the dense-array fast write was called with the right
/// ABI (the scaffold has no real array to store into).
pub extern "C" fn test_fast_array_element_write(
    _vm: *mut c_void,
    _object: u64,
    _key: u64,
    _value: u64,
) -> u64 {
    1
}

pub extern "C" fn test_dense_array_append(
    _vm: *mut c_void,
    _object: u64,
    _index: u64,
    _value: u64,
) -> u64 {
    1
}

/// Returns the stored value unchanged — proves `set_member_slot` was called
/// with the right ABI.
pub extern "C" fn test_set_member_slot(
    _vm: *mut c_void,
    _object: u64,
    _name: u64,
    value: u64,
) -> u64 {
    value
}

/// Returns `42` — proves `load_context` was called with the right ABI.
pub extern "C" fn test_load_context(_vm: *mut c_void, _depth: u64, _index: u64) -> u64 {
    Value::Number(42.0).bits()
}

/// Returns the stored value — proves `store_context` was called.
pub extern "C" fn test_store_context(
    _vm: *mut c_void,
    _depth: u64,
    _index: u64,
    value: u64,
) -> u64 {
    value
}

/// Returns the stored value — proves `init_context` was called.
pub extern "C" fn test_init_context(_vm: *mut c_void, _index: u64, value: u64) -> u64 {
    value
}

/// `old + 1` — proves the `update_context` arguments arrive in order.
pub extern "C" fn test_update_context(
    _vm: *mut c_void,
    _depth: u64,
    _index: u64,
    _op: u64,
    _prefix: u64,
) -> u64 {
    Value::Number(43.0).bits()
}

/// Returns `44` — proves `load_per_iter` was called with the right ABI.
pub extern "C" fn test_load_per_iter(_vm: *mut c_void, _depth: u64, _index: u64) -> u64 {
    Value::Number(44.0).bits()
}

/// Returns the stored value — proves `store_per_iter` was called.
pub extern "C" fn test_store_per_iter(
    _vm: *mut c_void,
    _depth: u64,
    _index: u64,
    value: u64,
) -> u64 {
    value
}

/// Returns `45` — proves `update_per_iter` was called.
pub extern "C" fn test_update_per_iter(
    _vm: *mut c_void,
    _depth: u64,
    _index: u64,
    _op: u64,
    _prefix: u64,
) -> u64 {
    Value::Number(45.0).bits()
}

/// Returns `46` — proves `get_var_reference` was called.
pub extern "C" fn test_get_var_reference(_vm: *mut c_void) -> u64 {
    Value::Number(46.0).bits()
}

/// `old + 1` — proves the `update_var_reference` arguments arrive in order.
pub extern "C" fn test_update_var_reference(
    _vm: *mut c_void,
    _op: u64,
    _prefix: u64,
    old: u64,
) -> u64 {
    let old = Value::from_bits(old).as_number().unwrap_or(0.0);
    Value::Number(old + 1.0).bits()
}

/// `old + value` — proves the `put_var_reference_op` arguments arrive.
pub extern "C" fn test_put_var_reference_op(
    _vm: *mut c_void,
    _op: u64,
    old: u64,
    value: u64,
) -> u64 {
    let old = Value::from_bits(old).as_number().unwrap_or(0.0);
    let value = Value::from_bits(value).as_number().unwrap_or(0.0);
    Value::Number(old + value).bits()
}

/// Returns `0` — proves `pop_var_reference` was called.
pub extern "C" fn test_pop_var_reference(_vm: *mut c_void) -> u64 {
    0
}

/// Returns `47` — proves `create_function` was called with the step index.
pub extern "C" fn test_create_function(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(47.0).bits()
}

/// Returns `48` — proves `create_arrow` was called with the step index.
pub extern "C" fn test_create_arrow(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(48.0).bits()
}

/// Returns `49` — proves `create_function_decl` was called with the step
/// index.
pub extern "C" fn test_create_function_decl(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(49.0).bits()
}

/// Returns `50` — proves `new_target` was called.
pub extern "C" fn test_new_target(_vm: *mut c_void) -> u64 {
    Value::Number(50.0).bits()
}

/// Returns `51` — proves `regexp_literal` was called with the step index.
pub extern "C" fn test_regexp_literal(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(51.0).bits()
}

/// Returns `52` — proves `tail_call` was called with the right ABI.
pub extern "C" fn test_tail_call(
    _vm: *mut c_void,
    _callee: u64,
    _this: u64,
    _argc: u64,
    _args: *mut u64,
    _direct_eval: u64,
) -> u64 {
    Value::Number(52.0).bits()
}

pub extern "C" fn test_args_base(_vm: *mut c_void) -> u64 {
    0
}

pub extern "C" fn test_args_push(_vm: *mut c_void, _value: u64) -> u64 {
    0
}

pub extern "C" fn test_args_spread(_vm: *mut c_void, _iterable: u64) -> u64 {
    0
}

pub extern "C" fn test_call_vector(
    _vm: *mut c_void,
    _this: u64,
    _callee: u64,
    _direct_eval: u64,
) -> u64 {
    Value::Number(53.0).bits()
}

pub extern "C" fn test_tail_call_vector(
    _vm: *mut c_void,
    _this: u64,
    _callee: u64,
    _direct_eval: u64,
) -> u64 {
    Value::Number(54.0).bits()
}

/// Returns `1` (the success signal) — proves `tail_call_self_vector` was
/// called with the right (ctx-only) ABI; the scaffold tests exercise the
/// frame-rebind against a real Vm through the runtime's own integration
/// tests instead (the rebind needs a live frame buffer).
pub extern "C" fn test_tail_call_self_vector(_vm: *mut c_void) -> u64 {
    1
}

/// `array_begin` double: returns a fixed heap value (the array) the element
/// helpers echo back, proving the ABI wiring.
pub extern "C" fn test_array_begin(_vm: *mut c_void) -> u64 {
    Value::Number(60.0).bits()
}

/// `array_element` double: echoes the array operand (the element write is
/// exercised by the runtime's own integration tests against a real array).
pub extern "C" fn test_array_element(_vm: *mut c_void, array: u64, _value: u64) -> u64 {
    array
}

pub extern "C" fn test_array_spread(_vm: *mut c_void, array: u64, _iterable: u64) -> u64 {
    array
}

pub extern "C" fn test_array_hole(_vm: *mut c_void) -> u64 {
    0
}

pub extern "C" fn test_array_end(_vm: *mut c_void, array: u64) -> u64 {
    array
}

/// `object_begin` double: returns 70 (the object the property steps echo).
pub extern "C" fn test_object_begin(_vm: *mut c_void) -> u64 {
    Value::Number(70.0).bits()
}

/// The property-step doubles echo the object operand; the real definitions
/// are exercised by the runtime's integration tests against a live object.
pub extern "C" fn test_object_init_name(
    _vm: *mut c_void,
    object: u64,
    _name: u64,
    _set_name: u64,
    _shorthand: u64,
    _value: u64,
) -> u64 {
    object
}

pub extern "C" fn test_object_init_computed(
    _vm: *mut c_void,
    object: u64,
    _key: u64,
    _set_name: u64,
    _value: u64,
) -> u64 {
    object
}

pub extern "C" fn test_object_key_to_property_key(_vm: *mut c_void, key: u64) -> u64 {
    key
}

pub extern "C" fn test_object_method_name(_vm: *mut c_void, object: u64, _step: u64) -> u64 {
    object
}

pub extern "C" fn test_object_method_computed(
    _vm: *mut c_void,
    object: u64,
    _key: u64,
    _step: u64,
) -> u64 {
    object
}

pub extern "C" fn test_object_accessor_name(_vm: *mut c_void, object: u64, _step: u64) -> u64 {
    object
}

pub extern "C" fn test_object_accessor_computed(
    _vm: *mut c_void,
    object: u64,
    _key: u64,
    _step: u64,
) -> u64 {
    object
}

pub extern "C" fn test_object_spread(_vm: *mut c_void, object: u64, _from: u64) -> u64 {
    object
}

/// `push_str` double: returns 80 (the literal value the concat steps echo).
pub extern "C" fn test_push_str(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(80.0).bits()
}

pub extern "C" fn test_concat_str(_vm: *mut c_void, _value: u64, acc: u64) -> u64 {
    acc
}

pub extern "C" fn test_concat_str_const(_vm: *mut c_void, acc: u64, _step: u64) -> u64 {
    acc
}

/// `push_const` double: returns 81 (the heap-constant payload value).
pub extern "C" fn test_push_const(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(81.0).bits()
}

/// `load_const` double: returns 82 (the register body's heap constant).
pub extern "C" fn test_load_const(_vm: *mut c_void, _step: u64, _op: u64, _field: u64) -> u64 {
    Value::Number(82.0).bits()
}

/// Cut 55 control-dispatch doubles: each returns a fixed marker value (the
/// scaffolds that exercise them never reach a real dispatch — they prove the
/// call ABI).
pub extern "C" fn test_enter_block(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(83.0).bits()
}

pub extern "C" fn test_leave_block(_vm: *mut c_void) -> u64 {
    Value::Number(84.0).bits()
}

pub extern "C" fn test_enter_try(_vm: *mut c_void, _handler: u64) -> u64 {
    Value::Number(85.0).bits()
}

pub extern "C" fn test_exit_try(_vm: *mut c_void, _ip: u64, _after: u64) -> u64 {
    Value::Number(86.0).bits()
}

pub extern "C" fn test_return_control(_vm: *mut c_void, _ip: u64, value: u64) -> u64 {
    value
}

pub extern "C" fn test_break_control(_vm: *mut c_void, _ip: u64, target: u64) -> u64 {
    target
}

pub extern "C" fn test_continue_control(_vm: *mut c_void, _ip: u64, target: u64) -> u64 {
    target
}

pub extern "C" fn test_throw_control(_vm: *mut c_void, _ip: u64, value: u64) -> u64 {
    value
}

pub extern "C" fn test_finally_end(_vm: *mut c_void, _ip: u64) -> u64 {
    Value::Number(87.0).bits()
}

pub extern "C" fn test_catch_bind(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(88.0).bits()
}

pub extern "C" fn test_dispatch_error(_vm: *mut c_void, _ip: u64) -> u64 {
    Value::Number(89.0).bits()
}

pub extern "C" fn test_switch_disc(_vm: *mut c_void, _value: u64) -> u64 {
    Value::Number(90.0).bits()
}

pub extern "C" fn test_switch_test(_vm: *mut c_void, _case: u64, test: u64) -> u64 {
    test
}

/// Cut 57 iterator-machinery doubles: the fetch helpers return 1 (element
/// fetched — the scaffolds never dereference the stack pointer), the env
/// creators return a marker value.
pub extern "C" fn test_for_in_begin(_vm: *mut c_void, _value: u64) -> u64 {
    Value::Number(91.0).bits()
}

pub extern "C" fn test_for_in_next(_vm: *mut c_void, _stack: u64) -> u64 {
    1
}

pub extern "C" fn test_for_of_begin(_vm: *mut c_void, _step: u64, _value: u64) -> u64 {
    Value::Number(92.0).bits()
}

pub extern "C" fn test_for_of_next(_vm: *mut c_void, _stack: u64) -> u64 {
    1
}

pub extern "C" fn test_for_of_next_bind_local(_vm: *mut c_void, _slot: u64) -> u64 {
    1
}

pub extern "C" fn test_for_of_close(_vm: *mut c_void) -> u64 {
    Value::Number(93.0).bits()
}

pub extern "C" fn test_for_of_close_all(_vm: *mut c_void) -> u64 {
    Value::Number(96.0).bits()
}

pub extern "C" fn test_enter_per_iteration(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(94.0).bits()
}

pub extern "C" fn test_per_iteration(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(95.0).bits()
}

/// Cut 58 suspension doubles: return the `DISPATCH_SUSPEND` sentinel
/// (`u64::MAX - 2`) so a scaffold body with a `yield`/`await` ends the
/// segment — the scaffolds never inspect the ctx suspension payload.
pub extern "C" fn test_yield_suspend(
    _vm: *mut c_void,
    _sp: u64,
    _value: u64,
    _delegate: u64,
    _ip: u64,
) -> u64 {
    u64::MAX - 2
}

pub extern "C" fn test_await_suspend(_vm: *mut c_void, _sp: u64, _value: u64, _ip: u64) -> u64 {
    u64::MAX - 2
}

/// Cut 59 destructure doubles: `DestructureNext`/`DestructureRest`/
/// `DestructureObjKeyGet` return a marker value the scaffolds push;
/// `DestructureObjKey`/`DestructureObjRest` read their step payload through
/// the same channel as the real helpers; the rest complete with `undefined`.
pub extern "C" fn test_destructure_begin(_vm: *mut c_void, _value: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_destructure_next(_vm: *mut c_void) -> u64 {
    Value::Number(42.0).bits()
}

pub extern "C" fn test_destructure_rest(_vm: *mut c_void) -> u64 {
    Value::Number(43.0).bits()
}

pub extern "C" fn test_destructure_obj_coercible(_vm: *mut c_void, _value: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_destructure_obj_key(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(44.0).bits()
}

pub extern "C" fn test_destructure_obj_key_computed(_vm: *mut c_void, _key: u64) -> u64 {
    Value::Number(45.0).bits()
}

pub extern "C" fn test_destructure_obj_key_store(_vm: *mut c_void, _key: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_destructure_obj_key_get(_vm: *mut c_void) -> u64 {
    Value::Number(46.0).bits()
}

pub extern "C" fn test_destructure_obj_rest(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(47.0).bits()
}

pub extern "C" fn test_destructure_close(_vm: *mut c_void) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_destructure_obj_end(_vm: *mut c_void) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_destructure_close_all(_vm: *mut c_void) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_create_arguments(_vm: *mut c_void, _step: u64) -> u64 {
    Value::Number(48.0).bits()
}

pub extern "C" fn test_typeof_top(_vm: *mut c_void, _value: u64) -> u64 {
    Value::String(crux::Handle::new(crux::JsString::from_utf8("function"))).bits()
}

/// The typed-array length probe double: the canonical-NaN sentinel (the
/// scaffold never reads a real typed array).
pub extern "C" fn test_typed_array_length(_vm: *mut c_void, _object: u64) -> u64 {
    Value::Number(f64::NAN).bits()
}

/// Cut 61 super doubles: reads return marker values, writes/updates return
/// the assigned/updated value, `DeleteSuper` returns 0 (the scaffolds never
/// hit the error path), and the reference resolutions complete with
/// `undefined`.
pub extern "C" fn test_get_super_base(_vm: *mut c_void) -> u64 {
    Value::Number(50.0).bits()
}

pub extern "C" fn test_this_value(_vm: *mut c_void) -> u64 {
    Value::Number(51.0).bits()
}

pub extern "C" fn test_get_super_name(_vm: *mut c_void, _base: u64, _name: u64) -> u64 {
    Value::Number(52.0).bits()
}

pub extern "C" fn test_get_super_computed(_vm: *mut c_void, _base: u64, _key: u64) -> u64 {
    Value::Number(53.0).bits()
}

pub extern "C" fn test_get_super_computed_keep(
    _vm: *mut c_void,
    _stack: u64,
    _base: u64,
    _key: u64,
) -> u64 {
    Value::Number(54.0).bits()
}

pub extern "C" fn test_assign_super_name(
    _vm: *mut c_void,
    _op: u64,
    _base: u64,
    _name: u64,
    _old: u64,
    value: u64,
) -> u64 {
    value
}

pub extern "C" fn test_assign_super_computed(
    _vm: *mut c_void,
    _op: u64,
    _base: u64,
    _key: u64,
    _old: u64,
    value: u64,
) -> u64 {
    value
}

pub extern "C" fn test_update_super_name(
    _vm: *mut c_void,
    _op: u64,
    _prefix: u64,
    _base: u64,
    _name: u64,
    old: u64,
) -> u64 {
    old
}

pub extern "C" fn test_update_super_computed(
    _vm: *mut c_void,
    _op: u64,
    _prefix: u64,
    _base: u64,
    _key: u64,
    old: u64,
) -> u64 {
    old
}

pub extern "C" fn test_delete_super(_vm: *mut c_void) -> u64 {
    0
}

pub extern "C" fn test_resolve_super_ref_name(_vm: *mut c_void, _name: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_resolve_super_ref_computed(_vm: *mut c_void, _base: u64, _key: u64) -> u64 {
    Value::Undefined.bits()
}

/// Sums its numeric arguments — proves the `args` pointer/`argc` ABI the
/// `CallFast` lowering passes. The callers (compiled test code and the test
/// harness) guarantee `args` points at `argc` valid slots.
#[cfg(test)]
pub(crate) extern "C" fn test_call_slow(
    _vm: *mut c_void,
    _callee: u64,
    _this: u64,
    argc: u64,
    args: *mut u64,
    _direct_eval: u64,
) -> u64 {
    let mut sum = 0.0;
    for i in 0..argc {
        // SAFETY: the test harness passes a buffer with `argc` slots.
        sum += Value::from_bits(unsafe { *args.add(i as usize) })
            .as_number()
            .unwrap_or(0.0);
    }
    Value::Number(sum).bits()
}

/// The probe test double: always rejects (the unit tests exercise the
/// `call_slow` fallback path).
#[cfg(test)]
pub(crate) extern "C" fn test_leaf_call_probe(
    _vm: *mut c_void,
    _callee: u64,
    _args: *mut u64,
    _argc: u64,
    _site: u64,
) -> u64 {
    0
}

/// Sums the argument region (the `thisArg` first), mirroring
/// `test_call_slow`'s ABI proof for the `CallApply` lowering.
#[cfg(test)]
pub(crate) extern "C" fn test_call_apply(
    _vm: *mut c_void,
    _resolved: u64,
    _callee: u64,
    argc: u64,
    args: *mut u64,
    _kind: u64,
) -> u64 {
    let mut sum = 0.0;
    for i in 0..argc {
        // SAFETY: the test harness passes a buffer with `argc` slots.
        sum += Value::from_bits(unsafe { *args.add(i as usize) })
            .as_number()
            .unwrap_or(0.0);
    }
    Value::Number(sum).bits()
}

/// The `apply_args_fill` ABI proof: writes `n` elements (1..=n) to `dest`
/// when the `arg_array` argument is the number `n` and returns `n` — the
/// compiled `CallApply` fast path's dense-fill test double never runs
/// against a real array, so the count/pointer convention is what matters.
#[cfg(test)]
pub(crate) extern "C" fn test_apply_args_fill(_vm: *mut c_void, arg_array: u64, dest: u64) -> u64 {
    let count = Value::from_bits(arg_array).as_number().unwrap_or(0.0) as usize;
    let dest = dest as *mut u64;
    for i in 0..count {
        // SAFETY: the test harness passes a buffer with `count` slots.
        unsafe { *dest.add(i) = Value::Number((i + 1) as f64).bits() };
    }
    count as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_table_has_no_helpers() {
        let none = JitHelpers::none();
        for h in [
            Helper::BinarySlow,
            Helper::RelationalSlow,
            Helper::UpdateValueSlow,
            Helper::ToBooleanSlow,
            Helper::TdzError,
            Helper::GetMemberName,
            Helper::GetMemberComputed,
            Helper::SetMemberName,
            Helper::SetMemberComputed,
            Helper::CallSlow,
            Helper::CallApply,
            Helper::GetGlobal,
            Helper::SetGlobal,
            Helper::LoadIdent,
            Helper::ResolveVarIdent,
            Helper::PutVarReference,
            Helper::UpdateIdent,
            Helper::AssignMemberName,
            Helper::AssignMemberComputed,
            Helper::FastArrayElementWrite,
            Helper::DenseArrayAppend,
            Helper::TypedArrayLength,
        ] {
            assert!(none.get(h).is_none(), "{} should be None", h.name());
        }
    }

    #[test]
    fn helper_names_are_stable() {
        assert_eq!(Helper::BinarySlow.name(), "binary_slow");
        assert_eq!(Helper::TdzError.name(), "tdz_error");
        assert_eq!(Helper::CallSlow.name(), "call_slow");
        assert_eq!(Helper::CallApply.name(), "call_apply");
        assert_eq!(Helper::GetGlobal.name(), "get_global");
        assert_eq!(Helper::SetGlobal.name(), "set_global");
        assert_eq!(Helper::LoadIdent.name(), "load_ident");
        assert_eq!(Helper::ResolveVarIdent.name(), "resolve_var_ident");
        assert_eq!(Helper::PutVarReference.name(), "put_var_reference");
        assert_eq!(Helper::UpdateIdent.name(), "update_ident");
        assert_eq!(Helper::AssignMemberName.name(), "assign_member_name");
        assert_eq!(
            Helper::AssignMemberComputed.name(),
            "assign_member_computed"
        );
        assert_eq!(
            Helper::FastArrayElementWrite.name(),
            "fast_array_element_write"
        );
        assert_eq!(Helper::DenseArrayAppend.name(), "dense_array_append");
        assert_eq!(Helper::TypedArrayLength.name(), "typed_array_length");
    }

    #[test]
    fn test_double_proves_the_c_abi() {
        // The test doubles are real extern "C" fn pointers, so a call through
        // them is exactly what the compiled code emits.
        let f = test_binary_slow as extern "C" fn(*mut c_void, u64, u64, u64) -> u64;
        let bits = f(std::ptr::null_mut(), 0, 1, 2);
        assert_eq!(bits, Value::Number(42.0).bits());
    }
}
