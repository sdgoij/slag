//! Wasm → native compilation (Cut 11, Wave 0/1).
//!
//! An optional Cranelift backend for the `crates/wasm` interpreter. The
//! interpreter stays the correctness oracle; this module lowers a subset of
//! module-defined leaf functions to native code and hands the store a
//! compiled entry it can invoke instead of running the `Engine` loop.
//!
//! Supported subset (everything else bails to the interpreter, per
//! function):
//! - function signatures and locals over `i32`/`i64`/`f32`/`f64`;
//! - `const`, `local.get/set/tee`, `drop`, `select`, numeric `Num` ops
//!   (integer arithmetic/comparisons/shifts, `clz`/`ctz`/`popcnt`, the
//!   float ops below, and `i64.wrap_i32`), `nop`, `unreachable`, and
//!   `return`;
//! - float arithmetic/rounding/sqrt/comparisons reproduce the
//!   interpreter's canonical-quiet-NaN policy exactly (`abs`/`neg`/
//!   `copysign` stay raw bit ops); `f32.min/max` and the
//!   promote/demote conversions are deferred;
//! - structured control flow (`block`/`loop`/`if`/`else`/`br`/`br_if`/
//!   `br_table`) with numeric block types of any parameter/result count
//!   (parameters enter the body as its initial operand stack and ride a
//!   loop's header as block parameters; a parameterized `if` needs an
//!   explicit else branch);
//! - numeric loads/stores, `memory.size`, and `memory.grow` over any 32-bit
//!   memory, each access bounds-checking against a per-call descriptor array
//!   (one (data pointer, byte length) pair per module memory index, entry
//!   ABI params 2/3). `memory.grow` calls a runtime helper (entry params
//!   7-9) that reallocates the store cell's backing `Vec` and rewrites the
//!   descriptor entry, so later accesses see the grown buffer;
//! - `global.get`/`global.set` of numeric module-defined globals, through a
//!   caller-owned buffer (one u64 slot per used global): the compiled body
//!   reads and mutates slots in place, so its writes reach the store even
//!   when it traps. Imported globals (whose cells can alias), memory64,
//!   tables, calls, refs, SIMD, and GC are not lowered yet.
//!
//! ABI: a compiled entry is
//! `unsafe extern "C" fn(args, nargs, mems, ncount, gvals, out, nout,
//! store, instance, grow) -> i32`. Arguments, the descriptor array
//! pointer/count, the global-values and result buffers, and results travel
//! as 64-bit values (an `i32` uses the low 32 bits), so the Rust trampoline
//! and the Cranelift signature can never disagree about float argument
//! classification. The returned `i32` is a trap code (0 = ok); the result
//! buffer is written only when the function completes normally, so a trap
//! never produces partial results.

use std::sync::{Arc, OnceLock};

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64, Offset32};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, Function, InstBuilder, MemFlagsData, Signature, Type, UserFuncName,
    Value as ClifValue, types,
};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_control::ControlPlane;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};

use crate::exec::ExecFail;
use crate::instr::{Instr, LoadOp, NumOp, StoreOp};
use crate::module::{FuncBody, ImportDesc, Module};
use crate::types::{BlockType, FuncType, ValType};
use crate::values::{QNAN32, QNAN64, Trap, Value};

/// Trap code returned by a compiled entry; 0 means success.
pub const TRAP_NONE: i32 = 0;
pub const TRAP_UNREACHABLE: i32 = 1;
pub const TRAP_INT_DIVIDE_BY_ZERO: i32 = 2;
pub const TRAP_INT_OVERFLOW: i32 = 3;
pub const TRAP_MEMORY_OOB: i32 = 4;

/// Map a compiled trap code back to the interpreter's [`Trap`].
pub fn trap_of(code: i32) -> Trap {
    match code {
        TRAP_UNREACHABLE => Trap::Unreachable,
        TRAP_INT_DIVIDE_BY_ZERO => Trap::IntegerDivideByZero,
        TRAP_INT_OVERFLOW => Trap::IntegerOverflow,
        TRAP_MEMORY_OOB => Trap::OutOfBoundsMemoryAccess,
        _ => Trap::Unreachable,
    }
}

/// Runtime pointers/helper addresses a compiled body may call back into,
/// passed as entry params 7-9 (`store`, `instance`, `grow`).
#[derive(Clone, Copy)]
pub struct CompiledRuntime {
    /// Raw `*mut Store` address.
    pub store: u64,
    /// The invoked instance's index.
    pub instance: u64,
    /// The `memory.grow` helper's address (0 when unused is fine; bodies that
    /// grow load it from this param).
    pub grow: u64,
}

/// The native entry point produced by the compiler.
///
/// `(args, nargs, mems, ncount, gvals, out, nout, store, instance, grow)`:
/// `mems` points at a caller-owned descriptor array holding, per module
/// memory index, its data pointer then byte length (each `u64`); `ncount`
/// is the descriptor count. `gvals` is a caller-owned buffer, one u64 slot
/// per module global the body uses, holding the initial values on entry and
/// the (possibly mutated) values on return — so `global.set` is visible to
/// the store even when a later instruction traps. `store`/`instance`/
/// `grow` let a body call the `memory.grow` helper. Returns the trap code
/// (0 = ok).
pub type CompiledEntry = unsafe extern "C" fn(
    args: *const u64,
    nargs: u64,
    mems: *const u64,
    ncount: u64,
    gvals: *mut u64,
    out: *mut u64,
    nout: u64,
    store: u64,
    instance: u64,
    grow: u64,
) -> i32;

/// One compiled function body: the entry plus the executable-code
/// allocation that must outlive every call into it.
pub struct CompiledFunc {
    _code: ExecutableCode,
    entry: CompiledEntry,
    /// The module global indices (index space) the body reads or writes, in
    /// the same order as their `gvals` buffer slots.
    globals: Vec<u32>,
}

impl CompiledFunc {
    /// The module global indices whose values ride the per-call `gvals`
    /// buffer (slot order).
    pub fn used_globals(&self) -> &[u32] {
        &self.globals
    }

    /// Invoke the compiled body with `args` (bit patterns) and return the
    /// results (bit patterns) plus the trap code. `runtime` carries the
    /// store/instance pointers and the `memory.grow` helper address the body
    /// may call back into (entry params 7-9).
    pub fn call(
        &self,
        args: &[u64],
        mems: (*const u64, u64),
        globals: &mut [u64],
        out: &mut [u64],
        runtime: CompiledRuntime,
    ) -> i32 {
        // SAFETY: `entry` is a plain function pointer into `_code`, which
        // this struct keeps alive and executable for its whole lifetime; the
        // caller passes buffers of the declared lengths and a descriptor
        // array that stays valid for the (leaf) call. `runtime` addresses
        // the owning store/instance, which the caller keeps alive for the
        // call.
        unsafe {
            (self.entry)(
                args.as_ptr(),
                args.len() as u64,
                mems.0,
                mems.1,
                globals.as_mut_ptr(),
                out.as_mut_ptr(),
                out.len() as u64,
                runtime.store,
                runtime.instance,
                runtime.grow,
            )
        }
    }
}

/// Run a compiled entry with the interpreter's [`Value`] argument model,
/// returning interpreter values or the trapped [`ExecFail`].
pub fn run_compiled(
    func: &CompiledFunc,
    ty: &FuncType,
    mems: (*const u64, u64),
    globals: &mut [u64],
    args: &[Value],
    runtime: CompiledRuntime,
) -> Result<Vec<Value>, ExecFail> {
    if args.len() != ty.params.len() {
        return Err(ExecFail::Unsupported("compiled function arity"));
    }
    let mut input = Vec::with_capacity(args.len());
    for (value, param) in args.iter().zip(&ty.params) {
        let bits = match (param, value) {
            (ValType::I32, Value::I32(bits)) => *bits as u32 as u64,
            (ValType::I64, Value::I64(bits)) => *bits as u64,
            (ValType::F32, Value::F32(bits)) => u64::from(*bits),
            (ValType::F64, Value::F64(bits)) => *bits,
            _ => return Err(ExecFail::Unsupported("compiled function argument type")),
        };
        input.push(bits);
    }
    let mut output = vec![0u64; ty.results.len()];
    let code = func.call(&input, mems, globals, &mut output, runtime);
    if code != TRAP_NONE {
        return Err(ExecFail::Trap(trap_of(code)));
    }
    let mut results = Vec::with_capacity(ty.results.len());
    for (bits, result) in output.into_iter().zip(&ty.results) {
        let value = match result {
            ValType::I32 => Value::I32(bits as u32 as i32),
            ValType::I64 => Value::I64(bits as i64),
            ValType::F32 => Value::F32(bits as u32),
            ValType::F64 => Value::F64(bits),
            _ => return Err(ExecFail::Unsupported("compiled function result type")),
        };
        results.push(value);
    }
    Ok(results)
}

/// Compile every module-defined function body. `None` at index `i` means
/// body `i` is not (yet) lowerable and keeps the interpreter path.
pub fn compile_module(module: &Module) -> Vec<Option<CompiledFunc>> {
    let Ok(engine) = Engine::new() else {
        return (0..module.bodies.len()).map(|_| None).collect();
    };
    (0..module.bodies.len())
        .map(|defined| engine.compile(module, defined))
        .collect()
}

/// The process-wide native `TargetIsa`, built once (an ISA construction runs
/// a host-CPU feature scan, far too expensive to repeat per module).
fn native_isa() -> Result<Arc<dyn TargetIsa>, String> {
    static ISA: OnceLock<Result<Arc<dyn TargetIsa>, String>> = OnceLock::new();
    ISA.get_or_init(|| {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|e| e.to_string())?;
        let flags = settings::Flags::new(flag_builder);
        let isa = cranelift_native::builder()?
            .finish(flags)
            .map_err(|e| e.to_string())?;
        Ok(isa)
    })
    .clone()
}

/// The native-code generation engine over a process-wide cached `TargetIsa`
/// (building one is expensive; corpus runs compile per module instantiation).
struct Engine {
    isa: Arc<dyn TargetIsa>,
}

impl Engine {
    fn new() -> Result<Self, String> {
        Ok(Self { isa: native_isa()? })
    }

    /// Compile `defined` of `module`, or `None` when the function is outside
    /// the supported subset.
    fn compile(&self, module: &Module, defined: usize) -> Option<CompiledFunc> {
        let body = module.bodies.get(defined)?;
        let type_index = *module.functions.get(defined)?;
        let func_type = module.func_at(type_index)?;
        if !lowerable(module, func_type, body) {
            return None;
        }
        let conv = platform_call_conv(&*self.isa);
        let mut func = Function::with_name_signature(
            UserFuncName::testcase("wasm_body"),
            entry_signature(conv),
        );
        let mut fctx = FunctionBuilderContext::new();
        let globals = body_globals(body);
        lower(
            module, body, func_type, &globals, &mut func, &mut fctx, &*self.isa,
        )
        .ok()?;
        let mut ctx = Context::for_function(func);
        let compiled = ctx.compile(&*self.isa, &mut ControlPlane::default()).ok()?;
        let code = ExecutableCode::new(compiled.code_buffer()).ok()?;
        // SAFETY: `code.as_ptr()` is an executable allocation that outlives
        // the cast; a data pointer to a function pointer is a plain integer
        // cast on every supported (64-bit) target.
        let entry: CompiledEntry = unsafe { std::mem::transmute(code.as_ptr()) };
        Some(CompiledFunc {
            _code: code,
            entry,
            globals,
        })
    }
}

fn platform_call_conv(isa: &dyn TargetIsa) -> CallConv {
    if isa.triple().operating_system == target_lexicon::OperatingSystem::Windows {
        CallConv::WindowsFastcall
    } else {
        CallConv::SystemV
    }
}

/// `fn(args, nargs, mems, ncount, gvals, out, nout, store, instance,
/// grow) -> i32` (trap code).
fn entry_signature(conv: CallConv) -> Signature {
    let mut sig = Signature::new(conv);
    sig.params.push(AbiParam::new(types::I64)); // args pointer
    sig.params.push(AbiParam::new(types::I64)); // nargs
    sig.params.push(AbiParam::new(types::I64)); // memory descriptor array
    sig.params.push(AbiParam::new(types::I64)); // descriptor count
    sig.params.push(AbiParam::new(types::I64)); // global-values buffer
    sig.params.push(AbiParam::new(types::I64)); // out pointer
    sig.params.push(AbiParam::new(types::I64)); // nout
    sig.params.push(AbiParam::new(types::I64)); // store pointer
    sig.params.push(AbiParam::new(types::I64)); // instance index
    sig.params.push(AbiParam::new(types::I64)); // memory.grow helper address
    sig.returns.push(AbiParam::new(types::I32)); // trap code
    sig
}

fn clif_type(ty: ValType) -> Option<Type> {
    match ty {
        ValType::I32 => Some(types::I32),
        ValType::I64 => Some(types::I64),
        ValType::F32 => Some(types::F32),
        ValType::F64 => Some(types::F64),
        _ => None,
    }
}

/// Resolve a block type to its parameter and result valtypes. `Type(i)`
/// indexes the module's type section; only numeric (float/int) parameters
/// and results lower today, so any other shape keeps the function on the
/// interpreter.
fn block_sig(module: &Module, bt: &BlockType) -> Option<(Vec<ValType>, Vec<ValType>)> {
    let (params, results) = match bt {
        BlockType::Empty => (Vec::new(), Vec::new()),
        BlockType::Val(ty) => (Vec::new(), vec![*ty]),
        BlockType::Type(index) => {
            let ty = module.func_at(*index)?;
            (ty.params.clone(), ty.results.clone())
        }
    };
    if params
        .iter()
        .chain(results.iter())
        .all(|t| clif_type(*t).is_some())
    {
        Some((params, results))
    } else {
        None
    }
}

/// A memory index's declared `memory64` flag, resolving index-space indices
/// (imported memories first, then the module's own) so the lowering subset
/// can accept any 32-bit memory.
fn memory_is64(module: &Module, index: u32) -> Option<bool> {
    let imported = module
        .imports
        .iter()
        .filter(|import| matches!(import.desc, ImportDesc::Memory(_)))
        .count() as u32;
    if index < imported {
        let mut seen = 0u32;
        for import in &module.imports {
            if let ImportDesc::Memory(memory) = &import.desc {
                if seen == index {
                    return Some(memory.memory64);
                }
                seen += 1;
            }
        }
        None
    } else {
        module
            .memories
            .get((index - imported) as usize)
            .map(|memory| memory.memory64)
    }
}

/// A module global's declared `(ValType, mutable)`, resolving index-space
/// indices past the imported globals (imported globals may alias a shared
/// cell, so compiled bodies only touch module-defined ones).
fn defined_global(module: &Module, index: u32) -> Option<(ValType, bool)> {
    let imported = module
        .imports
        .iter()
        .filter(|import| matches!(import.desc, ImportDesc::Global(_)))
        .count() as u32;
    let slot = index.checked_sub(imported)?;
    let global = module.globals.get(slot as usize)?;
    Some((global.ty.value, global.ty.mutable))
}

/// The module global indices (index space) a body reads or writes, sorted
/// and deduplicated (the order the `gvals` buffer slots use).
fn body_globals(body: &FuncBody) -> Vec<u32> {
    let mut indices: Vec<u32> = body
        .body
        .iter()
        .filter_map(|instr| match instr {
            Instr::GlobalGet(index) | Instr::GlobalSet(index) => Some(*index),
            _ => None,
        })
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// The byte width a load reads (little-endian, from the effective address).
fn load_width(op: LoadOp) -> u64 {
    use LoadOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Load8S | I32Load8U | I64Load8S | I64Load8U => 1,
        I32Load16S | I32Load16U | I64Load16S | I64Load16U => 2,
        I64Load32S | I64Load32U => 4,
    }
}

/// The byte width a store writes (little-endian, from the effective address).
fn store_width(op: StoreOp) -> u64 {
    use StoreOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Store8 | I64Store8 => 1,
        I32Store16 | I64Store16 => 2,
        I64Store32 => 4,
    }
}

/// Whether `module`'s `func_type` + `body` are inside the current lowering
/// subset.
fn lowerable(module: &Module, func_type: &FuncType, body: &FuncBody) -> bool {
    if func_type.params.iter().any(|t| clif_type(*t).is_none())
        || func_type.results.iter().any(|t| clif_type(*t).is_none())
        || body.locals.iter().any(|t| clif_type(*t).is_none())
    {
        return false;
    }
    body.body.iter().all(|instr| match instr {
        Instr::Nop
        | Instr::Unreachable
        | Instr::I32Const(_)
        | Instr::I64Const(_)
        | Instr::F32Const(_)
        | Instr::F64Const(_)
        | Instr::LocalGet(_)
        | Instr::LocalSet(_)
        | Instr::LocalTee(_)
        | Instr::Drop
        | Instr::Select
        | Instr::SelectTyped(_)
        | Instr::Return => true,
        Instr::Num(op) => supported_num(*op),
        Instr::Block(bt) | Instr::Loop(bt) | Instr::If(bt) => block_sig(module, bt).is_some(),
        Instr::Else | Instr::End | Instr::Br(_) | Instr::BrIf(_) | Instr::BrTable { .. } => true,
        Instr::Load { memory, .. } | Instr::Store { memory, .. } => {
            memory_is64(module, *memory) == Some(false)
        }
        Instr::MemorySize(memory) => memory_is64(module, *memory) == Some(false),
        Instr::MemoryGrow(memory) => memory_is64(module, *memory) == Some(false),
        Instr::GlobalGet(index) => {
            matches!(defined_global(module, *index), Some((ty, _)) if clif_type(ty).is_some())
        }
        Instr::GlobalSet(index) => matches!(
            defined_global(module, *index),
            Some((ty, true)) if clif_type(ty).is_some()
        ),
        _ => false,
    })
}

fn supported_num(op: NumOp) -> bool {
    use NumOp::*;
    matches!(
        op,
        // i32 arithmetic.
        I32Add
            | I32Sub
            | I32Mul
            | I32DivS
            | I32DivU
            | I32RemS
            | I32RemU
            | I32And
            | I32Or
            | I32Xor
            | I32Shl
            | I32ShrS
            | I32ShrU
            | I32Rotl
            | I32Rotr
            | I32Clz
            | I32Ctz
            | I32Popcnt
            // i64 arithmetic.
            | I64Add
            | I64Sub
            | I64Mul
            | I64DivS
            | I64DivU
            | I64RemS
            | I64RemU
            | I64And
            | I64Or
            | I64Xor
            | I64Shl
            | I64ShrS
            | I64ShrU
            | I64Rotl
            | I64Rotr
            | I64Clz
            | I64Ctz
            | I64Popcnt
            // Tests and comparisons.
            | I32Eqz
            | I32Eq
            | I32Ne
            | I32LtS
            | I32LtU
            | I32GtS
            | I32GtU
            | I32LeS
            | I32LeU
            | I32GeS
            | I32GeU
            | I64Eqz
            | I64Eq
            | I64Ne
            | I64LtS
            | I64LtU
            | I64GtS
            | I64GtU
            | I64LeS
            | I64LeU
            | I64GeS
            | I64GeU
            // Conversions.
            | I32WrapI64
            // f32 arithmetic (the lowering canonicalizes NaN results;
            // min/max and demote are deferred).
            | F32Abs
            | F32Neg
            | F32Ceil
            | F32Floor
            | F32Trunc
            | F32Nearest
            | F32Sqrt
            | F32Add
            | F32Sub
            | F32Mul
            | F32Div
            | F32Copysign
            | F32Eq
            | F32Ne
            | F32Lt
            | F32Gt
            | F32Le
            | F32Ge
            | F32ConvertI32S
            | F32ConvertI32U
            | F32ConvertI64S
            | F32ConvertI64U
            // f64 arithmetic.
            | F64Abs
            | F64Neg
            | F64Ceil
            | F64Floor
            | F64Trunc
            | F64Nearest
            | F64Sqrt
            | F64Add
            | F64Sub
            | F64Mul
            | F64Div
            | F64Copysign
            | F64Eq
            | F64Ne
            | F64Lt
            | F64Gt
            | F64Le
            | F64Ge
            | F64ConvertI32S
            | F64ConvertI32U
            | F64ConvertI64S
            | F64ConvertI64U
    )
}

/// Whether `op` is one of the lowered f32 ops (see `lower_float`).
fn is_f32_op(op: NumOp) -> bool {
    use NumOp::*;
    matches!(
        op,
        F32Abs
            | F32Neg
            | F32Ceil
            | F32Floor
            | F32Trunc
            | F32Nearest
            | F32Sqrt
            | F32Add
            | F32Sub
            | F32Mul
            | F32Div
            | F32Copysign
            | F32Eq
            | F32Ne
            | F32Lt
            | F32Gt
            | F32Le
            | F32Ge
            | F32ConvertI32S
            | F32ConvertI32U
            | F32ConvertI64S
            | F32ConvertI64U
    )
}

/// Whether `op` is one of the lowered f64 ops (see `lower_float`).
fn is_f64_op(op: NumOp) -> bool {
    use NumOp::*;
    matches!(
        op,
        F64Abs
            | F64Neg
            | F64Ceil
            | F64Floor
            | F64Trunc
            | F64Nearest
            | F64Sqrt
            | F64Add
            | F64Sub
            | F64Mul
            | F64Div
            | F64Copysign
            | F64Eq
            | F64Ne
            | F64Lt
            | F64Gt
            | F64Le
            | F64Ge
            | F64ConvertI32S
            | F64ConvertI32U
            | F64ConvertI64S
            | F64ConvertI64U
    )
}

/// Which shift instruction to lower (all mask their count to the width).
#[derive(Clone, Copy)]
enum ShiftKind {
    Left,
    RightSigned,
    RightUnsigned,
    RotateLeft,
    RotateRight,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CtlKind {
    Block,
    Loop,
    If,
}

/// One open structured construct (`block`/`loop`/`if`) during lowering.
///
/// A block type `[t1*] -> [t2*]` enters with its `t1*` parameters on the
/// operand stack: `height` is the depth of the stack below those parameters
/// (the label base). A `br` to a block/if label jumps to `after` carrying
/// the `t2*` results; a `br` to a loop label jumps back to its `header`
/// carrying `t1*` (the next iteration's parameters).
struct CtlFrame {
    kind: CtlKind,
    /// Loop header (the `br` target for a loop label); unsealed until the
    /// loop's `end`, so body back-edges stay legal.
    header: Option<Block>,
    /// The continuation after the construct; receives the results.
    after: Block,
    /// The false-condition branch of an `if`.
    else_block: Option<Block>,
    /// Operand-stack depth of the label base (below the construct's
    /// parameters, if any).
    height: usize,
    /// The construct's parameter count (`br` to a loop label carries this
    /// many values to the header).
    nparams: usize,
    /// The construct's parameter values, saved so an `if`'s else branch can
    /// restart from them (empty otherwise).
    params: Vec<ClifValue>,
    /// The construct's result count (`br` to a block/if label carries this
    /// many values to `after`).
    results: usize,
    /// Lowering is currently inside the `else` branch of an `if`.
    in_else: bool,
    /// Whether an `else` marker was seen.
    has_else: bool,
    /// Whether any branch already targets `after` (so closing a dead body
    /// must still fill the continuation).
    after_used: bool,
}

/// A lowering context: locals, the operand stack, and the entry pointers.
struct Lowerer<'a> {
    builder: FunctionBuilder<'a>,
    /// The module (block types resolve through its type section).
    module: &'a Module,
    /// The ABI call convention (the internal helper `call_indirect` sig must
    /// match the platform C ABI the helper was compiled with).
    conv: CallConv,
    /// `locals` and `stack` entry metadata mirror the interpreter's model.
    variables: Vec<Variable>,
    stack: Vec<ClifValue>,
    /// The per-call memory descriptor array (entry param 2): per module
    /// memory index, its data pointer then byte length (each a `u64`).
    /// `memory.grow` refreshes the entry in place, so `mem_ea`/`memory.size`
    /// must reload it on every use (they do).
    mems: ClifValue,
    /// The per-call global-values buffer (entry param 4): one u64 slot per
    /// used module global. `globals` maps a module global index to its slot
    /// position and clif type.
    globals_ptr: ClifValue,
    globals: Vec<(u32, Type)>,
    out_ptr: ClifValue,
    /// Entry params 7-9: the owning store pointer, the invoked instance
    /// index, and the `memory.grow` helper address (entry param 9; bodies
    /// that grow call it through a helper-signature `call_indirect`).
    store: ClifValue,
    instance: ClifValue,
    grow: ClifValue,
    results: Vec<ValType>,
    controls: Vec<CtlFrame>,
    /// The current path ended in an unconditional jump/trap/return.
    dead: bool,
    /// Structured markers opened since the path went dead (their `end`s must
    /// be skipped without touching the real frame stack).
    dead_depth: usize,
}

impl<'a> Lowerer<'a> {
    fn pop(&mut self) -> Option<ClifValue> {
        self.stack.pop()
    }

    fn iconst(&mut self, ty: Type, value: i64) -> ClifValue {
        self.builder.ins().iconst(ty, value)
    }

    /// Branch to a trap return when `cond` is true; the success path resumes
    /// in a fresh block.
    fn trap_if(&mut self, cond: ClifValue, code: i32) {
        let trap_block = self.builder.create_block();
        let cont = self.builder.create_block();
        self.builder.ins().brif(cond, trap_block, &[], cont, &[]);
        self.builder.switch_to_block(trap_block);
        let code_value = self.iconst(types::I32, i64::from(code));
        self.builder.ins().return_(&[code_value]);
        self.builder.switch_to_block(cont);
    }

    /// Store the results and return trap code 0. `stack` must hold exactly
    /// the results, in order (result 0 deepest).
    fn emit_return(&mut self) -> Result<(), String> {
        if self.stack.len() != self.results.len() {
            return Err("compiled return arity mismatch".to_string());
        }
        for (index, (value, ty)) in self.stack.iter().zip(&self.results).enumerate() {
            let address = self
                .builder
                .ins()
                .iadd_imm_s(self.out_ptr, 8 * index as i64);
            let wide = match ty {
                ValType::I32 => self.builder.ins().uextend(types::I64, *value),
                ValType::F32 => {
                    let bits = self
                        .builder
                        .ins()
                        .bitcast(types::I32, MemFlagsData::new(), *value);
                    self.builder.ins().uextend(types::I64, bits)
                }
                ValType::F64 => self
                    .builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), *value),
                _ => *value,
            };
            self.builder
                .ins()
                .store(MemFlagsData::new(), wide, address, Offset32::new(0));
        }
        let ok = self.iconst(types::I32, 0);
        self.builder.ins().return_(&[ok]);
        Ok(())
    }

    /// Emit a trap return unconditionally (used by `unreachable`).
    fn emit_trap(&mut self, code: i32) {
        let code_value = self.iconst(types::I32, i64::from(code));
        self.builder.ins().return_(&[code_value]);
    }

    fn lower_num(&mut self, op: NumOp) -> Result<(), String> {
        use NumOp::*;
        // Float ops take their own path (they produce typed float values and
        // canonicalize NaN results per the interpreter's policy).
        if is_f32_op(op) {
            return self.lower_float(op, types::F32);
        }
        if is_f64_op(op) {
            return self.lower_float(op, types::F64);
        }
        // Fetch operands: binary ops and comparisons pop two, unary pop one.
        let binary = matches!(
            op,
            I32Add
                | I32Sub
                | I32Mul
                | I32DivS
                | I32DivU
                | I32RemS
                | I32RemU
                | I32And
                | I32Or
                | I32Xor
                | I32Shl
                | I32ShrS
                | I32ShrU
                | I32Rotl
                | I32Rotr
                | I64Add
                | I64Sub
                | I64Mul
                | I64DivS
                | I64DivU
                | I64RemS
                | I64RemU
                | I64And
                | I64Or
                | I64Xor
                | I64Shl
                | I64ShrS
                | I64ShrU
                | I64Rotl
                | I64Rotr
                | I32Eq
                | I32Ne
                | I32LtS
                | I32LtU
                | I32GtS
                | I32GtU
                | I32LeS
                | I32LeU
                | I32GeS
                | I32GeU
                | I64Eq
                | I64Ne
                | I64LtS
                | I64LtU
                | I64GtS
                | I64GtU
                | I64LeS
                | I64LeU
                | I64GeS
                | I64GeU
        );
        let (a, b) = if binary {
            let b = self.pop().ok_or("operand stack underflow")?;
            let a = self.pop().ok_or("operand stack underflow")?;
            (a, b)
        } else {
            let a = self.pop().ok_or("operand stack underflow")?;
            (a, a)
        };
        let ty = match op {
            I32Add | I32Sub | I32Mul | I32DivS | I32DivU | I32RemS | I32RemU | I32And | I32Or
            | I32Xor | I32Shl | I32ShrS | I32ShrU | I32Rotl | I32Rotr | I32Clz | I32Ctz
            | I32Popcnt | I32Eqz | I32WrapI64 => types::I32,
            _ => types::I64,
        };
        let wide = ty == types::I64;
        let result = match op {
            I32Add | I64Add => self.builder.ins().iadd(a, b),
            I32Sub | I64Sub => self.builder.ins().isub(a, b),
            I32Mul | I64Mul => self.builder.ins().imul(a, b),
            I32And | I64And => self.builder.ins().band(a, b),
            I32Or | I64Or => self.builder.ins().bor(a, b),
            I32Xor | I64Xor => self.builder.ins().bxor(a, b),
            I32Shl | I64Shl => self.shift(a, b, ty, ShiftKind::Left)?,
            I32ShrS | I64ShrS => self.shift(a, b, ty, ShiftKind::RightSigned)?,
            I32ShrU | I64ShrU => self.shift(a, b, ty, ShiftKind::RightUnsigned)?,
            I32Rotl | I64Rotl => self.shift(a, b, ty, ShiftKind::RotateLeft)?,
            I32Rotr | I64Rotr => self.shift(a, b, ty, ShiftKind::RotateRight)?,
            I32DivS | I64DivS => self.lower_div_s(a, b, ty, wide)?,
            I32DivU | I64DivU => self.lower_div_u(a, b, ty)?,
            I32RemS | I64RemS => self.lower_rem_s(a, b, ty)?,
            I32RemU | I64RemU => self.lower_rem_u(a, b, ty)?,
            I32Eq | I64Eq => self.bin_bool(IntCC::Equal, a, b),
            I32Ne | I64Ne => self.bin_bool(IntCC::NotEqual, a, b),
            I32LtS | I64LtS => self.bin_bool(IntCC::SignedLessThan, a, b),
            I32LtU | I64LtU => self.bin_bool(IntCC::UnsignedLessThan, a, b),
            I32GtS | I64GtS => self.bin_bool(IntCC::SignedGreaterThan, a, b),
            I32GtU | I64GtU => self.bin_bool(IntCC::UnsignedGreaterThan, a, b),
            I32LeS | I64LeS => self.bin_bool(IntCC::SignedLessThanOrEqual, a, b),
            I32LeU | I64LeU => self.bin_bool(IntCC::UnsignedLessThanOrEqual, a, b),
            I32GeS | I64GeS => self.bin_bool(IntCC::SignedGreaterThanOrEqual, a, b),
            I32GeU | I64GeU => self.bin_bool(IntCC::UnsignedGreaterThanOrEqual, a, b),
            I32Clz | I64Clz => self.builder.ins().clz(a),
            I32Ctz | I64Ctz => self.builder.ins().ctz(a),
            I32Popcnt => self.builder.ins().popcnt(a),
            I64Popcnt => self.builder.ins().popcnt(a),
            I32Eqz | I64Eqz => self.un_bool(a, ty),
            I32WrapI64 => self.builder.ins().ireduce(types::I32, a),
            _ => return Err("unsupported numeric opcode reached the lowerer".to_string()),
        };
        self.stack.push(result);
        Ok(())
    }

    /// Shift with the count masked to the type width (wasm semantics; never
    /// relies on the backend's shift-amount behavior).
    fn shift(
        &mut self,
        a: ClifValue,
        b: ClifValue,
        ty: Type,
        kind: ShiftKind,
    ) -> Result<ClifValue, String> {
        let mask = if ty == types::I32 { 31 } else { 63 };
        let limit = self.iconst(ty, mask);
        let count = self.builder.ins().band(b, limit);
        Ok(match kind {
            ShiftKind::Left => self.builder.ins().ishl(a, count),
            ShiftKind::RightSigned => self.builder.ins().sshr(a, count),
            ShiftKind::RightUnsigned => self.builder.ins().ushr(a, count),
            ShiftKind::RotateLeft => self.builder.ins().rotl(a, count),
            ShiftKind::RotateRight => self.builder.ins().rotr(a, count),
        })
    }

    /// The byte address of a memory access: reload the memory's descriptor
    /// (data pointer and length), unsigned-widen the i32 address, add the
    /// static offset, then trap when `addr + offset + width` exceeds the
    /// memory's length. Returns the base-plus-effective-address pointer.
    fn mem_ea(
        &mut self,
        memory: u32,
        addr: ClifValue,
        offset: u64,
        width: u64,
    ) -> Result<ClifValue, String> {
        let ptr_slot = self.builder.ins().iadd_imm_s(self.mems, 16 * memory as i64);
        let len_slot = self
            .builder
            .ins()
            .iadd_imm_s(self.mems, 16 * memory as i64 + 8);
        let mem_ptr =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), ptr_slot, Offset32::new(0));
        let mem_len =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), len_slot, Offset32::new(0));
        let addr64 = self.builder.ins().uextend(types::I64, addr);
        let ea = self.builder.ins().iadd_imm_s(addr64, offset as i64);
        let end = self.builder.ins().iadd_imm_s(ea, width as i64);
        let oob = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, mem_len);
        self.trap_if(oob, TRAP_MEMORY_OOB);
        Ok(self.builder.ins().iadd(mem_ptr, ea))
    }

    /// Lower a numeric load (little-endian) with its sign/zero extension.
    fn do_load(&mut self, memory: u32, op: LoadOp, offset: u64) -> Result<(), String> {
        use LoadOp::*;
        let addr = self.pop().ok_or("operand stack underflow")?;
        let address = self.mem_ea(memory, addr, offset, load_width(op))?;
        let flags = MemFlagsData::new();
        let value = match op {
            I32 => self
                .builder
                .ins()
                .load(types::I32, flags, address, Offset32::new(0)),
            I64 => self
                .builder
                .ins()
                .load(types::I64, flags, address, Offset32::new(0)),
            F32 => {
                let bits = self
                    .builder
                    .ins()
                    .load(types::I32, flags, address, Offset32::new(0));
                self.builder.ins().bitcast(types::F32, flags, bits)
            }
            F64 => {
                let bits = self
                    .builder
                    .ins()
                    .load(types::I64, flags, address, Offset32::new(0));
                self.builder.ins().bitcast(types::F64, flags, bits)
            }
            I32Load8S => {
                let byte = self
                    .builder
                    .ins()
                    .load(types::I8, flags, address, Offset32::new(0));
                self.builder.ins().sextend(types::I32, byte)
            }
            I32Load8U => {
                let byte = self
                    .builder
                    .ins()
                    .load(types::I8, flags, address, Offset32::new(0));
                self.builder.ins().uextend(types::I32, byte)
            }
            I64Load8S => {
                let byte = self
                    .builder
                    .ins()
                    .load(types::I8, flags, address, Offset32::new(0));
                self.builder.ins().sextend(types::I64, byte)
            }
            I64Load8U => {
                let byte = self
                    .builder
                    .ins()
                    .load(types::I8, flags, address, Offset32::new(0));
                self.builder.ins().uextend(types::I64, byte)
            }
            I32Load16S => {
                let half = self
                    .builder
                    .ins()
                    .load(types::I16, flags, address, Offset32::new(0));
                self.builder.ins().sextend(types::I32, half)
            }
            I32Load16U => {
                let half = self
                    .builder
                    .ins()
                    .load(types::I16, flags, address, Offset32::new(0));
                self.builder.ins().uextend(types::I32, half)
            }
            I64Load16S => {
                let half = self
                    .builder
                    .ins()
                    .load(types::I16, flags, address, Offset32::new(0));
                self.builder.ins().sextend(types::I64, half)
            }
            I64Load16U => {
                let half = self
                    .builder
                    .ins()
                    .load(types::I16, flags, address, Offset32::new(0));
                self.builder.ins().uextend(types::I64, half)
            }
            I64Load32S => {
                let word = self
                    .builder
                    .ins()
                    .load(types::I32, flags, address, Offset32::new(0));
                self.builder.ins().sextend(types::I64, word)
            }
            I64Load32U => {
                let word = self
                    .builder
                    .ins()
                    .load(types::I32, flags, address, Offset32::new(0));
                self.builder.ins().uextend(types::I64, word)
            }
        };
        self.stack.push(value);
        Ok(())
    }

    /// Lower a numeric store (little-endian), truncating to the store width.
    fn do_store(&mut self, memory: u32, op: StoreOp, offset: u64) -> Result<(), String> {
        use StoreOp::*;
        let value = self.pop().ok_or("operand stack underflow")?;
        let addr = self.pop().ok_or("operand stack underflow")?;
        let address = self.mem_ea(memory, addr, offset, store_width(op))?;
        let flags = MemFlagsData::new();
        let stored = match op {
            I32 => value,
            I64 => value,
            F32 => self.builder.ins().bitcast(types::I32, flags, value),
            F64 => self.builder.ins().bitcast(types::I64, flags, value),
            I32Store8 => self.builder.ins().ireduce(types::I8, value),
            I32Store16 => self.builder.ins().ireduce(types::I16, value),
            I64Store8 => self.builder.ins().ireduce(types::I8, value),
            I64Store16 => self.builder.ins().ireduce(types::I16, value),
            I64Store32 => self.builder.ins().ireduce(types::I32, value),
        };
        self.builder
            .ins()
            .store(MemFlagsData::new(), stored, address, Offset32::new(0));
        Ok(())
    }

    /// `memory.size`: current size in pages as an i32 (32-bit memory).
    fn do_memory_size(&mut self, memory: u32) -> Result<(), String> {
        let len_slot = self
            .builder
            .ins()
            .iadd_imm_s(self.mems, 16 * memory as i64 + 8);
        let mem_len =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), len_slot, Offset32::new(0));
        let sixteen = self.iconst(types::I64, 16);
        let pages = self.builder.ins().ushr(mem_len, sixteen);
        let pages32 = self.builder.ins().ireduce(types::I32, pages);
        self.stack.push(pages32);
        Ok(())
    }

    /// `memory.grow`: pop the i32 delta and call the runtime grow helper
    /// (entry params 7-9) with the store/instance pointers, this memory's
    /// module index, the widened delta, and this memory's descriptor-slot
    /// address in the caller-owned array. The helper grows the store cell's
    /// backing `Vec` (enforcing the declared maximum and the memory32 2^16
    /// cap) and, on success, rewrites the descriptor entry in place — its
    /// data pointer is unstable across a resize, and every later access
    /// reloads the descriptor. The helper returns the old page count or
    /// `u64::MAX` (-1) on failure; narrowing to i32 reproduces wasm's
    /// "old pages or -1" result exactly (growth caps far below 2^32).
    fn do_memory_grow(&mut self, memory: u32) -> Result<(), String> {
        let delta = self.pop().ok_or("operand stack underflow")?;
        let delta64 = self.builder.ins().uextend(types::I64, delta);
        let desc = self.builder.ins().iadd_imm_s(self.mems, 16 * memory as i64);
        let memory_index = self.iconst(types::I64, i64::from(memory));
        // The helper's ABI: (store, instance, memory index, delta, desc
        // slot) -> old pages / -1, all u64 slots so no float classification
        // can disagree with the Rust trampoline.
        let mut sig = Signature::new(self.conv);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let sig = self.builder.import_signature(sig);
        let call = self.builder.ins().call_indirect(
            sig,
            self.grow,
            &[self.store, self.instance, memory_index, delta64, desc],
        );
        let old = self.builder.inst_results(call)[0];
        let old32 = self.builder.ins().ireduce(types::I32, old);
        self.stack.push(old32);
        Ok(())
    }

    /// `global.get`: read the module global's `gvals` buffer slot, narrowed
    /// from the u64 slot to the global's value type.
    fn do_global_get(&mut self, index: u32) -> Result<(), String> {
        let (slot, ty) = self
            .globals
            .iter()
            .position(|(i, _)| *i == index)
            .map(|position| (position, self.globals[position].1))
            .ok_or("unmapped global in the lowering subset")?;
        let address = self
            .builder
            .ins()
            .iadd_imm_s(self.globals_ptr, 8 * slot as i64);
        let wide =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), address, Offset32::new(0));
        let value = match ty {
            types::I32 => self.builder.ins().ireduce(types::I32, wide),
            types::F32 => {
                let bits = self.builder.ins().ireduce(types::I32, wide);
                self.builder
                    .ins()
                    .bitcast(types::F32, MemFlagsData::new(), bits)
            }
            types::I64 => wide,
            types::F64 => self
                .builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), wide),
            _ => return Err("non-numeric global reached the lowerer".to_string()),
        };
        self.stack.push(value);
        Ok(())
    }

    /// `global.set`: widen the value to the u64 `gvals` buffer slot. The
    /// write lands in caller-owned memory immediately, so it is visible to
    /// the store even when a later instruction traps.
    fn do_global_set(&mut self, index: u32) -> Result<(), String> {
        let value = self.pop().ok_or("operand stack underflow")?;
        let (slot, ty) = self
            .globals
            .iter()
            .position(|(i, _)| *i == index)
            .map(|position| (position, self.globals[position].1))
            .ok_or("unmapped global in the lowering subset")?;
        let wide = match ty {
            types::I32 => self.builder.ins().uextend(types::I64, value),
            types::F32 => {
                let bits = self
                    .builder
                    .ins()
                    .bitcast(types::I32, MemFlagsData::new(), value);
                self.builder.ins().uextend(types::I64, bits)
            }
            types::I64 => value,
            types::F64 => self
                .builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), value),
            _ => return Err("non-numeric global reached the lowerer".to_string()),
        };
        let address = self
            .builder
            .ins()
            .iadd_imm_s(self.globals_ptr, 8 * slot as i64);
        self.builder
            .ins()
            .store(MemFlagsData::new(), wide, address, Offset32::new(0));
        Ok(())
    }

    /// A float constant from its IEEE bits.
    fn fconst(&mut self, ty: Type, bits: u64) -> ClifValue {
        match ty {
            types::F32 => self.builder.ins().f32const(Ieee32::with_bits(bits as u32)),
            types::F64 => self.builder.ins().f64const(Ieee64::with_bits(bits)),
            _ => unreachable!("float constant of a non-float type"),
        }
    }

    /// Canonicalize any NaN to the engine's quiet NaN (the interpreter's
    /// `values.rs` policy: every arithmetic NaN result becomes QNAN32/64).
    fn canon_float(&mut self, value: ClifValue, ty: Type) -> ClifValue {
        let is_nan = self.builder.ins().fcmp(FloatCC::NotEqual, value, value);
        let q = match ty {
            types::F32 => self.fconst(types::F32, u64::from(QNAN32)),
            _ => self.fconst(types::F64, QNAN64),
        };
        self.builder.ins().select(is_nan, q, value)
    }

    fn fcmp_result(&mut self, cc: FloatCC, a: ClifValue, b: ClifValue) -> ClifValue {
        let flag = self.builder.ins().fcmp(cc, a, b);
        let one = self.iconst(types::I32, 1);
        let zero = self.iconst(types::I32, 0);
        self.builder.ins().select(flag, one, zero)
    }

    /// Lower one of the f32/f64 ops (see `is_f32_op`/`is_f64_op`). NaN
    /// arithmetic is canonicalized, abs/neg/copysign are raw bit ops, and
    /// comparisons yield i32 — mirroring `values.rs` exactly.
    fn lower_float(&mut self, op: NumOp, ty: Type) -> Result<(), String> {
        use NumOp::*;
        let binary = matches!(
            op,
            F32Add
                | F64Add
                | F32Sub
                | F64Sub
                | F32Mul
                | F64Mul
                | F32Div
                | F64Div
                | F32Copysign
                | F64Copysign
                | F32Eq
                | F64Eq
                | F32Ne
                | F64Ne
                | F32Lt
                | F64Lt
                | F32Gt
                | F64Gt
                | F32Le
                | F64Le
                | F32Ge
                | F64Ge
        );
        let (a, b) = if binary {
            let b = self.pop().ok_or("operand stack underflow")?;
            let a = self.pop().ok_or("operand stack underflow")?;
            (a, b)
        } else {
            let a = self.pop().ok_or("operand stack underflow")?;
            (a, a)
        };
        let canon_raw = match op {
            F32Add | F64Add => Some(self.builder.ins().fadd(a, b)),
            F32Sub | F64Sub => Some(self.builder.ins().fsub(a, b)),
            F32Mul | F64Mul => Some(self.builder.ins().fmul(a, b)),
            F32Div | F64Div => Some(self.builder.ins().fdiv(a, b)),
            F32Ceil | F64Ceil => Some(self.builder.ins().ceil(a)),
            F32Floor | F64Floor => Some(self.builder.ins().floor(a)),
            F32Trunc | F64Trunc => Some(self.builder.ins().trunc(a)),
            F32Nearest | F64Nearest => Some(self.builder.ins().nearest(a)),
            F32Sqrt | F64Sqrt => Some(self.builder.ins().sqrt(a)),
            _ => None,
        };
        let result = if let Some(raw) = canon_raw {
            self.canon_float(raw, ty)
        } else {
            match op {
                F32Abs | F64Abs => self.builder.ins().fabs(a),
                F32Neg | F64Neg => self.builder.ins().fneg(a),
                F32Copysign | F64Copysign => self.builder.ins().fcopysign(a, b),
                F32Eq | F64Eq => self.fcmp_result(FloatCC::Equal, a, b),
                F32Ne | F64Ne => self.fcmp_result(FloatCC::NotEqual, a, b),
                F32Lt | F64Lt => self.fcmp_result(FloatCC::LessThan, a, b),
                F32Gt | F64Gt => self.fcmp_result(FloatCC::GreaterThan, a, b),
                F32Le | F64Le => self.fcmp_result(FloatCC::LessThanOrEqual, a, b),
                F32Ge | F64Ge => self.fcmp_result(FloatCC::GreaterThanOrEqual, a, b),
                F32ConvertI32S | F64ConvertI32S => self.builder.ins().fcvt_from_sint(ty, a),
                F32ConvertI32U | F64ConvertI32U => self.builder.ins().fcvt_from_uint(ty, a),
                F32ConvertI64S | F64ConvertI64S => self.builder.ins().fcvt_from_sint(ty, a),
                F32ConvertI64U | F64ConvertI64U => self.builder.ins().fcvt_from_uint(ty, a),
                _ => return Err("unsupported float opcode".to_string()),
            }
        };
        self.stack.push(result);
        Ok(())
    }

    fn bin_bool(&mut self, cc: IntCC, a: ClifValue, b: ClifValue) -> ClifValue {
        let flag = self.builder.ins().icmp(cc, a, b);
        let one = self.iconst(types::I32, 1);
        let zero = self.iconst(types::I32, 0);
        self.builder.ins().select(flag, one, zero)
    }

    fn un_bool(&mut self, a: ClifValue, ty: Type) -> ClifValue {
        let zero = self.iconst(ty, 0);
        self.bin_bool(IntCC::Equal, a, zero)
    }

    /// Signed division: trap on divide-by-zero and on `MIN / -1`.
    fn lower_div_s(
        &mut self,
        a: ClifValue,
        b: ClifValue,
        ty: Type,
        wide: bool,
    ) -> Result<ClifValue, String> {
        let zero = self.iconst(ty, 0);
        let is_zero = self.builder.ins().icmp(IntCC::Equal, b, zero);
        self.trap_if(is_zero, TRAP_INT_DIVIDE_BY_ZERO);
        let (min, neg_one) = if wide {
            (self.iconst(ty, i64::MIN), self.iconst(ty, -1))
        } else {
            (self.iconst(ty, i64::from(i32::MIN)), self.iconst(ty, -1))
        };
        let is_min = self.builder.ins().icmp(IntCC::Equal, a, min);
        let is_neg_one = self.builder.ins().icmp(IntCC::Equal, b, neg_one);
        let overflow = self.builder.ins().band(is_min, is_neg_one);
        self.trap_if(overflow, TRAP_INT_OVERFLOW);
        Ok(self.builder.ins().sdiv(a, b))
    }

    /// Unsigned division: only divide-by-zero traps.
    fn lower_div_u(&mut self, a: ClifValue, b: ClifValue, ty: Type) -> Result<ClifValue, String> {
        let zero = self.iconst(ty, 0);
        let is_zero = self.builder.ins().icmp(IntCC::Equal, b, zero);
        self.trap_if(is_zero, TRAP_INT_DIVIDE_BY_ZERO);
        Ok(self.builder.ins().udiv(a, b))
    }

    /// Signed remainder: divide-by-zero traps; `MIN % -1` is 0 (no trap).
    /// Cranelift's `srem` traps on `MIN % -1`, so the divisor is replaced by
    /// 1 in that case (a value of any sign divides the dividend exactly).
    fn lower_rem_s(&mut self, a: ClifValue, b: ClifValue, ty: Type) -> Result<ClifValue, String> {
        let zero = self.iconst(ty, 0);
        let is_zero = self.builder.ins().icmp(IntCC::Equal, b, zero);
        self.trap_if(is_zero, TRAP_INT_DIVIDE_BY_ZERO);
        let neg_one = self.iconst(ty, -1);
        let is_neg_one = self.builder.ins().icmp(IntCC::Equal, b, neg_one);
        let one = self.iconst(ty, 1);
        let safe = self.builder.ins().select(is_neg_one, one, b);
        Ok(self.builder.ins().srem(a, safe))
    }

    fn lower_rem_u(&mut self, a: ClifValue, b: ClifValue, ty: Type) -> Result<ClifValue, String> {
        let zero = self.iconst(ty, 0);
        let is_zero = self.builder.ins().icmp(IntCC::Equal, b, zero);
        self.trap_if(is_zero, TRAP_INT_DIVIDE_BY_ZERO);
        Ok(self.builder.ins().urem(a, b))
    }

    /// The top `r` operand-stack values as branch arguments (empty when `r`
    /// is 0).
    fn label_args(&self, r: usize) -> Vec<BlockArg> {
        self.stack[self.stack.len() - r..]
            .iter()
            .map(|value| (*value).into())
            .collect()
    }

    /// Seal and switch into an `after` continuation, restoring the operand
    /// stack to `height` plus the continuation's parameters. Every jump into
    /// `after` must already have been emitted.
    fn resume_after(&mut self, after: Block, height: usize) {
        let params = self.builder.block_params(after).to_vec();
        self.stack.truncate(height);
        self.stack.extend(params);
        self.builder.seal_block(after);
        self.builder.switch_to_block(after);
        self.dead = false;
    }

    /// Append `results` as block parameters of `block` (the continuation's
    /// incoming values when a construct completes).
    fn append_results(&mut self, block: Block, results: &[ValType]) {
        for ty in results {
            self.builder
                .append_block_param(block, clif_type(*ty).expect("numeric block result"));
        }
    }

    fn open_block(&mut self, params: &[ValType], results: &[ValType]) {
        // The parameters are already the top `params.len()` operand-stack
        // values; they become the body's initial stack, so opening only moves
        // the label base below them.
        let height = self.stack.len() - params.len();
        let after = self.builder.create_block();
        self.append_results(after, results);
        self.controls.push(CtlFrame {
            kind: CtlKind::Block,
            header: None,
            after,
            else_block: None,
            height,
            nparams: params.len(),
            params: Vec::new(),
            results: results.len(),
            in_else: false,
            has_else: false,
            after_used: false,
        });
    }

    fn open_loop(&mut self, params: &[ValType], results: &[ValType]) {
        let height = self.stack.len() - params.len();
        let header = self.builder.create_block();
        for ty in params {
            self.builder
                .append_block_param(header, clif_type(*ty).expect("numeric block parameter"));
        }
        let after = self.builder.create_block();
        self.append_results(after, results);
        // Enter the header with the initial parameters (they become the
        // header's block parameters, i.e. the first iteration's values).
        let payload = self.label_args(params.len());
        self.builder.ins().jump(header, &payload);
        self.builder.switch_to_block(header);
        let header_params = self.builder.block_params(header).to_vec();
        self.stack.truncate(height);
        self.stack.extend(header_params);
        // The header is NOT sealed here: every back-edge (`br` to this loop
        // label) is a new predecessor, which may only target an unsealed
        // block. The header is sealed when the loop's `end` closes it, once
        // all of its predecessors are declared.
        self.controls.push(CtlFrame {
            kind: CtlKind::Loop,
            header: Some(header),
            after,
            else_block: None,
            height,
            nparams: params.len(),
            params: Vec::new(),
            results: results.len(),
            in_else: false,
            has_else: false,
            after_used: false,
        });
    }

    fn open_if(&mut self, params: &[ValType], results: &[ValType]) -> Result<(), String> {
        let condition = self.pop().ok_or("operand stack underflow")?;
        // The parameters sit below the popped condition; save them so the
        // else branch can restart from the same values.
        let saved = self.stack[self.stack.len() - params.len()..].to_vec();
        let height = self.stack.len() - params.len();
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let after = self.builder.create_block();
        self.append_results(after, results);
        let zero = self.iconst(types::I32, 0);
        let flag = self.builder.ins().icmp(IntCC::NotEqual, condition, zero);
        self.builder
            .ins()
            .brif(flag, then_block, &[], else_block, &[]);
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        self.controls.push(CtlFrame {
            kind: CtlKind::If,
            header: None,
            after,
            else_block: Some(else_block),
            height,
            nparams: params.len(),
            params: saved,
            results: results.len(),
            in_else: false,
            has_else: false,
            after_used: false,
        });
        Ok(())
    }

    /// `else` marker: close the live then-branch and enter the else branch
    /// (the false-condition path always reaches it).
    fn open_else(&mut self) -> Result<(), String> {
        let idx = self
            .controls
            .len()
            .checked_sub(1)
            .ok_or("else without an open if")?;
        if self.controls[idx].kind != CtlKind::If || self.controls[idx].in_else {
            return Err("misplaced else".to_string());
        }
        if !self.dead {
            let (height, r, after) = {
                let f = &self.controls[idx];
                (f.height, f.results, f.after)
            };
            let payload = self.label_args(r);
            self.controls[idx].after_used = true;
            self.builder.ins().jump(after, &payload);
            self.stack.truncate(height);
        }
        let (height, else_block, params) = {
            let f = &self.controls[idx];
            (
                f.height,
                f.else_block.expect("if else block"),
                f.params.clone(),
            )
        };
        self.controls[idx].in_else = true;
        self.controls[idx].has_else = true;
        self.stack.truncate(height);
        self.stack.extend(params);
        self.builder.seal_block(else_block);
        self.builder.switch_to_block(else_block);
        self.dead = false;
        Ok(())
    }

    /// `end` marker closing the innermost construct.
    fn close_construct(&mut self) -> Result<(), String> {
        let idx = self
            .controls
            .len()
            .checked_sub(1)
            .ok_or("end without an open construct")?;
        let kind = self.controls[idx].kind;
        let live = !self.dead;
        match kind {
            CtlKind::Loop => {
                let (height, after, r, header) = {
                    let f = &self.controls[idx];
                    (f.height, f.after, f.results, f.header)
                };
                if live {
                    let payload = self.label_args(r);
                    self.controls[idx].after_used = true;
                    self.builder.ins().jump(after, &payload);
                }
                // All predecessors of the header (the entry edge and every
                // back-edge from the body) are now declared: seal it so the
                // variable machinery can insert the loop-carried phis.
                if let Some(header) = header {
                    self.builder.seal_block(header);
                }
                self.controls.pop();
                if live {
                    self.resume_after(after, height);
                } else {
                    self.dead = true;
                }
            }
            CtlKind::Block => {
                let (height, after, r) = {
                    let f = &self.controls[idx];
                    (f.height, f.after, f.results)
                };
                if live {
                    let payload = self.label_args(r);
                    self.controls[idx].after_used = true;
                    self.builder.ins().jump(after, &payload);
                }
                let used = self.controls[idx].after_used;
                self.controls.pop();
                if live || used {
                    self.resume_after(after, height);
                } else {
                    self.dead = true;
                }
            }
            CtlKind::If => {
                let in_else = self.controls[idx].in_else;
                let (height, after, r) = {
                    let f = &self.controls[idx];
                    (f.height, f.after, f.results)
                };
                if !in_else {
                    // No else: close the then-branch, then the empty else
                    // (the false path) jumps straight to the continuation. An
                    // `if` that carries parameters needs an explicit else to
                    // consume them (the interpreter's skip would strand them
                    // on the false path), so it stays interpreted.
                    if self.controls[idx].nparams > 0 {
                        return Err("an if with parameters needs an else branch".to_string());
                    }
                    if live {
                        let payload = self.label_args(r);
                        self.controls[idx].after_used = true;
                        self.builder.ins().jump(after, &payload);
                    }
                    let else_block = self.controls[idx].else_block.expect("if else block");
                    self.controls.pop();
                    self.stack.truncate(height);
                    self.builder.seal_block(else_block);
                    self.builder.switch_to_block(else_block);
                    self.builder.ins().jump(after, &[]);
                    self.resume_after(after, height);
                } else {
                    if live {
                        let payload = self.label_args(r);
                        self.controls[idx].after_used = true;
                        self.builder.ins().jump(after, &payload);
                    }
                    let used = self.controls[idx].after_used;
                    self.controls.pop();
                    if live || used {
                        self.resume_after(after, height);
                    } else {
                        self.dead = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// The branch target and payload arity of the control frame at `idx`: a
    /// loop label branches to its header carrying `nparams` values (the next
    /// iteration's parameters); block/if labels branch to the continuation
    /// carrying `results` values. Marks the continuation used for block/if.
    fn frame_target(&mut self, idx: usize) -> (Block, usize) {
        if self.controls[idx].kind == CtlKind::Loop {
            (
                self.controls[idx].header.expect("loop header"),
                self.controls[idx].nparams,
            )
        } else {
            self.controls[idx].after_used = true;
            (self.controls[idx].after, self.controls[idx].results)
        }
    }

    /// `br`: an unconditional jump to a label, carrying the label's payload
    /// (a loop's parameters; a block/if's results).
    fn do_br(&mut self, depth: u32) -> Result<(), String> {
        let idx = self
            .controls
            .len()
            .checked_sub(1 + depth as usize)
            .ok_or("branch past the control stack")?;
        let (target, r) = self.frame_target(idx);
        let payload = self.label_args(r);
        self.builder.ins().jump(target, &payload);
        self.dead = true;
        Ok(())
    }

    /// `br_if`: a conditional jump that leaves the payload on the stack for
    /// the fall-through path.
    fn do_br_if(&mut self, depth: u32) -> Result<(), String> {
        let condition = self.pop().ok_or("operand stack underflow")?;
        let zero = self.iconst(types::I32, 0);
        let flag = self.builder.ins().icmp(IntCC::NotEqual, condition, zero);
        let idx = self
            .controls
            .len()
            .checked_sub(1 + depth as usize)
            .ok_or("branch past the control stack")?;
        let (target, r) = self.frame_target(idx);
        let payload = self.label_args(r);
        let cont = self.builder.create_block();
        self.builder.ins().brif(flag, target, &payload, cont, &[]);
        self.builder.switch_to_block(cont);
        Ok(())
    }

    /// `br_table`: pop an `i32` index, branch to `targets[index]` when it is
    /// in range and to `default` otherwise (the interpreter's exact rule,
    /// negative indices included). Every label shares one payload arity
    /// (wasm validates this); a chain of `index == k` guards dispatches to
    /// each target, ending in an unconditional jump to the default.
    fn do_br_table(&mut self, targets: &[u32], default: u32) -> Result<(), String> {
        let index = self.pop().ok_or("operand stack underflow")?;
        // Resolve the targets (loop labels branch to the header with their
        // parameters; block/if labels branch to the continuation with their
        // results) and check they all agree on the payload arity.
        let mut resolved: Vec<(Block, usize)> = Vec::with_capacity(targets.len() + 1);
        let mut arity: Option<usize> = None;
        for &depth in targets.iter().chain(std::iter::once(&default)) {
            let frame = self
                .controls
                .len()
                .checked_sub(1 + depth as usize)
                .ok_or("branch past the control stack")?;
            let (block, a) = self.frame_target(frame);
            if let Some(common) = arity {
                if common != a {
                    return Err("br_table labels have different arities".to_string());
                }
            } else {
                arity = Some(a);
            }
            resolved.push((block, a));
        }
        let payload = self.label_args(arity.unwrap_or(0));
        for (k, &(target, _)) in resolved[..targets.len()].iter().enumerate() {
            let k_value = self.iconst(types::I32, k as i64);
            let eq = self.builder.ins().icmp(IntCC::Equal, index, k_value);
            let next = self.builder.create_block();
            self.builder.ins().brif(eq, target, &payload, next, &[]);
            self.builder.switch_to_block(next);
        }
        let (default_target, _) = resolved[targets.len()];
        self.builder.ins().jump(default_target, &payload);
        self.dead = true;
        Ok(())
    }

    /// Lower one instruction. Dead paths skip code but still track the
    /// structured markers so `end`s reach the right frames.
    fn instruction(&mut self, instr: &Instr) -> Result<(), String> {
        if self.dead {
            match instr {
                Instr::Block(_) | Instr::Loop(_) | Instr::If(_) => self.dead_depth += 1,
                Instr::Else => {
                    if self.dead_depth == 0 {
                        self.open_else()?;
                    }
                }
                Instr::End => {
                    if self.dead_depth > 0 {
                        self.dead_depth -= 1;
                    } else {
                        self.close_construct()?;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        match instr {
            Instr::Nop => {}
            Instr::I32Const(value) => {
                let v = self.iconst(types::I32, i64::from(*value));
                self.stack.push(v);
            }
            Instr::I64Const(value) => {
                let v = self.iconst(types::I64, *value);
                self.stack.push(v);
            }
            Instr::F32Const(bits) => {
                let v = self.fconst(types::F32, u64::from(*bits));
                self.stack.push(v);
            }
            Instr::F64Const(bits) => {
                let v = self.fconst(types::F64, *bits);
                self.stack.push(v);
            }
            Instr::LocalGet(index) => {
                let variable = self.variables[*index as usize];
                let value = self.builder.use_var(variable);
                self.stack.push(value);
            }
            Instr::LocalSet(index) => {
                let value = self.pop().ok_or("operand stack underflow")?;
                let variable = self.variables[*index as usize];
                self.builder.def_var(variable, value);
            }
            Instr::LocalTee(index) => {
                let value = self.pop().ok_or("operand stack underflow")?;
                let variable = self.variables[*index as usize];
                self.builder.def_var(variable, value);
                self.stack.push(value);
            }
            Instr::GlobalGet(index) => self.do_global_get(*index)?,
            Instr::GlobalSet(index) => self.do_global_set(*index)?,
            Instr::Drop => {
                self.pop().ok_or("operand stack underflow")?;
            }
            Instr::Select | Instr::SelectTyped(_) => {
                let condition = self.pop().ok_or("operand stack underflow")?;
                let b = self.pop().ok_or("operand stack underflow")?;
                let a = self.pop().ok_or("operand stack underflow")?;
                let zero = self.iconst(types::I32, 0);
                let flag = self.builder.ins().icmp(IntCC::NotEqual, condition, zero);
                let value = self.builder.ins().select(flag, a, b);
                self.stack.push(value);
            }
            Instr::Num(op) => self.lower_num(*op)?,
            Instr::Load {
                memory, op, offset, ..
            } => self.do_load(*memory, *op, *offset)?,
            Instr::Store {
                memory, op, offset, ..
            } => self.do_store(*memory, *op, *offset)?,
            Instr::MemorySize(memory) => self.do_memory_size(*memory)?,
            Instr::MemoryGrow(memory) => self.do_memory_grow(*memory)?,
            Instr::Block(bt) => {
                let (params, results) =
                    block_sig(self.module, bt).ok_or("unsupported block type")?;
                self.open_block(&params, &results);
            }
            Instr::Loop(bt) => {
                let (params, results) =
                    block_sig(self.module, bt).ok_or("unsupported block type")?;
                self.open_loop(&params, &results);
            }
            Instr::If(bt) => {
                let (params, results) =
                    block_sig(self.module, bt).ok_or("unsupported block type")?;
                self.open_if(&params, &results)?;
            }
            Instr::Else => self.open_else()?,
            Instr::End => self.close_construct()?,
            Instr::Br(depth) => self.do_br(*depth)?,
            Instr::BrIf(depth) => self.do_br_if(*depth)?,
            Instr::BrTable { targets, default } => self.do_br_table(targets, *default)?,
            Instr::Return => {
                self.emit_return()?;
                self.dead = true;
            }
            Instr::Unreachable => {
                self.emit_trap(TRAP_UNREACHABLE);
                self.dead = true;
            }
            _ => return Err("unsupported instruction reached the lowerer".to_string()),
        }
        Ok(())
    }

    fn lower_body(&mut self, body: &FuncBody) -> Result<(), String> {
        for instr in &body.body {
            self.instruction(instr)?;
        }
        if !self.controls.is_empty() {
            return Err("unclosed control frame at the end of a body".to_string());
        }
        if !self.dead {
            self.emit_return()?;
        }
        Ok(())
    }
}

/// Lower `body` into `func`. Entry params: `(args, nargs, mems, ncount,
/// gvals, out, nout, store, instance, grow)`, all pointers/counts as `I64`;
/// returns the trap code `I32`. `mems` is the per-memory descriptor array;
/// `used_globals` lists the module globals the body touches (their initial
/// values arrive and final values leave through the `gvals` buffer);
/// `store`/`instance`/`grow` let the body call the `memory.grow` helper.
fn lower(
    module: &Module,
    body: &FuncBody,
    func_type: &FuncType,
    used_globals: &[u32],
    func: &mut Function,
    fctx: &mut FunctionBuilderContext,
    isa: &dyn TargetIsa,
) -> Result<(), String> {
    let conv = platform_call_conv(isa);
    let mut builder = FunctionBuilder::new(func, fctx);
    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let params = builder.block_params(entry_block).to_vec();
    let (args_ptr, mems, globals_ptr, out_ptr, store, instance, grow) = (
        params[0], params[2], params[4], params[5], params[7], params[8], params[9],
    );
    // Each used module global gets a `gvals` buffer slot (slot == position).
    let globals = used_globals
        .iter()
        .map(|index| {
            let (ty, _) = defined_global(module, *index).expect("global type checked by lowerable");
            (
                *index,
                clif_type(ty).expect("numeric global checked by lowerable"),
            )
        })
        .collect::<Vec<_>>();

    // Locals: params come from the args buffer; declared locals default 0.
    let mut value_types: Vec<ValType> = func_type.params.clone();
    value_types.extend(body.locals.iter().copied());
    let mut variables = Vec::with_capacity(value_types.len());
    for (index, ty) in value_types.iter().enumerate() {
        let variable = builder.declare_var(clif_type(*ty).expect("checked by lowerable"));
        let value = if index < func_type.params.len() {
            // Load the i-th param (an I64 slot) and narrow/narrow-convert it
            // to the parameter's value type.
            let address = builder.ins().iadd_imm_s(args_ptr, 8 * index as i64);
            let wide =
                builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), address, Offset32::new(0));
            match ty {
                ValType::I32 => builder.ins().ireduce(types::I32, wide),
                ValType::I64 => wide,
                ValType::F32 => {
                    let bits = builder.ins().ireduce(types::I32, wide);
                    builder.ins().bitcast(types::F32, MemFlagsData::new(), bits)
                }
                ValType::F64 => builder.ins().bitcast(types::F64, MemFlagsData::new(), wide),
                _ => return Err("unsupported parameter type".to_string()),
            }
        } else {
            match ty {
                ValType::I32 => builder.ins().iconst(types::I32, 0),
                ValType::I64 => builder.ins().iconst(types::I64, 0),
                ValType::F32 => builder.ins().f32const(Ieee32::with_bits(0)),
                ValType::F64 => builder.ins().f64const(Ieee64::with_bits(0)),
                _ => return Err("unsupported local type".to_string()),
            }
        };
        builder.def_var(variable, value);
        variables.push(variable);
    }

    let mut lowerer = Lowerer {
        builder,
        module,
        conv,
        variables,
        stack: Vec::new(),
        mems,
        globals_ptr,
        globals,
        out_ptr,
        store,
        instance,
        grow,
        results: func_type.results.clone(),
        controls: Vec::new(),
        dead: false,
        dead_depth: 0,
    };
    lowerer.lower_body(body)?;
    lowerer.builder.seal_all_blocks();
    lowerer.builder.finalize(isa.frontend_config());
    Ok(())
}

/// Executable memory for compiled bodies (W^X via `region`); mirrors
/// `crates/jit`'s allocator so the two JITs share one safety story.
struct ExecutableCode {
    allocation: region::Allocation,
}

impl ExecutableCode {
    fn new(bytes: &[u8]) -> Result<Self, region::Error> {
        let mut allocation = region::alloc(bytes.len().max(1), region::Protection::READ_WRITE)?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                allocation.as_mut_ptr::<u8>(),
                bytes.len(),
            );
            region::protect(
                allocation.as_ptr::<u8>(),
                allocation.len(),
                region::Protection::READ_EXECUTE,
            )?;
        }
        Ok(Self { allocation })
    }

    fn as_ptr(&self) -> *const u8 {
        self.allocation.as_ptr::<u8>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Store;
    use crate::instr::StoreOp;
    use crate::module::{FuncBody, Global, Module};
    use crate::types::{BlockType, GlobalType, Limits, MemType, SubType};

    fn int_module(body: Vec<Instr>, params: Vec<ValType>, results: Vec<ValType>) -> Module {
        Module {
            types: vec![SubType::func(params.clone(), results.clone())],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![],
                body,
            }],
            ..Module::default()
        }
    }

    fn invoke(module: &Module, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
        let mut store = Store::new();
        let instance = store
            .instantiate(module, &mut |_, _| None)
            .expect("module instantiates");
        store.invoke(instance, 0, args)
    }

    fn invoke_i32(module: &Module, args: &[i32]) -> Result<i32, ExecFail> {
        let values: Vec<Value> = args.iter().map(|a| Value::I32(*a)).collect();
        let results = invoke(module, &values)?;
        match results.as_slice() {
            [Value::I32(value)] => Ok(*value),
            _ => Err(ExecFail::Unsupported("expected one i32 result")),
        }
    }

    /// Run `module`'s function `index` through both paths (compiled and the
    /// interpreter forced) over every `cases` argument list and assert the
    /// outcomes agree exactly (values and trap kinds).
    fn assert_equiv(module: &Module, index: usize, cases: &[Vec<Value>]) {
        // Gate the harness: `Engine::compile` falls back to the interpreter
        // silently on any lowering error, so without this the comparison is
        // vacuous (interpreter vs interpreter) for a body that did not
        // compile.
        assert!(
            compile_module(module).iter().all(|entry| entry.is_some()),
            "assert_equiv module did not compile; the comparison is vacuous"
        );
        let mut compiled = Store::new();
        let compiled_instance = compiled
            .instantiate(module, &mut |_, _| None)
            .expect("module instantiates");
        let mut interpreter = Store::new();
        interpreter.set_compile(false);
        let interpreter_instance = interpreter
            .instantiate(module, &mut |_, _| None)
            .expect("module instantiates");
        for args in cases {
            let via_compiled = compiled.invoke(compiled_instance, index, args);
            let via_interpreter = interpreter.invoke(interpreter_instance, index, args);
            let agree = match (&via_compiled, &via_interpreter) {
                (Ok(a), Ok(b)) => a == b,
                (Err(a), Err(b)) => format!("{a:?}") == format!("{b:?}"),
                _ => false,
            };
            assert!(
                agree,
                "paths diverge for {args:?}: compiled={via_compiled:?} interpreter={via_interpreter:?}"
            );
        }
    }

    fn i32_pairs() -> Vec<Vec<Value>> {
        let values = [
            0i32,
            1,
            -1,
            7,
            -7,
            12345,
            0x4000_0000,
            -0x4000_0000,
            i32::MAX,
            i32::MIN,
        ];
        let mut cases = Vec::new();
        for a in values {
            for b in values {
                cases.push(vec![Value::I32(a), Value::I32(b)]);
            }
        }
        cases
    }

    fn i64_pairs() -> Vec<Vec<Value>> {
        let values = [0i64, 1, -1, 7, -7, 1 << 40, -(1 << 40), i64::MAX, i64::MIN];
        let mut cases = Vec::new();
        for a in values {
            for b in values {
                cases.push(vec![Value::I64(a), Value::I64(b)]);
            }
        }
        cases
    }

    /// f32 bit patterns exercising the canonicalization policy: signed
    /// zeros, subnormals, normals, infinities, and quiet/signaling NaNs of
    /// both signs plus non-canonical payloads.
    fn f32_bits() -> [u32; 20] {
        [
            0x0000_0000, // +0
            0x8000_0000, // -0
            0x0000_0001, // min subnormal
            0x007f_ffff, // max subnormal
            0x0080_0000, // min normal
            0x3e00_0000, // 0.125
            0x3f00_0000, // 0.5
            0x3f80_0000, // 1.0
            0x3f80_0001, // 1.0 + 1 ulp
            0x4000_0000, // 2.0
            0x4120_0000, // 10.0
            0x7f7f_ffff, // max finite
            0x7f80_0000, // +inf
            0xff80_0000, // -inf
            0x7fc0_0000, // +canonical quiet NaN
            0xffc0_0000, // -canonical quiet NaN
            0x7fa0_0000, // +signaling NaN
            0xffa0_0000, // -signaling NaN
            0x7fc1_2345, // +NaN payload
            0xffc1_2345, // -NaN payload
        ]
    }

    fn f64_bits() -> [u64; 18] {
        [
            0x0000_0000_0000_0000, // +0
            0x8000_0000_0000_0000, // -0
            0x0000_0000_0000_0001, // min subnormal
            0x000f_ffff_ffff_ffff, // max subnormal
            0x0010_0000_0000_0000, // min normal
            0x3fe0_0000_0000_0000, // 0.5
            0x3ff0_0000_0000_0000, // 1.0
            0x3ff0_0000_0000_0001, // 1.0 + 1 ulp
            0x4000_0000_0000_0000, // 2.0
            0x4024_0000_0000_0000, // 10.0
            0x7fef_ffff_ffff_ffff, // max finite
            0x7ff0_0000_0000_0000, // +inf
            0xfff0_0000_0000_0000, // -inf
            0x7ff8_0000_0000_0000, // +canonical quiet NaN
            0xfff8_0000_0000_0000, // -canonical quiet NaN
            0x7ff4_0000_0000_0000, // +signaling NaN
            0xfff4_0000_0000_0000, // -signaling NaN
            0x7ff8_1234_5678_9abc, // +NaN payload
        ]
    }

    fn f32_pairs() -> Vec<Vec<Value>> {
        let mut cases = Vec::new();
        for a in f32_bits() {
            for b in f32_bits() {
                cases.push(vec![Value::F32(a), Value::F32(b)]);
            }
        }
        cases
    }

    fn f64_pairs() -> Vec<Vec<Value>> {
        let mut cases = Vec::new();
        for a in f64_bits() {
            for b in f64_bits() {
                cases.push(vec![Value::F64(a), Value::F64(b)]);
            }
        }
        cases
    }

    fn f32_unary() -> Vec<Vec<Value>> {
        f32_bits()
            .into_iter()
            .map(|a| vec![Value::F32(a)])
            .collect()
    }

    fn f64_unary() -> Vec<Vec<Value>> {
        f64_bits()
            .into_iter()
            .map(|a| vec![Value::F64(a)])
            .collect()
    }

    #[test]
    fn compiles_and_runs_add() {
        let module = int_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::Num(NumOp::I32Add),
            ],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        assert_eq!(invoke_i32(&module, &[20, 22]).expect("runs"), 42);
        assert_eq!(invoke_i32(&module, &[-5, 3]).expect("runs"), -2);
    }

    #[test]
    fn compiles_and_runs_locals_and_select() {
        // (i32 i32 i32) -> i32: local.set a, then a ? b : c via select.
        let module = int_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::LocalGet(2),
                Instr::Select,
            ],
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        // select(v1, v2, c): c != 0 -> v1 (arg 0), else v2 (arg 1).
        assert_eq!(invoke_i32(&module, &[7, 9, 1]).expect("runs"), 7);
        assert_eq!(invoke_i32(&module, &[7, 9, 0]).expect("runs"), 9);
    }

    #[test]
    fn compiled_division_matches_wasm_semantics() {
        let module = int_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::Num(NumOp::I32DivS),
            ],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        assert_eq!(invoke_i32(&module, &[7, 2]).expect("runs"), 3);
        assert_eq!(invoke_i32(&module, &[-7, 2]).expect("runs"), -3);
        assert_eq!(
            invoke_i32(&module, &[1, 0]),
            Err(ExecFail::Trap(Trap::IntegerDivideByZero))
        );
        assert_eq!(
            invoke_i32(&module, &[i32::MIN, -1]),
            Err(ExecFail::Trap(Trap::IntegerOverflow))
        );
    }

    #[test]
    fn compiled_signed_remainder_min_neg_one_is_zero() {
        let module = int_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::Num(NumOp::I32RemS),
            ],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        assert_eq!(invoke_i32(&module, &[i32::MIN, -1]).expect("runs"), 0);
        assert_eq!(invoke_i32(&module, &[7, 3]).expect("runs"), 1);
        assert_eq!(
            invoke_i32(&module, &[1, 0]),
            Err(ExecFail::Trap(Trap::IntegerDivideByZero))
        );
    }

    #[test]
    fn unreachable_traps() {
        let module = int_module(
            vec![Instr::LocalGet(0), Instr::Unreachable],
            vec![ValType::I32],
            vec![ValType::I32],
        );
        assert_eq!(
            invoke_i32(&module, &[1]),
            Err(ExecFail::Trap(Trap::Unreachable))
        );
    }

    #[test]
    fn lowerable_functions_with_calls_stay_interpreted() {
        // A function with a Call is not compiled; it still runs (the hook
        // falls back to the interpreter) and produces the right answer.
        let module = Module {
            types: vec![
                SubType::func(vec![], vec![ValType::I32]),
                SubType::func(vec![], vec![ValType::I32]),
            ],
            functions: vec![0, 1],
            bodies: vec![
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(21),
                        Instr::I32Const(2),
                        Instr::Num(NumOp::I32Mul),
                    ],
                },
                FuncBody {
                    locals: vec![],
                    body: vec![Instr::Call(0)],
                },
            ],
            ..Module::default()
        };
        let mut store = Store::new();
        let instance = store
            .instantiate(&module, &mut |_, _| None)
            .expect("module instantiates");
        let results = store
            .invoke(instance, 1, &[])
            .expect("runs through the interpreter");
        assert_eq!(results, vec![Value::I32(42)]);
    }

    #[test]
    fn i32_binary_ops_match_the_interpreter() {
        use NumOp::*;
        let cases = i32_pairs();
        for (op, _) in [
            (I32Add, "add"),
            (I32Sub, "sub"),
            (I32Mul, "mul"),
            (I32DivS, "div_s"),
            (I32DivU, "div_u"),
            (I32RemS, "rem_s"),
            (I32RemU, "rem_u"),
            (I32And, "and"),
            (I32Or, "or"),
            (I32Xor, "xor"),
            (I32Shl, "shl"),
            (I32ShrS, "shr_s"),
            (I32ShrU, "shr_u"),
            (I32Rotl, "rotl"),
            (I32Rotr, "rotr"),
            (I32Eq, "eq"),
            (I32Ne, "ne"),
            (I32LtS, "lt_s"),
            (I32LtU, "lt_u"),
            (I32GtS, "gt_s"),
            (I32GtU, "gt_u"),
            (I32LeS, "le_s"),
            (I32LeU, "le_u"),
            (I32GeS, "ge_s"),
            (I32GeU, "ge_u"),
        ] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::LocalGet(1), Instr::Num(op)],
                vec![ValType::I32, ValType::I32],
                vec![ValType::I32],
            );
            assert_equiv(&module, 0, &cases);
        }
    }

    #[test]
    fn i64_binary_ops_match_the_interpreter() {
        use NumOp::*;
        let cases = i64_pairs();
        for op in [
            I64Add, I64Sub, I64Mul, I64DivS, I64DivU, I64RemS, I64RemU, I64And, I64Or, I64Xor,
            I64Shl, I64ShrS, I64ShrU, I64Rotl, I64Rotr,
        ] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::LocalGet(1), Instr::Num(op)],
                vec![ValType::I64, ValType::I64],
                vec![ValType::I64],
            );
            assert_equiv(&module, 0, &cases);
        }
        for op in [
            I64Eq, I64Ne, I64LtS, I64LtU, I64GtS, I64GtU, I64LeS, I64LeU, I64GeS, I64GeU,
        ] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::LocalGet(1), Instr::Num(op)],
                vec![ValType::I64, ValType::I64],
                vec![ValType::I32],
            );
            assert_equiv(&module, 0, &cases);
        }
    }

    #[test]
    fn i32_unary_and_conversion_ops_match_the_interpreter() {
        use NumOp::*;
        let values = i32_pairs()
            .into_iter()
            .map(|pair| vec![pair[0]])
            .collect::<Vec<_>>();
        for op in [I32Popcnt, I32Eqz, I32Clz, I32Ctz] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I32],
                vec![ValType::I32],
            );
            assert_equiv(&module, 0, &values);
        }
        // i64.popcnt and i64.eqz over the wide values (popcnt stays i64;
        // eqz narrows to i32).
        let wide = i64_pairs()
            .into_iter()
            .map(|pair| vec![pair[0]])
            .collect::<Vec<_>>();
        for (op, result) in [
            (I64Popcnt, ValType::I64),
            (I64Eqz, ValType::I32),
            (I64Clz, ValType::I64),
            (I64Ctz, ValType::I64),
        ] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I64],
                vec![result],
            );
            assert_equiv(&module, 0, &wide);
        }
        // i64.wrap_i32 truncates to the low 32 bits.
        let module = int_module(
            vec![Instr::LocalGet(0), Instr::Num(I32WrapI64)],
            vec![ValType::I64],
            vec![ValType::I32],
        );
        assert_equiv(&module, 0, &wide);
    }

    #[test]
    fn select_and_locals_match_the_interpreter() {
        // acc = a * b + c, using declared local 3 as a scratch tee slot, then
        // return it through a select that picks acc or c.
        let module = Module {
            types: vec![SubType::func(
                vec![ValType::I32, ValType::I32, ValType::I32],
                vec![ValType::I32],
            )],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![ValType::I32],
                body: vec![
                    Instr::LocalGet(0),
                    Instr::LocalGet(1),
                    Instr::Num(NumOp::I32Mul),
                    Instr::LocalTee(3),
                    Instr::LocalGet(2),
                    Instr::Num(NumOp::I32Add),
                    Instr::LocalSet(3),
                    // flag = a > 0 ? acc : c
                    Instr::LocalGet(3),
                    Instr::LocalGet(2),
                    Instr::LocalGet(0),
                    Instr::I32Const(0),
                    Instr::Num(NumOp::I32GtS),
                    Instr::Select,
                ],
            }],
            ..Module::default()
        };
        let cases = [
            vec![Value::I32(3), Value::I32(4), Value::I32(5)],
            vec![Value::I32(-1), Value::I32(2), Value::I32(-3)],
            vec![Value::I32(i32::MAX), Value::I32(2), Value::I32(1)],
            vec![Value::I32(0), Value::I32(9), Value::I32(-5)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn f32_binary_ops_match_the_interpreter() {
        use NumOp::*;
        let cases = f32_pairs();
        for (op, result) in [
            (F32Add, ValType::F32),
            (F32Sub, ValType::F32),
            (F32Mul, ValType::F32),
            (F32Div, ValType::F32),
            (F32Copysign, ValType::F32),
            (F32Eq, ValType::I32),
            (F32Ne, ValType::I32),
            (F32Lt, ValType::I32),
            (F32Gt, ValType::I32),
            (F32Le, ValType::I32),
            (F32Ge, ValType::I32),
        ] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::LocalGet(1), Instr::Num(op)],
                vec![ValType::F32, ValType::F32],
                vec![],
                vec![result],
            );
            assert_equiv(&module, 0, &cases);
        }
    }

    #[test]
    fn f64_binary_ops_match_the_interpreter() {
        use NumOp::*;
        let cases = f64_pairs();
        for (op, result) in [
            (F64Add, ValType::F64),
            (F64Sub, ValType::F64),
            (F64Mul, ValType::F64),
            (F64Div, ValType::F64),
            (F64Copysign, ValType::F64),
            (F64Eq, ValType::I32),
            (F64Ne, ValType::I32),
            (F64Lt, ValType::I32),
            (F64Gt, ValType::I32),
            (F64Le, ValType::I32),
            (F64Ge, ValType::I32),
        ] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::LocalGet(1), Instr::Num(op)],
                vec![ValType::F64, ValType::F64],
                vec![],
                vec![result],
            );
            assert_equiv(&module, 0, &cases);
        }
    }

    #[test]
    fn f32_unary_ops_match_the_interpreter() {
        use NumOp::*;
        let values = f32_unary();
        for op in [
            F32Abs, F32Neg, F32Ceil, F32Floor, F32Trunc, F32Nearest, F32Sqrt,
        ] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::F32],
                vec![],
                vec![ValType::F32],
            );
            assert_equiv(&module, 0, &values);
        }
    }

    #[test]
    fn f64_unary_ops_match_the_interpreter() {
        use NumOp::*;
        let values = f64_unary();
        for op in [
            F64Abs, F64Neg, F64Ceil, F64Floor, F64Trunc, F64Nearest, F64Sqrt,
        ] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::F64],
                vec![],
                vec![ValType::F64],
            );
            assert_equiv(&module, 0, &values);
        }
    }

    #[test]
    fn float_conversions_match_the_interpreter() {
        use NumOp::*;
        // Rounding-relevant integer inputs: values straddling the f32 and f64
        // exactness boundaries (2^24 and 2^53), plus extremes.
        let i32_cases: Vec<Vec<Value>> = [
            0i32,
            1,
            -1,
            0x00ff_ffff,  // 2^24 - 1 (exact in f32)
            0x0100_0000,  // 2^24
            0x0100_0001,  // 2^24 + 1 (rounds)
            -0x0100_0001, // -(2^24 + 1)
            123_456_789,
            -123_456_789,
            i32::MAX,
            i32::MIN,
        ]
        .into_iter()
        .map(|a| vec![Value::I32(a)])
        .collect();
        let i64_cases: Vec<Vec<Value>> = [
            0i64,
            1,
            -1,
            0x1f_ffff_ffff, // 2^37 - 1
            0x20_0000_0000, // 2^37
            0x20_0000_0001, // 2^37 + 1
            (1 << 53) - 1,
            1 << 53,
            (1 << 53) + 1,
            123_456_789_012_345,
            -123_456_789_012_345,
            i64::MAX,
            i64::MIN,
        ]
        .into_iter()
        .map(|a| vec![Value::I64(a)])
        .collect();
        for op in [F32ConvertI32S, F32ConvertI32U] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I32],
                vec![],
                vec![ValType::F32],
            );
            assert_equiv(&module, 0, &i32_cases);
        }
        for op in [F32ConvertI64S, F32ConvertI64U] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I64],
                vec![],
                vec![ValType::F32],
            );
            assert_equiv(&module, 0, &i64_cases);
        }
        for op in [F64ConvertI32S, F64ConvertI32U] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I32],
                vec![],
                vec![ValType::F64],
            );
            assert_equiv(&module, 0, &i32_cases);
        }
        for op in [F64ConvertI64S, F64ConvertI64U] {
            let module = module_with(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I64],
                vec![],
                vec![ValType::F64],
            );
            assert_equiv(&module, 0, &i64_cases);
        }
    }

    #[test]
    fn float_values_flow_through_control_structures() {
        // block (result f64): an f64 result carried through the
        // continuation's block params, then returned.
        let block = module_with(
            vec![
                Instr::Block(BlockType::Val(ValType::F64)),
                Instr::LocalGet(0),
                Instr::F64Const(0x3ff8_0000_0000_0000), // 1.5
                Instr::Num(NumOp::F64Add),
                Instr::End,
            ],
            vec![ValType::F64],
            vec![],
            vec![ValType::F64],
        );
        assert_equiv(&block, 0, &f64_unary());

        // if/else (result f32): cond ? x : -x.
        let if_else = module_with(
            vec![
                Instr::LocalGet(1),
                Instr::If(BlockType::Val(ValType::F32)),
                Instr::LocalGet(0),
                Instr::Else,
                Instr::LocalGet(0),
                Instr::Num(NumOp::F32Neg),
                Instr::End,
            ],
            vec![ValType::F32, ValType::I32],
            vec![],
            vec![ValType::F32],
        );
        let mut cases = Vec::new();
        for x in f32_bits() {
            for cond in [Value::I32(0), Value::I32(1)] {
                cases.push(vec![Value::F32(x), cond]);
            }
        }
        assert_equiv(&if_else, 0, &cases);

        // select over f32 operands (untyped `select` is valid for floats).
        let select = module_with(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::LocalGet(2),
                Instr::Select,
            ],
            vec![ValType::F32, ValType::F32, ValType::I32],
            vec![],
            vec![ValType::F32],
        );
        let mut cases = Vec::new();
        for a in f32_bits() {
            for b in f32_bits() {
                for cond in [Value::I32(0), Value::I32(1)] {
                    cases.push(vec![Value::F32(a), Value::F32(b), cond]);
                }
            }
        }
        assert_equiv(&select, 0, &cases);
    }

    fn module_with(
        body: Vec<Instr>,
        params: Vec<ValType>,
        locals: Vec<ValType>,
        results: Vec<ValType>,
    ) -> Module {
        Module {
            types: vec![SubType::func(params.clone(), results.clone())],
            functions: vec![0],
            bodies: vec![FuncBody { locals, body }],
            ..Module::default()
        }
    }

    /// `module_with` plus a second type-section entry (`type index 1`) so a
    /// `BlockType::Type(1)` can carry a parameterized/multi-value signature.
    fn module_with_type(
        body: Vec<Instr>,
        params: Vec<ValType>,
        locals: Vec<ValType>,
        results: Vec<ValType>,
        block: &FuncType,
    ) -> Module {
        Module {
            types: vec![
                SubType::func(params.clone(), results.clone()),
                SubType::func(block.params.clone(), block.results.clone()),
            ],
            functions: vec![0],
            bodies: vec![FuncBody { locals, body }],
            ..Module::default()
        }
    }

    /// A single-function module with one 1-page 32-bit memory (index 0).
    fn mem_module(body: Vec<Instr>, params: Vec<ValType>, results: Vec<ValType>) -> Module {
        Module {
            types: vec![SubType::func(params.clone(), results.clone())],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![],
                body,
            }],
            memories: vec![MemType {
                limits: Limits {
                    min: 1,
                    max: None,
                    shared: false,
                },
                memory64: false,
            }],
            ..Module::default()
        }
    }

    /// A single-function module with module-defined globals (`ty`, init expr).
    fn global_module(
        body: Vec<Instr>,
        params: Vec<ValType>,
        results: Vec<ValType>,
        globals: Vec<(GlobalType, Vec<Instr>)>,
    ) -> Module {
        Module {
            types: vec![SubType::func(params.clone(), results.clone())],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![],
                body,
            }],
            globals: globals
                .into_iter()
                .map(|(ty, init)| Global { ty, init })
                .collect(),
            ..Module::default()
        }
    }

    fn i32_global(mutable: bool, init: i32) -> (GlobalType, Vec<Instr>) {
        (
            GlobalType {
                value: ValType::I32,
                mutable,
            },
            vec![Instr::I32Const(init)],
        )
    }

    /// Run an ordered sequence of `(function index, args)` invocations through
    /// both paths on fresh stores and require every outcome (values or trap)
    /// to agree, returning the interpreter outcomes so tests can assert on
    /// cross-call global state.
    fn run_seq(
        module: &Module,
        sequence: &[(usize, Vec<Value>)],
    ) -> Vec<Result<Vec<Value>, ExecFail>> {
        let mut compiled = Store::new();
        let compiled_instance = compiled
            .instantiate(module, &mut |_, _| None)
            .expect("module instantiates");
        let mut interpreter = Store::new();
        interpreter.set_compile(false);
        let interpreter_instance = interpreter
            .instantiate(module, &mut |_, _| None)
            .expect("module instantiates");
        let mut outcomes = Vec::new();
        for (index, args) in sequence {
            let via_compiled = compiled.invoke(compiled_instance, *index, args);
            let via_interpreter = interpreter.invoke(interpreter_instance, *index, args);
            assert!(
                via_compiled == via_interpreter,
                "paths diverge for seq {sequence:?}: compiled={via_compiled:?} interpreter={via_interpreter:?}"
            );
            outcomes.push(via_interpreter);
        }
        outcomes
    }

    #[test]
    fn block_with_a_value_matches_the_interpreter() {
        // (block (result i32) (i32.const 40) (i32.const 2) i32.add)
        let module = module_with(
            vec![
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::I32Const(40),
                Instr::I32Const(2),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![],
            vec![],
            vec![ValType::I32],
        );
        assert_equiv(&module, 0, &[vec![]]);
    }

    #[test]
    fn br_if_exits_a_block_with_its_result() {
        // block (result i32) { i32.const 1; local.get 0; br_if 0;
        //                        i32.const 41; i32.add }
        // cond != 0 -> 1 (the payload); cond == 0 -> 1 + 41.
        let module = module_with(
            vec![
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::I32Const(1),
                Instr::LocalGet(0),
                Instr::BrIf(0),
                Instr::I32Const(41),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
        );
        let cases = [
            vec![Value::I32(0)],
            vec![Value::I32(1)],
            vec![Value::I32(-3)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn if_else_value_matches_the_interpreter() {
        // if (result i32) { 7 } else { 9 } -- cond on the stack.
        let module = module_with(
            vec![
                Instr::LocalGet(0),
                Instr::If(BlockType::Val(ValType::I32)),
                Instr::I32Const(7),
                Instr::Else,
                Instr::I32Const(9),
                Instr::End,
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
        );
        let cases = [
            vec![Value::I32(1)],
            vec![Value::I32(0)],
            vec![Value::I32(-1)],
            vec![Value::I32(12345)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn loop_with_a_back_edge_matches_the_interpreter() {
        // sum_{i<n} i via an outer empty block + br_if exit + br 0 loop:
        //   (func (param $n i32) (result i32) (local $acc i32) (local $i i32)
        //     block
        //       loop
        //         local.get $i local.get $n i32.ge_s br_if 1
        //         local.get $acc local.get $i i32.add local.set $acc
        //         local.get $i i32.const 1 i32.add local.set $i
        //         br 0
        //       end
        //     end
        //     local.get $acc)
        let module = module_with(
            vec![
                Instr::Block(BlockType::Empty),
                Instr::Loop(BlockType::Empty),
                // exit when i >= n
                Instr::LocalGet(2),
                Instr::LocalGet(0),
                Instr::Num(NumOp::I32GeS),
                Instr::BrIf(1),
                // acc += i
                Instr::LocalGet(1),
                Instr::LocalGet(2),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(1),
                // i += 1
                Instr::LocalGet(2),
                Instr::I32Const(1),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(2),
                Instr::Br(0),
                Instr::End,
                Instr::End,
                Instr::LocalGet(1),
            ],
            vec![ValType::I32],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        let cases = [
            vec![Value::I32(0)],
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(5)],
            vec![Value::I32(10)],
            vec![Value::I32(64)],
            vec![Value::I32(-4)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn loop_that_falls_out_matches_the_interpreter() {
        // Same sum, but the loop body ends with `br_if 0` (continue while
        // i < n) and falls out of the `end` when the increment makes i >= n:
        //   loop
        //     local.get $acc local.get $i i32.add local.set $acc
        //     local.get $i i32.const 1 i32.add local.set $i
        //     local.get $i local.get $n i32.lt_s br_if 0
        //   end
        let module = module_with(
            vec![
                Instr::Loop(BlockType::Empty),
                Instr::LocalGet(1),
                Instr::LocalGet(2),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(1),
                Instr::LocalGet(2),
                Instr::I32Const(1),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(2),
                Instr::LocalGet(2),
                Instr::LocalGet(0),
                Instr::Num(NumOp::I32LtS),
                Instr::BrIf(0),
                Instr::End,
                Instr::LocalGet(1),
            ],
            vec![ValType::I32],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        let cases = [
            vec![Value::I32(0)],
            vec![Value::I32(1)],
            vec![Value::I32(3)],
            vec![Value::I32(10)],
            vec![Value::I32(100)],
            vec![Value::I32(1000)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn nested_if_in_a_loop_matches_the_interpreter() {
        // Collatz-style iteration inside a loop with an if/else that updates
        // a local and a br_if exit:
        //   (func (param $n i32) (result i32)
        //     local.set 2 (t = n)
        //     block
        //       loop
        //         local.get 2 i32.const 1 i32.le_s br_if 1   ;; exit when t <= 1
        //         local.get 2 i32.const 2 i32.rem_s          ;; t % 2
        //         if (result i32)                            ;; 1 -> odd
        //           local.get 2 i32.const 3 i32.mul i32.const 1 i32.add
        //         else
        //           local.get 2 i32.const 2 i32.div_s
        //         end
        //         local.set 2
        //         br 0
        //       end
        //     end
        //     local.get 2)
        let module = module_with(
            vec![
                Instr::LocalGet(0),
                Instr::LocalSet(1),
                Instr::Block(BlockType::Empty),
                Instr::Loop(BlockType::Empty),
                Instr::LocalGet(1),
                Instr::I32Const(1),
                Instr::Num(NumOp::I32LeS),
                Instr::BrIf(1),
                Instr::LocalGet(1),
                Instr::I32Const(2),
                Instr::Num(NumOp::I32RemS),
                Instr::If(BlockType::Val(ValType::I32)),
                Instr::LocalGet(1),
                Instr::I32Const(3),
                Instr::Num(NumOp::I32Mul),
                Instr::I32Const(1),
                Instr::Num(NumOp::I32Add),
                Instr::Else,
                Instr::LocalGet(1),
                Instr::I32Const(2),
                Instr::Num(NumOp::I32DivS),
                Instr::End,
                Instr::LocalSet(1),
                Instr::Br(0),
                Instr::End,
                Instr::End,
                Instr::LocalGet(1),
            ],
            vec![ValType::I32],
            vec![ValType::I32],
            vec![ValType::I32],
        );
        let cases = [
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(3)],
            vec![Value::I32(6)],
            vec![Value::I32(27)],
        ];
        assert_equiv(&module, 0, &cases);
    }

    #[test]
    fn br_table_matches_the_interpreter() {
        // 1. Uniform arity-1 payload, every index branches to the same
        // block: block (result i32) { const 1; br_table 0 0 0 <index> }.
        let payload = module_with(
            vec![
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::I32Const(1),
                Instr::LocalGet(0),
                Instr::BrTable {
                    targets: vec![0, 0, 0],
                    default: 0,
                },
                Instr::End,
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
        );
        let index_cases = [-5i32, -1, 0, 1, 2, 3, 100]
            .into_iter()
            .map(|i| vec![Value::I32(i)])
            .collect::<Vec<_>>();
        assert_equiv(&payload, 0, &index_cases);

        // 2. Dispatch over three nested value blocks: exiting the k-th block
        // carries the payload into the code after it, so each index picks a
        // different post-processing chain.
        //   block $out (result i32)
        //     block $a (result i32)
        //       block $b (result i32)
        //         i32.const 5 (payload)
        //         br_table 0 1 2 2 (index)
        //       end
        //       i32.const 10 i32.add
        //     end
        //     i32.const 100 i32.add
        //   end
        let dispatch = module_with(
            vec![
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::Block(BlockType::Val(ValType::I32)),
                Instr::I32Const(5),
                Instr::LocalGet(0),
                Instr::BrTable {
                    targets: vec![0, 1, 2],
                    default: 2,
                },
                Instr::End,
                Instr::I32Const(10),
                Instr::Num(NumOp::I32Add),
                Instr::End,
                Instr::I32Const(100),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
        );
        assert_equiv(&dispatch, 0, &index_cases);

        // 3. Dispatch over empty blocks: each branch exits to a different
        // continuation that returns its own constant.
        //   block $out
        //     block $b1
        //       block $b0
        //         br_table 0 1 2 2 (index)
        //       end
        //       i32.const 100 return
        //     end
        //     i32.const 200 return
        //   end
        //   i32.const 300
        let empty = module_with(
            vec![
                Instr::Block(BlockType::Empty),
                Instr::Block(BlockType::Empty),
                Instr::Block(BlockType::Empty),
                Instr::LocalGet(0),
                Instr::BrTable {
                    targets: vec![0, 1, 2],
                    default: 2,
                },
                Instr::End,
                Instr::I32Const(100),
                Instr::Return,
                Instr::End,
                Instr::I32Const(200),
                Instr::Return,
                Instr::End,
                Instr::I32Const(300),
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
        );
        assert_equiv(&empty, 0, &index_cases);

        // 4. A loop whose exit is a br_table (one case targets the unsealed
        // loop header): sum_{i<n} i, exiting via index 0 when i >= n.
        let sum = module_with(
            vec![
                Instr::Block(BlockType::Empty),
                Instr::Loop(BlockType::Empty),
                Instr::LocalGet(1),
                Instr::LocalGet(2),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(1),
                Instr::LocalGet(2),
                Instr::I32Const(1),
                Instr::Num(NumOp::I32Add),
                Instr::LocalSet(2),
                // i < n ? continue the loop (index 1) : exit (index 0).
                Instr::LocalGet(2),
                Instr::LocalGet(0),
                Instr::Num(NumOp::I32LtS),
                Instr::BrTable {
                    targets: vec![1, 0],
                    default: 0,
                },
                Instr::End,
                Instr::End,
                Instr::LocalGet(1),
            ],
            vec![ValType::I32],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        let n_cases = [0i32, 1, 2, 3, 10, 64]
            .into_iter()
            .map(|n| vec![Value::I32(n)])
            .collect::<Vec<_>>();
        assert_equiv(&sum, 0, &n_cases);
    }

    #[test]
    fn parameterized_and_multi_value_blocks_match_the_interpreter() {
        let i32_to_i32 = FuncType {
            params: vec![ValType::I32],
            results: vec![ValType::I32],
        };
        let two_i32_to_i32 = FuncType {
            params: vec![ValType::I32, ValType::I32],
            results: vec![ValType::I32],
        };
        let empty_to_two_i32 = FuncType {
            params: vec![],
            results: vec![ValType::I32, ValType::I32],
        };

        // block (param i32) (result i32): the parameter is the block's
        // operand-stack input; the body adds 2 to it.
        let block_param = module_with_type(
            vec![
                Instr::LocalGet(0),
                Instr::Block(BlockType::Type(1)),
                Instr::I32Const(2),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![ValType::I32],
            vec![],
            vec![ValType::I32],
            &i32_to_i32,
        );
        let x_cases = [-5i32, -1, 0, 3, 100]
            .into_iter()
            .map(|x| vec![Value::I32(x)])
            .collect::<Vec<_>>();
        assert_equiv(&block_param, 0, &x_cases);

        // block (result i32 i32): multi-value results flow through the
        // continuation's block parameters.
        let block_multi = module_with_type(
            vec![
                Instr::Block(BlockType::Type(1)),
                Instr::I32Const(7),
                Instr::I32Const(9),
                Instr::End,
            ],
            vec![],
            vec![],
            vec![ValType::I32, ValType::I32],
            &empty_to_two_i32,
        );
        assert_equiv(&block_multi, 0, &[vec![]]);

        // if (param i32) (result i32) with an else: the parameter enters both
        // branches (the else restarts from the same value).
        let if_param = module_with_type(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::If(BlockType::Type(1)),
                Instr::I32Const(3),
                Instr::Num(NumOp::I32Mul),
                Instr::Else,
                Instr::I32Const(10),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![ValType::I32, ValType::I32],
            vec![],
            vec![ValType::I32],
            &i32_to_i32,
        );
        let cond_cases = [(1i32, 5i32), (0, 5), (1, -3), (0, -3), (0, 0), (1, 0)]
            .into_iter()
            .map(|(c, x)| vec![Value::I32(c), Value::I32(x)])
            .collect::<Vec<_>>();
        assert_equiv(&if_param, 0, &cond_cases);

        // loop (param i32) (result i32), single pass: the body adds 2 to the
        // parameter and falls out of the loop's end.
        let loop_param = module_with_type(
            vec![
                Instr::I32Const(1),
                Instr::Loop(BlockType::Type(1)),
                Instr::I32Const(2),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![],
            vec![],
            vec![ValType::I32],
            &i32_to_i32,
        );
        assert_equiv(&loop_param, 0, &[vec![]]);

        // loop (param i32 i32) (result i32), single pass adding both params.
        let loop_two_params = module_with_type(
            vec![
                Instr::I32Const(1),
                Instr::I32Const(2),
                Instr::Loop(BlockType::Type(1)),
                Instr::Num(NumOp::I32Add),
                Instr::End,
            ],
            vec![],
            vec![],
            vec![ValType::I32],
            &two_i32_to_i32,
        );
        assert_equiv(&loop_two_params, 0, &[vec![]]);

        // A loop whose br_if back-edge carries the loop parameter: count up by
        // 4 from 1 while below 10, exiting with the value that reached 10.
        //   i32.const 1
        //   loop (param i32) (result i32)
        //     i32.const 4 i32.add local.tee 0
        //     local.get 0 i32.const 10 i32.lt_u br_if 0
        //   end
        let loop_carry = module_with_type(
            vec![
                Instr::I32Const(1),
                Instr::Loop(BlockType::Type(1)),
                Instr::I32Const(4),
                Instr::Num(NumOp::I32Add),
                Instr::LocalTee(0),
                Instr::LocalGet(0),
                Instr::I32Const(10),
                Instr::Num(NumOp::I32LtU),
                Instr::BrIf(0),
                Instr::End,
            ],
            vec![],
            vec![ValType::I32],
            vec![ValType::I32],
            &i32_to_i32,
        );
        assert_equiv(&loop_carry, 0, &[vec![]]);
    }

    #[test]
    fn memory_ops_match_the_interpreter() {
        use crate::instr::LoadOp::*;
        // i32.store then i32.load at the same address (in-bounds and OOB).
        let store_load = mem_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::I32,
                    align: 2,
                    offset: 0,
                },
                Instr::LocalGet(0),
                Instr::Load {
                    memory: 0,
                    op: I32,
                    align: 2,
                    offset: 0,
                },
            ],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        let store_load_cases = [
            (0i32, 42i32),
            (100, -7),
            (65528, i32::MAX),
            (4, i32::MIN),
            // OOB stores/loads trap identically on both paths.
            (65533, 1),
            (0x4000_0000, 5),
        ]
        .into_iter()
        .map(|(addr, value)| vec![Value::I32(addr), Value::I32(value)])
        .collect::<Vec<_>>();
        assert_equiv(&store_load, 0, &store_load_cases);

        // Byte/half stores with sign- and zero-extending loads.
        for (store_op, load_op, values) in [
            (
                StoreOp::I32Store8,
                I32Load8S,
                vec![-1i32, 0, 1, 0x7f, 0x80, 0xff, -0x1234],
            ),
            (
                StoreOp::I32Store8,
                I32Load8U,
                vec![-1i32, 0, 1, 0x7f, 0x80, 0xff, 255],
            ),
            (
                StoreOp::I32Store16,
                I32Load16S,
                vec![-1i32, 0, 1, 0x7fff, 0x8000, 0xffff, -0x1234],
            ),
            (
                StoreOp::I32Store16,
                I32Load16U,
                vec![-1i32, 0, 1, 0x7fff, 0x8000, 0xffff, 0x12345],
            ),
        ] {
            let module = mem_module(
                vec![
                    Instr::I32Const(0),
                    Instr::LocalGet(0),
                    Instr::Store {
                        memory: 0,
                        op: store_op,
                        align: 0,
                        offset: 0,
                    },
                    Instr::I32Const(0),
                    Instr::Load {
                        memory: 0,
                        op: load_op,
                        align: 0,
                        offset: 0,
                    },
                ],
                vec![ValType::I32],
                vec![ValType::I32],
            );
            let cases = values
                .into_iter()
                .map(|value| vec![Value::I32(value)])
                .collect::<Vec<_>>();
            assert_equiv(&module, 0, &cases);
        }

        // 64-bit stores with sign/zero-extending loads.
        for (store_op, load_op, values) in [
            (
                StoreOp::I64,
                I64,
                vec![0i64, 1, -1, 0x0123_4567_89ab_cdef, i64::MAX, i64::MIN],
            ),
            (StoreOp::I64Store8, I64Load8U, vec![-1i64, 0, 255, 0x1ff]),
            (
                StoreOp::I64Store16,
                I64Load16U,
                vec![-1i64, 0, 0xffff, 0x1_ffff],
            ),
            (
                StoreOp::I64Store32,
                I64Load32S,
                vec![-1i64, 0, 0x7fff_ffff, 0x8000_0000, 0x1_0000_0000],
            ),
        ] {
            let module = mem_module(
                vec![
                    Instr::I32Const(0),
                    Instr::LocalGet(0),
                    Instr::Store {
                        memory: 0,
                        op: store_op,
                        align: 0,
                        offset: 0,
                    },
                    Instr::I32Const(0),
                    Instr::Load {
                        memory: 0,
                        op: load_op,
                        align: 0,
                        offset: 0,
                    },
                ],
                vec![ValType::I64],
                vec![ValType::I64],
            );
            let cases = values
                .into_iter()
                .map(|value| vec![Value::I64(value)])
                .collect::<Vec<_>>();
            assert_equiv(&module, 0, &cases);
        }

        // Float stores/loads round-trip the raw bit patterns.
        let f32_module = mem_module(
            vec![
                Instr::I32Const(8),
                Instr::LocalGet(0),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::F32,
                    align: 2,
                    offset: 0,
                },
                Instr::I32Const(8),
                Instr::Load {
                    memory: 0,
                    op: F32,
                    align: 2,
                    offset: 0,
                },
            ],
            vec![ValType::F32],
            vec![ValType::F32],
        );
        assert_equiv(&f32_module, 0, &f32_unary());
        let f64_module = mem_module(
            vec![
                Instr::I32Const(8),
                Instr::LocalGet(0),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::F64,
                    align: 3,
                    offset: 0,
                },
                Instr::I32Const(8),
                Instr::Load {
                    memory: 0,
                    op: F64,
                    align: 3,
                    offset: 0,
                },
            ],
            vec![ValType::F64],
            vec![ValType::F64],
        );
        assert_equiv(&f64_module, 0, &f64_unary());

        // A store with a non-zero offset must land at address + offset, not
        // address: store at (addr, offset 100), then load at addr + 100 with
        // offset 0 — a store that dropped its offset would read 0 here.
        let offset_store = mem_module(
            vec![
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::I32,
                    align: 0,
                    offset: 100,
                },
                Instr::LocalGet(0),
                Instr::I32Const(100),
                Instr::Num(NumOp::I32Add),
                Instr::Load {
                    memory: 0,
                    op: I32,
                    align: 2,
                    offset: 0,
                },
            ],
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        let offset_cases = [
            (0i32, 0x1111_2222i32),
            (100, -1),
            (2048, 7),
            // OOB through the offseted effective address.
            (0x10000, 3),
        ]
        .into_iter()
        .map(|(addr, value)| vec![Value::I32(addr), Value::I32(value)])
        .collect::<Vec<_>>();
        assert_equiv(&offset_store, 0, &offset_cases);

        // A load with a non-zero offset must read address + offset: two
        // stores at offset 0 and offset 100, then the offset-100 load must
        // see the second (and the offset-0 slot keeps the first).
        let offset_load = mem_module(
            vec![
                Instr::I32Const(8),
                Instr::I32Const(0x1111),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::I32,
                    align: 0,
                    offset: 0,
                },
                Instr::I32Const(8),
                Instr::I32Const(0x2222),
                Instr::Store {
                    memory: 0,
                    op: StoreOp::I32,
                    align: 0,
                    offset: 100,
                },
                Instr::I32Const(8),
                Instr::Load {
                    memory: 0,
                    op: I32,
                    align: 0,
                    offset: 100,
                },
            ],
            vec![],
            vec![ValType::I32],
        );
        assert_equiv(&offset_load, 0, &[vec![]]);

        // memory.size reports the 1-page memory on both paths.
        let size_module = mem_module(vec![Instr::MemorySize(0)], vec![], vec![ValType::I32]);
        assert_equiv(&size_module, 0, &[vec![]]);
    }

    #[test]
    fn globals_match_the_interpreter() {
        // An immutable global read.
        let get_only = global_module(
            vec![Instr::GlobalGet(0)],
            vec![],
            vec![ValType::I32],
            vec![i32_global(false, 5)],
        );
        assert_equiv(&get_only, 0, &[vec![]]);

        // Mutable i32/i64 set-then-get inside one call (read-your-write).
        let i32_rw = global_module(
            vec![Instr::LocalGet(0), Instr::GlobalSet(0), Instr::GlobalGet(0)],
            vec![ValType::I32],
            vec![ValType::I32],
            vec![i32_global(true, 0)],
        );
        let i32_cases = [-5i32, 0, 3, i32::MAX, i32::MIN]
            .into_iter()
            .map(|v| vec![Value::I32(v)])
            .collect::<Vec<_>>();
        assert_equiv(&i32_rw, 0, &i32_cases);
        let i64_rw = global_module(
            vec![Instr::LocalGet(0), Instr::GlobalSet(0), Instr::GlobalGet(0)],
            vec![ValType::I64],
            vec![ValType::I64],
            vec![(
                GlobalType {
                    value: ValType::I64,
                    mutable: true,
                },
                vec![Instr::I64Const(0)],
            )],
        );
        let i64_cases = [-1i64, 0, 1, (1 << 40), i64::MAX, i64::MIN]
            .into_iter()
            .map(|v| vec![Value::I64(v)])
            .collect::<Vec<_>>();
        assert_equiv(&i64_rw, 0, &i64_cases);

        // Float globals round-trip their raw bits.
        let f32_rw = global_module(
            vec![Instr::LocalGet(0), Instr::GlobalSet(0), Instr::GlobalGet(0)],
            vec![ValType::F32],
            vec![ValType::F32],
            vec![(
                GlobalType {
                    value: ValType::F32,
                    mutable: true,
                },
                vec![Instr::F32Const(0)],
            )],
        );
        assert_equiv(&f32_rw, 0, &f32_unary());
        let f64_rw = global_module(
            vec![Instr::LocalGet(0), Instr::GlobalSet(0), Instr::GlobalGet(0)],
            vec![ValType::F64],
            vec![ValType::F64],
            vec![(
                GlobalType {
                    value: ValType::F64,
                    mutable: true,
                },
                vec![Instr::F64Const(0)],
            )],
        );
        assert_equiv(&f64_rw, 0, &f64_unary());

        // A global write before a trap must persist (wasm traps do not roll
        // back prior writes): the compiled body sets the global through its
        // caller-owned buffer and only then hits `unreachable`, so the store
        // must still see the write afterwards.
        let setter_getter = Module {
            types: vec![
                SubType::func(vec![ValType::I32], vec![]),
                SubType::func(vec![], vec![ValType::I32]),
            ],
            functions: vec![0, 1],
            bodies: vec![
                FuncBody {
                    locals: vec![],
                    body: vec![Instr::LocalGet(0), Instr::GlobalSet(0), Instr::Unreachable],
                },
                FuncBody {
                    locals: vec![],
                    body: vec![Instr::GlobalGet(0)],
                },
            ],
            globals: vec![Global {
                ty: GlobalType {
                    value: ValType::I32,
                    mutable: true,
                },
                init: vec![Instr::I32Const(0)],
            }],
            ..Module::default()
        };
        let outcomes = run_seq(
            &setter_getter,
            &[
                (0, vec![Value::I32(42)]),
                (1, vec![]),
                (0, vec![Value::I32(-3)]),
                (1, vec![]),
            ],
        );
        assert!(matches!(
            outcomes[0],
            Err(ExecFail::Trap(Trap::Unreachable))
        ));
        assert!(matches!(
            outcomes[2],
            Err(ExecFail::Trap(Trap::Unreachable))
        ));
        assert_eq!(outcomes[1], Ok(vec![Value::I32(42)]));
        assert_eq!(outcomes[3], Ok(vec![Value::I32(-3)]));
    }

    #[test]
    fn memory_grow_matches_the_interpreter() {
        use crate::instr::LoadOp::*;
        // Unbounded 1-page memory. func 0 stores pre-growth data, grows to 2
        // pages, stores on the freshly grown second page and at its last
        // aligned slot (a stale descriptor would write through a freed
        // buffer after the `Vec` realloc), then returns `memory.size`. func 1
        // reads every slot back — cross-call, so the growth must have
        // reached the store.
        let module = Module {
            types: vec![SubType::func(vec![], vec![ValType::I32])],
            functions: vec![0, 0],
            bodies: vec![
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(4),
                        Instr::I32Const(0x1111_1111),
                        Instr::Store {
                            memory: 0,
                            op: StoreOp::I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::I32Const(1),
                        Instr::MemoryGrow(0),
                        Instr::Drop,
                        Instr::I32Const(0x1_0004),
                        Instr::I32Const(0x2222_2222),
                        Instr::Store {
                            memory: 0,
                            op: StoreOp::I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::I32Const(0x1_fffc),
                        Instr::I32Const(0x3333_3333),
                        Instr::Store {
                            memory: 0,
                            op: StoreOp::I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::MemorySize(0),
                    ],
                },
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(4),
                        Instr::Load {
                            memory: 0,
                            op: I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::I32Const(0x1_0004),
                        Instr::Load {
                            memory: 0,
                            op: I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::I32Const(0x1_fffc),
                        Instr::Load {
                            memory: 0,
                            op: I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::Num(NumOp::I32Add),
                        Instr::Num(NumOp::I32Add),
                    ],
                },
            ],
            memories: vec![MemType {
                limits: Limits {
                    min: 1,
                    max: None,
                    shared: false,
                },
                memory64: false,
            }],
            ..Module::default()
        };
        let outcomes = run_seq(&module, &[(0, vec![]), (1, vec![])]);
        assert_eq!(outcomes[0], Ok(vec![Value::I32(2)]));
        assert_eq!(outcomes[1], Ok(vec![Value::I32(0x6666_6666)]));
    }

    #[test]
    fn memory_grow_respects_the_declared_maximum() {
        use crate::instr::LoadOp::*;
        // Memory (1 2): starts 1 page, declared max 2. func 0 grows to the
        // max and returns old-pages × new size (1 × 2). func 1 then tries
        // another grow (must fail with -1) and reports the size (still 2),
        // so the success/failure results and the refreshed length are all
        // pinned. func 2 stores at the newly grown page boundary and reads
        // it back — a stale descriptor after the successful grow would write
        // out of bounds.
        let module = Module {
            types: vec![SubType::func(vec![], vec![ValType::I32])],
            functions: vec![0, 0, 0],
            bodies: vec![
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(1),
                        Instr::MemoryGrow(0),
                        Instr::MemorySize(0),
                        Instr::Num(NumOp::I32Mul),
                    ],
                },
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(1),
                        Instr::MemoryGrow(0),
                        Instr::MemorySize(0),
                        Instr::Num(NumOp::I32Mul),
                    ],
                },
                FuncBody {
                    locals: vec![],
                    body: vec![
                        Instr::I32Const(0x1_0000),
                        Instr::I32Const(7),
                        Instr::Store {
                            memory: 0,
                            op: StoreOp::I32,
                            align: 2,
                            offset: 0,
                        },
                        Instr::I32Const(0x1_0000),
                        Instr::Load {
                            memory: 0,
                            op: I32,
                            align: 2,
                            offset: 0,
                        },
                    ],
                },
            ],
            memories: vec![MemType {
                limits: Limits {
                    min: 1,
                    max: Some(2),
                    shared: false,
                },
                memory64: false,
            }],
            ..Module::default()
        };
        let outcomes = run_seq(&module, &[(0, vec![]), (1, vec![]), (2, vec![])]);
        assert_eq!(outcomes[0], Ok(vec![Value::I32(2)]));
        assert_eq!(outcomes[1], Ok(vec![Value::I32(-2)]));
        assert_eq!(outcomes[2], Ok(vec![Value::I32(7)]));
    }

    #[test]
    fn multi_memory_ops_match_the_interpreter() {
        let two_memories = || {
            vec![
                MemType {
                    limits: Limits {
                        min: 1,
                        max: None,
                        shared: false,
                    },
                    memory64: false,
                },
                MemType {
                    limits: Limits {
                        min: 2,
                        max: None,
                        shared: false,
                    },
                    memory64: false,
                },
            ]
        };
        // Store different values in each memory, then read both back and add:
        // if both loads hit the same descriptor the sum is wrong.
        let module = Module {
            types: vec![SubType::func(
                vec![ValType::I32, ValType::I32],
                vec![ValType::I32],
            )],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![],
                body: vec![
                    Instr::I32Const(0),
                    Instr::LocalGet(0),
                    Instr::Store {
                        memory: 0,
                        op: StoreOp::I32,
                        align: 0,
                        offset: 0,
                    },
                    Instr::I32Const(0),
                    Instr::LocalGet(1),
                    Instr::Store {
                        memory: 1,
                        op: StoreOp::I32,
                        align: 0,
                        offset: 0,
                    },
                    Instr::I32Const(0),
                    Instr::Load {
                        memory: 0,
                        op: LoadOp::I32,
                        align: 2,
                        offset: 0,
                    },
                    Instr::I32Const(0),
                    Instr::Load {
                        memory: 1,
                        op: LoadOp::I32,
                        align: 2,
                        offset: 0,
                    },
                    Instr::Num(NumOp::I32Add),
                ],
            }],
            memories: two_memories(),
            ..Module::default()
        };
        let cases = [(11i32, 22i32), (-1, 7), (i32::MAX, i32::MIN)]
            .into_iter()
            .map(|(a, b)| vec![Value::I32(a), Value::I32(b)])
            .collect::<Vec<_>>();
        assert_equiv(&module, 0, &cases);

        // memory.size is per memory: 1 page + 2 pages.
        let sizes = Module {
            types: vec![SubType::func(vec![], vec![ValType::I32])],
            functions: vec![0],
            bodies: vec![FuncBody {
                locals: vec![],
                body: vec![
                    Instr::MemorySize(0),
                    Instr::MemorySize(1),
                    Instr::Num(NumOp::I32Add),
                ],
            }],
            memories: two_memories(),
            ..Module::default()
        };
        assert_equiv(&sizes, 0, &[vec![]]);
    }
}
