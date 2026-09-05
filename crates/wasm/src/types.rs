//! The wasm type system (spec ch. 2.3) and the module-level types built
//! from it. The binary encodings live in `binary`; validation of these
//! types is Cut 2.

use std::fmt;

/// A heap type: an abstract one (`func`, `extern`, `exn`, the GC hierarchy
/// `any`/`eq`/`i31`/`struct`/`array` and their bottoms) or a type index
/// (typed references). The abstract forms are encoded as small negative type
/// indices in the binary format (spec 5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeapType {
    Func,
    Extern,
    /// Exception references (`exn`, the `exnref` heap type).
    Exn,
    /// The common subtype of all external references.
    NoExtern,
    /// The common subtype of all function types.
    NoFunc,
    /// The common subtype of all exception types.
    NoExn,
    /// The common subtype of all aggregate types (`none`).
    None,
    /// Any GC object (struct/array/i31).
    Any,
    /// Anything `ref.eq` can compare (struct/array/i31).
    Eq,
    /// An unboxed scalar reference (`i31`).
    I31,
    /// The common supertype of all struct types.
    Struct,
    /// The common supertype of all array types.
    Array,
    /// A user-defined type index (`(ref $t)`), carried as an s33 index.
    Type(u32),
}

impl HeapType {
    /// The abstract heap type a negative type index encodes, if any
    /// (spec 5.3: `func` = -0x10 ... `array` = -0x16).
    pub const fn from_s33(index: i64) -> Option<HeapType> {
        Some(match index {
            -0x10 => HeapType::Func,
            -0x11 => HeapType::Extern,
            -0x12 => HeapType::Any,
            -0x13 => HeapType::Eq,
            -0x14 => HeapType::I31,
            -0x15 => HeapType::Struct,
            -0x16 => HeapType::Array,
            -0x17 => HeapType::Exn,
            -0x0f => HeapType::None,
            -0x0e => HeapType::NoExtern,
            -0x0d => HeapType::NoFunc,
            -0x0c => HeapType::NoExn,
            _ => return None,
        })
    }
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
            HeapType::NoFunc => f.write_str("nofunc"),
            HeapType::NoExtern => f.write_str("noextern"),
            HeapType::NoExn => f.write_str("noexn"),
            HeapType::None => f.write_str("none"),
            HeapType::Any => f.write_str("any"),
            HeapType::Eq => f.write_str("eq"),
            HeapType::I31 => f.write_str("i31"),
            HeapType::Struct => f.write_str("struct"),
            HeapType::Array => f.write_str("array"),
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

/// A storage type (spec 2.3.7): the type stored in a struct field or array
/// element, including the packed `i8`/`i16` forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageType {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    V128,
    Ref(RefType),
}

impl StorageType {
    /// The value type read from/written to a field whose storage type is
    /// wider than a packed integer. Packed fields use the i32 value type.
    pub fn val(&self) -> ValType {
        match self {
            StorageType::I8 | StorageType::I16 | StorageType::I32 => ValType::I32,
            StorageType::I64 => ValType::I64,
            StorageType::F32 => ValType::F32,
            StorageType::F64 => ValType::F64,
            StorageType::V128 => ValType::V128,
            StorageType::Ref(r) => ValType::Ref(*r),
        }
    }
}

impl From<ValType> for StorageType {
    fn from(value: ValType) -> Self {
        match value {
            ValType::I32 => StorageType::I32,
            ValType::I64 => StorageType::I64,
            ValType::F32 => StorageType::F32,
            ValType::F64 => StorageType::F64,
            ValType::V128 => StorageType::V128,
            ValType::Ref(r) => StorageType::Ref(r),
        }
    }
}

/// A struct field (spec 2.3.7): a storage type plus mutability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldType {
    pub ty: StorageType,
    pub mutable: bool,
}

/// A composite type (spec 2.3.9): a function, struct, or array type. The
/// type-index space of a module is the flat list of its subtypes; a rec
/// group's members occupy consecutive indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeType {
    Func(FuncType),
    Struct(Vec<FieldType>),
    Array(FieldType),
}

impl CompositeType {
    pub fn as_func(&self) -> Option<&FuncType> {
        match self {
            CompositeType::Func(func) => Some(func),
            _ => None,
        }
    }
}

/// A defined type (spec 2.3.10): an optionally-final subtype with up to one
/// supertype, over a composite type. Bare types (no `sub`) are final with no
/// supertypes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubType {
    pub is_final: bool,
    /// Supertype indices within the same module's type space (at most one in
    /// the GC MVP).
    pub supertypes: Vec<u32>,
    pub composite: CompositeType,
}

impl SubType {
    pub fn func(params: Vec<ValType>, results: Vec<ValType>) -> SubType {
        SubType {
            is_final: true,
            supertypes: Vec::new(),
            composite: CompositeType::Func(FuncType { params, results }),
        }
    }
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

/// A table type: element reference type plus limits (spec 2.3.16). `table64`
/// selects an i64 address/index type (mirroring memory64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableType {
    pub element: RefType,
    pub limits: Limits,
    pub table64: bool,
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
