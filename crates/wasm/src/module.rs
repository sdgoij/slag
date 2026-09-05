//! The decoded module model (spec ch. 2.5).
//!
//! Produced by the binary decoder (`binary`); validated in Cut 2 and
//! instantiated/executed in later cuts.

use crate::instr::Instr;
use crate::types::{FuncType, GlobalType, Limits, MemType, RefType, TableType, ValType};

/// An import description (spec 2.5.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDesc {
    Func(u32),
    Table(TableType),
    Memory(MemType),
    Global(GlobalType),
}

/// A single import: module and field names plus the imported descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub desc: ImportDesc,
}

/// An exported name → index (spec 2.5.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub kind: ExportKind,
    pub index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

/// A global definition: its type plus a constant initializer expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    pub ty: GlobalType,
    pub init: Vec<Instr>,
}

/// A function body (spec 2.5.7/5.5.13): local declarations then the
/// instruction stream (without the terminating `end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncBody {
    pub locals: Vec<ValType>,
    pub body: Vec<Instr>,
}

/// How an element segment is instantiated (spec 2.5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementMode {
    /// `(table $t) (offset $e)` — run the offset expression at instantiation.
    Active {
        table: u32,
        offset: Vec<Instr>,
    },
    Passive,
    Declarative,
}

/// An element segment: the reference type of its items and the per-item
/// initializer expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementSegment {
    pub ty: RefType,
    pub mode: ElementMode,
    pub init: Vec<Vec<Instr>>,
}

/// How a data segment is instantiated (spec 2.5.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataMode {
    Active { memory: u32, offset: Vec<Instr> },
    Passive,
}

/// A data segment: destination mode plus raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSegment {
    pub mode: DataMode,
    pub bytes: Vec<u8>,
}

/// A custom section: its (possibly non-UTF-8) name and raw payload. Kept
/// verbatim for the JS-API `customSections` surface; contents are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSection {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// The decoded module. Index spaces follow the spec: the *module's* funcs
/// are the import funcs followed by the function section's functions, and
/// `bodies` parallels the non-import function declarations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    /// Type index of each module-defined function (not imports).
    pub functions: Vec<u32>,
    pub tables: Vec<TableType>,
    pub memories: Vec<MemType>,
    pub globals: Vec<Global>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elements: Vec<ElementSegment>,
    pub data: Vec<DataSegment>,
    /// The data-count section's count, if present.
    pub data_count: Option<u32>,
    pub custom: Vec<CustomSection>,
    /// Parallels `functions`.
    pub bodies: Vec<FuncBody>,
}

/// Convenience for building tiny modules in tests and fixtures: a minimal
/// limits/default constructor set.
impl Module {
    pub fn memory_type(pages: u32) -> MemType {
        MemType {
            limits: Limits::new(u64::from(pages), None),
            memory64: false,
        }
    }

    pub fn func_type(params: Vec<ValType>, results: Vec<ValType>) -> FuncType {
        FuncType { params, results }
    }
}
