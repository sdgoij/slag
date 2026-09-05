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

use crate::instr::{Instr, LoadOp, NumOp, StoreOp};
use crate::module::{DataMode, ExportKind, ImportDesc, Module};
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

/// The page size of linear memory, in bytes.
pub const PAGE_SIZE: u64 = 65536;

/// A linear memory: growable byte storage plus its declared maximum.
#[derive(Debug)]
pub struct Memory {
    pub bytes: Vec<u8>,
    max_pages: Option<u64>,
}

impl Memory {
    fn pages(&self) -> u64 {
        self.bytes.len() as u64 / PAGE_SIZE
    }

    /// Grow by `delta` pages; returns the old size in pages, or `-1` when the
    /// growth would exceed the limits (a wasm32 memory is capped at 2^16
    /// pages even without a declared maximum).
    fn grow(&mut self, delta: u64) -> i32 {
        let old = self.pages();
        let Some(new) = old.checked_add(delta) else {
            return -1;
        };
        let cap = self.max_pages.unwrap_or(1 << 16);
        if new > cap {
            return -1;
        }
        self.bytes.resize((new * PAGE_SIZE) as usize, 0);
        old as i32
    }
}

/// A module instance. State per instance: globals and linear memories;
/// tables arrive in Cut 5.
pub struct Instance {
    module: Module,
    globals: Vec<Value>,
    memories: Vec<Memory>,
    depth_limit: usize,
}

/// Instantiate a module. Host imports are unsupported in this cut.
pub fn instantiate(module: &Module) -> Result<Instance, ExecFail> {
    if !module.imports.is_empty() {
        return Err(ExecFail::Trap(Trap::UnsupportedImport));
    }
    let mut memories = Vec::with_capacity(module.memories.len());
    for mem in &module.memories {
        if mem.memory64 {
            return Err(ExecFail::Unsupported("memory64 (Cut 8)"));
        }
        let min_bytes = (mem.limits.min * PAGE_SIZE) as usize;
        memories.push(Memory {
            bytes: vec![0; min_bytes],
            max_pages: mem.limits.max,
        });
    }
    let mut instance = Instance {
        module: module.clone(),
        globals: Vec::new(),
        memories,
        depth_limit: DEFAULT_DEPTH_LIMIT,
    };
    for global in &module.globals {
        let value = eval_const(&global.init, &instance.globals)?;
        instance.globals.push(value);
    }
    // Apply active data segments in order.
    for segment in &module.data {
        if let DataMode::Active { memory, offset } = &segment.mode {
            let Some(mem) = instance.memories.get_mut(*memory as usize) else {
                return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
            };
            let offset_value = eval_const(offset, &instance.globals)?;
            let Value::I32(offset) = offset_value else {
                return Err(ExecFail::Unsupported("non-i32 data offset"));
            };
            let start = offset as u32 as usize;
            let end = start.checked_add(segment.bytes.len());
            let Some(end) = end else {
                return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
            };
            if end > mem.bytes.len() {
                return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
            }
            mem.bytes[start..end].copy_from_slice(&segment.bytes);
        }
    }
    // Run the start function, if any.
    if let Some(start) = module.start {
        instance.run_defined(start as usize, &[])?;
    }
    Ok(instance)
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
        self.run_defined(index - imported, args)
    }

    /// Run a module-defined function by its index into `Module::bodies`.
    fn run_defined(&mut self, defined: usize, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
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
            Instr::ReturnCall(index) => {
                self.do_tail_call(index as usize)?;
                Ok(Ctl::Settled)
            }
            Instr::ReturnCallIndirect { .. }
            | Instr::ReturnCallRef(_)
            | Instr::CallIndirect { .. }
            | Instr::CallRef(_) => Err(ExecFail::Unsupported("indirect calls (Cut 5)")),
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
            Instr::Load { op, offset, .. } => {
                let addr = self.pop_i32()? as u32 as u64;
                let size = load_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let Some(mem) = self.inst.memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let len = mem.bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let value = read_mem(op, &mem.bytes[start..start + size]);
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::Store { op, offset, .. } => {
                let value = self.pop()?;
                let addr = self.pop_i32()? as u32 as u64;
                let size = store_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let Some(mem) = self.inst.memories.first_mut() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let len = mem.bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                write_mem(op, value, &mut mem.bytes[start..start + size]);
                Ok(Ctl::Next)
            }
            Instr::MemorySize => {
                let Some(mem) = self.inst.memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                self.stack.push(Value::I32(mem.pages() as i32));
                Ok(Ctl::Next)
            }
            Instr::MemoryGrow => {
                let delta = self.pop_i32()? as u32 as u64;
                let Some(mem) = self.inst.memories.first_mut() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let old = mem.grow(delta);
                self.stack.push(Value::I32(old));
                Ok(Ctl::Next)
            }
            Instr::MemoryInit { data_index: _, .. } | Instr::DataDrop(_) => {
                Err(ExecFail::Unsupported("bulk memory (Cut 5)"))
            }
            Instr::MemoryCopy | Instr::MemoryFill => {
                Err(ExecFail::Unsupported("bulk memory (Cut 5)"))
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
            | Instr::TableInit { .. }
            | Instr::ElemDrop(_)
            | Instr::TableCopy
            | Instr::TableGrow
            | Instr::TableSize
            | Instr::TableFill => Err(ExecFail::Unsupported("tables (Cut 5)")),
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

    /// Make a tail call (`return_call`) from the top frame: pop the callee's
    /// arguments, then replace the top frame in place with the callee's frame
    /// at the same operand-stack base, so the callee's results flow to the
    /// caller of the (now replaced) frame. Everything else above the base —
    /// values owned by the tail frame's not-yet-exited labels — is discarded
    /// when the callee finishes, matching the spec's unwinding of `return_call`.
    fn do_tail_call(&mut self, index: usize) -> Result<(), ExecFail> {
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
        let top = self.frames.len() - 1;
        let base = self.frames[top].base;
        self.frames.pop();
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

/// Bytes an `iN.load*` reads from memory (spec 2.4.5).
fn load_size(op: LoadOp) -> usize {
    use LoadOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Load8S | I32Load8U | I64Load8S | I64Load8U => 1,
        I32Load16S | I32Load16U | I64Load16S | I64Load16U => 2,
        I64Load32S | I64Load32U => 4,
    }
}

/// Bytes an `iN.store*` writes to memory.
fn store_size(op: StoreOp) -> usize {
    use StoreOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Store8 | I64Store8 => 1,
        I32Store16 | I64Store16 => 2,
        I64Store32 => 4,
    }
}

/// Little-endian read of `bytes` as a u64 (the caller slices exactly
/// [`load_size`] bytes; unused high bytes are zero).
fn le_u64(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (shift, &byte) in bytes.iter().enumerate() {
        value |= u64::from(byte) << (8 * shift);
    }
    value
}

/// Interpret the memory slice as the loaded value (with the load's sign or
/// zero extension).
fn read_mem(op: LoadOp, bytes: &[u8]) -> Value {
    use LoadOp::*;
    let raw = le_u64(bytes);
    match op {
        I32 => Value::I32(raw as u32 as i32),
        I64 => Value::I64(raw as i64),
        F32 => Value::F32(raw as u32),
        F64 => Value::F64(raw),
        I32Load8S => Value::I32((raw as u8) as i8 as i32),
        I32Load8U => Value::I32((raw as u8) as i32),
        I32Load16S => Value::I32((raw as u16) as i16 as i32),
        I32Load16U => Value::I32((raw as u16) as i32),
        I64Load8S => Value::I64((raw as u8) as i8 as i64),
        I64Load8U => Value::I64((raw as u8) as i64),
        I64Load16S => Value::I64((raw as u16) as i16 as i64),
        I64Load16U => Value::I64((raw as u16) as i64),
        I64Load32S => Value::I64((raw as u32) as i32 as i64),
        I64Load32U => Value::I64((raw as u32) as i64),
    }
}

/// Store the value's low bits, little-endian, over the memory slice (the
/// caller slices exactly [`store_size`] bytes).
fn write_mem(_op: StoreOp, value: Value, bytes: &mut [u8]) {
    let bits = match value {
        Value::I32(v) => v as u32 as u64,
        Value::I64(v) => v as u64,
        Value::F32(bits) => bits as u64,
        Value::F64(bits) => bits,
    };
    // The caller slices exactly `store_size(_op)` bytes, so writing the low
    // bytes of `bits` little-endian covers every store width.
    for (shift, byte) in bytes.iter_mut().enumerate() {
        *byte = (bits >> (8 * shift)) as u8;
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
