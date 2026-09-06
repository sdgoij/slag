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
//!   (integer arithmetic/comparisons, the float ops below, and
//!   `i64.wrap_i32`), `nop`, `unreachable`, and `return`;
//! - float arithmetic/rounding/sqrt/comparisons reproduce the
//!   interpreter's canonical-quiet-NaN policy exactly (`abs`/`neg`/
//!   `copysign` stay raw bit ops); `f32.min/max` and the
//!   promote/demote conversions are deferred;
//! - structured control flow (`block`/`loop`/`if`/`else`/`br`/`br_if`)
//!   with empty or single-result block types (float or integer).
//! - memory, globals, tables, calls, refs, SIMD, and GC are not lowered yet.
//!
//! ABI: a compiled entry is
//! `unsafe extern "C" fn(args: *const u64, nargs: u64, out: *mut u64,
//! nout: u64) -> i32`. Arguments and results travel as 64-bit bit patterns
//! (a `i32` uses the low 32 bits), so the Rust trampoline and the Cranelift
//! signature can never disagree about float argument classification. The
//! returned `i32` is a trap code (0 = ok); results are written only when the
//! function completes normally, so a trap never produces partial results.

use std::sync::Arc;

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
use crate::instr::{Instr, NumOp};
use crate::module::{FuncBody, Module};
use crate::types::{BlockType, FuncType, ValType};
use crate::values::{QNAN32, QNAN64, Trap, Value};

/// Trap code returned by a compiled entry; 0 means success.
pub const TRAP_NONE: i32 = 0;
pub const TRAP_UNREACHABLE: i32 = 1;
pub const TRAP_INT_DIVIDE_BY_ZERO: i32 = 2;
pub const TRAP_INT_OVERFLOW: i32 = 3;

/// Map a compiled trap code back to the interpreter's [`Trap`].
pub fn trap_of(code: i32) -> Trap {
    match code {
        TRAP_UNREACHABLE => Trap::Unreachable,
        TRAP_INT_DIVIDE_BY_ZERO => Trap::IntegerDivideByZero,
        TRAP_INT_OVERFLOW => Trap::IntegerOverflow,
        _ => Trap::Unreachable,
    }
}

/// The native entry point produced by the compiler.
pub type CompiledEntry =
    unsafe extern "C" fn(args: *const u64, nargs: u64, out: *mut u64, nout: u64) -> i32;

/// One compiled function body: the entry plus the executable-code
/// allocation that must outlive every call into it.
pub struct CompiledFunc {
    _code: ExecutableCode,
    entry: CompiledEntry,
}

impl CompiledFunc {
    /// Invoke the compiled body with `args` (bit patterns) and return the
    /// results (bit patterns) plus the trap code.
    pub fn call(&self, args: &[u64], out: &mut [u64]) -> i32 {
        // SAFETY: `entry` is a plain function pointer into `_code`, which
        // this struct keeps alive and executable for its whole lifetime; the
        // caller passes buffers of the declared lengths.
        unsafe {
            (self.entry)(
                args.as_ptr(),
                args.len() as u64,
                out.as_mut_ptr(),
                out.len() as u64,
            )
        }
    }
}

/// Run a compiled entry with the interpreter's [`Value`] argument model,
/// returning interpreter values or the trapped [`ExecFail`].
pub fn run_compiled(
    func: &CompiledFunc,
    ty: &FuncType,
    args: &[Value],
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
    let code = func.call(&input, &mut output);
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

/// The native-code generation engine: a cached native `TargetIsa`.
struct Engine {
    isa: Arc<dyn TargetIsa>,
}

impl Engine {
    fn new() -> Result<Self, String> {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|e| e.to_string())?;
        let flags = settings::Flags::new(flag_builder);
        let isa = cranelift_native::builder()?
            .finish(flags)
            .map_err(|e| e.to_string())?;
        Ok(Self { isa })
    }

    /// Compile `defined` of `module`, or `None` when the function is outside
    /// the supported subset.
    fn compile(&self, module: &Module, defined: usize) -> Option<CompiledFunc> {
        let body = module.bodies.get(defined)?;
        let type_index = *module.functions.get(defined)?;
        let func_type = module.func_at(type_index)?;
        if !lowerable(func_type, body) {
            return None;
        }
        let conv = platform_call_conv(&*self.isa);
        let mut func = Function::with_name_signature(
            UserFuncName::testcase("wasm_body"),
            entry_signature(conv),
        );
        let mut fctx = FunctionBuilderContext::new();
        lower(body, func_type, &mut func, &mut fctx, &*self.isa).ok()?;
        let mut ctx = Context::for_function(func);
        let compiled = ctx.compile(&*self.isa, &mut ControlPlane::default()).ok()?;
        let code = ExecutableCode::new(compiled.code_buffer()).ok()?;
        // SAFETY: `code.as_ptr()` is an executable allocation that outlives
        // the cast; a data pointer to a function pointer is a plain integer
        // cast on every supported (64-bit) target.
        let entry: CompiledEntry = unsafe { std::mem::transmute(code.as_ptr()) };
        Some(CompiledFunc { _code: code, entry })
    }
}

fn platform_call_conv(isa: &dyn TargetIsa) -> CallConv {
    if isa.triple().operating_system == target_lexicon::OperatingSystem::Windows {
        CallConv::WindowsFastcall
    } else {
        CallConv::SystemV
    }
}

/// `fn(args: *const u64, nargs: u64, out: *mut u64, nout: u64) -> i32`
fn entry_signature(conv: CallConv) -> Signature {
    let mut sig = Signature::new(conv);
    sig.params.push(AbiParam::new(types::I64)); // args pointer
    sig.params.push(AbiParam::new(types::I64)); // nargs
    sig.params.push(AbiParam::new(types::I64)); // out pointer
    sig.params.push(AbiParam::new(types::I64)); // nout
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

/// The result types of a supported block type. Only the parameter-free
/// forms lower today (empty or a single integer result); multi-value and
/// parameterized blocks keep the function on the interpreter.
fn block_results(bt: &BlockType) -> Option<Vec<ValType>> {
    match bt {
        BlockType::Empty => Some(Vec::new()),
        BlockType::Val(ty) if clif_type(*ty).is_some() => Some(vec![*ty]),
        _ => None,
    }
}

/// Whether `func_type` + `body` are inside the current lowering subset.
fn lowerable(func_type: &FuncType, body: &FuncBody) -> bool {
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
        Instr::Block(bt) | Instr::Loop(bt) | Instr::If(bt) => block_results(bt).is_some(),
        Instr::Else | Instr::End | Instr::Br(_) | Instr::BrIf(_) => true,
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
/// Supported block types carry no parameters and at most one result, so a
/// construct never consumes the operand stack on entry: `height` is the
/// stack depth the construct's results (if any) sit on top of. `br` to a
/// block/if label jumps to `after` carrying the results; `br` to a loop
/// label jumps back to its (sealed) `header` carrying nothing.
struct CtlFrame {
    kind: CtlKind,
    /// Loop header (the `br` target for a loop label); sealed on entry so
    /// body back-edges are legal.
    header: Option<Block>,
    /// The continuation after the construct; receives the results.
    after: Block,
    /// The false-condition branch of an `if`.
    else_block: Option<Block>,
    height: usize,
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
    /// `locals` and `stack` entry metadata mirror the interpreter's model.
    variables: Vec<Variable>,
    stack: Vec<ClifValue>,
    out_ptr: ClifValue,
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
            | I32Xor | I32Shl | I32ShrS | I32ShrU | I32Rotl | I32Rotr | I32Popcnt | I32Eqz
            | I32WrapI64 => types::I32,
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

    fn open_block(&mut self, results: Vec<ValType>) {
        let after = self.builder.create_block();
        for ty in &results {
            self.builder
                .append_block_param(after, clif_type(*ty).expect("int block result"));
        }
        self.controls.push(CtlFrame {
            kind: CtlKind::Block,
            header: None,
            after,
            else_block: None,
            height: self.stack.len(),
            results: results.len(),
            in_else: false,
            has_else: false,
            after_used: false,
        });
    }

    fn open_loop(&mut self, results: Vec<ValType>) {
        let header = self.builder.create_block();
        let after = self.builder.create_block();
        for ty in &results {
            self.builder
                .append_block_param(after, clif_type(*ty).expect("int block result"));
        }
        self.builder.ins().jump(header, &[]);
        self.builder.switch_to_block(header);
        // The header is NOT sealed here: every back-edge (`br` to this loop
        // label) is a new predecessor, which may only target an unsealed
        // block. The header is sealed when the loop's `end` closes it, once
        // all of its predecessors are declared.
        self.controls.push(CtlFrame {
            kind: CtlKind::Loop,
            header: Some(header),
            after,
            else_block: None,
            height: self.stack.len(),
            results: results.len(),
            in_else: false,
            has_else: false,
            after_used: false,
        });
    }

    fn open_if(&mut self, results: Vec<ValType>) -> Result<(), String> {
        let condition = self.pop().ok_or("operand stack underflow")?;
        let height = self.stack.len();
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let after = self.builder.create_block();
        for ty in &results {
            self.builder
                .append_block_param(after, clif_type(*ty).expect("int block result"));
        }
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
        let (height, else_block) = {
            let f = &self.controls[idx];
            (f.height, f.else_block.expect("if else block"))
        };
        self.controls[idx].in_else = true;
        self.controls[idx].has_else = true;
        self.stack.truncate(height);
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
                    // (the false path) jumps straight to the continuation.
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

    /// `br`: an unconditional jump to a label, carrying the label's payload
    /// (the target block's results; nothing for a loop).
    fn do_br(&mut self, depth: u32) -> Result<(), String> {
        let idx = self
            .controls
            .len()
            .checked_sub(1 + depth as usize)
            .ok_or("branch past the control stack")?;
        let (kind, header, after, r) = {
            let f = &self.controls[idx];
            (
                f.kind,
                f.header,
                f.after,
                if f.kind == CtlKind::Loop {
                    0
                } else {
                    f.results
                },
            )
        };
        let payload = self.label_args(r);
        let target = match kind {
            CtlKind::Loop => header.ok_or("loop without a header")?,
            _ => {
                self.controls[idx].after_used = true;
                after
            }
        };
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
        let (kind, header, after, r) = {
            let f = &self.controls[idx];
            (
                f.kind,
                f.header,
                f.after,
                if f.kind == CtlKind::Loop {
                    0
                } else {
                    f.results
                },
            )
        };
        let payload = self.label_args(r);
        let cont = self.builder.create_block();
        match kind {
            CtlKind::Loop => {
                let header = header.ok_or("loop without a header")?;
                self.builder.ins().brif(flag, header, &[], cont, &[]);
            }
            _ => {
                self.controls[idx].after_used = true;
                self.builder.ins().brif(flag, after, &payload, cont, &[]);
            }
        }
        self.builder.switch_to_block(cont);
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
            Instr::Block(bt) => {
                let results = block_results(bt).ok_or("unsupported block type")?;
                self.open_block(results);
            }
            Instr::Loop(bt) => {
                let results = block_results(bt).ok_or("unsupported block type")?;
                self.open_loop(results);
            }
            Instr::If(bt) => {
                let results = block_results(bt).ok_or("unsupported block type")?;
                self.open_if(results)?;
            }
            Instr::Else => self.open_else()?,
            Instr::End => self.close_construct()?,
            Instr::Br(depth) => self.do_br(*depth)?,
            Instr::BrIf(depth) => self.do_br_if(*depth)?,
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

/// Lower `body` into `func`. Entry params: `(args, nargs, out, nout)`, all
/// pointers/counts as `I64`; returns the trap code `I32`.
fn lower(
    body: &FuncBody,
    func_type: &FuncType,
    func: &mut Function,
    fctx: &mut FunctionBuilderContext,
    isa: &dyn TargetIsa,
) -> Result<(), String> {
    let mut builder = FunctionBuilder::new(func, fctx);
    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let params = builder.block_params(entry_block).to_vec();
    let (args_ptr, out_ptr) = (params[0], params[2]);

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
        variables,
        stack: Vec::new(),
        out_ptr,
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
    use crate::module::{FuncBody, Module};
    use crate::types::{BlockType, SubType};

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
        for op in [I32Popcnt, I32Eqz] {
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
        for (op, result) in [(I64Popcnt, ValType::I64), (I64Eqz, ValType::I32)] {
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
}
