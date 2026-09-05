//! The wasm type system (spec ch. 2.3) and the module-level types built
//! from it. The binary encodings live in `binary`; validation of these
//! types is Cut 2.

use std::fmt;

/// A heap type: an abstract one (`func`, `extern`, `exn`, ...) or a type
/// index (typed references). GC-era abstract heap types are added with their
/// cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeapType {
    Func,
    Extern,
    /// Exception references (`exn`, the `exnref` heap type).
    Exn,
    /// A user-defined type index (`(ref $t)`), carried as an s33 index.
    Type(u32),
}

/// A reference type: nullable/`ref null` or non-null `ref`, over a heap type.
/// The abstract `funcref`/`externref` are `RefNull` over `Func`/`Extern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RefType {
    pub nullable: bool,
    pub heap: HeapType,
}

impl RefType {
    pub const FUNC: Self = RefType {
        nullable: true,
        heap: HeapType::Func,
    };
    pub const EXTERN: Self = RefType {
        nullable: true,
        heap: HeapType::Extern,
    };
    pub const EXN: Self = RefType {
        nullable: true,
        heap: HeapType::Exn,
    };
}

/// A value type (spec 2.3.6). `V128` and the GC heap types are decoded now
/// so the model is stable, but only exercised by their own feature cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    V128,
    Ref(RefType),
}

impl ValType {
    /// The single-byte binary form of the compact value types, if any.
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x7f => Self::I32,
            0x7e => Self::I64,
            0x7d => Self::F32,
            0x7c => Self::F64,
            0x7b => Self::V128,
            0x70 => Self::Ref(RefType::FUNC),
            0x6f => Self::Ref(RefType::EXTERN),
            0x69 => Self::Ref(RefType::EXN),
            _ => return None,
        })
    }
}

impl fmt::Display for HeapType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeapType::Func => f.write_str("func"),
            HeapType::Extern => f.write_str("extern"),
            HeapType::Exn => f.write_str("exn"),
            HeapType::Type(index) => write!(f, "${index}"),
        }
    }
}

impl fmt::Display for ValType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValType::I32 => f.write_str("i32"),
            ValType::I64 => f.write_str("i64"),
            ValType::F32 => f.write_str("f32"),
            ValType::F64 => f.write_str("f64"),
            ValType::V128 => f.write_str("v128"),
            ValType::Ref(r) => write!(
                f,
                "(ref {}{})",
                if r.nullable { "null " } else { "" },
                r.heap
            ),
        }
    }
}

/// A function type: parameter and result value types (spec 2.3.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

/// Limits for tables/memories (spec 2.3.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min: u64,
    pub max: Option<u64>,
    /// Shared memories (`shared` flag) — threads cut.
    pub shared: bool,
}

impl Limits {
    pub const fn new(min: u64, max: Option<u64>) -> Self {
        Self {
            min,
            max,
            shared: false,
        }
    }
}

/// A table type: element reference type plus limits (spec 2.3.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableType {
    pub element: RefType,
    pub limits: Limits,
}

/// A memory type (spec 2.3.15). Memory64 uses a 64-bit index type; the
/// flag arrives with that cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemType {
    pub limits: Limits,
    pub memory64: bool,
}

/// A global type (spec 2.3.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalType {
    pub value: ValType,
    pub mutable: bool,
}

/// The type of a block result in the binary: empty, a value type, or a
/// function type index (multi-value), spec 2.4.2/2.3.8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    Empty,
    Val(ValType),
    Type(u32),
}
