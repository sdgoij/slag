//! Wasm → native compilation (Cut 11, Wave 0/1).
//!
//! An optional Cranelift backend for the `crates/wasm` interpreter. The
//! interpreter stays the correctness oracle; this module lowers a subset of
//! module-defined leaf functions to native code and hands the store a
//! compiled entry it can invoke instead of running the `Engine` loop.
//!
//! Supported subset (everything else bails to the interpreter, per
//! function):
//! - function signatures and locals over `i32`/`i64` only;
//! - straight-line bodies: `const`, `local.get/set/tee`, `drop`, `select`,
//!   integer `Num` ops (no floats, no conversions beyond `i64.wrap_i32`),
//!   `nop`, `unreachable`, and `return`;
//! - structured control flow (`block`/`loop`/`if`/`br*`), memory, globals,
//!   tables, calls, refs, SIMD, and GC are not lowered yet.
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
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, MemFlagsData, Signature, Type, UserFuncName,
    Value as ClifValue, types,
};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_control::ControlPlane;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};

use crate::exec::ExecFail;
use crate::instr::{Instr, NumOp};
use crate::module::{FuncBody, Module};
use crate::types::{FuncType, ValType};
use crate::values::{Trap, Value};

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
        | Instr::LocalGet(_)
        | Instr::LocalSet(_)
        | Instr::LocalTee(_)
        | Instr::Drop
        | Instr::Select
        | Instr::SelectTyped(_)
        | Instr::Return => true,
        Instr::Num(op) => supported_num(*op),
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

/// A lowering context: locals, the operand stack, and the entry pointers.
struct Lowerer<'a> {
    builder: FunctionBuilder<'a>,
    /// `locals` and `stack` entry metadata mirror the interpreter's model.
    variables: Vec<Variable>,
    stack: Vec<ClifValue>,
    out_ptr: ClifValue,
    results: Vec<ValType>,
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
            // Load the i-th param (an I64 slot) and narrow i32 params.
            let address = builder.ins().iadd_imm_s(args_ptr, 8 * index as i64);
            let wide =
                builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), address, Offset32::new(0));
            match ty {
                ValType::I32 => builder.ins().ireduce(types::I32, wide),
                _ => wide,
            }
        } else {
            match ty {
                ValType::I32 => builder.ins().iconst(types::I32, 0),
                _ => builder.ins().iconst(types::I64, 0),
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
    };
    let mut dead = false;
    for instr in &body.body {
        if dead {
            continue;
        }
        match instr {
            Instr::Nop => {}
            Instr::I32Const(value) => {
                let v = lowerer.iconst(types::I32, i64::from(*value));
                lowerer.stack.push(v);
            }
            Instr::I64Const(value) => {
                let v = lowerer.iconst(types::I64, *value);
                lowerer.stack.push(v);
            }
            Instr::LocalGet(index) => {
                let variable = lowerer.variables[*index as usize];
                let value = lowerer.builder.use_var(variable);
                lowerer.stack.push(value);
            }
            Instr::LocalSet(index) => {
                let value = lowerer.pop().ok_or("operand stack underflow")?;
                let variable = lowerer.variables[*index as usize];
                lowerer.builder.def_var(variable, value);
            }
            Instr::LocalTee(index) => {
                let value = lowerer.pop().ok_or("operand stack underflow")?;
                let variable = lowerer.variables[*index as usize];
                lowerer.builder.def_var(variable, value);
                lowerer.stack.push(value);
            }
            Instr::Drop => {
                lowerer.pop().ok_or("operand stack underflow")?;
            }
            Instr::Select | Instr::SelectTyped(_) => {
                let condition = lowerer.pop().ok_or("operand stack underflow")?;
                let b = lowerer.pop().ok_or("operand stack underflow")?;
                let a = lowerer.pop().ok_or("operand stack underflow")?;
                let zero = lowerer.iconst(types::I32, 0);
                let flag = lowerer.builder.ins().icmp(IntCC::NotEqual, condition, zero);
                let value = lowerer.builder.ins().select(flag, a, b);
                lowerer.stack.push(value);
            }
            Instr::Num(op) => lowerer.lower_num(*op)?,
            Instr::Return => {
                lowerer.emit_return()?;
                dead = true;
            }
            Instr::Unreachable => {
                lowerer.emit_trap(TRAP_UNREACHABLE);
                dead = true;
            }
            _ => return Err("unsupported instruction reached the lowerer".to_string()),
        }
    }
    if !dead {
        lowerer.emit_return()?;
    }
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
    use crate::types::SubType;

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
            I64Shl, I64ShrS, I64ShrU, I64Rotl, I64Rotr, I64Eq, I64Ne, I64LtS, I64LtU, I64GtS,
            I64GtU, I64LeS, I64LeU, I64GeS, I64GeU,
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
        // i64.popcnt and i64.eqz over the wide values.
        let wide = i64_pairs()
            .into_iter()
            .map(|pair| vec![pair[0]])
            .collect::<Vec<_>>();
        for op in [I64Popcnt, I64Eqz] {
            let module = int_module(
                vec![Instr::LocalGet(0), Instr::Num(op)],
                vec![ValType::I64],
                vec![ValType::I32],
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
}
