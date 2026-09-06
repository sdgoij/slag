//! The wasm execution machine (spec ch. 4) over a [`Store`] of module
//! instances (spec ch. 4.2).
//!
//! Instances share the store's mutable state: globals, linear memories, and
//! tables are cells in store-owned pools, so a module that imports another
//! instance's memory, mutable global, or table aliases the same cell. Function
//! imports bind to a flattened target (a defined body in some instance, or a
//! host function).
//!
//! One shared operand stack plus an explicit stack of function frames (each
//! with its own instance, pc, locals, and control labels) keeps call depth
//! bounded by a configured limit instead of the host stack. Numeric semantics
//! live in [`crate::values`].
//!
//! Not executed yet ([`ExecFail::Unsupported`]): GC reference operations
//! (Cut 9) and threads/shared memory.

use crate::instr::{Catch, Instr, LoadOp, NumOp, StoreOp, VecLoadOp};
use crate::module::{DataMode, ElementMode, ExportKind, ImportDesc, Module};
use crate::types::{
    BlockType, CompositeType, FieldType, FuncType, GlobalType, HeapType, Limits, MemType, RefType,
    StorageType, TableType, ValType,
};
use crate::values::{ExternInner, FuncAddr, RefValue, Trap, Value, exec_num};

/// How execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecFail {
    Trap(Trap),
    /// A feature this cut does not execute yet (later cuts).
    Unsupported(&'static str),
    /// An uncaught wasm exception, thrown by `throw`/`throw_ref` and not
    /// handled by any `try_table`: the id of the exception in the store's
    /// pool. Only the outermost invocation edge sees this — the run loop
    /// converts it into a branch whenever a matching catch clause exists.
    Exception(usize),
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
            ExecFail::Exception(_) => {
                InstantiateError::Unsupported("exception during instantiation")
            }
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

/// A host call the resumable driver must resolve: an *external* host function
/// (registered by [`Store::external_host`] — a JS-API function import) was
/// called from running wasm. The driver runs the embedder's function with
/// `args` while the store is not borrowed, then hands the results back to
/// [`Store::resume`].
#[derive(Debug)]
pub struct HostRequest {
    /// The token [`Store::external_host`] was registered with; the JS-API
    /// maps it back to the JS function it wraps.
    pub token: u64,
    /// The function's arguments, in parameter order.
    pub args: Vec<Value>,
}

/// What a resumable run produced at its next host-call boundary (or the
/// end).
#[derive(Debug)]
pub enum RunProgress {
    /// The invocation finished with these results.
    Finished(Vec<Value>),
    /// The invocation needs an external host call (see [`HostRequest`]).
    Host(HostRequest),
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

    /// Grow by `delta` pages; returns the old size in pages, or `None` when
    /// the growth would exceed the declared maximum (a memory32 is also
    /// capped at 2^16 pages — 4 GiB — even without a declared maximum;
    /// memory64 is only bounded by what a byte vector can address).
    pub fn grow(&mut self, delta: u64, memory64: bool) -> Option<u64> {
        let old = self.pages();
        let new = old.checked_add(delta)?;
        let cap = match self.max_pages {
            Some(max) => max,
            None if memory64 => (usize::MAX / PAGE_SIZE as usize) as u64,
            None => 1 << 16,
        };
        if new > cap {
            return None;
        }
        let byte_len = new.checked_mul(PAGE_SIZE)?;
        let Ok(byte_len) = usize::try_from(byte_len) else {
            return None;
        };
        self.bytes.resize(byte_len, 0);
        Some(old)
    }
}

/// A table instance: reference slots plus its declared maximum.
#[derive(Debug)]
pub struct TableInst {
    elements: Vec<RefValue>,
    max: Option<u64>,
}

impl TableInst {
    fn new(min: u64, max: Option<u64>, init: RefValue) -> Self {
        TableInst {
            elements: vec![init; min as usize],
            max,
        }
    }

    /// Grow by `delta` slots, filling with `init`; returns the old size, or
    /// `None` when the growth would exceed the limits (a 32-bit table is
    /// capped at 2^32-1 slots even without a declared maximum).
    fn grow(&mut self, delta: u64, init: RefValue, table64: bool) -> Option<u64> {
        let old = self.elements.len() as u64;
        let new = old.checked_add(delta)?;
        let cap = self.max.unwrap_or(if table64 {
            usize::MAX as u64
        } else {
            u64::from(u32::MAX)
        });
        if new > cap {
            return None;
        }
        self.elements.resize(new as usize, init);
        Some(old)
    }
}

/// A tag instance: the exception's payload shape (its function type's
/// parameters; the results are empty by validation).
#[derive(Debug)]
pub struct TagInst {
    ty: FuncType,
    /// The instance and module type index that declared the tag, when it came
    /// from a module (used for cross-module import matching); `None` for
    /// standalone JS-API tags.
    owner: Option<(usize, u32)>,
}

/// A concrete exception: which tag was thrown plus the argument values.
/// Both in-flight exceptions (unwinding to a catch) and `exnref` values
/// reference these cells by id.
#[derive(Debug)]
pub struct ExceptionInst {
    pub tag: usize,
    pub args: Vec<Value>,
}

/// A GC aggregate object (struct or array) in the store's object pool. The
/// owning instance's module owns `ty`, which fixes the field/element storage
/// layout; each data slot is a [`Value`] (packed cells keep the wrapped
/// unsigned bits). Objects are append-only, so ids stay stable.
#[derive(Debug)]
pub struct GcObject {
    /// The instance whose module allocated this object (its type space owns
    /// `ty`).
    pub owner: usize,
    /// The object's type index in the owner's module.
    pub ty: u32,
    pub data: GcData,
}

#[derive(Debug)]
pub enum GcData {
    /// One value per declared struct field.
    Struct(Vec<Value>),
    /// One value per array element.
    Array(Vec<Value>),
}

/// A value handed to [`Store::instantiate`] for one import. Function, global,
/// memory, table, and tag values reference cells/instances that stay alive in
/// the store.
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
    /// A shared table cell.
    Table(usize),
    /// A shared tag cell.
    Tag(usize),
    /// The value exists but its kind is not executable this cut.
    Unsupported(&'static str),
}

/// The canonical identity of a function: instance function spaces alias the
/// target they resolve to, so one function surfaced through any number of
/// import/export chains shares a key (the JS-API's wrapper-object memo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FuncKey {
    /// A host function (the spectest `print*` family or an external JS
    /// closure registered through [`Store::external_host`]).
    Host(usize),
    /// The `defined`-th function declared by `instance`.
    Owned { instance: usize, defined: usize },
}

/// Where a function index ultimately executes: a host routine or a defined
/// body of some instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncTarget {
    Host(usize),
    Owned { instance: usize, defined: usize },
}

/// A host function: the spectest `print*` family is type-only (no body, no
/// results), while the JS-API's function imports are *external* — their
/// results come from the embedder when a resumable run reaches a
/// [`HostRequest`].
struct HostFunc {
    ty: FuncType,
    token: Option<u64>,
}

/// A module instance: the module (code, types, exports) plus its index
/// spaces. Globals, memories, tables, and tags are cell ids into the owning
/// store, so imports alias the exporter's cells; functions are flattened
/// [`FuncTarget`]s. Data and element segments are retained per instance for
/// the bulk-memory instructions (active segments are dropped after use).
pub struct Instance {
    module: Module,
    funcs: Vec<FuncTarget>,
    tables: Vec<usize>,
    globals: Vec<usize>,
    memories: Vec<usize>,
    tags: Vec<usize>,
    element_segments: Vec<Option<Vec<RefValue>>>,
    data_segments: Vec<Option<Vec<u8>>>,
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
    tables: Vec<TableInst>,
    table_types: Vec<TableType>,
    tags: Vec<TagInst>,
    exceptions: Vec<ExceptionInst>,
    /// Live GC struct/array objects; ids double as `RefValue::Struct/Array`
    /// payloads.
    objects: Vec<GcObject>,
    /// Runs suspended at an external-host-call boundary, innermost last
    /// ([`Store::start`] parks here; [`Store::resume`] pops and continues).
    suspended: Vec<Suspended>,
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
            tables: Vec::new(),
            table_types: Vec::new(),
            tags: Vec::new(),
            exceptions: Vec::new(),
            objects: Vec::new(),
            suspended: Vec::new(),
        }
    }

    /// Register a type-only host function (spectest `print*` family); returns
    /// its id.
    pub fn host_func(&mut self, ty: FuncType) -> usize {
        self.host_funcs.push(HostFunc { ty, token: None });
        self.host_funcs.len() - 1
    }

    /// Register an *external* host function — one whose results the caller of
    /// a resumable run supplies via [`Store::resume`]. The JS-API registers a
    /// JS closure import this way, keyed by an embedder `token`. Returns its
    /// id.
    pub fn external_host(&mut self, ty: FuncType, token: u64) -> usize {
        self.host_funcs.push(HostFunc {
            ty,
            token: Some(token),
        });
        self.host_funcs.len() - 1
    }

    /// Register a standalone tag cell (a JS-API `WebAssembly.Tag`); returns
    /// its id. The payload shape is the tag's function type (parameters only).
    pub fn tag(&mut self, ty: FuncType) -> usize {
        self.tags.push(TagInst { ty, owner: None });
        self.tags.len() - 1
    }

    /// Register a standalone global cell (spectest values); returns its id.
    pub fn global(&mut self, ty: GlobalType, value: Value) -> usize {
        self.global_types.push(ty);
        self.globals.push(value);
        self.globals.len() - 1
    }

    /// Register a standalone memory cell (spectest memory); returns its id.
    pub fn memory(&mut self, ty: MemType) -> Result<usize, ExecFail> {
        self.memory_types.push(ty);
        self.memories
            .push(Memory::new(ty.limits.min, ty.limits.max));
        Ok(self.memories.len() - 1)
    }

    /// Register a standalone table cell (spectest table); returns its id.
    pub fn table(&mut self, ty: TableType, init: RefValue) -> Result<usize, ExecFail> {
        self.table_types.push(ty);
        self.tables
            .push(TableInst::new(ty.limits.min, ty.limits.max, init));
        Ok(self.tables.len() - 1)
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
        let mut tables = Vec::with_capacity(module.imports.len());
        let mut globals = Vec::with_capacity(module.imports.len());
        let mut memories = Vec::with_capacity(module.imports.len());
        let mut tags = Vec::with_capacity(module.imports.len());

        for import in &module.imports {
            match &import.desc {
                ImportDesc::Func(type_index) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::HostFunc(id) => {
                            let target = FuncTarget::Host(id);
                            if !self.func_type_matches(module, *type_index, target) {
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
                            if !self.func_type_matches(module, *type_index, target) {
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
                            // Mutability must match exactly. An immutable
                            // import's value type may subsume (a non-null
                            // `(ref func)` satisfies `funcref`), but a mutable
                            // global is an invariant container: the importer
                            // could write into it, so the value type must be
                            // identical.
                            let value_ok = if gty.mutable {
                                gty.value == actual.value
                            } else {
                                matches_val(gty.value, actual.value)
                            };
                            if actual.mutable != gty.mutable || !value_ok {
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
                ImportDesc::Table(tt) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Table(cell) => {
                            let actual = self.table_type_of(cell);
                            if !table_matches(actual, *tt) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            tables.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Tag(type_index) => {
                    let expected = module
                        .func_at_cloned(*type_index)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Tag(cell) => {
                            // Tags match by payload shape: the imported type
                            // must be equivalent to the tag's declared type
                            // (cross-module rec-group aware when the tag was
                            // declared by a module).
                            let ok = match self.tags.get(cell).and_then(|t| t.owner) {
                                Some((owner_instance, owner_type)) => {
                                    self.instances.get(owner_instance).is_some_and(|owner| {
                                        crate::module::type_indices_equivalent(
                                            &module.types,
                                            &module.rec_groups,
                                            *type_index,
                                            &owner.module.types,
                                            &owner.module.rec_groups,
                                            owner_type,
                                        )
                                    })
                                }
                                None => self.tags.get(cell).map(|t| t.ty.clone()) == Some(expected),
                            };
                            if !ok {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            tags.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
            }
        }

        for (defined, _) in module.functions.iter().enumerate() {
            funcs.push(FuncTarget::Owned {
                instance: self_id,
                defined,
            });
        }

        for type_index in &module.tags {
            let ty = module
                .func_at_cloned(*type_index)
                .ok_or(InstantiateError::Unlinkable("unknown tag type"))?;
            self.tags.push(TagInst {
                ty,
                owner: Some((self_id, *type_index)),
            });
            tags.push(self.tags.len() - 1);
        }

        for table in &module.tables {
            let init = if let Some(init_expr) = &table.init {
                let values = self.global_values(&globals);
                eval_ref_const(self, module, init_expr, &values, self_id)?
            } else {
                RefValue::Null
            };
            self.table_types.push(table.ty);
            self.tables.push(TableInst::new(
                table.ty.limits.min,
                table.ty.limits.max,
                init,
            ));
            tables.push(self.tables.len() - 1);
        }

        for memory in &module.memories {
            self.memory_types.push(*memory);
            self.memories
                .push(Memory::new(memory.limits.min, memory.limits.max));
            memories.push(self.memories.len() - 1);
        }

        for global in &module.globals {
            let values = self.global_values(&globals);
            let value = eval_const(self, module, &global.init, &values, self_id)
                .map_err(InstantiateError::from)?;
            self.global_types.push(global.ty);
            self.globals.push(value);
            globals.push(self.globals.len() - 1);
        }

        self.instances.push(Instance {
            module: module.clone(),
            funcs,
            tables,
            globals,
            memories,
            tags,
            element_segments: Vec::new(),
            data_segments: Vec::new(),
            depth_limit: DEFAULT_DEPTH_LIMIT,
        });

        // Instantiate element segments in order (all elements before data),
        // then data segments. Writes to imported tables/memories land in the
        // shared cells and persist even if a later segment traps.
        for segment in &module.elements {
            let values = self.global_values(&self.instances[self_id].globals);
            let items: Vec<RefValue> = segment
                .init
                .iter()
                .map(|item| eval_ref_const(self, module, item, &values, self_id))
                .collect::<Result<_, _>>()?;
            match &segment.mode {
                ElementMode::Active { table, offset } => {
                    let cell = self.instances[self_id]
                        .tables
                        .get(*table as usize)
                        .copied()
                        .ok_or(InstantiateError::Trap(Trap::OutOfBoundsTableAccess))?;
                    let offset_value = eval_const(self, module, offset, &values, self_id)
                        .map_err(InstantiateError::from)?;
                    // The active offset's width follows the table's address
                    // type: i32 for a 32-bit table, i64 for a table64.
                    let start = match offset_value {
                        Value::I32(offset) => offset as u32 as usize,
                        Value::I64(offset) => offset as u64 as usize,
                        _ => {
                            return Err(InstantiateError::Unsupported("non-address table offset"));
                        }
                    };
                    let end = start
                        .checked_add(items.len())
                        .ok_or(InstantiateError::Trap(Trap::OutOfBoundsTableAccess))?;
                    let table = &mut self.tables[cell];
                    if end > table.elements.len() {
                        return Err(InstantiateError::Trap(Trap::OutOfBoundsTableAccess));
                    }
                    table.elements[start..end].copy_from_slice(&items);
                    self.instances[self_id].element_segments.push(None);
                }
                ElementMode::Passive => {
                    self.instances[self_id].element_segments.push(Some(items));
                }
                ElementMode::Declarative => {
                    self.instances[self_id].element_segments.push(None);
                }
            }
        }

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
                let offset_value = eval_const(self, module, offset, &values, self_id)
                    .map_err(InstantiateError::from)?;
                // The active offset's width follows the memory's address type:
                // i32 for a memory32, i64 for a memory64.
                let start = match offset_value {
                    Value::I32(offset) => offset as u32 as usize,
                    Value::I64(offset) => offset as u64 as usize,
                    _ => {
                        return Err(InstantiateError::Unsupported("non-address data offset"));
                    }
                };
                let end = start
                    .checked_add(segment.bytes.len())
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))
                    .map_err(InstantiateError::from)?;
                let memory = &mut self.memories[cell];
                if end > memory.bytes.len() {
                    return Err(InstantiateError::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                memory.bytes[start..end].copy_from_slice(&segment.bytes);
                self.instances[self_id].data_segments.push(None);
            } else {
                self.instances[self_id]
                    .data_segments
                    .push(Some(segment.bytes.clone()));
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
            ExportKind::Table => ExternVal::Table(inst.tables[export.index as usize]),
            ExportKind::Tag => ExternVal::Tag(inst.tags[export.index as usize]),
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
                            ExportKind::Table => {
                                ExternVal::Table(inst.tables[export.index as usize])
                            }
                            ExportKind::Tag => ExternVal::Tag(inst.tags[export.index as usize]),
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

    /// The canonical identity of the function at full-space `index` of
    /// `instance`: imports alias the target they resolve to, so a function
    /// re-exported through later instances keeps its defining instance's key.
    pub fn func_key(&self, instance: usize, index: usize) -> Option<FuncKey> {
        let inst = self.instances.get(instance)?;
        let target = inst.funcs.get(index)?;
        Some(match target {
            FuncTarget::Host(id) => FuncKey::Host(*id),
            FuncTarget::Owned { instance, defined } => FuncKey::Owned {
                instance: *instance,
                defined: *defined,
            },
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

    /// The declared function type at `index` (full function index space) of
    /// `instance`, if any. Used by the runner to shape reference arguments
    /// (a `ref.host` payload argument is an internal `any` value, while a
    /// `ref.extern` payload is wrapped as an external).
    pub fn func_type(&self, instance: usize, index: usize) -> Option<FuncType> {
        let inst = self.instances.get(instance)?;
        let func_imports = inst
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.desc, ImportDesc::Func(_)))
            .count();
        let ti = if index < func_imports {
            inst.module
                .imports
                .iter()
                .filter_map(|import| match import.desc {
                    ImportDesc::Func(ty) => Some(ty),
                    _ => None,
                })
                .nth(index)
        } else {
            inst.module.functions.get(index - func_imports).copied()
        }?;
        inst.module.func_at_cloned(ti)
    }

    /// The parameter types of function `index` (full function index space) of
    /// `instance`.
    pub fn func_params(&self, instance: usize, index: usize) -> Option<Vec<ValType>> {
        self.func_type(instance, index).map(|ty| ty.params)
    }

    /// Invoke function `index` (full index space) of `instance` with `args`.
    /// Non-resumable: an external host function reached mid-run (possible only
    /// when a module imported a JS-API closure) fails as unsupported here —
    /// the JS-API uses [`Store::start`]/[`Store::resume`] instead.
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

    /// Start a resumable invocation of function `index` (full index space) of
    /// `instance` with `args`, running until the next external-host-call
    /// boundary or completion. A [`RunProgress::Host`] result parks the run
    /// inside the store; the caller runs the embedder host function with the
    /// store unborrowed and passes its results to [`Store::resume`]. Reentrant
    /// invocations simply nest: each suspension pushes, each resume pops.
    pub fn start(
        &mut self,
        instance: usize,
        index: usize,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
        let target = *self
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.start_target(target, args)
    }

    /// Resume the most recently suspended run with an external host function's
    /// results (or an error, which abandons that run and propagates).
    pub fn resume(&mut self, reply: Result<Vec<Value>, ExecFail>) -> Result<RunProgress, ExecFail> {
        let suspended = self
            .suspended
            .pop()
            .ok_or(ExecFail::Unsupported("resume without a suspended run"))?;
        let results = reply?;
        let mut engine = Engine {
            store: self,
            stack: suspended.stack,
            frames: suspended.frames,
        };
        engine.stack.extend(results);
        engine.run()
    }

    /// Discard the most recently suspended run (its external host function
    /// threw and the caller abandons the invocation it was part of). Returns
    /// whether there was a run to discard.
    pub fn abandon(&mut self) -> bool {
        self.suspended.pop().is_some()
    }

    /// Resume the most recently suspended run with an in-flight exception
    /// instead of host results: the exception unwinds inside wasm (a matching
    /// `try_table` catches it, or it escapes to this caller as
    /// [`ExecFail::Exception`]). Used when an imported JS function throws a
    /// `WebAssembly.Exception` whose tag the wasm frames can catch.
    pub fn resume_exception(&mut self, exn: usize) -> Result<RunProgress, ExecFail> {
        let suspended = self
            .suspended
            .pop()
            .ok_or(ExecFail::Unsupported("resume without a suspended run"))?;
        let mut engine = Engine {
            store: self,
            stack: suspended.stack,
            frames: suspended.frames,
        };
        // No wasm frames are left to catch (a top-level external-host call or
        // an outermost tail call): the exception escapes immediately.
        if engine.frames.is_empty() {
            return Err(ExecFail::Exception(exn));
        }
        match engine.unwind(exn)? {
            Some(results) => Ok(RunProgress::Finished(results)),
            None => engine.run(),
        }
    }

    fn run_target(&mut self, target: FuncTarget, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
        match target {
            FuncTarget::Owned { instance, defined } => self.run_owned(instance, defined, args),
            FuncTarget::Host(id) => {
                if self.host_funcs[id].token.is_some() {
                    return Err(ExecFail::Unsupported(
                        "external host function needs a resumable run",
                    ));
                }
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

    /// Start a resumable run against `target`. An external host target parks
    /// an empty run so [`Store::resume`] delivers its results directly as the
    /// invocation's; a type-only host target finishes immediately.
    fn start_target(
        &mut self,
        target: FuncTarget,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
        match target {
            FuncTarget::Owned { instance, defined } => self.start_owned(instance, defined, args),
            FuncTarget::Host(id) => {
                let ty = self.host_funcs[id].ty.clone();
                if ty.params.len() != args.len() {
                    return Err(ExecFail::Unsupported("host function arity"));
                }
                match self.host_funcs[id].token {
                    Some(token) => {
                        self.suspended.push(Suspended {
                            stack: Vec::new(),
                            frames: Vec::new(),
                        });
                        Ok(RunProgress::Host(HostRequest {
                            token,
                            args: args.to_vec(),
                        }))
                    }
                    None => {
                        if !ty.results.is_empty() {
                            return Err(ExecFail::Unsupported("host function results"));
                        }
                        Ok(RunProgress::Finished(Vec::new()))
                    }
                }
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
        match self.start_owned(instance, defined, args)? {
            RunProgress::Finished(results) => Ok(results),
            // A non-resumable caller reached an external host function (only
            // possible through an instance start function): discard the parked
            // run and report the limitation.
            RunProgress::Host(_) => {
                self.suspended.pop();
                Err(ExecFail::Unsupported(
                    "external host function in a non-resumable run",
                ))
            }
        }
    }

    /// Construct and run a root frame for `defined` of `instance`, parking at
    /// the next external-host boundary instead of failing.
    fn start_owned(
        &mut self,
        instance: usize,
        defined: usize,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
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
            let signature = module.func_at_cloned(type_index);
            let signature = signature.ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
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

    /// Whether a resolved function target satisfies a declared module type
    /// index: an owned function's type must be rec-group equivalent across the
    /// two module type spaces; a host function's signature must equal the
    /// declared one (host signatures carry no module-defined types).
    fn func_type_matches(&self, module: &Module, type_index: u32, target: FuncTarget) -> bool {
        match target {
            FuncTarget::Host(id) => {
                let declared = module.func_at_cloned(type_index).unwrap_or(FuncType {
                    params: Vec::new(),
                    results: Vec::new(),
                });
                self.host_funcs[id].ty == declared
            }
            FuncTarget::Owned { instance, defined } => {
                let Some(owner) = self.instances.get(instance) else {
                    return false;
                };
                let Some(&owner_type) = owner.module.functions.get(defined) else {
                    return false;
                };
                crate::module::type_indices_equivalent(
                    &module.types,
                    &module.rec_groups,
                    type_index,
                    &owner.module.types,
                    &owner.module.rec_groups,
                    owner_type,
                )
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

    /// A table cell's type for import matching (current size as the minimum).
    fn table_type_of(&self, cell: usize) -> TableType {
        let declared = self.table_types[cell];
        TableType {
            element: declared.element,
            limits: Limits {
                min: self.tables[cell].elements.len() as u64,
                max: declared.limits.max,
                shared: declared.limits.shared,
            },
            table64: declared.table64,
        }
    }

    // ---- cell accessors for the JS-API wrapper objects (Cut 10 wave 3b) ----
    //
    // The runtime's `WebAssembly.Memory`/`Table`/`Global` wrappers hold these
    // store cell ids and read/write the shared cells through this surface so
    // imports alias the exporter's state.

    /// A global cell's declared type.
    pub fn global_type(&self, cell: usize) -> Option<GlobalType> {
        self.global_types.get(cell).copied()
    }

    /// A memory cell's declared type.
    pub fn memory_type(&self, cell: usize) -> Option<MemType> {
        self.memory_types.get(cell).copied()
    }

    /// A table cell's declared type.
    pub fn table_type(&self, cell: usize) -> Option<TableType> {
        self.table_types.get(cell).copied()
    }

    /// A global cell's current value.
    pub fn global_value(&self, cell: usize) -> Option<Value> {
        self.globals.get(cell).copied()
    }

    /// Overwrite a global cell. The caller is responsible for checking
    /// mutability and value-type compatibility first.
    pub fn set_global(&mut self, cell: usize, value: Value) -> bool {
        match self.globals.get_mut(cell) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }

    /// A tag cell's declared payload type.
    pub fn tag_type(&self, cell: usize) -> Option<FuncType> {
        self.tags.get(cell).map(|tag| tag.ty.clone())
    }

    /// Allocate a thrown exception instance carrying `tag`'s payload `args`;
    /// returns the exception id an escaping run reports. Used both by the
    /// interpreter's `throw` and by the JS-API when a JS-thrown
    /// `WebAssembly.Exception` enters wasm.
    pub fn new_exception(&mut self, tag: usize, args: Vec<Value>) -> usize {
        self.exceptions.push(ExceptionInst { tag, args });
        self.exceptions.len() - 1
    }

    /// The tag cell an exception instance was thrown with.
    pub fn exception_tag(&self, id: usize) -> Option<usize> {
        self.exceptions.get(id).map(|exception| exception.tag)
    }

    /// The payload of an exception instance.
    pub fn exception_args(&self, id: usize) -> Option<Vec<Value>> {
        self.exceptions
            .get(id)
            .map(|exception| exception.args.clone())
    }

    /// A memory cell's current size in pages.
    pub fn memory_size(&self, cell: usize) -> Option<u64> {
        self.memories.get(cell).map(Memory::pages)
    }

    /// Grow a memory cell by `delta` pages, enforcing the cell's declared
    /// maximum (and the memory32 2^16-page cap); the old size in pages on
    /// success.
    pub fn grow_memory(&mut self, cell: usize, delta: u64) -> Option<u64> {
        let memory64 = self.memory_types.get(cell)?.memory64;
        self.memories.get_mut(cell)?.grow(delta, memory64)
    }

    /// A memory cell's current bytes (the whole linear memory).
    pub fn memory_bytes(&self, cell: usize) -> Option<&[u8]> {
        self.memories
            .get(cell)
            .map(|memory| memory.bytes.as_slice())
    }

    /// Overwrite a whole memory cell. The caller matches lengths; returns
    /// false when the cell is unknown or the byte slice does not fill it.
    pub fn write_memory(&mut self, cell: usize, bytes: &[u8]) -> bool {
        match self.memories.get_mut(cell) {
            Some(memory) if memory.bytes.len() == bytes.len() => {
                memory.bytes.copy_from_slice(bytes);
                true
            }
            _ => false,
        }
    }

    /// A table cell's current length.
    pub fn table_size(&self, cell: usize) -> Option<u64> {
        self.tables
            .get(cell)
            .map(|table| table.elements.len() as u64)
    }

    /// Read one table slot (an out-of-bounds index is a table-access trap).
    pub fn table_get(&self, cell: usize, index: u64) -> Option<Result<RefValue, Trap>> {
        let table = self.tables.get(cell)?;
        Some(match table.elements.get(index as usize) {
            Some(value) => Ok(*value),
            None => Err(Trap::OutOfBoundsTableAccess),
        })
    }

    /// Write one table slot (an out-of-bounds index is a table-access trap).
    pub fn table_set(
        &mut self,
        cell: usize,
        index: u64,
        value: RefValue,
    ) -> Option<Result<(), Trap>> {
        let table = self.tables.get_mut(cell)?;
        Some(match table.elements.get_mut(index as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Trap::OutOfBoundsTableAccess),
        })
    }

    /// Grow a table cell by `delta` slots, filling with `init`; the old
    /// length on success (a 32-bit table caps at 2^32-1 slots).
    pub fn grow_table(&mut self, cell: usize, delta: u64, init: RefValue) -> Option<u64> {
        let table64 = self.table_types.get(cell)?.table64;
        self.tables.get_mut(cell)?.grow(delta, init, table64)
    }
}

/// One invocation parked at an external-host-call boundary: the operand and
/// frame stacks exactly as the engine left them ([`Store::resume`] pops the
/// entry, pushes the host results, and continues).
struct Suspended {
    stack: Vec<Value>,
    frames: Vec<Frame>,
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
    /// An external host function was called: the run must park and let the
    /// resumable driver supply its results (`token`, `args`).
    Host { token: u64, args: Vec<Value> },
}

/// Where an in-flight exception's unwind stopped.
enum CatchResult {
    /// No clause in the scanned frame matched; pop the frame and keep going.
    Miss,
    /// A clause matched and branched; execution continues (pc already set).
    Caught,
    /// A clause targeted the outermost frame's function label.
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
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) | Instr::TryTable { .. } => {
                opens.push(pc)
            }
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

    fn pop_i64(&mut self) -> Result<i64, ExecFail> {
        match self.pop()? {
            Value::I64(v) => Ok(v),
            _ => Err(ExecFail::Trap(Trap::Unreachable)),
        }
    }

    /// The store cell a memory instruction's `memidx` resolves to.
    fn mem_cell(&self, instance: usize, memory: u32) -> Result<usize, ExecFail> {
        self.store.instances[instance]
            .memories
            .get(memory as usize)
            .copied()
            .ok_or(ExecFail::Unsupported("no memory"))
    }

    /// Whether the memory at `memidx` is a memory64 (its address operand and
    /// `memory.size/grow` values are i64).
    fn memory_is64(&self, instance: usize, memory: u32) -> Result<bool, ExecFail> {
        let cell = self.mem_cell(instance, memory)?;
        Ok(self.store.memory_types[cell].memory64)
    }

    /// Pop a memory instruction's address operand, sized by the referenced
    /// memory's index type (i32 for a memory32, i64 for a memory64), widened
    /// to the unsigned u64 used for effective-address arithmetic.
    fn pop_mem_addr(&mut self, instance: usize, memory: u32) -> Result<u64, ExecFail> {
        if self.memory_is64(instance, memory)? {
            Ok(self.pop_i64()? as u64)
        } else {
            Ok(self.pop_i32()? as u32 as u64)
        }
    }

    /// The table cell the top frame addresses at `table_index`.
    fn table_cell(&self, frame_index: usize, table_index: usize) -> Result<usize, ExecFail> {
        let instance = self.frames[frame_index].instance;
        self.store.instances[instance]
            .tables
            .get(table_index)
            .copied()
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))
    }

    /// Whether the table at `table_index` is a table64 (its address operands
    /// and `table.size/grow` values are i64).
    fn table_is64(&self, frame_index: usize, table_index: usize) -> Result<bool, ExecFail> {
        let cell = self.table_cell(frame_index, table_index)?;
        Ok(self.store.table_types[cell].table64)
    }

    /// Pop a table instruction's address operand, sized by the referenced
    /// table's index type (i32 for a 32-bit table, i64 for a table64),
    /// widened to the unsigned u64 used for bounds checks.
    fn pop_table_addr(&mut self, frame_index: usize, table_index: usize) -> Result<u64, ExecFail> {
        if self.table_is64(frame_index, table_index)? {
            Ok(self.pop_i64()? as u64)
        } else {
            Ok(self.pop_i32()? as u32 as u64)
        }
    }

    /// Run until the outermost frame returns (or an external host function is
    /// reached, parking the run so the resumable driver can supply its
    /// results). A `throw`/`throw_ref` inside any frame becomes an in-flight
    /// exception that unwinds to the nearest matching `try_table` catch
    /// clause (across call frames); only an exception no handler matches
    /// reaches the caller of `run`.
    fn run(&mut self) -> Result<RunProgress, ExecFail> {
        loop {
            // A parked run with no frames left (a top-level external-host
            // invocation, or an outermost tail call to one) is done: the
            // operand stack holds the results.
            if self.frames.is_empty() {
                return Ok(RunProgress::Finished(self.stack.split_off(0)));
            }
            let ctl = match self.step() {
                Ok(ctl) => ctl,
                Err(ExecFail::Exception(exn)) => match self.unwind(exn)? {
                    Some(results) => return Ok(RunProgress::Finished(results)),
                    None => continue,
                },
                Err(error) => return Err(error),
            };
            match ctl {
                Ctl::Next => {
                    let top = self.frames.len() - 1;
                    self.frames[top].pc += 1;
                }
                Ctl::Settled => {}
                Ctl::Finished(results) => return Ok(RunProgress::Finished(results)),
                Ctl::Host { token, args } => {
                    self.store.suspended.push(Suspended {
                        stack: std::mem::take(&mut self.stack),
                        frames: std::mem::take(&mut self.frames),
                    });
                    return Ok(RunProgress::Host(HostRequest { token, args }));
                }
            }
        }
    }

    /// Deliver an in-flight exception (a cell in `store.exceptions`) to the
    /// nearest matching catch clause, popping call frames that cannot handle
    /// it. Returns `Some(results)` when a catch targeted the outermost
    /// frame's function label (the invocation returns those results),
    /// `None` when execution continues after a caught branch, and the
    /// original error when the exception escapes to the host.
    fn unwind(&mut self, exn: usize) -> Result<Option<Vec<Value>>, ExecFail> {
        let (tag, args) = {
            let exception = &self.store.exceptions[exn];
            (exception.tag, exception.args.clone())
        };
        loop {
            match self.catch_in_top_frame(tag, &args, exn)? {
                CatchResult::Miss => {}
                CatchResult::Caught => return Ok(None),
                CatchResult::Finished(results) => return Ok(Some(results)),
            }
            if self.frames.len() == 1 {
                return Err(ExecFail::Exception(exn));
            }
            // The top frame had no handler: abandon it (and its operand-stack
            // contribution) and continue searching in its caller.
            let frame = self.frames.pop().unwrap();
            self.stack.truncate(frame.base);
        }
    }

    /// Scan the top frame's active `try_table` scopes (labels still on the
    /// stack) for the first catch clause matching `tag`, innermost scope
    /// first, and branch to its target label with the clause's payload when
    /// one matches.
    fn catch_in_top_frame(
        &mut self,
        tag: usize,
        args: &[Value],
        exn: usize,
    ) -> Result<CatchResult, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let count = self.frames[frame_index].labels.len();
        for pos in (0..count).rev() {
            let open = self.frames[frame_index].labels[pos].open;
            let instr = self.body(frame_index).get(open).cloned();
            let clauses = match instr {
                Some(Instr::TryTable { catches, .. }) => catches,
                _ => continue,
            };
            // The clause tags are indices into this frame's tag space; the
            // thrown exception carries the tag's store cell, so resolve each
            // clause against the same instance before comparing.
            let instance = self.frames[frame_index].instance;
            let cells: Vec<Option<usize>> = clauses
                .iter()
                .map(|clause| match clause {
                    Catch::Tag { tag, .. } | Catch::TagRef { tag, .. } => self.store.instances
                        [instance]
                        .tags
                        .get(*tag as usize)
                        .copied(),
                    Catch::All { .. } | Catch::AllRef { .. } => None,
                })
                .collect();
            for (clause, cell) in clauses.iter().zip(&cells) {
                match (clause, cell) {
                    (Catch::Tag { label, .. }, Some(cell)) if *cell == tag => {
                        return self.catch_branch(pos, *label, args.to_vec());
                    }
                    (Catch::TagRef { label, .. }, Some(cell)) if *cell == tag => {
                        let mut payload = args.to_vec();
                        payload.push(Value::Ref(RefValue::Exn(exn)));
                        return self.catch_branch(pos, *label, payload);
                    }
                    (Catch::All { label }, _) => {
                        return self.catch_branch(pos, *label, Vec::new());
                    }
                    (Catch::AllRef { label }, _) => {
                        return self.catch_branch(
                            pos,
                            *label,
                            vec![Value::Ref(RefValue::Exn(exn))],
                        );
                    }
                    (Catch::Tag { .. } | Catch::TagRef { .. }, _) => {}
                }
            }
        }
        Ok(CatchResult::Miss)
    }

    /// Perform the branch a matched catch clause requests: drop the try's
    /// inner labels plus the try label itself, then deliver `payload` to the
    /// clause's target label (relative to the labels that enclosed the
    /// `try_table`), exactly like a `br` to that label.
    fn catch_branch(
        &mut self,
        pos: usize,
        label: u32,
        payload: Vec<Value>,
    ) -> Result<CatchResult, ExecFail> {
        let frame_index = self.frames.len() - 1;
        self.frames[frame_index].labels.truncate(pos);
        let target_pos = pos
            .checked_sub(1 + label as usize)
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let target = self.frames[frame_index].labels[target_pos];
        self.stack.truncate(target.height);
        self.stack.extend(payload);
        if target_pos == 0 {
            // The clause targeted the function label: the payload is this
            // frame's results, so return from it.
            self.frames.pop();
            if self.frames.is_empty() {
                let results = self.stack.split_off(0);
                return Ok(CatchResult::Finished(results));
            }
            return Ok(CatchResult::Caught);
        }
        let map = self.map_for(frame_index);
        if target.is_loop {
            self.frames[frame_index].labels.truncate(target_pos + 1);
            self.frames[frame_index].pc = target.open + 1;
        } else {
            self.frames[frame_index].labels.truncate(target_pos);
            self.frames[frame_index].pc = map.end[target.open] + 1;
        }
        Ok(CatchResult::Caught)
    }

    /// Execute one instruction of the top frame.
    fn step(&mut self) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        if self.frames[frame_index].pc >= self.body(frame_index).len() {
            return self.finish();
        }
        let pc = self.frames[frame_index].pc;
        let instr = self.body(frame_index)[pc].clone();
        match instr {
            Instr::Unreachable => Err(ExecFail::Trap(Trap::Unreachable)),
            Instr::Nop => Ok(Ctl::Next),
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) | Instr::TryTable { .. } => {
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
            Instr::Call(index) => self.do_call(index as usize),
            Instr::ReturnCall(index) => self.do_tail_call(index as usize),
            Instr::CallIndirect {
                type_index,
                table_index,
            } => self.call_indirect(type_index as usize, table_index as usize),
            Instr::ReturnCallIndirect {
                type_index,
                table_index,
            } => self.return_call_indirect(type_index as usize, table_index as usize),
            Instr::CallRef(type_index) => self.call_ref(type_index as usize),
            Instr::ReturnCallRef(type_index) => self.return_call_ref(type_index as usize),
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
            Instr::TableGet(table) => {
                let frame_index = self.frames.len() - 1;
                let index = self.pop_table_addr(frame_index, table as usize)?;
                let cell = self.table_cell(frame_index, table as usize)?;
                let elements = &self.store.tables[cell].elements;
                if index >= elements.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let value = Value::Ref(elements[index as usize]);
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::TableSet(table) => {
                let value = self.pop()?;
                let frame_index = self.frames.len() - 1;
                let index = self.pop_table_addr(frame_index, table as usize)?;
                let cell = self.table_cell(frame_index, table as usize)?;
                let elements = &mut self.store.tables[cell].elements;
                if index >= elements.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("non-reference table value"));
                };
                elements[index as usize] = reference;
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
            // ---- v128 (Cut 7) ----
            Instr::V128Const(bits) => {
                self.stack.push(Value::V128(bits));
                Ok(Ctl::Next)
            }
            Instr::VecLoad {
                memory, op, offset, ..
            } => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = vec_load_bytes(op) as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let bytes = &self.store.memories[cell].bytes[start..start + size as usize];
                let value = load_vec(op, bytes).ok_or(ExecFail::Unsupported("simd load form"))?;
                self.stack.push(Value::V128(value));
                Ok(Ctl::Next)
            }
            Instr::VecStore { memory, offset, .. } => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let Value::V128(bits) = value else {
                    return Err(ExecFail::Unsupported("non-v128 store"));
                };
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(16).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                self.store.memories[cell].bytes[start..start + 16]
                    .copy_from_slice(&bits.to_le_bytes());
                Ok(Ctl::Next)
            }
            Instr::VecLaneLoad {
                memory,
                size,
                offset,
                lane,
                ..
            } => {
                let vector = self.pop_v128()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = size as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let mut bits = 0u64;
                for (i, &byte) in self.store.memories[cell].bytes[start..start + size as usize]
                    .iter()
                    .enumerate()
                {
                    bits |= u64::from(byte) << (8 * i);
                }
                let out = crate::simd::set_lane(vector, lane as usize, size as usize, bits);
                self.stack.push(Value::V128(out));
                Ok(Ctl::Next)
            }
            Instr::VecLaneStore {
                memory,
                size,
                offset,
                lane,
                ..
            } => {
                let vector = self.pop_v128()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = size as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let bits = crate::simd::lane_bits(vector, lane as usize, size as usize);
                for (i, byte) in self.store.memories[cell].bytes[start..start + size as usize]
                    .iter_mut()
                    .enumerate()
                {
                    *byte = (bits >> (8 * i)) as u8;
                }
                Ok(Ctl::Next)
            }
            Instr::VecShuffle(lanes) => {
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                let mut out = 0u128;
                for (i, &lane) in lanes.iter().enumerate() {
                    let source = if lane < 16 { a } else { b };
                    let index = (lane & 15) as usize;
                    let byte = crate::simd::lane_bits(source, index, 1);
                    out = crate::simd::set_lane(out, i, 1, byte);
                }
                self.stack.push(Value::V128(out));
                Ok(Ctl::Next)
            }
            Instr::Vec(sub) => self.simd_exec(sub, None),
            Instr::VecLane { op, lane } => self.simd_exec(op, Some(lane)),
            Instr::Load {
                memory, op, offset, ..
            } => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
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
            Instr::Store {
                memory, op, offset, ..
            } => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
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
            Instr::MemorySize(memory) => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let pages = self.store.memories[cell].pages();
                if self.store.memory_types[cell].memory64 {
                    self.stack.push(Value::I64(pages as i64));
                } else {
                    self.stack.push(Value::I32(pages as i32));
                }
                Ok(Ctl::Next)
            }
            Instr::MemoryGrow(memory) => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let memory64 = self.store.memory_types[cell].memory64;
                let delta = if memory64 {
                    self.pop_i64()? as u64
                } else {
                    self.pop_i32()? as u32 as u64
                };
                let old = self.store.memories[cell].grow(delta, memory64);
                match old {
                    Some(old) if memory64 => self.stack.push(Value::I64(old as i64)),
                    Some(old) => self.stack.push(Value::I32(old as i32)),
                    None if memory64 => self.stack.push(Value::I64(-1)),
                    None => self.stack.push(Value::I32(-1)),
                }
                Ok(Ctl::Next)
            }
            Instr::MemoryInit { data_index, memory } => {
                self.memory_init(data_index as usize, frame_index, memory)
            }
            Instr::DataDrop(data_index) => {
                let instance = self.frames[frame_index].instance;
                self.store.instances[instance].data_segments[data_index as usize] = None;
                Ok(Ctl::Next)
            }
            Instr::MemoryCopy { dst, src } => self.memory_copy(frame_index, dst, src),
            Instr::MemoryFill(memory) => self.memory_fill(frame_index, memory),
            Instr::RefNull(_) => {
                self.stack.push(Value::Ref(RefValue::Null));
                Ok(Ctl::Next)
            }
            Instr::RefIsNull => {
                let value = self.pop()?;
                let is_null = matches!(value, Value::Ref(RefValue::Null));
                self.stack.push(Value::I32(i32::from(is_null)));
                Ok(Ctl::Next)
            }
            Instr::RefFunc(index) => {
                let instance = self.frames[frame_index].instance;
                let value = Value::Ref(RefValue::Func(FuncAddr {
                    instance,
                    index: index as usize,
                }));
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::RefEq => {
                let b = self.pop()?;
                let a = self.pop()?;
                let equal = match (a, b) {
                    (Value::Ref(x), Value::Ref(y)) => x == y,
                    _ => return Err(ExecFail::Unsupported("ref.eq on non-reference")),
                };
                self.stack.push(Value::I32(i32::from(equal)));
                Ok(Ctl::Next)
            }
            Instr::RefAsNonNull => {
                let value = self.pop()?;
                match value {
                    Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullReference)),
                    reference => {
                        self.stack.push(reference);
                        Ok(Ctl::Next)
                    }
                }
            }
            Instr::BrOnNull(label) => {
                let value = self.pop()?;
                if matches!(value, Value::Ref(RefValue::Null)) {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    self.stack.push(value);
                    Ok(Ctl::Next)
                }
            }
            Instr::BrOnNonNull(label) => {
                let value = self.pop()?;
                if !matches!(value, Value::Ref(RefValue::Null)) {
                    self.stack.push(value);
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    // The null is consumed: the branch is not taken and the
                    // value does not continue on the fall-through path.
                    Ok(Ctl::Next)
                }
            }
            Instr::Throw(index) => {
                // Pop the tag's arguments and raise an in-flight exception.
                let frame_index = self.frames.len() - 1;
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance]
                    .tags
                    .get(index as usize)
                    .copied()
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                let param_count = self.store.tags[cell].ty.params.len();
                let args = self.take_top(param_count);
                let exn = self.store.exceptions.len();
                self.store
                    .exceptions
                    .push(ExceptionInst { tag: cell, args });
                Err(ExecFail::Exception(exn))
            }
            Instr::ThrowRef => {
                let value = self.pop()?;
                match value {
                    Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullExceptionReference)),
                    Value::Ref(RefValue::Exn(exn)) => Err(ExecFail::Exception(exn)),
                    _ => Err(ExecFail::Unsupported("throw_ref on non-exception ref")),
                }
            }
            Instr::TableInit {
                element_index,
                table,
            } => self.table_init(element_index as usize, table as usize),
            Instr::ElemDrop(element_index) => {
                let instance = self.frames[frame_index].instance;
                self.store.instances[instance].element_segments[element_index as usize] = None;
                Ok(Ctl::Next)
            }
            Instr::TableCopy { dst, src } => self.table_copy(dst as usize, src as usize),
            Instr::TableGrow(table) => self.table_grow(table as usize),
            Instr::TableSize(table) => self.table_size(table as usize),
            Instr::TableFill(table) => self.table_fill(table as usize),
            // ---- GC aggregates (Cut 9 wave 3) ----
            Instr::StructNew(ty) => {
                let instance = self.frames[frame_index].instance;
                let storage = self.frame_struct_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(storage.len());
                for st in storage.iter().rev() {
                    let value = self.pop()?;
                    cells.push(wrap_cell(*st, value)?);
                }
                cells.reverse();
                let id = alloc_struct(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Struct(id)));
                Ok(Ctl::Next)
            }
            Instr::StructNewDefault(ty) => {
                let instance = self.frames[frame_index].instance;
                let storage = self.frame_struct_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(storage.len());
                for st in storage {
                    cells.push(default_storage(st)?);
                }
                let id = alloc_struct(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Struct(id)));
                Ok(Ctl::Next)
            }
            Instr::StructGet { field, .. } => self.struct_field_read(field as usize, GcRead::Plain),
            Instr::StructGetS { field, .. } => {
                self.struct_field_read(field as usize, GcRead::Signed)
            }
            Instr::StructGetU { field, .. } => {
                self.struct_field_read(field as usize, GcRead::Unsigned)
            }
            Instr::StructSet { field, .. } => {
                let value = self.pop()?;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference struct operand"));
                };
                let RefValue::Struct(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullStructReference)
                    } else {
                        ExecFail::Unsupported("non-struct struct.set operand")
                    });
                };
                let storage = gc_struct_storage(self.store, id)?;
                let st = *storage
                    .get(field as usize)
                    .ok_or(ExecFail::Unsupported("struct field out of range"))?;
                let cell = wrap_cell(st, value)?;
                match &mut self.store.objects[id].data {
                    GcData::Struct(cells) => {
                        cells[field as usize] = cell;
                    }
                    _ => return Err(ExecFail::Unsupported("struct.set of non-struct")),
                }
                Ok(Ctl::Next)
            }
            Instr::ArrayNew(ty) => {
                let instance = self.frames[frame_index].instance;
                let len = self.pop_i32()?;
                let value = self.pop()?;
                let st = self.frame_array_storage(instance, ty)?;
                let cell = wrap_cell(st, value)?;
                let id = alloc_array_filled(self.store, instance, ty, len, cell)?;
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewDefault(ty) => {
                let instance = self.frames[frame_index].instance;
                let len = self.pop_i32()?;
                let st = self.frame_array_storage(instance, ty)?;
                let cell = default_storage(st)?;
                let id = alloc_array_filled(self.store, instance, ty, len, cell)?;
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewFixed { ty, n } => {
                let instance = self.frames[frame_index].instance;
                let st = self.frame_array_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let value = self.pop()?;
                    cells.push(wrap_cell(st, value)?);
                }
                cells.reverse();
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewData { ty, data } => {
                let instance = self.frames[frame_index].instance;
                let st = self.frame_array_storage(instance, ty)?;
                let n = self.pop_i32()? as u32 as u64;
                let src = self.pop_i32()? as u32 as u64;
                let width = u64::from(
                    storage_width(st).ok_or(ExecFail::Unsupported("array.new_data of refs"))?
                        as u32,
                );
                let instance2 = self.frames[frame_index].instance;
                // A dropped data segment behaves as empty (spec: `data.drop`
                // empties the segment, so zero-length uses still succeed).
                let bytes = self.store.instances[instance2].data_segments[data as usize]
                    .as_deref()
                    .unwrap_or(&[]);
                let total = n
                    .checked_mul(width)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let end = src
                    .checked_add(total)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                if end > bytes.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let mut cells = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let at = (src + i * width) as usize;
                    cells.push(
                        data_element(st, bytes, at)
                            .ok_or(ExecFail::Unsupported("array.new_data element read"))?,
                    );
                }
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewElem { ty, elem } => {
                let instance = self.frames[frame_index].instance;
                let n = self.pop_i32()? as u32 as u64;
                let src = self.pop_i32()? as u32 as u64;
                // A dropped element segment behaves as empty.
                let items = self.store.instances[instance].element_segments[elem as usize]
                    .as_deref()
                    .unwrap_or(&[]);
                let end = src
                    .checked_add(n)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))?;
                if end > items.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let cells = (0..n)
                    .map(|i| Value::Ref(items[(src + i) as usize]))
                    .collect();
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayGet(ty) => self.array_read(ty, GcRead::Plain),
            Instr::ArrayGetS(ty) => self.array_read(ty, GcRead::Signed),
            Instr::ArrayGetU(ty) => self.array_read(ty, GcRead::Unsigned),
            Instr::ArraySet(_ty) => {
                let value = self.pop()?;
                let index = self.pop_i32()?;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("non-array array.set operand")
                    });
                };
                if (index as u32 as usize) >= gc_array_len(self.store, id)? {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                }
                let st = gc_array_storage(self.store, id)?;
                let cell = wrap_cell(st, value)?;
                match &mut self.store.objects[id].data {
                    GcData::Array(cells) => cells[index as u32 as usize] = cell,
                    _ => return Err(ExecFail::Unsupported("array.set of non-array")),
                }
                Ok(Ctl::Next)
            }
            Instr::ArrayLen => {
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("array.len of non-array")
                    });
                };
                let len = match &self.store.objects[id].data {
                    GcData::Array(cells) => cells.len(),
                    _ => return Err(ExecFail::Unsupported("array.len of non-array")),
                };
                self.stack.push(Value::I32(len as i32));
                Ok(Ctl::Next)
            }
            Instr::ArrayFill(_ty) => {
                let n = self.pop_i32()? as u32 as usize;
                let value = self.pop()?;
                let start = self.pop_i32()? as u32 as usize;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("array.fill of non-array")
                    });
                };
                let st = gc_array_storage(self.store, id)?;
                let cell = wrap_cell(st, value)?;
                let cells = match &mut self.store.objects[id].data {
                    GcData::Array(cells) => cells,
                    _ => return Err(ExecFail::Unsupported("array.fill of non-array")),
                };
                let Some(end) = start.checked_add(n) else {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                };
                if end > cells.len() {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                }
                cells[start..end].fill(cell);
                Ok(Ctl::Next)
            }
            Instr::ArrayCopy { .. } => self.array_copy(),
            Instr::ArrayInitData { data, .. } => self.array_init_data(data as usize),
            Instr::ArrayInitElem { elem, .. } => self.array_init_elem(elem as usize),
            Instr::RefTest { nullable, heap } => {
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("ref.test of non-reference"));
                };
                let matches = self.ref_matches(reference, nullable, heap)?;
                self.stack.push(Value::I32(i32::from(matches)));
                Ok(Ctl::Next)
            }
            Instr::RefCast { nullable, heap } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("ref.cast of non-reference"));
                };
                if self.ref_matches(reference, nullable, heap)? {
                    self.stack.push(Value::Ref(reference));
                    Ok(Ctl::Next)
                } else {
                    // The spec reports every failed cast (including casting a
                    // null to a non-nullable target) as `cast failure`.
                    Err(ExecFail::Trap(Trap::CastFailure))
                }
            }
            Instr::BrOnCast { label, to, .. } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("br_on_cast of non-reference"));
                };
                let matches = self.ref_matches(reference, to.nullable, to.heap)?;
                self.stack.push(Value::Ref(reference));
                if matches {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    Ok(Ctl::Next)
                }
            }
            Instr::BrOnCastFail { label, to, .. } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("br_on_cast_fail of non-reference"));
                };
                let matches = self.ref_matches(reference, to.nullable, to.heap)?;
                self.stack.push(Value::Ref(reference));
                if matches {
                    Ok(Ctl::Next)
                } else if self.branch_to(label as usize)? {
                    self.finish()
                } else {
                    Ok(Ctl::Settled)
                }
            }
            Instr::AnyConvertExtern => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("any.convert_extern of non-reference"));
                };
                self.stack.push(Value::Ref(reference.unwrap_extern()));
                Ok(Ctl::Next)
            }
            Instr::ExternConvertAny => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("extern.convert_any of non-reference"));
                };
                let wrapped = match reference {
                    RefValue::Null => RefValue::Null,
                    other => match ExternInner::wrap(other) {
                        Some(inner) => RefValue::Extern(inner),
                        None => {
                            return Err(ExecFail::Unsupported(
                                "extern.convert_any of unboxable reference",
                            ));
                        }
                    },
                };
                self.stack.push(Value::Ref(wrapped));
                Ok(Ctl::Next)
            }
            Instr::RefI31 => {
                let value = self.pop_i32()?;
                self.stack.push(Value::Ref(RefValue::i31(value)));
                Ok(Ctl::Next)
            }
            Instr::I31GetS => self.i31_read(true),
            Instr::I31GetU => self.i31_read(false),
        }
    }

    // ---- GC execution helpers ----

    /// The struct field storage types declared at type index `ty` of the
    /// given instance's module (cloned so no borrow outlives the call).
    fn frame_struct_storage(&self, instance: usize, ty: u32) -> Result<Vec<StorageType>, ExecFail> {
        let module = &self.store.instances[instance].module;
        match struct_field_types(module, ty) {
            Some(fields) => Ok(fields.iter().map(|field| field.ty).collect()),
            None => Err(ExecFail::Unsupported("unknown struct type")),
        }
    }

    /// The array element storage declared at type index `ty` of the given
    /// instance's module.
    fn frame_array_storage(&self, instance: usize, ty: u32) -> Result<StorageType, ExecFail> {
        let module = &self.store.instances[instance].module;
        match array_element_type(module, ty) {
            Some(field) => Ok(field.ty),
            None => Err(ExecFail::Unsupported("unknown array type")),
        }
    }

    fn pop_struct_ref(&mut self) -> Result<usize, ExecFail> {
        let value = self.pop()?;
        let Value::Ref(reference) = value else {
            return Err(ExecFail::Unsupported("non-reference struct operand"));
        };
        match reference {
            RefValue::Struct(id) => Ok(id),
            RefValue::Null => Err(ExecFail::Trap(Trap::NullStructReference)),
            _ => Err(ExecFail::Unsupported("struct operand is not a struct")),
        }
    }

    fn pop_array_ref(&mut self) -> Result<usize, ExecFail> {
        let value = self.pop()?;
        let Value::Ref(reference) = value else {
            return Err(ExecFail::Unsupported("non-reference array operand"));
        };
        match reference {
            RefValue::Array(id) => Ok(id),
            RefValue::Null => Err(ExecFail::Trap(Trap::NullArrayReference)),
            _ => Err(ExecFail::Unsupported("array operand is not an array")),
        }
    }

    /// `struct.get[_s|_u]` on the popped object.
    fn struct_field_read(&mut self, field: usize, mode: GcRead) -> Result<Ctl, ExecFail> {
        let id = self.pop_struct_ref()?;
        let storage = gc_struct_storage(self.store, id)?;
        let st = *storage
            .get(field)
            .ok_or(ExecFail::Unsupported("struct field out of range"))?;
        let cell = match &self.store.objects[id].data {
            GcData::Struct(cells) => *cells
                .get(field)
                .ok_or(ExecFail::Unsupported("struct field out of range"))?,
            _ => return Err(ExecFail::Unsupported("struct.get of non-struct")),
        };
        let out = match mode {
            GcRead::Plain => cell,
            GcRead::Signed => extend_cell(st, cell, true)?,
            GcRead::Unsigned => extend_cell(st, cell, false)?,
        };
        self.stack.push(out);
        Ok(Ctl::Next)
    }

    /// `array.get[_s|_u]`: pop the index, then the array; bounds-checked.
    fn array_read(&mut self, _ty: u32, mode: GcRead) -> Result<Ctl, ExecFail> {
        let index = self.pop_i32()? as u32 as usize;
        let id = self.pop_array_ref()?;
        let st = gc_array_storage(self.store, id)?;
        let cell = match &self.store.objects[id].data {
            GcData::Array(cells) => cells
                .get(index)
                .copied()
                .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?,
            _ => return Err(ExecFail::Unsupported("array.get of non-array")),
        };
        let out = match mode {
            GcRead::Plain => cell,
            GcRead::Signed => extend_cell(st, cell, true)?,
            GcRead::Unsigned => extend_cell(st, cell, false)?,
        };
        self.stack.push(out);
        Ok(Ctl::Next)
    }

    fn i31_read(&mut self, signed: bool) -> Result<Ctl, ExecFail> {
        let reference = self.pop()?;
        let Value::Ref(reference) = reference else {
            return Err(ExecFail::Unsupported("non-reference i31 operand"));
        };
        match reference {
            RefValue::Null => Err(ExecFail::Trap(Trap::NullI31Reference)),
            RefValue::I31(value) => {
                // `get_s` sign-extends bit 30; `get_u` zero-extends it.
                let out = if signed { (value << 1) >> 1 } else { value };
                self.stack.push(Value::I32(out));
                Ok(Ctl::Next)
            }
            _ => Err(ExecFail::Unsupported("i31.get of non-i31")),
        }
    }

    fn array_copy(&mut self) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: dst array, dst offset, src array, src
        // offset, length.
        let n = self.pop_i32()? as u32 as usize;
        let src_offset = self.pop_i32()? as u32 as usize;
        let src = self.pop_array_ref()?;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let dst = self.pop_array_ref()?;
        let src_len = gc_array_len(self.store, src)?;
        let dst_len = gc_array_len(self.store, dst)?;
        let Some(src_end) = src_offset.checked_add(n) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        };
        let Some(dst_end) = dst_offset.checked_add(n) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        };
        if src_end > src_len || dst_end > dst_len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // Copy through a temporary so overlapping source/destination ranges
        // behave like memmove.
        let values = match &self.store.objects[src].data {
            GcData::Array(cells) => cells[src_offset..src_end].to_vec(),
            _ => return Err(ExecFail::Unsupported("array.copy of non-array")),
        };
        match &mut self.store.objects[dst].data {
            GcData::Array(cells) => cells[dst_offset..dst_end].copy_from_slice(&values),
            _ => return Err(ExecFail::Unsupported("array.copy of non-array")),
        }
        Ok(Ctl::Next)
    }

    fn array_init_data(&mut self, data: usize) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: array, dst offset, data offset, length.
        let n = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let array = self.pop_array_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let st = gc_array_storage(self.store, array)?;
        let width = u64::from(
            storage_width(st).ok_or(ExecFail::Unsupported("array.init_data of refs"))? as u32,
        );
        let len = gc_array_len(self.store, array)?;
        let dst_end = dst_offset
            .checked_add(n as usize)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?;
        if dst_end > len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // A dropped data segment behaves as empty (zero-length uses succeed).
        let bytes = self.store.instances[instance].data_segments[data]
            .as_deref()
            .unwrap_or(&[]);
        let total = n
            .checked_mul(width)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
        let data_end = src
            .checked_add(total)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
        if data_end > bytes.len() as u64 {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let cells = match &mut self.store.objects[array].data {
            GcData::Array(cells) => cells,
            _ => return Err(ExecFail::Unsupported("array.init_data of non-array")),
        };
        for i in 0..n {
            let at = (src + i * width) as usize;
            let value = data_element(st, bytes, at)
                .ok_or(ExecFail::Unsupported("array.init_data element read"))?;
            cells[dst_offset + i as usize] = wrap_cell(st, value)?;
        }
        Ok(Ctl::Next)
    }

    fn array_init_elem(&mut self, elem: usize) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: array, dst offset, elem offset, length.
        let n = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let array = self.pop_array_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let len = gc_array_len(self.store, array)?;
        let dst_end = dst_offset
            .checked_add(n as usize)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?;
        if dst_end > len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // A dropped element segment behaves as empty.
        let items = self.store.instances[instance].element_segments[elem]
            .as_deref()
            .unwrap_or(&[]);
        let src_end = src
            .checked_add(n)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))?;
        if src_end > items.len() as u64 {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let cells = match &mut self.store.objects[array].data {
            GcData::Array(cells) => cells,
            _ => return Err(ExecFail::Unsupported("array.init_elem of non-array")),
        };
        for i in 0..n {
            cells[dst_offset + i as usize] = Value::Ref(items[(src + i) as usize]);
        }
        Ok(Ctl::Next)
    }

    /// Whether a runtime reference matches the target reftype (`nullable`,
    /// `heap`) whose heap index is in the top frame's module type space.
    fn ref_matches(
        &self,
        reference: RefValue,
        nullable: bool,
        heap: HeapType,
    ) -> Result<bool, ExecFail> {
        if reference == RefValue::Null {
            return Ok(nullable);
        }
        let frame_index = self.frames.len() - 1;
        let frame_instance = self.frames[frame_index].instance;
        let concrete = match reference {
            RefValue::Func(addr) => match self.func_type_index(addr) {
                Some(ty) => Some((addr.instance, ty)),
                None => return Ok(runtime_abs_matches(HeapType::Func, heap)),
            },
            RefValue::Struct(id) => Some((self.store.objects[id].owner, self.store.objects[id].ty)),
            RefValue::Array(id) => Some((self.store.objects[id].owner, self.store.objects[id].ty)),
            RefValue::Host(_) => return Ok(runtime_abs_matches(HeapType::Any, heap)),
            RefValue::I31(_) => return Ok(runtime_abs_matches(HeapType::I31, heap)),
            RefValue::Extern(_) => return Ok(runtime_abs_matches(HeapType::Extern, heap)),
            RefValue::Exn(_) => return Ok(runtime_abs_matches(HeapType::Exn, heap)),
            RefValue::Null => unreachable!(),
        };
        let Some((owner, ty)) = concrete else {
            return Ok(false);
        };
        let module = &self.store.instances[owner].module;
        let composite = module
            .types
            .get(ty as usize)
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?
            .composite
            .clone();
        let matches = match heap {
            HeapType::Type(target) => {
                let owner_module = &self.store.instances[owner].module;
                let frame_module = &self.store.instances[frame_instance].module;
                if owner == frame_instance {
                    owner_module.type_is_subtype(ty, target)
                } else {
                    // A cross-module cast: the object's concrete type must be
                    // equivalent to the target, or reach it through the owner's
                    // declared supertype chain (each edge compared by
                    // equivalence against the frame module's target).
                    let mut current = ty;
                    loop {
                        if crate::module::type_indices_equivalent(
                            &frame_module.types,
                            &frame_module.rec_groups,
                            target,
                            &owner_module.types,
                            &owner_module.rec_groups,
                            current,
                        ) {
                            break true;
                        }
                        let Some(parent) = owner_module
                            .types
                            .get(current as usize)
                            .and_then(|sub| sub.supertypes.first())
                            .copied()
                        else {
                            break false;
                        };
                        current = parent;
                    }
                }
            }
            abstract_target => {
                let kind = match &composite {
                    CompositeType::Func(_) => HeapType::Func,
                    CompositeType::Struct(_) => HeapType::Struct,
                    CompositeType::Array(_) => HeapType::Array,
                };
                runtime_abs_matches(kind, abstract_target)
            }
        };
        Ok(matches)
    }

    /// Whether a resolved function target satisfies a declared module type
    /// index of the top frame's module (see [`Store::func_type_matches`]).
    fn func_type_matches(&self, module: &Module, type_index: u32, target: FuncTarget) -> bool {
        self.store.func_type_matches(module, type_index, target)
    }

    /// The type index of a function reference's declared type, within the
    /// module instance that owns the reference. Function references cannot be
    /// resolved to a declared type only when they are bound to a host
    /// function from outside the module's type space (treated as abstract
    /// `func` by the caller).
    fn func_type_index(&self, addr: FuncAddr) -> Option<u32> {
        let instance = self.store.instances.get(addr.instance)?;
        let func_imports = instance
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.desc, ImportDesc::Func(_)))
            .count();
        if addr.index < func_imports {
            instance
                .module
                .imports
                .iter()
                .filter_map(|import| match import.desc {
                    ImportDesc::Func(ty) => Some(ty),
                    _ => None,
                })
                .nth(addr.index)
        } else {
            instance
                .module
                .functions
                .get(addr.index - func_imports)
                .copied()
        }
    }

    // ---- bulk memory ----

    fn memory_init(
        &mut self,
        data_index: usize,
        frame_index: usize,
        memory: u32,
    ) -> Result<Ctl, ExecFail> {
        // Operands (bottom to top): dst address, data offset, length; the
        // data offset and length are i32 in both address models.
        let len = self.pop_i32()? as u32 as usize;
        let src = self.pop_i32()? as u32 as usize;
        let instance = self.frames[frame_index].instance;
        let cell = self.mem_cell(instance, memory)?;
        let dst = self.pop_mem_addr(instance, memory)? as usize;
        let Some(segment) = self.store.instances[instance].data_segments.get(data_index) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        };
        let source = segment.as_deref().unwrap_or(&[]);
        let ok = dst
            .checked_add(len)
            .is_some_and(|end| end <= self.store.memories[cell].bytes.len())
            && src.checked_add(len).is_some_and(|end| end <= source.len());
        if !ok {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let bytes = &mut self.store.memories[cell].bytes;
        bytes[dst..dst + len].copy_from_slice(&source[src..src + len]);
        Ok(Ctl::Next)
    }

    fn memory_copy(
        &mut self,
        frame_index: usize,
        dst_memory: u32,
        src_memory: u32,
    ) -> Result<Ctl, ExecFail> {
        let instance = self.frames[frame_index].instance;
        // The length is the smaller of the two memories' address types.
        let len =
            if self.memory_is64(instance, dst_memory)? && self.memory_is64(instance, src_memory)? {
                self.pop_i64()? as u64
            } else {
                u64::from(self.pop_i32()? as u32)
            };
        let src = self.pop_mem_addr(instance, src_memory)?;
        let dst = self.pop_mem_addr(instance, dst_memory)?;
        let dst_cell = self.mem_cell(instance, dst_memory)?;
        let src_cell = self.mem_cell(instance, src_memory)?;
        let dst_size = self.store.memories[dst_cell].bytes.len() as u64;
        let src_size = self.store.memories[src_cell].bytes.len() as u64;
        if dst.checked_add(len).is_none_or(|end| end > dst_size)
            || src.checked_add(len).is_none_or(|end| end > src_size)
        {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        if dst_cell == src_cell {
            // Copy as if through a temporary, so overlapping regions behave
            // like a memmove (Vec::copy_within does exactly that).
            let bytes = &mut self.store.memories[dst_cell].bytes;
            bytes.copy_within(src..src + len, dst);
        } else {
            let source = self.store.memories[src_cell].bytes[src..src + len].to_vec();
            self.store.memories[dst_cell].bytes[dst..dst + len].copy_from_slice(&source);
        }
        Ok(Ctl::Next)
    }

    fn memory_fill(&mut self, frame_index: usize, memory: u32) -> Result<Ctl, ExecFail> {
        let instance = self.frames[frame_index].instance;
        let cell = self.mem_cell(instance, memory)?;
        // Operands (bottom to top): dst address, byte value, length; all of
        // the address type except the value, which is i32.
        let len = self.pop_mem_addr(instance, memory)? as usize;
        let value = self.pop_i32()? as u8;
        let dst = self.pop_mem_addr(instance, memory)? as usize;
        let bytes = &mut self.store.memories[cell].bytes;
        if dst.checked_add(len).is_none_or(|end| end > bytes.len()) {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        bytes[dst..dst + len].fill(value);
        Ok(Ctl::Next)
    }

    // ---- table instructions ----

    fn table_init(&mut self, element_index: usize, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        // Operands (bottom to top): dst index, elem offset, length; the elem
        // offset and length are i32 in both table address models.
        let len = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst = self.pop_table_addr(frame_index, table)?;
        let instance = self.frames[frame_index].instance;
        let cell = self.table_cell(frame_index, table)?;
        let Some(segment) = self.store.instances[instance]
            .element_segments
            .get(element_index)
        else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        };
        let source = segment.as_deref().unwrap_or(&[]);
        let size = self.store.tables[cell].elements.len() as u64;
        let ok = dst.checked_add(len).is_some_and(|end| end <= size)
            && src
                .checked_add(len)
                .is_some_and(|end| end <= source.len() as u64);
        if !ok {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        let elements = &mut self.store.tables[cell].elements;
        elements[dst..dst + len].copy_from_slice(&source[src..src + len]);
        Ok(Ctl::Next)
    }

    fn table_copy(&mut self, dst_table: usize, src_table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        // The length is the smaller of the two tables' address types.
        let len = if self.table_is64(frame_index, dst_table)?
            && self.table_is64(frame_index, src_table)?
        {
            self.pop_i64()? as u64
        } else {
            u64::from(self.pop_i32()? as u32)
        };
        let src = self.pop_table_addr(frame_index, src_table)?;
        let dst = self.pop_table_addr(frame_index, dst_table)?;
        let dst_cell = self.table_cell(frame_index, dst_table)?;
        let src_cell = self.table_cell(frame_index, src_table)?;
        let dst_size = self.store.tables[dst_cell].elements.len() as u64;
        let src_size = self.store.tables[src_cell].elements.len() as u64;
        if dst.checked_add(len).is_none_or(|end| end > dst_size)
            || src.checked_add(len).is_none_or(|end| end > src_size)
        {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        if dst_cell == src_cell {
            // Copy as if through a temporary, so overlapping regions behave
            // like a memmove (Vec::copy_within does exactly that).
            let elements = &mut self.store.tables[dst_cell].elements;
            elements.copy_within(src..src + len, dst);
        } else {
            let source = self.store.tables[src_cell].elements[src..src + len].to_vec();
            self.store.tables[dst_cell].elements[dst..dst + len].copy_from_slice(&source);
        }
        Ok(Ctl::Next)
    }

    fn table_grow(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let table64 = self.table_is64(frame_index, table)?;
        let delta = if table64 {
            self.pop_i64()? as u64
        } else {
            u64::from(self.pop_i32()? as u32)
        };
        let init = self.pop()?;
        let Value::Ref(init) = init else {
            return Err(ExecFail::Unsupported("non-reference table value"));
        };
        let cell = self.table_cell(frame_index, table)?;
        let old = self.store.tables[cell].grow(delta, init, table64);
        match old {
            Some(old) if table64 => self.stack.push(Value::I64(old as i64)),
            Some(old) => self.stack.push(Value::I32(old as i32)),
            None if table64 => self.stack.push(Value::I64(-1)),
            None => self.stack.push(Value::I32(-1)),
        }
        Ok(Ctl::Next)
    }

    fn table_size(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let cell = self.table_cell(frame_index, table)?;
        let size = self.store.tables[cell].elements.len() as u64;
        if self.table_is64(frame_index, table)? {
            self.stack.push(Value::I64(size as i64));
        } else {
            self.stack.push(Value::I32(size as i32));
        }
        Ok(Ctl::Next)
    }

    fn table_fill(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let len = self.pop_table_addr(frame_index, table)? as usize;
        let value = self.pop()?;
        let Value::Ref(value) = value else {
            return Err(ExecFail::Unsupported("non-reference table value"));
        };
        let dst = self.pop_table_addr(frame_index, table)? as usize;
        let cell = self.table_cell(frame_index, table)?;
        let elements = &mut self.store.tables[cell].elements;
        if dst.checked_add(len).is_none_or(|end| end > elements.len()) {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        elements[dst..dst + len].fill(value);
        Ok(Ctl::Next)
    }

    fn advance(&mut self, next: usize) {
        let frame_index = self.frames.len() - 1;
        self.frames[frame_index].pc = next;
    }

    /// Pop a v128 value (validated code only ever has one here).
    fn pop_v128(&mut self) -> Result<u128, ExecFail> {
        match self.pop()? {
            Value::V128(value) => Ok(value),
            _ => Err(ExecFail::Unsupported("expected v128 operand")),
        }
    }

    /// Execute a pure-register (or lane-immediate) v128 op. `lane` is present
    /// for the extract/replace forms.
    fn simd_exec(&mut self, sub: u16, lane: Option<u8>) -> Result<Ctl, ExecFail> {
        use crate::simd::VecSig;
        let unsupported = || ExecFail::Unsupported("simd opcode");
        // Relaxed SIMD ops take a fixed operand count and one deterministic
        // behavior each (the spec allows a set of results per op); dispatch
        // them before the category match so the ternary forms do not fall
        // into the bitselect-only `Ternop` arm.
        if crate::simd::is_relaxed(sub) {
            let arity = crate::simd::relaxed_arity(sub);
            let c = (arity == 3).then(|| self.pop_v128()).transpose()?;
            let b = (arity >= 2).then(|| self.pop_v128()).transpose()?;
            let a = self.pop_v128()?;
            let out = crate::simd::exec_relaxed(sub, a, b, c).ok_or_else(unsupported)?;
            self.stack.push(Value::V128(out));
            return Ok(Ctl::Next);
        }
        match crate::simd::sig(sub) {
            Some(VecSig::Not) => {
                let v = self.pop_v128()?;
                self.stack.push(Value::V128(crate::simd::exec_not(v)));
            }
            Some(VecSig::Unop) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_unop(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Binop) => {
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                let out = crate::simd::exec_binop(sub, a, b).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Ternop) => {
                let c = self.pop_v128()?;
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                self.stack
                    .push(Value::V128(crate::simd::exec_bitselect(a, b, c)));
            }
            Some(VecSig::Shift) => {
                let count = self.pop_i32()? as u32;
                let v = self.pop_v128()?;
                let out = crate::simd::exec_shift(sub, v, count).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Splat(kind)) => {
                let scalar = self.pop()?;
                let bits =
                    crate::simd::scalar_to_lane_bits(scalar, kind).ok_or_else(unsupported)?;
                let out = crate::simd::exec_splat(sub, bits).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::ExtractS(kind) | VecSig::ExtractU(kind) | VecSig::Extract(kind)) => {
                let v = self.pop_v128()?;
                let index = lane.ok_or_else(unsupported)? as usize;
                let value = crate::simd::exec_extract(sub, v, index).ok_or_else(unsupported)?;
                let _ = kind;
                self.stack.push(value);
            }
            Some(VecSig::Replace(kind)) => {
                let scalar = self.pop()?;
                let bits =
                    crate::simd::scalar_to_lane_bits(scalar, kind).ok_or_else(unsupported)?;
                let v = self.pop_v128()?;
                let index = lane.ok_or_else(unsupported)? as usize;
                let out = crate::simd::exec_replace(sub, v, index, bits).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Test) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_any_all_true(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::I32(out));
            }
            Some(VecSig::Bitmask) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_bitmask(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::I32(out));
            }
            None => return Err(unsupported()),
        }
        Ok(Ctl::Next)
    }

    /// Enter a `block`/`loop`/`if` at `pc` (which is the top frame's pc).
    fn enter_structured(&mut self, pc: usize) -> Result<(), ExecFail> {
        let frame_index = self.frames.len() - 1;
        let instr = self.body(frame_index)[pc].clone();
        let (params, arity) = self.block_counts(&instr)?;
        let height = self.stack.len().saturating_sub(params);
        match instr {
            Instr::Block(_) | Instr::TryTable { .. } => {
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
            Instr::TryTable { blocktype, .. } => *blocktype,
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
                    .and_then(|i| i.module.func_at(index))
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                Ok((ty.params.len(), ty.results.len()))
            }
        }
    }

    fn resolve_target(&self, address: FuncAddr) -> Result<FuncTarget, ExecFail> {
        self.store
            .instances
            .get(address.instance)
            .and_then(|i| i.funcs.get(address.index))
            .copied()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))
    }

    /// Pop a function reference from the operand stack.
    fn pop_func_ref(&mut self) -> Result<FuncAddr, ExecFail> {
        match self.pop()? {
            Value::Ref(RefValue::Func(address)) => Ok(address),
            Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullFunctionReference)),
            _ => Err(ExecFail::Trap(Trap::NullFunctionReference)),
        }
    }

    fn call_indirect(&mut self, type_index: usize, table_index: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let index = self.pop_table_addr(frame_index, table_index)?;
        let instance = self.frames[frame_index].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let cell = self.table_cell(frame_index, table_index)?;
        let elements = &self.store.tables[cell].elements;
        if index >= elements.len() as u64 {
            return Err(ExecFail::Trap(Trap::UndefinedElement));
        }
        let entry = elements[index as usize];
        let RefValue::Func(address) = entry else {
            return Err(ExecFail::Trap(Trap::UninitializedElement));
        };
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.call_target(target)
    }

    fn return_call_indirect(
        &mut self,
        type_index: usize,
        table_index: usize,
    ) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let index = self.pop_table_addr(frame_index, table_index)?;
        let instance = self.frames[frame_index].instance;
        let cell = self.table_cell(frame_index, table_index)?;
        let elements = &self.store.tables[cell].elements;
        if index >= elements.len() as u64 {
            return Err(ExecFail::Trap(Trap::UndefinedElement));
        }
        let entry = elements[index as usize];
        let RefValue::Func(address) = entry else {
            return Err(ExecFail::Trap(Trap::UninitializedElement));
        };
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.tail_target(target)
    }

    fn call_ref(&mut self, type_index: usize) -> Result<Ctl, ExecFail> {
        let address = self.pop_func_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.call_target(target)
    }

    fn return_call_ref(&mut self, type_index: usize) -> Result<Ctl, ExecFail> {
        let address = self.pop_func_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.tail_target(target)
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

    /// Call `target` from the top frame: pop its arguments, push its frame,
    /// run a type-only host body, or suspend for an external host call — and
    /// advance past the call.
    fn call_target(&mut self, target: FuncTarget) -> Result<Ctl, ExecFail> {
        let caller = self.frames.len() - 1;
        match target {
            FuncTarget::Host(id) => {
                let ty = self.store.host_funcs[id].ty.clone();
                match self.store.host_funcs[id].token {
                    Some(token) => {
                        let mut args = Vec::with_capacity(ty.params.len());
                        for _ in 0..ty.params.len() {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        self.frames[caller].pc += 1;
                        Ok(Ctl::Host { token, args })
                    }
                    None => {
                        self.call_host(id)?;
                        let caller = self.frames.len() - 1;
                        self.frames[caller].pc += 1;
                        Ok(Ctl::Settled)
                    }
                }
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
                        .func_at(type_index)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    (
                        signature.params.len(),
                        signature.results.len(),
                        body.locals.clone(),
                    )
                };
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
                self.frames[caller].pc += 1;
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

    /// Make a (direct, non-tail) call from the top frame.
    fn do_call(&mut self, index: usize) -> Result<Ctl, ExecFail> {
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
        self.call_target(target)
    }

    /// Tail-call `target`: replace the top frame in place at the same
    /// operand-stack base, so the callee's results flow to the caller of the
    /// (now replaced) frame.
    fn tail_target(&mut self, target: FuncTarget) -> Result<Ctl, ExecFail> {
        let top = self.frames.len() - 1;
        match target {
            FuncTarget::Host(id) => {
                let ty = self.store.host_funcs[id].ty.clone();
                match self.store.host_funcs[id].token {
                    Some(token) => {
                        let mut args = Vec::with_capacity(ty.params.len());
                        for _ in 0..ty.params.len() {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        // The tail-called frame ends now: its results will be
                        // the host reply, delivered to its caller (or, for the
                        // outermost frame, as the invocation's results) when
                        // the run resumes.
                        let base = self.frames[top].base;
                        self.stack.truncate(base);
                        self.frames.pop();
                        Ok(Ctl::Host { token, args })
                    }
                    None => {
                        if !ty.results.is_empty() {
                            return Err(ExecFail::Unsupported("host function results"));
                        }
                        for _ in 0..ty.params.len() {
                            self.pop()?;
                        }
                        self.finish()
                    }
                }
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
                        .func_at(type_index)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    (
                        signature.params.len(),
                        signature.results.len(),
                        body.locals.clone(),
                    )
                };
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

    /// Make a tail call (`return_call`) from the top frame.
    fn do_tail_call(&mut self, index: usize) -> Result<Ctl, ExecFail> {
        let top = self.frames.len() - 1;
        let instance = self.frames[top].instance;
        let target = *self
            .store
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.tail_target(target)
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
        ValType::V128 => Value::V128(0),
        // Non-defaultable (non-null) reference locals are validated as
        // initialized-before-use; the null placeholder below is never read by
        // a valid module.
        ValType::Ref(_) => Value::Ref(RefValue::Null),
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

/// Bytes a `v128.load*` reads (spec 2.4.5).
fn vec_load_bytes(op: VecLoadOp) -> usize {
    match op {
        VecLoadOp::V128 => 16,
        VecLoadOp::I8x8S
        | VecLoadOp::I8x8U
        | VecLoadOp::I16x4S
        | VecLoadOp::I16x4U
        | VecLoadOp::I32x2S
        | VecLoadOp::I32x2U
        | VecLoadOp::I64Splat
        | VecLoadOp::I64Zero => 8,
        VecLoadOp::I8Splat => 1,
        VecLoadOp::I16Splat => 2,
        VecLoadOp::I32Splat | VecLoadOp::I32Zero => 4,
    }
}

/// Materialize a v128 from a `v128.load*` memory slice.
fn load_vec(op: VecLoadOp, bytes: &[u8]) -> Option<u128> {
    use crate::simd::set_lane;
    let le = |bytes: &[u8]| -> u64 {
        let mut out = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            out |= u64::from(b) << (8 * i);
        }
        out
    };
    let mut out = 0u128;
    match op {
        VecLoadOp::V128 => out = u128::from_le_bytes(bytes.try_into().ok()?),
        VecLoadOp::I8x8S | VecLoadOp::I8x8U => {
            let signed = matches!(op, VecLoadOp::I8x8S);
            for (i, &b) in bytes.iter().enumerate() {
                let wide = if signed {
                    (b as i8) as i16 as u16 as u64
                } else {
                    u64::from(b)
                };
                out = set_lane(out, i, 2, wide);
            }
        }
        VecLoadOp::I16x4S | VecLoadOp::I16x4U => {
            let signed = matches!(op, VecLoadOp::I16x4S);
            for i in 0..4 {
                let raw = le(&bytes[2 * i..2 * i + 2]) as u16;
                let wide = if signed {
                    (raw as i16) as i32 as u32 as u64
                } else {
                    u64::from(raw)
                };
                out = set_lane(out, i, 4, wide);
            }
        }
        VecLoadOp::I32x2S | VecLoadOp::I32x2U => {
            let signed = matches!(op, VecLoadOp::I32x2S);
            for i in 0..2 {
                let raw = le(&bytes[4 * i..4 * i + 4]) as u32;
                let wide = if signed {
                    (raw as i32) as i64 as u64
                } else {
                    u64::from(raw)
                };
                out = set_lane(out, i, 8, wide);
            }
        }
        VecLoadOp::I8Splat => {
            for i in 0..16 {
                out = set_lane(out, i, 1, u64::from(bytes[0]));
            }
        }
        VecLoadOp::I16Splat => {
            let raw = le(&bytes[0..2]);
            for i in 0..8 {
                out = set_lane(out, i, 2, raw);
            }
        }
        VecLoadOp::I32Splat => {
            let raw = le(&bytes[0..4]);
            for i in 0..4 {
                out = set_lane(out, i, 4, raw);
            }
        }
        VecLoadOp::I64Splat => {
            let raw = le(&bytes[0..8]);
            for i in 0..2 {
                out = set_lane(out, i, 8, raw);
            }
        }
        VecLoadOp::I32Zero => {
            out = set_lane(0, 0, 4, le(&bytes[0..4]));
        }
        VecLoadOp::I64Zero => {
            out = set_lane(0, 0, 8, le(&bytes[0..8]));
        }
    }
    Some(out)
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
        Value::V128(bits) => bits as u64,
        Value::Ref(_) => 0,
    };
    for (shift, byte) in bytes.iter_mut().enumerate() {
        *byte = (bits >> (8 * shift)) as u8;
    }
}

/// Whether an imported memory type is satisfied by a provided one: the
/// provider must have at least the requested minimum, and no larger a maximum
/// than the request allows (spec 4.5.2 external-type subsumption).
fn memory_matches(actual: MemType, requested: MemType) -> bool {
    actual.memory64 == requested.memory64 && limits_match(actual.limits, requested.limits)
}

/// Whether an imported table type is satisfied by a provided one: element
/// reference types must be identical (tables are invariant containers) and
/// limits subsume.
fn table_matches(actual: TableType, requested: TableType) -> bool {
    actual.table64 == requested.table64
        && actual.element == requested.element
        && limits_match(actual.limits, requested.limits)
}

fn limits_match(actual: Limits, requested: Limits) -> bool {
    actual.min >= requested.min
        && requested
            .max
            .is_none_or(|max| actual.max.is_some_and(|a| a <= max))
}

fn matches_ref(expected: RefType, actual: RefType) -> bool {
    if !expected.nullable && actual.nullable {
        return false;
    }
    match (expected.heap, actual.heap) {
        (crate::types::HeapType::Func, crate::types::HeapType::Func) => true,
        (crate::types::HeapType::Extern, crate::types::HeapType::Extern) => true,
        (crate::types::HeapType::Exn, crate::types::HeapType::Exn) => true,
        (crate::types::HeapType::Type(x), crate::types::HeapType::Type(y)) => x == y,
        // A typed function reference is a function reference.
        (crate::types::HeapType::Func, crate::types::HeapType::Type(_)) => true,
        _ => false,
    }
}

/// Whether `actual` value type fits an `expected` value type (reference
/// subtyping for refs, equality otherwise).
fn matches_val(expected: ValType, actual: ValType) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (ValType::Ref(e), ValType::Ref(a)) => matches_ref(e, a),
        _ => false,
    }
}

/// Evaluate a constant initializer expression (no terminating `end`).
/// `module` provides the type layout for GC aggregate constant expressions
/// (spec const-expr rules) and `func_instance` is the instance whose function
/// space a `ref.func` refers to; aggregate allocations land in `store`
/// (constant `struct.new`/`array.new` and friends).
fn eval_const(
    store: &mut Store,
    module: &Module,
    expr: &[Instr],
    globals: &[Value],
    func_instance: usize,
) -> Result<Value, ExecFail> {
    let mut stack: Vec<Value> = Vec::new();
    for instr in expr {
        match instr {
            Instr::I32Const(v) => stack.push(Value::I32(*v)),
            Instr::I64Const(v) => stack.push(Value::I64(*v)),
            Instr::F32Const(bits) => stack.push(Value::F32(*bits)),
            Instr::F64Const(bits) => stack.push(Value::F64(*bits)),
            Instr::V128Const(bits) => stack.push(Value::V128(*bits)),
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
            Instr::RefNull(_) => stack.push(Value::Ref(RefValue::Null)),
            Instr::RefFunc(index) => stack.push(Value::Ref(RefValue::Func(FuncAddr {
                instance: func_instance,
                index: *index as usize,
            }))),
            Instr::RefI31 => {
                let Value::I32(value) = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?
                else {
                    return Err(ExecFail::Unsupported("non-i32 ref.i31 constant"));
                };
                stack.push(Value::Ref(RefValue::i31(value)));
            }
            Instr::AnyConvertExtern => {
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let unwrapped = match value {
                    Value::Ref(reference) => reference.unwrap_extern(),
                    _ => return Err(ExecFail::Unsupported("non-reference internalize")),
                };
                stack.push(Value::Ref(unwrapped));
            }
            Instr::ExternConvertAny => {
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let wrapped = match value {
                    Value::Ref(RefValue::Null) => RefValue::Null,
                    Value::Ref(reference) => match ExternInner::wrap(reference) {
                        Some(inner) => RefValue::Extern(inner),
                        None => return Err(ExecFail::Unsupported("unboxable externalize")),
                    },
                    _ => return Err(ExecFail::Unsupported("non-reference externalize")),
                };
                stack.push(Value::Ref(wrapped));
            }
            Instr::StructNew(ty) => {
                let fields = struct_field_types(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown struct type"))?;
                let mut cells = Vec::with_capacity(fields.len());
                for field in fields.iter().rev() {
                    let value = stack
                        .pop()
                        .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                    cells.push(wrap_cell(field.ty, value)?);
                }
                cells.reverse();
                let id = alloc_struct(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Struct(id)));
            }
            Instr::StructNewDefault(ty) => {
                let fields = struct_field_types(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown struct type"))?;
                let mut cells = Vec::with_capacity(fields.len());
                for field in fields {
                    cells.push(default_storage(field.ty)?);
                }
                let id = alloc_struct(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Struct(id)));
            }
            Instr::ArrayNew(ty) => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let len = pop_const_i32(&mut stack)?;
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let cell = wrap_cell(field.ty, value)?;
                let id = alloc_array_filled(store, func_instance, *ty, len, cell)?;
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            Instr::ArrayNewDefault(ty) => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let len = pop_const_i32(&mut stack)?;
                let cell = default_storage(field.ty)?;
                let id = alloc_array_filled(store, func_instance, *ty, len, cell)?;
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            Instr::ArrayNewFixed { ty, n } => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let mut cells = Vec::with_capacity(*n as usize);
                for _ in 0..*n {
                    let value = stack
                        .pop()
                        .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                    cells.push(wrap_cell(field.ty, value)?);
                }
                cells.reverse();
                let id = alloc_array(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            _ => return Err(ExecFail::Unsupported("constant expression")),
        }
    }
    stack
        .pop()
        .ok_or(ExecFail::Unsupported("empty constant expression"))
}

/// Pop an i32 from a constant-expression value stack.
fn pop_const_i32(stack: &mut Vec<Value>) -> Result<i32, ExecFail> {
    match stack.pop() {
        Some(Value::I32(v)) => Ok(v),
        _ => Err(ExecFail::Unsupported("malformed constant expression")),
    }
}

/// Evaluate a constant expression that must produce a reference value.
fn eval_ref_const(
    store: &mut Store,
    module: &Module,
    expr: &[Instr],
    globals: &[Value],
    func_instance: usize,
) -> Result<RefValue, InstantiateError> {
    match eval_const(store, module, expr, globals, func_instance)? {
        Value::Ref(reference) => Ok(reference),
        _ => Err(InstantiateError::Unsupported("non-reference constant")),
    }
}

// ---- GC object helpers ----

/// The declared struct fields at type index `ty` of `module`.
fn struct_field_types(module: &Module, ty: u32) -> Option<&[FieldType]> {
    module
        .types
        .get(ty as usize)
        .and_then(|sub| match &sub.composite {
            CompositeType::Struct(fields) => Some(fields.as_slice()),
            _ => None,
        })
}

/// The declared array element field at type index `ty` of `module`.
fn array_element_type(module: &Module, ty: u32) -> Option<&FieldType> {
    module
        .types
        .get(ty as usize)
        .and_then(|sub| match &sub.composite {
            CompositeType::Array(field) => Some(field),
            _ => None,
        })
}

/// The field storage of a live struct object (its own declared type's
/// fields, in order).
fn gc_struct_storage(store: &Store, object: usize) -> Result<Vec<StorageType>, ExecFail> {
    let gc_object = &store.objects[object];
    let module = &store.instances[gc_object.owner].module;
    match struct_field_types(module, gc_object.ty) {
        Some(fields) => Ok(fields.iter().map(|f| f.ty).collect()),
        None => Err(ExecFail::Unsupported("struct object layout")),
    }
}

/// The element storage of a live array object.
fn gc_array_storage(store: &Store, object: usize) -> Result<StorageType, ExecFail> {
    let gc_object = &store.objects[object];
    let module = &store.instances[gc_object.owner].module;
    match array_element_type(module, gc_object.ty) {
        Some(field) => Ok(field.ty),
        None => Err(ExecFail::Unsupported("array object layout")),
    }
}

/// The default (zero/null) value for a storage type. Non-null reference
/// storage is not defaultable (validation rejects those declarations).
fn default_storage(storage: StorageType) -> Result<Value, ExecFail> {
    Ok(match storage {
        StorageType::I8 | StorageType::I16 | StorageType::I32 => Value::I32(0),
        StorageType::I64 => Value::I64(0),
        StorageType::F32 => Value::F32(0),
        StorageType::F64 => Value::F64(0),
        StorageType::V128 => Value::V128(0),
        StorageType::Ref(reftype) if reftype.nullable => Value::Ref(RefValue::Null),
        StorageType::Ref(_) => {
            return Err(ExecFail::Unsupported("non-defaultable reference field"));
        }
    })
}

/// Store `value` into a cell of the given storage type: packed cells keep
/// the wrapped unsigned low bits, everything else keeps the value verbatim.
fn wrap_cell(storage: StorageType, value: Value) -> Result<Value, ExecFail> {
    Ok(match storage {
        StorageType::I8 => match value {
            Value::I32(v) => Value::I32((v as u8) as i32),
            _ => return Err(ExecFail::Unsupported("i8 cell write")),
        },
        StorageType::I16 => match value {
            Value::I32(v) => Value::I32((v as u16) as i32),
            _ => return Err(ExecFail::Unsupported("i16 cell write")),
        },
        StorageType::I32
        | StorageType::I64
        | StorageType::F32
        | StorageType::F64
        | StorageType::V128
        | StorageType::Ref(_) => value,
    })
}

/// Read a cell with the signed/unsigned extension of the packed `get_s`/
/// `get_u` forms. Only valid for packed cells (which validation restricts the
/// `_s`/`_u` instructions to).
fn extend_cell(storage: StorageType, cell: Value, signed: bool) -> Result<Value, ExecFail> {
    let Value::I32(bits) = cell else {
        return Err(ExecFail::Unsupported("packed read of non-i32 cell"));
    };
    let out = match (storage, signed) {
        (StorageType::I8, false) => (bits as u8) as i32,
        (StorageType::I8, true) => (bits as u8) as i8 as i32,
        (StorageType::I16, false) => (bits as u16) as i32,
        (StorageType::I16, true) => (bits as u16) as i16 as i32,
        _ => return Err(ExecFail::Unsupported("unpacked signed/unsigned read")),
    };
    Ok(Value::I32(out))
}

/// The byte width of a storage type (packed, numeric, or vector). Reference
/// storage has no byte width (refs never come from data segments).
fn storage_width(storage: StorageType) -> Option<usize> {
    match storage {
        StorageType::I8 => Some(1),
        StorageType::I16 => Some(2),
        StorageType::I32 | StorageType::F32 => Some(4),
        StorageType::I64 | StorageType::F64 => Some(8),
        StorageType::V128 => Some(16),
        StorageType::Ref(_) => None,
    }
}

/// Load one data-segment element of `storage` at byte offset `at` (little
/// endian). Packed and integer loads are unsigned; packed cells keep the
/// wrapped low bits. Returns `None` when the slice is short or the storage is
/// a reference.
fn data_element(storage: StorageType, bytes: &[u8], at: usize) -> Option<Value> {
    let width = storage_width(storage)?;
    let bytes = bytes.get(at..at + width)?;
    let mut bits = 0u64;
    for (i, &byte) in bytes.iter().enumerate() {
        bits |= u64::from(byte) << (8 * i);
    }
    Some(match storage {
        StorageType::I8 | StorageType::I16 => Value::I32(bits as u32 as i32),
        StorageType::I32 => Value::I32(bits as u32 as i32),
        StorageType::I64 => Value::I64(bits as i64),
        StorageType::F32 => Value::F32(bits as u32),
        StorageType::F64 => Value::F64(bits),
        StorageType::V128 => {
            let mut wide = 0u128;
            for (i, &byte) in bytes.iter().enumerate() {
                wide |= u128::from(byte) << (8 * i);
            }
            Value::V128(wide)
        }
        StorageType::Ref(_) => return None,
    })
}

/// Append a struct object to the pool; returns its stable id.
fn alloc_struct(store: &mut Store, owner: usize, ty: u32, cells: Vec<Value>) -> usize {
    store.objects.push(GcObject {
        owner,
        ty,
        data: GcData::Struct(cells),
    });
    store.objects.len() - 1
}

/// Append an array object to the pool; returns its stable id.
fn alloc_array(store: &mut Store, owner: usize, ty: u32, cells: Vec<Value>) -> usize {
    store.objects.push(GcObject {
        owner,
        ty,
        data: GcData::Array(cells),
    });
    store.objects.len() - 1
}

/// Append an array of `len` copies of `cell`; an oversized request traps like
/// an out-of-bounds memory access instead of aborting on allocation.
fn alloc_array_filled(
    store: &mut Store,
    owner: usize,
    ty: u32,
    len: i32,
    cell: Value,
) -> Result<usize, ExecFail> {
    let count = len as u32 as usize;
    let mut elements = Vec::new();
    elements
        .try_reserve_exact(count)
        .map_err(|_| ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
    elements.resize(count, cell);
    Ok(alloc_array(store, owner, ty, elements))
}

/// How a `get` reads a GC field/element cell: verbatim, or with the packed
/// sign/zero extension of `get_s`/`get_u`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GcRead {
    Plain,
    Signed,
    Unsigned,
}

/// The live length of an array object.
fn gc_array_len(store: &Store, id: usize) -> Result<usize, ExecFail> {
    match &store.objects[id].data {
        GcData::Array(cells) => Ok(cells.len()),
        _ => Err(ExecFail::Unsupported("array operand is not an array")),
    }
}

/// The abstract-heap lattice for runtime `ref.test`/`ref.cast` between an
/// actual value's (abstract) heap kind and the target heap (spec 3.3.3):
/// `eq` covers `i31`/`struct`/`array`, all of which sit under `any`. The
/// `_`-arm rejects unrelated kinds (`func`/`extern`/`exn`/bottom) and the
/// concrete `Type` form (handled before the abstract lattice is consulted).
fn runtime_abs_matches(sub: HeapType, sup: HeapType) -> bool {
    use HeapType::*;
    if sub == sup {
        return true;
    }
    match sub {
        Eq => sup == Any,
        I31 | Struct | Array => sup == Eq || sup == Any,
        _ => false,
    }
}
