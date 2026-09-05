//! The wasm execution machine (spec ch. 4) — Cut 3 scope.
//!
//! One shared operand stack plus an explicit stack of function frames (each
//! with its own pc, locals, and control labels), so call depth is bounded by
//! a configured limit instead of the host stack. Numeric semantics live in
//! [`crate::values`].
//!
//! Not executed yet (reported as [`ExecFail::Unsupported`]): host imports,
//! memory/table instructions, reference-typed values, bulk-memory
//! instructions, and exceptions — those land in later cuts. Internal `call`s
//! and numeric `global.get/set` work.

use crate::instr::{Instr, NumOp};
use crate::module::{ExportKind, ImportDesc, Module};
use crate::types::{BlockType, ValType};
use crate::values::{Trap, Value, exec_num};

/// How execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecFail {
    Trap(Trap),
    /// A feature this cut does not execute yet (later cuts).
    Unsupported(&'static str),
}

impl From<Trap> for ExecFail {
    fn from(trap: Trap) -> Self {
        ExecFail::Trap(trap)
    }
}

/// Default call-depth budget (the corpus's `assert_exhaustion` expects a
/// bounded stack).
pub const DEFAULT_DEPTH_LIMIT: usize = 4096;

/// A module instance. Cut 3 state is the globals only; memories/tables are
/// allocated in later cuts.
pub struct Instance {
    module: Module,
    globals: Vec<Value>,
    depth_limit: usize,
}

/// Instantiate a module. Host imports and reference-typed globals are
/// unsupported in this cut.
pub fn instantiate(module: &Module) -> Result<Instance, ExecFail> {
    if !module.imports.is_empty() {
        return Err(ExecFail::Trap(Trap::UnsupportedImport));
    }
    let mut globals = Vec::with_capacity(module.globals.len());
    for global in &module.globals {
        globals.push(eval_const(&global.init, &globals)?);
    }
    Ok(Instance {
        module: module.clone(),
        globals,
        depth_limit: DEFAULT_DEPTH_LIMIT,
    })
}

fn imported_func_count(module: &Module) -> usize {
    module
        .imports
        .iter()
        .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
        .count()
}

impl Instance {
    pub fn with_depth_limit(mut self, limit: usize) -> Self {
        self.depth_limit = limit;
        self
    }

    /// The full index-space index of exported function `name`, if present.
    pub fn exported_func(&self, name: &str) -> Option<usize> {
        self.module.exports.iter().find_map(|export| {
            (export.name == name && export.kind == ExportKind::Func)
                .then_some(export.index as usize)
        })
    }

    /// Invoke function `index` (full index space, imports first) with `args`.
    pub fn invoke(&mut self, index: usize, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
        let imported = imported_func_count(&self.module);
        if index < imported {
            return Err(ExecFail::Trap(Trap::UnsupportedImport));
        }
        let defined = index - imported;
        let body = self
            .module
            .bodies
            .get(defined)
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let type_index = self
            .module
            .functions
            .get(defined)
            .copied()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let signature = self
            .module
            .types
            .get(type_index as usize)
            .cloned()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let mut locals = args.to_vec();
        for ty in &body.locals {
            locals.push(default_value(*ty)?);
        }
        let mut engine = Engine {
            inst: self,
            stack: Vec::new(),
            frames: vec![Frame {
                pc: 0,
                locals,
                base: 0,
                results: signature.results.len(),
                body_index: defined,
                labels: vec![Label {
                    arity: signature.results.len(),
                    height: 0,
                    is_loop: false,
                    open: 0,
                }],
            }],
        };
        engine.run()
    }
}

/// A function frame on the machine's call stack.
struct Frame {
    pc: usize,
    locals: Vec<Value>,
    /// Operand-stack height when this frame's body started (arguments
    /// popped; results are produced above this line).
    base: usize,
    /// Number of result values this function produces.
    results: usize,
    /// Defined-function index into `Module::bodies`.
    body_index: usize,
    /// Control labels of the current function (the function label is the
    /// bottom-most entry).
    labels: Vec<Label>,
}

#[derive(Clone, Copy)]
struct Label {
    /// Values a branch to this label carries (results for block/if/func,
    /// parameters for loop).
    arity: usize,
    /// Operand-stack height below this construct's inputs.
    height: usize,
    /// Whether branching to this label restarts the body (loop) or exits it.
    is_loop: bool,
    /// Instruction index of the opening `block`/`loop`/`if`.
    open: usize,
}

struct Engine<'a> {
    inst: &'a mut Instance,
    stack: Vec<Value>,
    frames: Vec<Frame>,
}

/// What the next engine iteration must do.
enum Ctl {
    /// Advance the top frame's pc by one.
    Next,
    /// The instruction already set the pc / pushed or popped a frame.
    Settled,
    /// The whole invocation finished.
    Finished(Vec<Value>),
}

/// Per-body maps: the matching `End` for each opening structured
/// instruction, and the `Else` (if any) for each `if`.
struct BodyMap {
    end: Vec<usize>,
    else_: Vec<Option<usize>>,
}

fn precompute(body: &[Instr]) -> BodyMap {
    let mut end = vec![usize::MAX; body.len()];
    let mut else_ = vec![None; body.len()];
    let mut opens: Vec<usize> = Vec::new();
    for (pc, instr) in body.iter().enumerate() {
        match instr {
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) => opens.push(pc),
            Instr::Else => {
                if let Some(&open) = opens.last() {
                    else_[open] = Some(pc);
                }
            }
            Instr::End => {
                if let Some(open) = opens.pop() {
                    end[open] = pc;
                }
            }
            _ => {}
        }
    }
    BodyMap { end, else_ }
}

impl<'a> Engine<'a> {
    fn body(&self, frame_index: usize) -> &[Instr] {
        &self.inst.module.bodies[self.frames[frame_index].body_index].body
    }

    fn pop(&mut self) -> Result<Value, ExecFail> {
        self.stack.pop().ok_or(ExecFail::Trap(Trap::Unreachable))
    }

    fn pop_i32(&mut self) -> Result<i32, ExecFail> {
        match self.pop()? {
            Value::I32(v) => Ok(v),
            _ => Err(ExecFail::Trap(Trap::Unreachable)),
        }
    }

    /// Run until the outermost frame returns.
    fn run(&mut self) -> Result<Vec<Value>, ExecFail> {
        loop {
            match self.step()? {
                Ctl::Next => {
                    let top = self.frames.len() - 1;
                    self.frames[top].pc += 1;
                }
                Ctl::Settled => {}
                Ctl::Finished(results) => return Ok(results),
            }
        }
    }

    /// Execute one instruction of the top frame.
    fn step(&mut self) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        if self.frames[frame_index].pc >= self.body(frame_index).len() {
            // Fall off the end of the function: return its results.
            return self.finish();
        }
        let pc = self.frames[frame_index].pc;
        let instr = self.body(frame_index)[pc].clone();
        match instr {
            Instr::Unreachable => Err(ExecFail::Trap(Trap::Unreachable)),
            Instr::Nop => Ok(Ctl::Next),
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) => {
                self.enter_structured(pc)?;
                Ok(Ctl::Settled)
            }
            Instr::Else => {
                // Only reached by falling out of `then`: exit the `if`.
                self.exit_if()?;
                Ok(Ctl::Settled)
            }
            Instr::End => {
                self.frames[frame_index].labels.pop();
                Ok(Ctl::Next)
            }
            Instr::Br(label) => {
                if self.branch_to(label as usize)? {
                    return self.finish();
                }
                Ok(Ctl::Settled)
            }
            Instr::BrIf(label) => {
                if self.pop_i32()? != 0 {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    Ok(Ctl::Next)
                }
            }
            Instr::BrTable { targets, default } => {
                let index = self.pop_i32()?;
                let target = if index >= 0 && (index as usize) < targets.len() {
                    targets[index as usize]
                } else {
                    default
                };
                if self.branch_to(target as usize)? {
                    return self.finish();
                }
                Ok(Ctl::Settled)
            }
            Instr::Return => self.finish(),
            Instr::Call(index) => {
                self.do_call(index as usize)?;
                Ok(Ctl::Settled)
            }
            Instr::ReturnCall(_)
            | Instr::ReturnCallIndirect { .. }
            | Instr::ReturnCallRef(_)
            | Instr::CallIndirect { .. }
            | Instr::CallRef(_) => Err(ExecFail::Unsupported("indirect/tail calls (Cuts 4-5)")),
            Instr::Drop => {
                self.pop()?;
                Ok(Ctl::Next)
            }
            Instr::Select | Instr::SelectTyped(_) => {
                let cond = self.pop_i32()?;
                let b = self.pop()?;
                let a = self.pop()?;
                self.stack.push(if cond != 0 { a } else { b });
                Ok(Ctl::Next)
            }
            Instr::LocalGet(index) => {
                let value = self.frames[frame_index].locals[index as usize];
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::LocalSet(index) => {
                let value = self.pop()?;
                self.frames[frame_index].locals[index as usize] = value;
                Ok(Ctl::Next)
            }
            Instr::LocalTee(index) => {
                let value = self.pop()?;
                self.frames[frame_index].locals[index as usize] = value;
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::GlobalGet(index) => {
                let value = self.inst.globals[index as usize];
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::GlobalSet(index) => {
                let value = self.pop()?;
                self.inst.globals[index as usize] = value;
                Ok(Ctl::Next)
            }
            Instr::I32Const(v) => {
                self.stack.push(Value::I32(v));
                Ok(Ctl::Next)
            }
            Instr::I64Const(v) => {
                self.stack.push(Value::I64(v));
                Ok(Ctl::Next)
            }
            Instr::F32Const(bits) => {
                self.stack.push(Value::F32(bits));
                Ok(Ctl::Next)
            }
            Instr::F64Const(bits) => {
                self.stack.push(Value::F64(bits));
                Ok(Ctl::Next)
            }
            Instr::Num(op) => {
                let inputs = num_inputs(op);
                let mut operands = Vec::with_capacity(inputs);
                for _ in 0..inputs {
                    operands.push(self.pop()?);
                }
                operands.reverse();
                self.stack.push(exec_num(op, &operands)?);
                Ok(Ctl::Next)
            }
            Instr::RefNull(_)
            | Instr::RefIsNull
            | Instr::RefFunc(_)
            | Instr::RefEq
            | Instr::RefAsNonNull
            | Instr::BrOnNull(_)
            | Instr::BrOnNonNull(_) => Err(ExecFail::Unsupported("reference values (Cut 5)")),
            Instr::TableGet(_)
            | Instr::TableSet(_)
            | Instr::Load { .. }
            | Instr::Store { .. }
            | Instr::MemorySize
            | Instr::MemoryGrow
            | Instr::MemoryInit { .. }
            | Instr::DataDrop(_)
            | Instr::MemoryCopy
            | Instr::MemoryFill
            | Instr::TableInit { .. }
            | Instr::ElemDrop(_)
            | Instr::TableCopy
            | Instr::TableGrow
            | Instr::TableSize
            | Instr::TableFill => Err(ExecFail::Unsupported("memory/table (Cuts 4-5)")),
        }
    }

    fn advance(&mut self, next: usize) {
        let frame_index = self.frames.len() - 1;
        self.frames[frame_index].pc = next;
    }

    /// Enter a `block`/`loop`/`if` at `pc` (which is the top frame's pc).
    fn enter_structured(&mut self, pc: usize) -> Result<(), ExecFail> {
        let frame_index = self.frames.len() - 1;
        let instr = self.body(frame_index)[pc].clone();
        let (params, arity) = self.block_counts(&instr)?;
        let height = self.stack.len().saturating_sub(params);
        match instr {
            Instr::Block(_) => {
                self.frames[frame_index].labels.push(Label {
                    arity,
                    height,
                    is_loop: false,
                    open: pc,
                });
                self.advance(pc + 1);
            }
            Instr::Loop(_) => {
                self.frames[frame_index].labels.push(Label {
                    arity: params,
                    height,
                    is_loop: true,
                    open: pc,
                });
                self.advance(pc + 1);
            }
            Instr::If(_) => {
                let cond = self.pop_i32()?;
                let map = self.map_for(frame_index);
                if cond != 0 {
                    self.frames[frame_index].labels.push(Label {
                        arity,
                        height,
                        is_loop: false,
                        open: pc,
                    });
                    self.advance(pc + 1);
                } else if let Some(else_pc) = map.else_[pc] {
                    self.frames[frame_index].labels.push(Label {
                        arity,
                        height,
                        is_loop: false,
                        open: pc,
                    });
                    self.advance(else_pc + 1);
                } else {
                    // No else, condition false: skip the whole `if`.
                    self.advance(map.end[pc] + 1);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn exit_if(&mut self) -> Result<(), ExecFail> {
        let frame_index = self.frames.len() - 1;
        let label = self.frames[frame_index]
            .labels
            .pop()
            .ok_or(ExecFail::Trap(Trap::Unreachable))?;
        let end = self.map_for(frame_index).end[label.open];
        self.advance(end + 1);
        Ok(())
    }

    /// Branch to `label` in the top frame. Returns `true` when the target is
    /// the function label (i.e. the function must return).
    fn branch_to(&mut self, label: usize) -> Result<bool, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let (pos, is_loop, arity, height, open, is_func) = {
            let frame = &self.frames[frame_index];
            let count = frame.labels.len();
            let Some(pos) = count.checked_sub(1 + label) else {
                return Err(ExecFail::Trap(Trap::UnknownFunction));
            };
            let target = frame.labels[pos];
            (
                pos,
                target.is_loop,
                target.arity,
                target.height,
                target.open,
                pos == 0,
            )
        };
        let values = self.take_top(arity);
        self.stack.truncate(height);
        self.stack.extend(values);
        if is_func {
            return Ok(true);
        }
        let map = self.map_for(frame_index);
        let frame = &mut self.frames[frame_index];
        if is_loop {
            frame.labels.truncate(pos + 1);
            frame.pc = open + 1;
        } else {
            frame.labels.truncate(pos);
            frame.pc = map.end[open] + 1;
        }
        Ok(false)
    }

    fn map_for(&self, frame_index: usize) -> BodyMap {
        precompute(self.body(frame_index))
    }

    fn block_counts(&self, instr: &Instr) -> Result<(usize, usize), ExecFail> {
        let bt = match instr {
            Instr::Block(bt) | Instr::Loop(bt) | Instr::If(bt) => *bt,
            _ => return Ok((0, 0)),
        };
        match bt {
            BlockType::Empty => Ok((0, 0)),
            BlockType::Val(_) => Ok((0, 1)),
            BlockType::Type(index) => {
                let ty = self
                    .inst
                    .module
                    .types
                    .get(index as usize)
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                Ok((ty.params.len(), ty.results.len()))
            }
        }
    }

    /// Make a (direct, non-tail) call from the top frame.
    fn do_call(&mut self, index: usize) -> Result<(), ExecFail> {
        if self.frames.len() + 1 > self.inst.depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let caller = self.frames.len() - 1;
        let imported = imported_func_count(&self.inst.module);
        if index < imported {
            return Err(ExecFail::Trap(Trap::UnsupportedImport));
        }
        let defined = index - imported;
        let (param_count, result_count, declared) = {
            let module = &self.inst.module;
            let body = module
                .bodies
                .get(defined)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let type_index = *module
                .functions
                .get(defined)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let signature = module
                .types
                .get(type_index as usize)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            (
                signature.params.len(),
                signature.results.len(),
                body.locals.clone(),
            )
        };
        // Pop arguments (last parameter sits on top), in parameter order.
        let mut args = Vec::with_capacity(param_count);
        for _ in 0..param_count {
            args.push(self.pop()?);
        }
        args.reverse();
        let mut locals = args;
        for ty in &declared {
            locals.push(default_value(*ty)?);
        }
        let base = self.stack.len();
        self.frames[caller].pc += 1; // resume after the call
        self.frames.push(Frame {
            pc: 0,
            locals,
            base,
            results: result_count,
            body_index: defined,
            labels: vec![Label {
                arity: result_count,
                height: base,
                is_loop: false,
                open: 0,
            }],
        });
        Ok(())
    }

    /// Pop the top frame, transferring its results to its caller. Returns the
    /// results when the popped frame was the outermost one.
    fn finish_top(&mut self) -> Result<Option<Vec<Value>>, ExecFail> {
        let frame = self
            .frames
            .pop()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let results = self.take_top(frame.results);
        if self.frames.is_empty() {
            return Ok(Some(results));
        }
        self.stack.truncate(frame.base);
        self.stack.extend(results);
        Ok(None)
    }

    /// `finish_top` wrapped into a [`Ctl`].
    fn finish(&mut self) -> Result<Ctl, ExecFail> {
        match self.finish_top()? {
            Some(results) => Ok(Ctl::Finished(results)),
            None => Ok(Ctl::Settled),
        }
    }

    fn take_top(&mut self, count: usize) -> Vec<Value> {
        let len = self.stack.len();
        let start = len.saturating_sub(count);
        let values = self.stack[start..].to_vec();
        self.stack.truncate(start);
        values
    }
}

fn default_value(ty: ValType) -> Result<Value, ExecFail> {
    Ok(match ty {
        ValType::I32 => Value::I32(0),
        ValType::I64 => Value::I64(0),
        ValType::F32 => Value::F32(0),
        ValType::F64 => Value::F64(0),
        ValType::V128 => return Err(ExecFail::Unsupported("v128 (Cut 7)")),
        ValType::Ref(_) => return Err(ExecFail::Unsupported("reference values (Cut 5)")),
    })
}

fn num_inputs(op: NumOp) -> usize {
    use NumOp::*;
    match op {
        I32Eqz | I64Eqz | I32Clz | I32Ctz | I32Popcnt | I64Clz | I64Ctz | I64Popcnt | F32Abs
        | F32Neg | F32Ceil | F32Floor | F32Trunc | F32Nearest | F32Sqrt | F64Abs | F64Neg
        | F64Ceil | F64Floor | F64Trunc | F64Nearest | F64Sqrt | I32WrapI64 | I32TruncF32S
        | I32TruncF32U | I32TruncF64S | I32TruncF64U | I64ExtendI32S | I64ExtendI32U
        | I64TruncF32S | I64TruncF32U | I64TruncF64S | I64TruncF64U | F32ConvertI32S
        | F32ConvertI32U | F32ConvertI64S | F32ConvertI64U | F32DemoteF64 | F64ConvertI32S
        | F64ConvertI32U | F64ConvertI64S | F64ConvertI64U | F64PromoteF32 | I32ReinterpretF32
        | I64ReinterpretF64 | F32ReinterpretI32 | F64ReinterpretI64 | I32Extend8S
        | I32Extend16S | I64Extend8S | I64Extend16S | I64Extend32S | I32TruncSatF32S
        | I32TruncSatF32U | I32TruncSatF64S | I32TruncSatF64U | I64TruncSatF32S
        | I64TruncSatF32U | I64TruncSatF64S | I64TruncSatF64U => 1,
        _ => 2,
    }
}

/// Evaluate a constant initializer expression (no terminating `end`).
fn eval_const(expr: &[Instr], globals: &[Value]) -> Result<Value, ExecFail> {
    let mut stack: Vec<Value> = Vec::new();
    for instr in expr {
        match instr {
            Instr::I32Const(v) => stack.push(Value::I32(*v)),
            Instr::I64Const(v) => stack.push(Value::I64(*v)),
            Instr::F32Const(bits) => stack.push(Value::F32(*bits)),
            Instr::F64Const(bits) => stack.push(Value::F64(*bits)),
            Instr::GlobalGet(index) => {
                let value = *globals
                    .get(*index as usize)
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                stack.push(value);
            }
            Instr::Num(op) => {
                let inputs = num_inputs(*op);
                let mut operands = Vec::with_capacity(inputs);
                for _ in 0..inputs {
                    operands.push(
                        stack
                            .pop()
                            .ok_or(ExecFail::Unsupported("malformed constant expression"))?,
                    );
                }
                operands.reverse();
                stack.push(exec_num(*op, &operands)?);
            }
            Instr::RefNull(_) | Instr::RefFunc(_) => {
                return Err(ExecFail::Unsupported("reference-typed globals (Cut 5)"));
            }
            _ => return Err(ExecFail::Unsupported("constant expression")),
        }
    }
    stack
        .pop()
        .ok_or(ExecFail::Unsupported("empty constant expression"))
}
