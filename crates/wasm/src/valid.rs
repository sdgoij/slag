//! The wasm validator (spec ch. 3).
//!
//! Takes a decoded [`Module`] and type-checks every function body, constant
//! expression, and module-level constraint. Structural problems were already
//! rejected by the decoder; anything rejected here is *invalid* (not
//! malformed), which is exactly the distinction the conformance runner's
//! `assert_invalid`/`assert_malformed` commands need.
//!
//! The type-checking algorithm follows the spec's operational formulation:
//! an operand stack plus a control stack of frames (function/block/loop/if/
//! else), with unreachable-code polymorphism after `unreachable`/`br`/...

use std::fmt;

use crate::instr::{Instr, LoadOp, NumOp, StoreOp};
use crate::module::{DataMode, ElementMode, ExportKind, ImportDesc, Module};
use crate::types::{
    BlockType, FuncType, GlobalType, HeapType, Limits, MemType, RefType, TableType, ValType,
};

/// A validation error. Messages mirror the spec's diagnostics where they are
/// stable; the runner only needs the invalid-vs-malformed split for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(message) => write!(f, "invalid: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// The per-module context: index spaces and counts used by instruction
/// validation, precomputed so bodies can be checked without recomputation.
struct Spaces<'a> {
    module: &'a Module,
    /// Full function index space (imports first, then module functions),
    /// with each entry's resolved signature.
    funcs: Vec<FuncType>,
    /// Full global index space with the *global type* of each entry.
    globals: Vec<GlobalType>,
    /// Number of imported globals (constant exprs may only read those).
    imported_globals: usize,
    /// Full table index space.
    tables: Vec<TableType>,
    /// Full memory index space.
    memories: Vec<MemType>,
}

/// Validate a whole module.
pub fn validate(module: &Module) -> Result<(), Error> {
    let spaces = build_spaces(module)?;
    validate_module_limits(module)?;
    validate_tags(module)?;
    validate_tables(module, &spaces)?;
    validate_exports(module)?;
    validate_globals(module, &spaces)?;
    validate_elements(module, &spaces)?;
    validate_data(module, &spaces)?;
    validate_start(module, &spaces)?;
    validate_code(module, &spaces)?;
    Ok(())
}

fn build_spaces(module: &Module) -> Result<Spaces<'_>, Error> {
    // Resolve every referenced type index once so bodies can index freely.
    for (index, ty) in module.types.iter().enumerate() {
        check_value_types(&ty.params, module.types.len(), index)?;
        check_value_types(&ty.results, module.types.len(), index)?;
    }

    let mut funcs = Vec::new();
    let mut tables = Vec::new();
    let mut memories = Vec::new();
    let mut globals = Vec::new();
    for import in &module.imports {
        match &import.desc {
            ImportDesc::Func(type_index) => {
                funcs.push(type_at(module, *type_index)?);
            }
            ImportDesc::Table(ty) => {
                validate_table_type(ty, module.types.len())?;
                tables.push(*ty);
            }
            ImportDesc::Memory(ty) => {
                validate_mem_type(ty)?;
                memories.push(*ty)
            }
            ImportDesc::Global(ty) => {
                check_value_types(&[ty.value], module.types.len(), 0)?;
                globals.push(*ty);
            }
            ImportDesc::Tag(type_index) => validate_tag_type(module, *type_index)?,
        }
    }
    let imported_globals = globals.len();

    for type_index in &module.functions {
        funcs.push(type_at(module, *type_index)?);
    }
    for table in &module.tables {
        validate_table_type(&table.ty, module.types.len())?;
        tables.push(table.ty);
    }
    for ty in &module.memories {
        memories.push(*ty);
    }
    for global in &module.globals {
        check_value_types(&[global.ty.value], module.types.len(), 0)?;
        globals.push(global.ty);
    }

    Ok(Spaces {
        module,
        funcs,
        globals,
        imported_globals,
        tables,
        memories,
    })
}

fn type_at(module: &Module, index: u32) -> Result<FuncType, Error> {
    module
        .types
        .get(index as usize)
        .cloned()
        .ok_or(Error::Invalid("unknown type"))
}

fn check_value_types(types: &[ValType], type_count: usize, _in: usize) -> Result<(), Error> {
    for ty in types {
        if let ValType::Ref(RefType {
            heap: HeapType::Type(index),
            ..
        }) = ty
            && *index as usize >= type_count
        {
            return Err(Error::Invalid("unknown type"));
        }
    }
    Ok(())
}

fn validate_table_type(ty: &TableType, type_count: usize) -> Result<(), Error> {
    if let HeapType::Type(index) = ty.element.heap
        && index as usize >= type_count
    {
        return Err(Error::Invalid("unknown type"));
    }
    validate_limits(&ty.limits)
}

/// A tag's type use must resolve to a function type whose results are empty;
/// non-empty results are reserved for non-exception tag uses.
fn validate_tag_type(module: &Module, type_index: u32) -> Result<(), Error> {
    let ty = type_at(module, type_index)?;
    if !ty.results.is_empty() {
        return Err(Error::Invalid("tag type"));
    }
    Ok(())
}

fn validate_limits(limits: &Limits) -> Result<(), Error> {
    if let Some(max) = limits.max
        && max < limits.min
    {
        return Err(Error::Invalid(
            "size minimum must not be greater than maximum",
        ));
    }
    Ok(())
}

/// A memory type must have ordered limits and, for wasm32 (the only form this
/// engine decodes), page counts within 2^16 (4 GiB).
fn validate_mem_type(memory: &MemType) -> Result<(), Error> {
    validate_limits(&memory.limits)?;
    if !memory.memory64 {
        const MAX_MEM32_PAGES: u64 = 1 << 16;
        let within = memory.limits.min <= MAX_MEM32_PAGES
            && memory.limits.max.is_none_or(|max| max <= MAX_MEM32_PAGES);
        if !within {
            return Err(Error::Invalid("memory size"));
        }
    }
    Ok(())
}

fn validate_module_limits(module: &Module) -> Result<(), Error> {
    if module.memories.len() > 1 {
        // Baseline: at most one memory (multi-memory is a later cut).
        return Err(Error::Invalid("multiple memories"));
    }
    for memory in &module.memories {
        validate_mem_type(memory)?;
    }
    if let Some(count) = module.data_count
        && count as usize != module.data.len()
    {
        return Err(Error::Invalid(
            "data count and data section have inconsistent lengths",
        ));
    }
    Ok(())
}

/// Module-defined tags point at function types that must be exception-shaped
/// (no results); imported tags were checked while building the index spaces.
fn validate_tags(module: &Module) -> Result<(), Error> {
    for tag_index in &module.tags {
        validate_tag_type(module, *tag_index)?;
    }
    Ok(())
}

/// Module-defined tables are validated while building the index spaces; this
/// checks their optional initializer expressions. The table section precedes
/// the globals, so only imported globals are visible to those expressions.
fn validate_tables(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    for table in &module.tables {
        if let Some(init) = &table.init {
            validate_const_expr(
                init,
                ValType::Ref(table.ty.element),
                spaces,
                spaces.imported_globals,
            )?;
        }
    }
    Ok(())
}

fn validate_exports(module: &Module) -> Result<(), Error> {
    let mut names: Vec<&str> = Vec::new();
    for export in &module.exports {
        if names.contains(&export.name.as_str()) {
            return Err(Error::Invalid("duplicate export name"));
        }
        names.push(&export.name);
        let total = match export.kind {
            ExportKind::Func => {
                module
                    .imports
                    .iter()
                    .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
                    .count()
                    + module.functions.len()
            }
            ExportKind::Table => {
                module
                    .imports
                    .iter()
                    .filter(|i| matches!(i.desc, ImportDesc::Table(_)))
                    .count()
                    + module.tables.len()
            }
            ExportKind::Memory => {
                module
                    .imports
                    .iter()
                    .filter(|i| matches!(i.desc, ImportDesc::Memory(_)))
                    .count()
                    + module.memories.len()
            }
            ExportKind::Global => {
                module
                    .imports
                    .iter()
                    .filter(|i| matches!(i.desc, ImportDesc::Global(_)))
                    .count()
                    + module.globals.len()
            }
            ExportKind::Tag => {
                module
                    .imports
                    .iter()
                    .filter(|i| matches!(i.desc, ImportDesc::Tag(_)))
                    .count()
                    + module.tags.len()
            }
        };
        if export.index as usize >= total {
            return Err(Error::Invalid("unknown export"));
        }
    }
    Ok(())
}

fn validate_globals(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    for (defined_index, global) in module.globals.iter().enumerate() {
        // An initializer may read imports plus globals defined before it.
        let visible = spaces.imported_globals + defined_index;
        validate_const_expr(&global.init, global.ty.value, spaces, visible)?;
    }
    Ok(())
}

fn validate_elements(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    for element in &module.elements {
        if let ElementMode::Active { table, offset } = &element.mode {
            let Some(table_ty) = spaces.tables.get(*table as usize) else {
                return Err(Error::Invalid("unknown table"));
            };
            // The segment's declared type must fit the target table's
            // element type.
            if !matches_val(ValType::Ref(table_ty.element), ValType::Ref(element.ty)) {
                return Err(Error::Invalid("type mismatch"));
            }
            validate_const_expr(offset, ValType::I32, spaces, spaces.globals.len())?;
        }
        for item in &element.init {
            let item_type = const_expr_type(item, spaces, spaces.globals.len())?;
            if !matches_val(ValType::Ref(element.ty), item_type) {
                return Err(Error::Invalid("type mismatch"));
            }
        }
    }
    Ok(())
}

fn validate_data(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    for segment in &module.data {
        if let DataMode::Active { memory, offset } = &segment.mode {
            if *memory as usize >= spaces.memories.len() {
                return Err(Error::Invalid("unknown memory"));
            }
            validate_const_expr(offset, ValType::I32, spaces, spaces.globals.len())?;
        }
    }
    Ok(())
}

fn validate_start(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    let Some(index) = module.start else {
        return Ok(());
    };
    let Some(func) = spaces.funcs.get(index as usize) else {
        return Err(Error::Invalid("unknown function"));
    };
    if !func.params.is_empty() || !func.results.is_empty() {
        return Err(Error::Invalid("start function"));
    }
    Ok(())
}

fn validate_code(module: &Module, spaces: &Spaces<'_>) -> Result<(), Error> {
    if module.bodies.len() != module.functions.len() {
        return Err(Error::Invalid(
            "function and code section have inconsistent lengths",
        ));
    }
    for (defined_index, body) in module.bodies.iter().enumerate() {
        let func_index = spaces
            .module
            .imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
            .count()
            + defined_index;
        let signature = &spaces.funcs[func_index];
        let mut machine = Machine::new();
        machine.push_frame(Ctrl::Func, vec![], signature.results.clone());
        let mut locals = signature.params.clone();
        locals.extend_from_slice(&body.locals);
        machine.validate_body(&body.body, signature, &locals, spaces)?;
    }
    Ok(())
}

// ---- value typing helpers ----

fn matches_val(expected: ValType, actual: ValType) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (ValType::Ref(e), ValType::Ref(a)) => matches_ref(e, a),
        _ => false,
    }
}

fn matches_ref(expected: RefType, actual: RefType) -> bool {
    if !expected.nullable && actual.nullable {
        return false;
    }
    match (expected.heap, actual.heap) {
        (HeapType::Func, HeapType::Func) => true,
        (HeapType::Extern, HeapType::Extern) => true,
        (HeapType::Type(x), HeapType::Type(y)) => x == y,
        // A typed function reference is a function reference.
        (HeapType::Func, HeapType::Type(_)) => true,
        _ => false,
    }
}

// ---- the type-checking machine ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctrl {
    Func,
    Block,
    Loop,
    If,
    Else,
}

/// An operand-stack entry: a known value type, or [`StackTy::Bot`], the
/// unknown type that pops produce once a frame is unreachable and its stack
/// is exhausted (the spec algorithm's `Bot`). `Bot` matches any expected
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StackTy {
    Known(ValType),
    Bot,
}

/// The spec algorithm's unary predicates treat the unknown type as
/// satisfying every category.
fn is_num_ty(ty: StackTy) -> bool {
    match ty {
        StackTy::Bot => true,
        StackTy::Known(ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64) => true,
        StackTy::Known(_) => false,
    }
}

fn is_vec_ty(ty: StackTy) -> bool {
    matches!(ty, StackTy::Bot | StackTy::Known(ValType::V128))
}

fn is_ref_ty(ty: StackTy) -> bool {
    matches!(ty, StackTy::Bot | StackTy::Known(ValType::Ref(_)))
}

struct Frame {
    ctrl: Ctrl,
    start_types: Vec<ValType>,
    end_types: Vec<ValType>,
    height: usize,
    unreachable: bool,
}

struct Machine {
    vals: Vec<StackTy>,
    frames: Vec<Frame>,
}

impl Machine {
    fn new() -> Self {
        Machine {
            vals: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn push_frame(&mut self, ctrl: Ctrl, start: Vec<ValType>, end: Vec<ValType>) {
        let height = self.vals.len();
        self.push_vals(&start);
        self.frames.push(Frame {
            ctrl,
            start_types: start,
            end_types: end,
            height,
            unreachable: false,
        });
    }

    fn pop_frame(&mut self) -> Result<Frame, Error> {
        let Some(top) = self.frames.last() else {
            return Err(Error::Invalid("type mismatch"));
        };
        let end = top.end_types.clone();
        let height = top.height;
        // Pop the frame's results while it is still the innermost frame, so
        // unreachable polymorphism applies.
        self.pop_vals(&end)?;
        let frame = self.frames.pop().ok_or(Error::Invalid("type mismatch"))?;
        if self.vals.len() != height {
            return Err(Error::Invalid("type mismatch"));
        }
        Ok(frame)
    }

    fn unreachable(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            self.vals.truncate(frame.height);
            frame.unreachable = true;
        }
    }

    fn push(&mut self, ty: StackTy) {
        self.vals.push(ty);
    }

    fn push_val(&mut self, ty: ValType) {
        self.push(StackTy::Known(ty));
    }

    fn push_vals(&mut self, types: &[ValType]) {
        self.vals.extend(types.iter().copied().map(StackTy::Known));
    }

    fn pop_val(&mut self) -> Result<StackTy, Error> {
        let Some(frame) = self.frames.last() else {
            return Err(Error::Invalid("type mismatch"));
        };
        if self.vals.len() == frame.height {
            if frame.unreachable {
                return Ok(StackTy::Bot);
            }
            return Err(Error::Invalid("type mismatch"));
        }
        self.vals.pop().ok_or(Error::Invalid("type mismatch"))
    }

    fn pop_expected(&mut self, expected: ValType) -> Result<(), Error> {
        match self.pop_val()? {
            StackTy::Bot => Ok(()),
            StackTy::Known(actual) => {
                if matches_val(expected, actual) {
                    Ok(())
                } else {
                    Err(Error::Invalid("type mismatch"))
                }
            }
        }
    }

    fn pop_vals(&mut self, types: &[ValType]) -> Result<(), Error> {
        for ty in types.iter().rev() {
            self.pop_expected(*ty)?;
        }
        Ok(())
    }

    /// Pop one value per entry of `types` (each checked against it) and push
    /// the actual entries back, so a later consumer still sees them.
    fn pop_vals_peek(&mut self, types: &[ValType]) -> Result<(), Error> {
        let mut actuals = Vec::with_capacity(types.len());
        for ty in types.iter().rev() {
            actuals.push(self.pop_val_of(*ty)?);
        }
        for actual in actuals.into_iter().rev() {
            self.push(actual);
        }
        Ok(())
    }

    /// Pop a value that must be a reference (or unknown); returns the popped
    /// entry so callers can push a derived non-null reference.
    fn pop_ref(&mut self) -> Result<StackTy, Error> {
        match self.pop_val()? {
            entry @ (StackTy::Bot | StackTy::Known(ValType::Ref(_))) => Ok(entry),
            StackTy::Known(_) => Err(Error::Invalid("type mismatch")),
        }
    }

    /// Pop a value that must match `expected` (Bot matches anything) and
    /// return whether the actual type is known.
    fn pop_val_of(&mut self, expected: ValType) -> Result<StackTy, Error> {
        match self.pop_val()? {
            StackTy::Bot => Ok(StackTy::Bot),
            StackTy::Known(actual) => {
                if matches_val(expected, actual) {
                    Ok(StackTy::Known(actual))
                } else {
                    Err(Error::Invalid("type mismatch"))
                }
            }
        }
    }

    /// Pop a value that must equal `expected` unless either is unknown.
    fn pop_matching(&mut self, expected: StackTy) -> Result<(), Error> {
        let actual = self.pop_val()?;
        if actual == expected || actual == StackTy::Bot || expected == StackTy::Bot {
            return Ok(());
        }
        Err(Error::Invalid("type mismatch"))
    }

    fn label_types(&self, index: usize) -> Result<Vec<ValType>, Error> {
        // Labels count every enclosing frame including the function's
        // implicit label (whose types are the function results).
        if index >= self.frames.len() {
            return Err(Error::Invalid("unknown label"));
        }
        let frame = &self.frames[self.frames.len() - 1 - index];
        Ok(match frame.ctrl {
            Ctrl::Loop => frame.start_types.clone(),
            _ => frame.end_types.clone(),
        })
    }

    fn current_results(&self) -> &[ValType] {
        // The function frame is the bottom of the control stack.
        &self.frames[0].end_types
    }

    fn validate_body(
        &mut self,
        body: &[Instr],
        signature: &FuncType,
        locals: &[ValType],
        spaces: &Spaces<'_>,
    ) -> Result<(), Error> {
        for instr in body {
            self.step(instr, signature, locals, spaces)?;
        }
        // The decoder omits the function-terminating `end`; close the
        // function frame and require the stack to be empty.
        let frame = self.pop_frame()?;
        if frame.ctrl != Ctrl::Func || !self.frames.is_empty() || !self.vals.is_empty() {
            return Err(Error::Invalid("type mismatch"));
        }
        Ok(())
    }

    fn step(
        &mut self,
        instr: &Instr,
        signature: &FuncType,
        locals: &[ValType],
        spaces: &Spaces<'_>,
    ) -> Result<(), Error> {
        match instr {
            Instr::Unreachable => self.unreachable(),
            Instr::Nop => {}
            Instr::Block(bt) => {
                let (params, results) = resolve_block_type(bt, spaces)?;
                self.pop_vals(&params)?;
                self.push_frame(Ctrl::Block, params, results);
            }
            Instr::Loop(bt) => {
                let (params, results) = resolve_block_type(bt, spaces)?;
                self.pop_vals(&params)?;
                self.push_frame(Ctrl::Loop, params, results);
            }
            Instr::If(bt) => {
                self.pop_expected(ValType::I32)?;
                let (params, results) = resolve_block_type(bt, spaces)?;
                self.pop_vals(&params)?;
                self.push_frame(Ctrl::If, params, results);
            }
            Instr::Else => {
                let frame = self.pop_frame()?;
                if frame.ctrl != Ctrl::If {
                    return Err(Error::Invalid("else instruction outside of an if block"));
                }
                // The else arm re-sees the if's block parameters.
                self.push_frame(Ctrl::Else, frame.start_types, frame.end_types);
            }
            Instr::End => {
                let frame = self.pop_frame()?;
                if frame.ctrl == Ctrl::Func {
                    // The function end is implicit; re-push as the caller's
                    // frame so outer validation still sees it.
                    self.frames.push(frame);
                } else {
                    if frame.ctrl == Ctrl::If && frame.end_types != frame.start_types {
                        // An `if` without an `else` leaves the block's
                        // parameters as its results on the false path, so the
                        // block type must be the identity.
                        return Err(Error::Invalid("type mismatch"));
                    }
                    // The block's results become the enclosing frame's
                    // operands.
                    self.push_vals(&frame.end_types);
                }
            }
            Instr::Br(label) => {
                let types = self.label_types(*label as usize)?;
                self.pop_vals(&types)?;
                self.unreachable();
            }
            Instr::BrIf(label) => {
                self.pop_expected(ValType::I32)?;
                let types = self.label_types(*label as usize)?;
                self.pop_vals(&types)?;
                for ty in types {
                    self.push_val(ty);
                }
            }
            Instr::BrTable { targets, default } => {
                self.pop_expected(ValType::I32)?;
                let default_types = self.label_types(*default as usize)?;
                let arity = default_types.len();
                for target in targets {
                    let types = self.label_types(*target as usize)?;
                    if types.len() != arity {
                        return Err(Error::Invalid("type mismatch"));
                    }
                    // Check the current stack against each target's label
                    // types, re-pushing the actual values so the next target
                    // (and the default) sees them. In unreachable code the
                    // actuals are `Bot` and match everything.
                    self.pop_vals_peek(&types)?;
                }
                self.pop_vals(&default_types)?;
                self.unreachable();
            }
            Instr::Return => {
                let results = self.current_results().to_vec();
                self.pop_vals(&results)?;
                self.unreachable();
            }
            Instr::Call(index) => {
                let ty = func_at(spaces, *index as usize)?;
                self.pop_vals(&ty.params)?;
                for result in &ty.results {
                    self.push_val(*result);
                }
            }
            Instr::ReturnCall(index) => {
                let ty = func_at(spaces, *index as usize)?.clone();
                self.tail_call(ty, signature)?;
            }
            Instr::CallIndirect {
                type_index,
                table_index,
            } => {
                let table = table_at(spaces, *table_index as usize)?;
                if !table_is_func(table) {
                    return Err(Error::Invalid("type mismatch"));
                }
                let ty = type_at(spaces.module, *type_index)?;
                self.pop_expected(ValType::I32)?;
                self.pop_vals(&ty.params)?;
                for result in &ty.results {
                    self.push_val(*result);
                }
            }
            Instr::ReturnCallIndirect {
                type_index,
                table_index,
            } => {
                let table = table_at(spaces, *table_index as usize)?;
                if !table_is_func(table) {
                    return Err(Error::Invalid("type mismatch"));
                }
                let ty = type_at(spaces.module, *type_index)?;
                self.pop_expected(ValType::I32)?;
                self.tail_call(ty, signature)?;
            }
            Instr::ReturnCallRef(type_index) => {
                let ty = type_at(spaces.module, *type_index)?;
                self.pop_expected(ref_of_func())?;
                self.tail_call(ty, signature)?;
            }
            Instr::CallRef(type_index) => {
                let ty = type_at(spaces.module, *type_index)?;
                self.pop_expected(ref_of_func())?;
                self.pop_vals(&ty.params)?;
                for result in &ty.results {
                    self.push_val(*result);
                }
            }
            Instr::Drop => {
                self.pop_val()?;
            }
            Instr::Select => {
                self.pop_expected(ValType::I32)?;
                let t1 = self.pop_val()?;
                let t2 = self.pop_val()?;
                let numeric = is_num_ty(t1) && is_num_ty(t2);
                let vector = is_vec_ty(t1) && is_vec_ty(t2);
                if !(numeric || vector) {
                    return Err(Error::Invalid("type mismatch"));
                }
                if t1 != t2 && t1 != StackTy::Bot && t2 != StackTy::Bot {
                    return Err(Error::Invalid("type mismatch"));
                }
                self.push(if t1 == StackTy::Bot { t2 } else { t1 });
            }
            Instr::SelectTyped(types) => {
                let [t] = types.as_slice() else {
                    return Err(Error::Invalid("invalid result arity"));
                };
                self.pop_expected(ValType::I32)?;
                self.pop_expected(*t)?;
                self.pop_expected(*t)?;
                self.push_val(*t);
            }
            Instr::LocalGet(index) => {
                let ty = *locals
                    .get(*index as usize)
                    .ok_or(Error::Invalid("unknown local"))?;
                self.push_val(ty);
            }
            Instr::LocalSet(index) => {
                let ty = *locals
                    .get(*index as usize)
                    .ok_or(Error::Invalid("unknown local"))?;
                self.pop_expected(ty)?;
            }
            Instr::LocalTee(index) => {
                let ty = *locals
                    .get(*index as usize)
                    .ok_or(Error::Invalid("unknown local"))?;
                self.pop_val_of(ty)?;
                self.push_val(ty);
            }
            Instr::GlobalGet(index) => {
                let ty = global_at(spaces, *index as usize)?;
                self.push_val(ty.value);
            }
            Instr::GlobalSet(index) => {
                let ty = global_at(spaces, *index as usize)?;
                if !ty.mutable {
                    return Err(Error::Invalid("global is immutable"));
                }
                self.pop_expected(ty.value)?;
            }
            Instr::TableGet(index) => {
                let table = table_at(spaces, *index as usize)?;
                self.pop_expected(ValType::I32)?;
                self.push_val(ValType::Ref(table.element));
            }
            Instr::TableSet(index) => {
                let table = table_at(spaces, *index as usize)?;
                self.pop_expected(ValType::Ref(table.element))?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::Load { op, align, offset } => {
                memory_ok(spaces)?;
                if *offset > u64::from(u32::MAX) {
                    // Memory32 memarg offsets must fit the address type.
                    return Err(Error::Invalid("offset out of range"));
                }
                let natural = load_natural(*op);
                if *align > natural {
                    return Err(Error::Invalid("alignment must not be larger than natural"));
                }
                self.pop_expected(ValType::I32)?;
                self.push_val(load_result(*op));
            }
            Instr::Store { op, align, offset } => {
                memory_ok(spaces)?;
                if *offset > u64::from(u32::MAX) {
                    return Err(Error::Invalid("offset out of range"));
                }
                let natural = store_natural(*op);
                if *align > natural {
                    return Err(Error::Invalid("alignment must not be larger than natural"));
                }
                self.pop_expected(store_value(*op))?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::MemorySize => {
                memory_ok(spaces)?;
                self.push_val(ValType::I32);
            }
            Instr::MemoryGrow => {
                memory_ok(spaces)?;
                self.pop_expected(ValType::I32)?;
                self.push_val(ValType::I32);
            }
            Instr::I32Const(_) => self.push_val(ValType::I32),
            Instr::I64Const(_) => self.push_val(ValType::I64),
            Instr::F32Const(_) => self.push_val(ValType::F32),
            Instr::F64Const(_) => self.push_val(ValType::F64),
            Instr::Num(op) => self.num_op(*op)?,
            Instr::RefNull(heap) => self.push_val(ValType::Ref(RefType {
                nullable: true,
                heap: heap_checked(*heap, spaces)?,
            })),
            Instr::RefIsNull => {
                self.pop_ref()?;
                self.push_val(ValType::I32);
            }
            Instr::RefFunc(index) => {
                if *index as usize >= spaces.funcs.len() {
                    return Err(Error::Invalid("unknown function"));
                }
                let type_index = func_type_of(spaces, *index as usize)?;
                self.push_val(ValType::Ref(RefType {
                    nullable: false,
                    heap: HeapType::Type(type_index),
                }));
            }
            Instr::RefEq => {
                let t = self.pop_val()?;
                if !is_ref_ty(t) {
                    return Err(Error::Invalid("type mismatch"));
                }
                self.pop_matching(t)?;
                self.push_val(ValType::I32);
            }
            Instr::RefAsNonNull => match self.pop_val()? {
                StackTy::Bot => self.push(StackTy::Bot),
                StackTy::Known(ValType::Ref(reference)) => {
                    self.push_val(ValType::Ref(RefType {
                        nullable: false,
                        heap: reference.heap,
                    }));
                }
                StackTy::Known(_) => return Err(Error::Invalid("type mismatch")),
            },
            Instr::BrOnNull(label) => {
                let operand = self.pop_val()?;
                let types = self.label_types(*label as usize)?;
                let known = match operand {
                    StackTy::Bot => None,
                    StackTy::Known(ValType::Ref(reference)) => Some(reference),
                    StackTy::Known(_) => return Err(Error::Invalid("type mismatch")),
                };
                self.pop_vals(&types)?;
                self.push_vals(&types);
                match known {
                    None => self.push(StackTy::Bot),
                    Some(reference) => self.push_val(ValType::Ref(RefType {
                        nullable: false,
                        heap: reference.heap,
                    })),
                }
            }
            Instr::BrOnNonNull(label) => {
                let operand = self.pop_val()?;
                if !is_ref_ty(operand) {
                    return Err(Error::Invalid("type mismatch"));
                }
                let types = self.label_types(*label as usize)?;
                self.pop_vals(&types)?;
                self.push_vals(&types);
                self.push(operand);
            }
            Instr::MemoryInit { data_index, .. } => {
                self.data_index(*data_index as usize, spaces)?;
                memory_ok(spaces)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::DataDrop(data_index) => self.data_index(*data_index as usize, spaces)?,
            Instr::MemoryCopy => {
                memory_ok(spaces)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::MemoryFill => {
                memory_ok(spaces)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::TableInit {
                element_index,
                table,
            } => {
                let table_ty = table_at(spaces, *table as usize)?;
                let Some(element) = spaces.module.elements.get(*element_index as usize) else {
                    return Err(Error::Invalid("unknown elem segment"));
                };
                if !matches_ref(table_ty.element, element.ty) {
                    return Err(Error::Invalid("type mismatch"));
                }
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::ElemDrop(element_index) => {
                if *element_index as usize >= spaces.module.elements.len() {
                    return Err(Error::Invalid("unknown elem segment"));
                }
            }
            Instr::TableCopy => {
                table_at(spaces, 0)?;
                table_at(spaces, 0)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
            }
            Instr::TableGrow => {
                let table = table_at(spaces, 0)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::Ref(table.element))?;
                self.push_val(ValType::I32);
            }
            Instr::TableSize => {
                table_at(spaces, 0)?;
                self.push_val(ValType::I32);
            }
            Instr::TableFill => {
                let table = table_at(spaces, 0)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::I32)?;
                self.pop_expected(ValType::Ref(table.element))?;
            }
        }
        Ok(())
    }

    fn data_index(&mut self, index: usize, spaces: &Spaces<'_>) -> Result<(), Error> {
        let Some(count) = spaces.module.data_count else {
            return Err(Error::Invalid("data count section required"));
        };
        if index as u32 >= count {
            return Err(Error::Invalid("unknown data segment"));
        }
        Ok(())
    }

    fn tail_call(&mut self, ty: FuncType, signature: &FuncType) -> Result<(), Error> {
        self.pop_vals(&ty.params)?;
        // The callee's results become this function's results, so each must
        // be a subtype of the declared result type (typed references: a
        // callee may produce `(ref null $t)` where `funcref` was declared).
        if ty.results.len() != signature.results.len() {
            return Err(Error::Invalid("type mismatch"));
        }
        for (declared, actual) in signature.results.iter().zip(&ty.results) {
            if !matches_val(*declared, *actual) {
                return Err(Error::Invalid("type mismatch"));
            }
        }
        self.unreachable();
        Ok(())
    }

    fn num_op(&mut self, op: NumOp) -> Result<(), Error> {
        let (inputs, outputs) = num_signature(op);
        for ty in inputs.iter().rev() {
            self.pop_expected(*ty)?;
        }
        for ty in outputs {
            self.push_val(ty);
        }
        Ok(())
    }
}

fn resolve_block_type(
    bt: &BlockType,
    spaces: &Spaces<'_>,
) -> Result<(Vec<ValType>, Vec<ValType>), Error> {
    Ok(match bt {
        BlockType::Empty => (vec![], vec![]),
        BlockType::Val(ty) => (vec![], vec![*ty]),
        BlockType::Type(index) => {
            let ty = type_at(spaces.module, *index)?;
            (ty.params, ty.results)
        }
    })
}

fn func_at<'s>(spaces: &'s Spaces<'_>, index: usize) -> Result<&'s FuncType, Error> {
    spaces
        .funcs
        .get(index)
        .ok_or(Error::Invalid("unknown function"))
}

fn global_at<'s>(spaces: &'s Spaces<'_>, index: usize) -> Result<&'s GlobalType, Error> {
    spaces
        .globals
        .get(index)
        .ok_or(Error::Invalid("unknown global"))
}

fn table_at<'s>(spaces: &'s Spaces<'_>, index: usize) -> Result<&'s TableType, Error> {
    spaces
        .tables
        .get(index)
        .ok_or(Error::Invalid("unknown table"))
}

fn memory_ok(spaces: &Spaces<'_>) -> Result<(), Error> {
    if spaces.memories.is_empty() {
        return Err(Error::Invalid("unknown memory"));
    }
    Ok(())
}

fn table_is_func(table: &TableType) -> bool {
    matches_ref(RefType::FUNC, table.element)
}

fn ref_of_func() -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap: HeapType::Func,
    })
}

/// The function *type index* backing function index `index` (a function
/// reference's heap type is its declared type index).
fn func_type_of(spaces: &Spaces<'_>, index: usize) -> Result<u32, Error> {
    let module = spaces.module;
    let imports = module
        .imports
        .iter()
        .filter_map(|import| match import.desc {
            ImportDesc::Func(ty) => Some(ty),
            _ => None,
        });
    let defined = module.functions.iter().copied();
    imports
        .chain(defined)
        .nth(index)
        .ok_or(Error::Invalid("unknown function"))
}

fn heap_checked(heap: HeapType, spaces: &Spaces<'_>) -> Result<HeapType, Error> {
    if let HeapType::Type(index) = heap
        && index as usize >= spaces.module.types.len()
    {
        return Err(Error::Invalid("unknown type"));
    }
    Ok(heap)
}

fn load_natural(op: LoadOp) -> u32 {
    let bytes: u32 = match op {
        LoadOp::I32 | LoadOp::F32 => 4,
        LoadOp::I64 | LoadOp::F64 => 8,
        LoadOp::I32Load8S | LoadOp::I32Load8U | LoadOp::I64Load8S | LoadOp::I64Load8U => 1,
        LoadOp::I32Load16S | LoadOp::I32Load16U | LoadOp::I64Load16S | LoadOp::I64Load16U => 2,
        LoadOp::I64Load32S | LoadOp::I64Load32U => 4,
    };
    bytes.trailing_zeros()
}

fn store_natural(op: StoreOp) -> u32 {
    let bytes: u32 = match op {
        StoreOp::I32 | StoreOp::F32 => 4,
        StoreOp::I64 | StoreOp::F64 => 8,
        StoreOp::I32Store8 | StoreOp::I64Store8 => 1,
        StoreOp::I32Store16 | StoreOp::I64Store16 => 2,
        StoreOp::I64Store32 => 4,
    };
    bytes.trailing_zeros()
}

fn load_result(op: LoadOp) -> ValType {
    match op {
        LoadOp::I32
        | LoadOp::I32Load8S
        | LoadOp::I32Load8U
        | LoadOp::I32Load16S
        | LoadOp::I32Load16U => ValType::I32,
        LoadOp::I64
        | LoadOp::I64Load8S
        | LoadOp::I64Load8U
        | LoadOp::I64Load16S
        | LoadOp::I64Load16U
        | LoadOp::I64Load32S
        | LoadOp::I64Load32U => ValType::I64,
        LoadOp::F32 => ValType::F32,
        LoadOp::F64 => ValType::F64,
    }
}

fn store_value(op: StoreOp) -> ValType {
    match op {
        StoreOp::I32 | StoreOp::I32Store8 | StoreOp::I32Store16 => ValType::I32,
        StoreOp::I64 | StoreOp::I64Store8 | StoreOp::I64Store16 | StoreOp::I64Store32 => {
            ValType::I64
        }
        StoreOp::F32 => ValType::F32,
        StoreOp::F64 => ValType::F64,
    }
}

// ---- constant expressions ----

/// The type a constant expression produces (exactly one value). `visible`
/// bounds the global index space the expression may read: initializers may
/// only see imports plus previously defined globals, while segment offsets
/// see the full space.
fn const_expr_type(expr: &[Instr], spaces: &Spaces<'_>, visible: usize) -> Result<ValType, Error> {
    let mut stack: Vec<ValType> = Vec::new();
    for instr in expr {
        match instr {
            Instr::I32Const(_) => stack.push(ValType::I32),
            Instr::I64Const(_) => stack.push(ValType::I64),
            Instr::F32Const(_) => stack.push(ValType::F32),
            Instr::F64Const(_) => stack.push(ValType::F64),
            Instr::RefNull(heap) => stack.push(ValType::Ref(RefType {
                nullable: true,
                heap: heap_checked(*heap, spaces)?,
            })),
            Instr::RefFunc(index) => {
                if *index as usize >= spaces.funcs.len() {
                    return Err(Error::Invalid("unknown function"));
                }
                let type_index = func_type_of(spaces, *index as usize)?;
                stack.push(ValType::Ref(RefType {
                    nullable: false,
                    heap: HeapType::Type(type_index),
                }));
            }
            Instr::GlobalGet(index) => {
                if *index as usize >= visible {
                    return Err(Error::Invalid("unknown global"));
                }
                let global = &spaces.globals[*index as usize];
                if global.mutable {
                    return Err(Error::Invalid("constant expression required"));
                }
                stack.push(global.value);
            }
            Instr::Num(op) => {
                if !extended_const_op(*op) {
                    return Err(Error::Invalid("constant expression required"));
                }
                let t = num_operand(*op);
                let a = stack.pop().ok_or(Error::Invalid("type mismatch"))?;
                let b = stack.pop().ok_or(Error::Invalid("type mismatch"))?;
                if a != t || b != t {
                    return Err(Error::Invalid("type mismatch"));
                }
                stack.push(t);
            }
            _ => return Err(Error::Invalid("constant expression required")),
        }
    }
    if stack.len() != 1 {
        return Err(Error::Invalid("type mismatch"));
    }
    Ok(stack[0])
}

fn validate_const_expr(
    expr: &[Instr],
    expected: ValType,
    spaces: &Spaces<'_>,
    visible_globals: usize,
) -> Result<(), Error> {
    let actual = const_expr_type(expr, spaces, visible_globals)?;
    if !matches_val(expected, actual) {
        return Err(Error::Invalid("type mismatch"));
    }
    Ok(())
}

fn extended_const_op(op: NumOp) -> bool {
    matches!(
        op,
        NumOp::I32Add
            | NumOp::I32Sub
            | NumOp::I32Mul
            | NumOp::I64Add
            | NumOp::I64Sub
            | NumOp::I64Mul
    )
}

fn num_operand(op: NumOp) -> ValType {
    if matches!(op, NumOp::I32Add | NumOp::I32Sub | NumOp::I32Mul) {
        ValType::I32
    } else {
        ValType::I64
    }
}

// ---- numeric instruction signatures ----

fn num_signature(op: NumOp) -> (Vec<ValType>, Vec<ValType>) {
    use NumOp::*;
    let i32 = ValType::I32;
    let i64 = ValType::I64;
    let f32 = ValType::F32;
    let f64 = ValType::F64;

    let unary = |t| (vec![t], vec![t]);
    let binary = |t| (vec![t, t], vec![t]);
    let test = |t| (vec![t], vec![i32]);
    let compare = |t| (vec![t, t], vec![i32]);
    let convert = |from, to| (vec![from], vec![to]);

    let (inputs, outputs) =
        match op {
            I32Eqz => test(i32),
            I32Eq | I32Ne | I32LtS | I32LtU | I32GtS | I32GtU | I32LeS | I32LeU | I32GeS
            | I32GeU => compare(i32),
            I64Eqz => test(i64),
            I64Eq | I64Ne | I64LtS | I64LtU | I64GtS | I64GtU | I64LeS | I64LeU | I64GeS
            | I64GeU => compare(i64),
            F32Eq | F32Ne | F32Lt | F32Gt | F32Le | F32Ge => compare(f32),
            F64Eq | F64Ne | F64Lt | F64Gt | F64Le | F64Ge => compare(f64),
            I32Clz | I32Ctz | I32Popcnt => unary(i32),
            I32Add | I32Sub | I32Mul | I32DivS | I32DivU | I32RemS | I32RemU | I32And | I32Or
            | I32Xor | I32Shl | I32ShrS | I32ShrU | I32Rotl | I32Rotr => binary(i32),
            I64Clz | I64Ctz | I64Popcnt => unary(i64),
            I64Add | I64Sub | I64Mul | I64DivS | I64DivU | I64RemS | I64RemU | I64And | I64Or
            | I64Xor | I64Shl | I64ShrS | I64ShrU | I64Rotl | I64Rotr => binary(i64),
            F32Abs | F32Neg | F32Ceil | F32Floor | F32Trunc | F32Nearest | F32Sqrt => unary(f32),
            F32Add | F32Sub | F32Mul | F32Div | F32Min | F32Max | F32Copysign => binary(f32),
            F64Abs | F64Neg | F64Ceil | F64Floor | F64Trunc | F64Nearest | F64Sqrt => unary(f64),
            F64Add | F64Sub | F64Mul | F64Div | F64Min | F64Max | F64Copysign => binary(f64),
            I32WrapI64 => convert(i64, i32),
            I32TruncF32S | I32TruncF32U => convert(f32, i32),
            I32TruncF64S | I32TruncF64U => convert(f64, i32),
            I64ExtendI32S | I64ExtendI32U => convert(i32, i64),
            I64TruncF32S | I64TruncF32U => convert(f32, i64),
            I64TruncF64S | I64TruncF64U => convert(f64, i64),
            F32ConvertI32S | F32ConvertI32U => convert(i32, f32),
            F32ConvertI64S | F32ConvertI64U => convert(i64, f32),
            F32DemoteF64 => convert(f64, f32),
            F64ConvertI32S | F64ConvertI32U => convert(i32, f64),
            F64ConvertI64S | F64ConvertI64U => convert(i64, f64),
            F64PromoteF32 => convert(f32, f64),
            I32ReinterpretF32 => convert(f32, i32),
            I64ReinterpretF64 => convert(f64, i64),
            F32ReinterpretI32 => convert(i32, f32),
            F64ReinterpretI64 => convert(i64, f64),
            I32Extend8S | I32Extend16S => unary(i32),
            I64Extend8S | I64Extend16S | I64Extend32S => unary(i64),
            I32TruncSatF32S | I32TruncSatF32U => convert(f32, i32),
            I32TruncSatF64S | I32TruncSatF64U => convert(f64, i32),
            I64TruncSatF32S | I64TruncSatF32U => convert(f32, i64),
            I64TruncSatF64S | I64TruncSatF64U => convert(f64, i64),
        };
    (inputs, outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::{DataMode, DataSegment, FuncBody, Global, Import, ImportDesc};

    fn func_type(params: Vec<ValType>, results: Vec<ValType>) -> FuncType {
        FuncType { params, results }
    }

    /// A module with the given types and defined functions `(type idx, locals,
    /// body)`.
    fn module(types: Vec<FuncType>, funcs: Vec<(u32, Vec<ValType>, Vec<Instr>)>) -> Module {
        Module {
            functions: funcs.iter().map(|(ty, _, _)| *ty).collect(),
            bodies: funcs
                .iter()
                .map(|(_, locals, body)| FuncBody {
                    locals: locals.clone(),
                    body: body.clone(),
                })
                .collect(),
            types,
            ..Module::default()
        }
    }

    #[test]
    fn empty_module_is_valid() {
        assert!(validate(&Module::default()).is_ok());
    }

    #[test]
    fn call_index_must_resolve() {
        let known = module(
            vec![func_type(vec![], vec![])],
            vec![(0, vec![], vec![Instr::Call(0)])],
        );
        assert!(validate(&known).is_ok());
        let unknown = module(
            vec![func_type(vec![], vec![])],
            vec![(0, vec![], vec![Instr::Call(1)])],
        );
        assert!(validate(&unknown).is_err());
    }

    #[test]
    fn untyped_select_rejects_references() {
        let funcref = ValType::Ref(RefType::FUNC);
        // `(select (local.get 0) (local.get 1) (local.get 2)) (drop)` over
        // funcref operands: the implicit select only covers numeric types.
        let body = vec![
            Instr::LocalGet(0),
            Instr::LocalGet(1),
            Instr::LocalGet(2),
            Instr::Select,
            Instr::Drop,
        ];
        let untyped = module(
            vec![func_type(vec![funcref, funcref, ValType::I32], vec![])],
            vec![(0, vec![], body)],
        );
        assert!(validate(&untyped).is_err());

        let typed_body = vec![
            Instr::LocalGet(0),
            Instr::LocalGet(1),
            Instr::LocalGet(2),
            Instr::SelectTyped(vec![funcref]),
            Instr::Drop,
        ];
        let typed = module(
            vec![func_type(vec![funcref, funcref, ValType::I32], vec![])],
            vec![(0, vec![], typed_body)],
        );
        assert!(validate(&typed).is_ok());
    }

    #[test]
    fn typed_select_arity_is_exactly_one() {
        let body = vec![
            Instr::I32Const(1),
            Instr::I32Const(2),
            Instr::I32Const(3),
            Instr::SelectTyped(vec![]),
        ];
        let m = module(
            vec![func_type(vec![], vec![ValType::I32])],
            vec![(0, vec![], body)],
        );
        assert!(validate(&m).is_err());
    }

    #[test]
    fn unreachable_code_is_polymorphic() {
        // `ref.is_null` over the unreachable hole is fine: the hole may be any
        // reference type.
        let body = vec![Instr::Unreachable, Instr::RefIsNull, Instr::Drop];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], body)]);
        assert!(validate(&m).is_ok());

        // Concrete values still type-check in dead code: an i32 cannot feed
        // an `i64.add`.
        let bad = vec![
            Instr::Unreachable,
            Instr::I32Const(0),
            Instr::Num(NumOp::I64Add),
        ];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], bad)]);
        assert!(validate(&m).is_err());
    }

    #[test]
    fn unreachable_br_table_ignores_label_types() {
        // Spec's `meet-bottom` shape: an inner f32 block whose `br_table`
        // targets an outer f64 block. Only the arity (1) must agree; the
        // types are unchecked because the code is unreachable.
        let body = vec![
            Instr::Block(BlockType::Val(ValType::F64)),
            Instr::Block(BlockType::Val(ValType::F32)),
            Instr::Unreachable,
            Instr::I32Const(1),
            Instr::BrTable {
                targets: vec![0, 1],
                default: 1,
            },
            Instr::End,
            Instr::Drop,
            Instr::F64Const(0),
            Instr::End,
            Instr::Drop,
        ];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], body)]);
        assert!(validate(&m).is_ok(), "meet-bottom should validate");
    }

    #[test]
    fn block_results_flow_to_the_enclosing_frame() {
        let body = vec![
            Instr::Block(BlockType::Val(ValType::I32)),
            Instr::I32Const(7),
            Instr::End,
            Instr::Drop,
        ];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], body)]);
        assert!(validate(&m).is_ok());

        // A stray value left on the stack at the function end is rejected.
        let stray = vec![Instr::I32Const(7)];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], stray)]);
        assert!(validate(&m).is_err());
    }

    #[test]
    fn function_implicit_label_is_a_branch_target() {
        // `(func (br 0))` branches to the function's own label.
        let body = vec![Instr::Br(0)];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], body)]);
        assert!(validate(&m).is_ok());

        // Branching needs the function's results on the stack.
        let body = vec![Instr::I32Const(1), Instr::Br(0)];
        let m = module(
            vec![func_type(vec![], vec![ValType::I32])],
            vec![(0, vec![], body)],
        );
        assert!(validate(&m).is_ok());
        let missing = vec![Instr::Br(0)];
        let m = module(
            vec![func_type(vec![], vec![ValType::I32])],
            vec![(0, vec![], missing)],
        );
        assert!(validate(&m).is_err());

        // Label 1 from inside a block targets the function label too.
        let deep = vec![Instr::Block(BlockType::Empty), Instr::Br(1), Instr::End];
        let m = module(vec![func_type(vec![], vec![])], vec![(0, vec![], deep)]);
        assert!(validate(&m).is_ok());
    }

    #[test]
    fn if_without_else_must_be_identity_typed() {
        // `(if (result i32) (then (i32.const 1)))` cannot feed the false
        // path, so it is invalid.
        let body = vec![
            Instr::I32Const(1),
            Instr::If(BlockType::Val(ValType::I32)),
            Instr::I32Const(1),
            Instr::End,
        ];
        let m = module(
            vec![func_type(vec![], vec![ValType::I32])],
            vec![(0, vec![], body)],
        );
        assert!(validate(&m).is_err());

        // But an else-less `if (param i32 i32) (result i32 i32)` is the
        // identity on the false path and is valid.
        let identity = func_type(
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32, ValType::I32],
        );
        let body = vec![
            Instr::I32Const(1),
            Instr::I32Const(2),
            Instr::I32Const(0), // condition
            Instr::If(BlockType::Type(0)),
            Instr::End,
        ];
        let m = module(vec![identity], vec![(0, vec![], body)]);
        assert!(
            validate(&m).is_ok(),
            "identity if without else should validate"
        );
    }

    #[test]
    fn segment_offsets_may_read_defined_immutable_globals() {
        let memory = Module {
            memories: vec![MemType {
                limits: Limits::new(1, None),
                memory64: false,
            }],
            globals: vec![Global {
                ty: GlobalType {
                    value: ValType::I32,
                    mutable: false,
                },
                init: vec![Instr::I32Const(0)],
            }],
            data: vec![DataSegment {
                mode: DataMode::Active {
                    memory: 0,
                    offset: vec![Instr::GlobalGet(0)],
                },
                bytes: vec![],
            }],
            ..Module::default()
        };
        assert!(validate(&memory).is_ok());

        let mutable = Module {
            globals: vec![Global {
                ty: GlobalType {
                    value: ValType::I32,
                    mutable: true,
                },
                init: vec![Instr::I32Const(0)],
            }],
            data: vec![DataSegment {
                mode: DataMode::Active {
                    memory: 0,
                    offset: vec![Instr::GlobalGet(0)],
                },
                bytes: vec![],
            }],
            ..memory
        };
        assert!(validate(&mutable).is_err());
    }

    #[test]
    fn active_data_segment_needs_a_memory() {
        let m = Module {
            data: vec![DataSegment {
                mode: DataMode::Active {
                    memory: 0,
                    offset: vec![Instr::I32Const(0)],
                },
                bytes: vec![],
            }],
            ..Module::default()
        };
        assert!(validate(&m).is_err());
    }

    #[test]
    fn tag_imports_need_empty_result_types() {
        // Exception tags refer to function types whose results are empty.
        let ok = Module {
            types: vec![func_type(vec![], vec![])],
            imports: vec![Import {
                module: "m".into(),
                name: "t".into(),
                desc: ImportDesc::Tag(0),
            }],
            ..Module::default()
        };
        assert!(validate(&ok).is_ok());

        let bad = Module {
            types: vec![func_type(vec![], vec![ValType::I32])],
            imports: vec![Import {
                module: "m".into(),
                name: "t".into(),
                desc: ImportDesc::Tag(0),
            }],
            ..Module::default()
        };
        assert!(validate(&bad).is_err());
    }
}
