//! The wasm execution machine (spec ch. 4) over a [`Store`] of module
//! instances (spec ch. 4.2).
//!
//! Instances share the store's mutable state: globals and linear memories are
//! cells in store-owned pools, so a module that imports another instance's
//! memory or mutable global aliases the same cell. Function imports bind to a
//! flattened target (a defined body in some instance, or a host function).
//!
//! One shared operand stack plus an explicit stack of function frames (each
//! with its own instance, pc, locals, and control labels) keeps call depth
//! bounded by a configured limit instead of the host stack. Numeric semantics
//! live in [`crate::values`].
//!
//! Not executed yet ([`ExecFail::Unsupported`]): table instructions and
//! reference-typed values (Cut 5), bulk memory (Cut 5), exceptions (Cut 6),
//! SIMD (Cut 7), and memory64 (Cut 8).

use crate::instr::{Instr, LoadOp, NumOp, StoreOp};
use crate::module::{DataMode, ExportKind, ImportDesc, Module};
use crate::types::{BlockType, FuncType, GlobalType, Limits, MemType, ValType};
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

/// How instantiation stopped, split by the .wast command that asserts it:
/// [`InstantiateError::Unlinkable`] (import resolution/type failure),
/// [`InstantiateError::Trap`] (a trap during instantiation), or an
/// unsupported feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstantiateError {
    Unlinkable(&'static str),
    Trap(Trap),
    Unsupported(&'static str),
}

impl From<Trap> for InstantiateError {
    fn from(trap: Trap) -> Self {
        InstantiateError::Trap(trap)
    }
}

impl From<ExecFail> for InstantiateError {
    fn from(fail: ExecFail) -> Self {
        match fail {
            ExecFail::Trap(trap) => InstantiateError::Trap(trap),
            ExecFail::Unsupported(reason) => InstantiateError::Unsupported(reason),
        }
    }
}

impl std::fmt::Display for InstantiateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstantiateError::Unlinkable(reason) => write!(f, "link error: {reason}"),
            InstantiateError::Trap(trap) => write!(f, "trap: {trap:?}"),
            InstantiateError::Unsupported(reason) => write!(f, "unsupported: {reason}"),
        }
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
    /// A zero-filled memory of `min_pages` pages, capped at `max_pages`.
    pub fn new(min_pages: u64, max_pages: Option<u64>) -> Self {
        Memory {
            bytes: vec![0; (min_pages * PAGE_SIZE) as usize],
            max_pages,
        }
    }

    pub fn pages(&self) -> u64 {
        self.bytes.len() as u64 / PAGE_SIZE
    }

    /// Grow by `delta` pages; returns the old size in pages, or `-1` when the
    /// growth would exceed the limits (a wasm32 memory is capped at 2^16
    /// pages even without a declared maximum).
    pub fn grow(&mut self, delta: u64) -> i32 {
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

/// A value handed to [`Store::instantiate`] for one import. Function, global,
/// and memory values reference cells/instances that stay alive in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternVal {
    /// A host function (e.g. the spectest `print*` family).
    HostFunc(usize),
    /// The function at `index` in the full function space of `instance`.
    Func { instance: usize, index: usize },
    /// A shared global cell.
    Global(usize),
    /// A shared memory cell.
    Memory(usize),
    /// A shared table cell (Cut 5 has no table runtime yet).
    Table(usize),
    /// The value exists but its kind is not executable this cut.
    Unsupported(&'static str),
}

/// Where a function index ultimately executes: a host routine or a defined
/// body of some instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncTarget {
    Host(usize),
    Owned { instance: usize, defined: usize },
}

/// A host function: the engine has no host bodies, only signatures (spectest
/// `print*` return nothing).
struct HostFunc {
    ty: FuncType,
}

/// A module instance: the module (code, types, exports) plus its index
/// spaces. Globals and memories are cell ids into the owning store, so
/// imports alias the exporter's cells; functions are flattened [`FuncTarget`]s.
pub struct Instance {
    module: Module,
    funcs: Vec<FuncTarget>,
    globals: Vec<usize>,
    memories: Vec<usize>,
    depth_limit: usize,
}

/// The store: every live instance plus the shared pools they reference.
pub struct Store {
    pub instances: Vec<Instance>,
    host_funcs: Vec<HostFunc>,
    globals: Vec<Value>,
    global_types: Vec<GlobalType>,
    memories: Vec<Memory>,
    memory_types: Vec<MemType>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Store {
            instances: Vec::new(),
            host_funcs: Vec::new(),
            globals: Vec::new(),
            global_types: Vec::new(),
            memories: Vec::new(),
            memory_types: Vec::new(),
        }
    }

    /// Register a host function (spectest `print*` family); returns its id.
    pub fn host_func(&mut self, ty: FuncType) -> usize {
        self.host_funcs.push(HostFunc { ty });
        self.host_funcs.len() - 1
    }

    /// Register a standalone global cell (spectest values); returns its id.
    pub fn global(&mut self, ty: GlobalType, value: Value) -> usize {
        self.global_types.push(ty);
        self.globals.push(value);
        self.globals.len() - 1
    }

    /// Register a standalone memory cell (spectest memory); returns its id.
    pub fn memory(&mut self, ty: MemType) -> Result<usize, ExecFail> {
        if ty.memory64 {
            return Err(ExecFail::Unsupported("memory64 (Cut 8)"));
        }
        self.memory_types.push(ty);
        self.memories
            .push(Memory::new(ty.limits.min, ty.limits.max));
        Ok(self.memories.len() - 1)
    }

    /// Instantiate `module`, resolving each import through `resolve` (module
    /// name, field name) to an [`ExternVal`] or `None` for an unknown import.
    /// Returns the new instance's id.
    pub fn instantiate(
        &mut self,
        module: &Module,
        resolve: &mut dyn FnMut(&str, &str) -> Option<ExternVal>,
    ) -> Result<usize, InstantiateError> {
        let self_id = self.instances.len();
        let mut funcs = Vec::with_capacity(module.imports.len() + module.functions.len());
        let mut globals = Vec::with_capacity(module.imports.len());
        let mut memories = Vec::with_capacity(module.imports.len());

        for import in &module.imports {
            match &import.desc {
                ImportDesc::Func(type_index) => {
                    let expected = module
                        .types
                        .get(*type_index as usize)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::HostFunc(id) => {
                            if self.host_funcs[id].ty != *expected {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            funcs.push(FuncTarget::Host(id));
                        }
                        ExternVal::Func { instance, index } => {
                            let target = *self
                                .instances
                                .get(instance)
                                .and_then(|i| i.funcs.get(index))
                                .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                            if self.target_type(target) != *expected {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            funcs.push(target);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Global(gty) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Global(cell) => {
                            let actual = *self
                                .global_types
                                .get(cell)
                                .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                            if actual != *gty {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            globals.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Memory(mt) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Memory(cell) => {
                            let actual = self.memory_type_of(cell);
                            if !memory_matches(actual, *mt) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            memories.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Table(_) => {
                    // Table imports need a table runtime (Cut 5); only an
                    // absent export is a real (linkable) unknown.
                    match resolve(&import.module, &import.name) {
                        None => return Err(InstantiateError::Unlinkable("unknown import")),
                        Some(ExternVal::Unsupported(reason)) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        Some(ExternVal::Table(_)) | Some(_) => {
                            return Err(InstantiateError::Unsupported("tables (Cut 5)"));
                        }
                    }
                }
                ImportDesc::Tag(_) => match resolve(&import.module, &import.name) {
                    None => return Err(InstantiateError::Unlinkable("unknown import")),
                    Some(ExternVal::Unsupported(reason)) => {
                        return Err(InstantiateError::Unsupported(reason));
                    }
                    Some(_) => return Err(InstantiateError::Unsupported("tags (Cut 6)")),
                },
            }
        }

        for (defined, _) in module.functions.iter().enumerate() {
            funcs.push(FuncTarget::Owned {
                instance: self_id,
                defined,
            });
        }

        for memory in &module.memories {
            if memory.memory64 {
                return Err(InstantiateError::Unsupported("memory64 (Cut 8)"));
            }
            self.memory_types.push(*memory);
            self.memories
                .push(Memory::new(memory.limits.min, memory.limits.max));
            memories.push(self.memories.len() - 1);
        }

        for global in &module.globals {
            let values = self.global_values(&globals);
            let value = eval_const(&global.init, &values).map_err(InstantiateError::from)?;
            self.global_types.push(global.ty);
            self.globals.push(value);
            globals.push(self.globals.len() - 1);
        }

        self.instances.push(Instance {
            module: module.clone(),
            funcs,
            globals,
            memories,
            depth_limit: DEFAULT_DEPTH_LIMIT,
        });

        // Apply active data segments in order. Writes to imported memories
        // land in the shared cell and persist even if a later segment traps.
        for segment in &module.data {
            if let DataMode::Active { memory, offset } = &segment.mode {
                let memory_index = *memory as usize;
                let cell = self.instances[self_id]
                    .memories
                    .get(memory_index)
                    .copied()
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))
                    .map_err(InstantiateError::from)?;
                let values = self.global_values(&self.instances[self_id].globals);
                let offset_value = eval_const(offset, &values).map_err(InstantiateError::from)?;
                let Value::I32(offset) = offset_value else {
                    return Err(InstantiateError::Unsupported("non-i32 data offset"));
                };
                let start = offset as u32 as usize;
                let end = start
                    .checked_add(segment.bytes.len())
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))
                    .map_err(InstantiateError::from)?;
                let memory = &mut self.memories[cell];
                if end > memory.bytes.len() {
                    return Err(InstantiateError::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                memory.bytes[start..end].copy_from_slice(&segment.bytes);
            }
        }

        // Run the start function, if any.
        if let Some(start) = module.start {
            let target = *self.instances[self_id]
                .funcs
                .get(start as usize)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))
                .map_err(InstantiateError::from)?;
            self.run_target(target, &[])
                .map_err(InstantiateError::from)?;
        }
        Ok(self_id)
    }

    /// The export `name` of `instance` as an importable value.
    pub fn export(&self, instance: usize, name: &str) -> Option<ExternVal> {
        let inst = self.instances.get(instance)?;
        let export = inst.module.exports.iter().find(|e| e.name == name)?;
        Some(match export.kind {
            ExportKind::Func => ExternVal::Func {
                instance,
                index: export.index as usize,
            },
            ExportKind::Global => ExternVal::Global(inst.globals[export.index as usize]),
            ExportKind::Memory => ExternVal::Memory(inst.memories[export.index as usize]),
            ExportKind::Table => ExternVal::Table(usize::MAX),
            ExportKind::Tag => ExternVal::Unsupported("tags (Cut 6)"),
        })
    }

    /// All exports of `instance`, for `register`-style name resolution.
    pub fn exports(&self, instance: usize) -> Vec<(String, ExternVal)> {
        self.instances
            .get(instance)
            .map(|inst| {
                inst.module
                    .exports
                    .iter()
                    .map(|export| {
                        let value = match export.kind {
                            ExportKind::Func => ExternVal::Func {
                                instance,
                                index: export.index as usize,
                            },
                            ExportKind::Global => {
                                ExternVal::Global(inst.globals[export.index as usize])
                            }
                            ExportKind::Memory => {
                                ExternVal::Memory(inst.memories[export.index as usize])
                            }
                            ExportKind::Table => ExternVal::Table(usize::MAX),
                            ExportKind::Tag => ExternVal::Unsupported("tags (Cut 6)"),
                        };
                        (export.name.clone(), value)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The full-space function index of exported function `name`, if any.
    pub fn exported_func(&self, instance: usize, name: &str) -> Option<usize> {
        self.instances
            .get(instance)?
            .module
            .exports
            .iter()
            .find_map(|export| {
                (export.name == name && export.kind == ExportKind::Func)
                    .then_some(export.index as usize)
            })
    }

    /// The current value of exported global `name`, if any.
    pub fn export_global_value(&self, instance: usize, name: &str) -> Option<Value> {
        let inst = self.instances.get(instance)?;
        let export = inst
            .module
            .exports
            .iter()
            .find(|e| e.name == name && e.kind == ExportKind::Global)?;
        let cell = inst.globals[export.index as usize];
        self.globals.get(cell).copied()
    }

    /// Invoke function `index` (full index space) of `instance` with `args`.
    pub fn invoke(
        &mut self,
        instance: usize,
        index: usize,
        args: &[Value],
    ) -> Result<Vec<Value>, ExecFail> {
        let target = *self
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.run_target(target, args)
    }

    fn run_target(&mut self, target: FuncTarget, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
        match target {
            FuncTarget::Owned { instance, defined } => self.run_owned(instance, defined, args),
            FuncTarget::Host(id) => {
                let ty = self.host_funcs[id].ty.clone();
                if ty.params.len() != args.len() {
                    return Err(ExecFail::Unsupported("host function arity"));
                }
                if !ty.results.is_empty() {
                    return Err(ExecFail::Unsupported("host function results"));
                }
                Ok(Vec::new())
            }
        }
    }

    /// Run a module-defined function by its index into `Module::bodies`.
    fn run_owned(
        &mut self,
        instance: usize,
        defined: usize,
        args: &[Value],
    ) -> Result<Vec<Value>, ExecFail> {
        let (signature, declared) = {
            let module = &self.instances[instance].module;
            let body = module
                .bodies
                .get(defined)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let type_index = module
                .functions
                .get(defined)
                .copied()
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let signature = module
                .types
                .get(type_index as usize)
                .cloned()
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            (signature, body.locals.clone())
        };
        let mut locals = args.to_vec();
        for ty in &declared {
            locals.push(default_value(*ty)?);
        }
        let results = signature.results.len();
        let mut engine = Engine {
            store: self,
            stack: Vec::new(),
            frames: vec![Frame {
                instance,
                pc: 0,
                locals,
                base: 0,
                results,
                body_index: defined,
                labels: vec![Label {
                    arity: results,
                    height: 0,
                    is_loop: false,
                    open: 0,
                }],
            }],
        };
        engine.run()
    }

    fn target_type(&self, target: FuncTarget) -> FuncType {
        match target {
            FuncTarget::Host(id) => self.host_funcs[id].ty.clone(),
            FuncTarget::Owned { instance, defined } => {
                let module = &self.instances[instance].module;
                module
                    .functions
                    .get(defined)
                    .and_then(|&ti| module.types.get(ti as usize))
                    .cloned()
                    .unwrap_or(FuncType {
                        params: Vec::new(),
                        results: Vec::new(),
                    })
            }
        }
    }

    fn global_values(&self, cells: &[usize]) -> Vec<Value> {
        cells.iter().map(|&cell| self.globals[cell]).collect()
    }

    /// A memory cell's type for import matching: the declared maximum but the
    /// *current* size as the minimum (a grown memory can satisfy a larger
    /// import minimum).
    fn memory_type_of(&self, cell: usize) -> MemType {
        let declared = self.memory_types[cell];
        MemType {
            limits: Limits {
                min: self.memories[cell].pages(),
                max: declared.limits.max,
                shared: declared.limits.shared,
            },
            memory64: declared.memory64,
        }
    }
}

/// A function frame on the machine's call stack.
struct Frame {
    /// The instance whose module (code) and cells this frame executes in.
    instance: usize,
    pc: usize,
    locals: Vec<Value>,
    /// Operand-stack height when this frame's body started (arguments
    /// popped; results are produced above this line).
    base: usize,
    /// Number of result values this function produces.
    results: usize,
    /// Defined-function index into `Module::bodies` of the frame's instance.
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
    store: &'a mut Store,
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
        &self.store.instances[self.frames[frame_index].instance]
            .module
            .bodies[self.frames[frame_index].body_index]
            .body
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
            Instr::ReturnCall(index) => self.do_tail_call(index as usize),
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
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance].globals[index as usize];
                let value = self.store.globals[cell];
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::GlobalSet(index) => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance].globals[index as usize];
                self.store.globals[cell] = value;
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
                let instance = self.frames[frame_index].instance;
                let Some(&cell) = self.store.instances[instance].memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let addr = self.pop_i32()? as u32 as u64;
                let size = load_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let value = read_mem(op, &self.store.memories[cell].bytes[start..start + size]);
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::Store { op, offset, .. } => {
                let value = self.pop()?;
                let addr = self.pop_i32()? as u32 as u64;
                let instance = self.frames[frame_index].instance;
                let Some(&cell) = self.store.instances[instance].memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let size = store_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                write_mem(
                    op,
                    value,
                    &mut self.store.memories[cell].bytes[start..start + size],
                );
                Ok(Ctl::Next)
            }
            Instr::MemorySize => {
                let instance = self.frames[frame_index].instance;
                let Some(&cell) = self.store.instances[instance].memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let pages = self.store.memories[cell].pages();
                self.stack.push(Value::I32(pages as i32));
                Ok(Ctl::Next)
            }
            Instr::MemoryGrow => {
                let delta = self.pop_i32()? as u32 as u64;
                let instance = self.frames[frame_index].instance;
                let Some(&cell) = self.store.instances[instance].memories.first() else {
                    return Err(ExecFail::Unsupported("no memory"));
                };
                let old = self.store.memories[cell].grow(delta);
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
                let frame_index = self.frames.len() - 1;
                let instance = self.frames[frame_index].instance;
                let ty = self
                    .store
                    .instances
                    .get(instance)
                    .and_then(|i| i.module.types.get(index as usize))
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                Ok((ty.params.len(), ty.results.len()))
            }
        }
    }

    /// Run a host function called from the top frame.
    fn call_host(&mut self, id: usize) -> Result<(), ExecFail> {
        let ty = self.store.host_funcs[id].ty.clone();
        if !ty.results.is_empty() {
            return Err(ExecFail::Unsupported("host function results"));
        }
        for _ in 0..ty.params.len() {
            self.pop()?;
        }
        Ok(())
    }

    /// Make a (direct, non-tail) call from the top frame.
    fn do_call(&mut self, index: usize) -> Result<(), ExecFail> {
        let caller = self.frames.len() - 1;
        let instance = self.frames[caller].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let target = *self
            .store
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        match target {
            FuncTarget::Host(id) => {
                self.call_host(id)?;
                // A host call returns in place: advance past the call.
                let caller = self.frames.len() - 1;
                self.frames[caller].pc += 1;
                Ok(())
            }
            FuncTarget::Owned {
                instance: own,
                defined,
            } => {
                let (param_count, result_count, declared) = {
                    let module = &self.store.instances[own].module;
                    let body = module
                        .bodies
                        .get(defined)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let type_index = module
                        .functions
                        .get(defined)
                        .copied()
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
                    instance: own,
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
        }
    }

    /// Make a tail call (`return_call`) from the top frame: replace the top
    /// frame in place with the callee's frame at the same operand-stack base,
    /// so the callee's results flow to the caller of the (now replaced)
    /// frame. Everything else above the base — values owned by the tail
    /// frame's not-yet-exited labels — is discarded when the callee finishes,
    /// matching the spec's unwinding of `return_call`.
    fn do_tail_call(&mut self, index: usize) -> Result<Ctl, ExecFail> {
        let top = self.frames.len() - 1;
        let instance = self.frames[top].instance;
        let target = *self
            .store
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        match target {
            FuncTarget::Host(id) => {
                let ty = self.store.host_funcs[id].ty.clone();
                if !ty.results.is_empty() {
                    return Err(ExecFail::Unsupported("host function results"));
                }
                for _ in 0..ty.params.len() {
                    self.pop()?;
                }
                self.finish()
            }
            FuncTarget::Owned {
                instance: own,
                defined,
            } => {
                let (param_count, result_count, declared) = {
                    let module = &self.store.instances[own].module;
                    let body = module
                        .bodies
                        .get(defined)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let type_index = module
                        .functions
                        .get(defined)
                        .copied()
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
                let base = self.frames[top].base;
                self.frames.pop();
                self.frames.push(Frame {
                    instance: own,
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
                Ok(Ctl::Settled)
            }
        }
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
/// caller slices exactly `store_size(op)` bytes).
fn write_mem(_op: StoreOp, value: Value, bytes: &mut [u8]) {
    let bits = match value {
        Value::I32(v) => v as u32 as u64,
        Value::I64(v) => v as u64,
        Value::F32(bits) => bits as u64,
        Value::F64(bits) => bits,
    };
    for (shift, byte) in bytes.iter_mut().enumerate() {
        *byte = (bits >> (8 * shift)) as u8;
    }
}

/// Whether an imported memory type is satisfied by a provided one: the
/// provider must have at least the requested minimum, and no larger a maximum
/// than the request allows (spec 4.5.2 external-type subsumption).
fn memory_matches(actual: MemType, requested: MemType) -> bool {
    !actual.memory64 && !requested.memory64 && limits_match(actual.limits, requested.limits)
}

fn limits_match(actual: Limits, requested: Limits) -> bool {
    actual.min >= requested.min
        && requested
            .max
            .is_none_or(|max| actual.max.is_some_and(|a| a <= max))
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
