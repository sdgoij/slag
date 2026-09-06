//! The wasm binary decoder (spec ch. 5): bytes → [`Module`].
//!
//! Cut 1 scope: every section payload (types, imports, functions, tables,
//! memories, globals, exports, start, elements, data, data count, custom)
//! and the full baseline instruction stream, including the merged post-MVP
//! opcodes the core suite exercises (multi-value, sign extension,
//! saturating conversions, reference types, tail calls, bulk memory).
//! Structural errors are reported as malformed; typing is Cut 2.

use std::fmt;

use crate::instr::{Catch, Instr, LoadOp, NumOp, StoreOp, VecLoadOp};
use crate::module::{
    CustomSection, DataMode, DataSegment, ElementMode, ElementSegment, Export, ExportKind,
    FuncBody, Global, Import, ImportDesc, Module, Table,
};
use crate::types::{
    BlockType, CompositeType, FieldType, FuncType, GlobalType, HeapType, Limits, MemType, RefType,
    StorageType, SubType, TableType, ValType,
};

/// A binary-format error. `Malformed` mirrors the spec's diagnostics; the
/// runner classifies modules by *which* phase fails (decode vs validation),
/// so an error here must always mean "the bytes are structurally wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed(&'static str),
    /// Structurally fine but from a feature this engine does not decode yet
    /// (SIMD, GC, exceptions, memory64, multi-memory). Never reported as
    /// malformed, so conformance counts don't lie.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(message) => write!(f, "malformed: {message}"),
            Error::Unsupported(message) => write!(f, "unsupported: {message}"),
        }
    }
}

impl std::error::Error for Error {}

const MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
const VERSION: u32 = 1;

/// Sanity bound on expanded locals per function body; generous enough for
/// any real module while keeping hostile count fields from exhausting memory.
const MAX_LOCALS: usize = 1 << 20;

/// Section ids and the order the spec requires (custom sections may appear
/// anywhere; the tag section sits between memory and global; `DataCount`
/// between element and code). Ordering for the "sections in order" check is
/// positional (see [`SectionId::position`]), not the numeric id, because the
/// tag id 13 sorts positionally between memory and global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionId {
    Custom,
    Type,
    Import,
    Function,
    Table,
    Memory,
    Tag,
    Global,
    Export,
    Start,
    Element,
    DataCount,
    Code,
    Data,
}

impl SectionId {
    fn from_u8(id: u8) -> Option<Self> {
        Some(match id {
            0 => Self::Custom,
            1 => Self::Type,
            2 => Self::Import,
            3 => Self::Function,
            4 => Self::Table,
            5 => Self::Memory,
            6 => Self::Global,
            7 => Self::Export,
            8 => Self::Start,
            9 => Self::Element,
            10 => Self::Code,
            11 => Self::Data,
            12 => Self::DataCount,
            13 => Self::Tag,
            _ => return None,
        })
    }

    /// Position in the required section sequence; `None` for custom.
    fn position(self) -> Option<u8> {
        Some(match self {
            Self::Type => 1,
            Self::Import => 2,
            Self::Function => 3,
            Self::Table => 4,
            Self::Memory => 5,
            Self::Tag => 6,
            Self::Global => 7,
            Self::Export => 8,
            Self::Start => 9,
            Self::Element => 10,
            Self::DataCount => 11,
            Self::Code => 12,
            Self::Data => 13,
            Self::Custom => return None,
        })
    }
}

/// Decode a whole module.
pub fn decode(bytes: &[u8]) -> Result<Module, Error> {
    if bytes.len() < 8 {
        return Err(Error::Malformed("unexpected end of module"));
    }
    if bytes[0..4] != MAGIC {
        return Err(Error::Malformed("magic header not detected"));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("checked length"));
    if version != VERSION {
        return Err(Error::Malformed("unknown binary version"));
    }

    let mut module = Module::default();
    let mut cursor = 8;
    let mut last: Option<SectionId> = None;
    while cursor < bytes.len() {
        let id = bytes[cursor];
        cursor += 1;
        let Some(id) = SectionId::from_u8(id) else {
            return Err(Error::Malformed("malformed section id"));
        };
        let mut size_pos = cursor;
        let size = read_u32(bytes, &mut size_pos)?;
        let start = size_pos;
        let Some(end) = start.checked_add(size as usize) else {
            return Err(Error::Malformed("section size mismatch"));
        };
        if end > bytes.len() {
            return Err(Error::Malformed("section size mismatch"));
        }
        if id != SectionId::Custom {
            if let Some(previous) = last {
                let order = id.position().expect("non-custom section");
                let previous_order = previous.position().expect("non-custom section");
                if order == previous_order {
                    return Err(Error::Malformed("duplicate section"));
                }
                if order < previous_order {
                    return Err(Error::Malformed("section out of order"));
                }
            }
            last = Some(id);
        }
        let payload = &bytes[start..end];
        decode_section(id, payload, &mut module)?;
        cursor = end;
    }
    // Cross-section structural rules (spec 5.5): a function section without a
    // matching code section, and a data count that disagrees with the data
    // section's actual segment count, are malformed.
    if module.functions.len() != module.bodies.len() {
        return Err(Error::Malformed(
            "function and code section have inconsistent lengths",
        ));
    }
    if let Some(declared) = module.data_count
        && declared as usize != module.data.len()
    {
        return Err(Error::Malformed(
            "data count and data section have inconsistent lengths",
        ));
    }
    Ok(module)
}

fn decode_section(id: SectionId, payload: &[u8], module: &mut Module) -> Result<(), Error> {
    let mut pos = 0;
    match id {
        SectionId::Custom => {
            let name = read_custom_name(payload, &mut pos)?;
            module.custom.push(CustomSection {
                name,
                data: payload[pos..].to_vec(),
            });
        }
        SectionId::Type => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                decode_rectype(payload, &mut pos, &mut module.types)?;
            }
        }
        SectionId::Import => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                let import_module = read_name(payload, &mut pos)?;
                let import_name = read_name(payload, &mut pos)?;
                let kind = read_u8(payload, &mut pos)?;
                let desc = match kind {
                    0x00 => ImportDesc::Func(read_u32(payload, &mut pos)?),
                    0x01 => ImportDesc::Table(decode_table_type(payload, &mut pos)?),
                    0x02 => ImportDesc::Memory(decode_mem_type(payload, &mut pos)?),
                    0x03 => ImportDesc::Global(decode_global_type(payload, &mut pos)?),
                    0x04 => {
                        let attribute = read_u8(payload, &mut pos)?;
                        if attribute != 0 {
                            return Err(Error::Malformed("malformed tag attribute"));
                        }
                        ImportDesc::Tag(read_u32(payload, &mut pos)?)
                    }
                    _ => return Err(Error::Malformed("malformed import kind")),
                };
                module.imports.push(Import {
                    module: import_module,
                    name: import_name,
                    desc,
                });
            }
        }
        SectionId::Function => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                module.functions.push(read_u32(payload, &mut pos)?);
            }
        }
        SectionId::Table => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                module.tables.push(decode_table_entry(payload, &mut pos)?);
            }
        }
        SectionId::Memory => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                module.memories.push(decode_mem_type(payload, &mut pos)?);
            }
        }
        SectionId::Tag => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                let attribute = read_u8(payload, &mut pos)?;
                if attribute != 0 {
                    return Err(Error::Malformed("malformed tag attribute"));
                }
                module.tags.push(read_u32(payload, &mut pos)?);
            }
        }
        SectionId::Global => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                let ty = decode_global_type(payload, &mut pos)?;
                let init = decode_expr(payload, &mut pos)?;
                module.globals.push(Global { ty, init });
            }
        }
        SectionId::Export => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                let name = read_name(payload, &mut pos)?;
                let kind = read_u8(payload, &mut pos)?;
                let index = read_u32(payload, &mut pos)?;
                let kind = match kind {
                    0x00 => ExportKind::Func,
                    0x01 => ExportKind::Table,
                    0x02 => ExportKind::Memory,
                    0x03 => ExportKind::Global,
                    0x04 => ExportKind::Tag,
                    _ => return Err(Error::Malformed("malformed export kind")),
                };
                module.exports.push(Export { name, kind, index });
            }
        }
        SectionId::Start => {
            module.start = Some(read_u32(payload, &mut pos)?);
        }
        SectionId::Element => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                module.elements.push(decode_element(payload, &mut pos)?);
            }
        }
        SectionId::DataCount => {
            module.data_count = Some(read_u32(payload, &mut pos)?);
        }
        SectionId::Code => {
            let count = read_u32(payload, &mut pos)?;
            if count != module.functions.len() as u32 {
                return Err(Error::Malformed(
                    "function and code section have inconsistent lengths",
                ));
            }
            for _ in 0..count {
                let size = read_u32(payload, &mut pos)? as usize;
                let start = pos;
                let Some(end) = start.checked_add(size) else {
                    return Err(Error::Malformed("function body too large"));
                };
                if end > payload.len() {
                    return Err(Error::Malformed("unexpected end of section or function"));
                }
                let body = decode_body(&payload[start..end])?;
                // `memory.init`/`data.drop` are only encodable when the module
                // carries a data count section (spec 5.4.5): without one the
                // body is malformed, whatever validation would later say.
                if module.data_count.is_none()
                    && body
                        .body
                        .iter()
                        .any(|instr| matches!(instr, Instr::MemoryInit { .. } | Instr::DataDrop(_)))
                {
                    return Err(Error::Malformed("data count section required"));
                }
                module.bodies.push(body);
                pos = end;
            }
        }
        SectionId::Data => {
            let count = read_u32(payload, &mut pos)?;
            for _ in 0..count {
                module.data.push(decode_data(payload, &mut pos)?);
            }
        }
    }
    // A section payload must be consumed exactly: a count that leaves trailing
    // bytes (more items than declared) or stops short is malformed, not a
    // partially-read module (custom sections are the exception — their data is
    // read to the payload end by construction).
    if id != SectionId::Custom && pos != payload.len() {
        return Err(Error::Malformed("section size mismatch"));
    }
    Ok(())
}

/// A type-section entry (spec 5.3): a recursion group (0x4e followed by its
/// subtypes) or, as the unary shorthand, a single subtype written bare.
fn decode_rectype(bytes: &[u8], pos: &mut usize, types: &mut Vec<SubType>) -> Result<(), Error> {
    if bytes.get(*pos) == Some(&0x4e) {
        *pos += 1;
        let count = read_u32(bytes, pos)?;
        for _ in 0..count {
            decode_subtype(bytes, pos, types)?;
        }
    } else {
        decode_subtype(bytes, pos, types)?;
    }
    Ok(())
}

/// A defined type (spec 5.3). The 0x4f/0x50 prefixes mark `sub final`/`sub`;
/// a bare composite type is the shorthand for a final type without
/// supertypes. Both prefixes are followed by the supertype list (0 or 1 in
/// the GC MVP), then the composite type.
fn decode_subtype(bytes: &[u8], pos: &mut usize, types: &mut Vec<SubType>) -> Result<(), Error> {
    let byte = bytes
        .get(*pos)
        .copied()
        .ok_or(Error::Malformed("unexpected end of section or function"))?;
    let (is_final, prefixed) = match byte {
        0x4f => (true, true),
        0x50 => (false, true),
        _ => (true, false),
    };
    let supertypes = if prefixed {
        *pos += 1;
        let count = read_u32(bytes, pos)?;
        // The GC MVP allows a single supertype; any count is a malformed
        // module rather than a later feature.
        if count > 1 {
            return Err(Error::Malformed("malformed subtype"));
        }
        let mut list = Vec::with_capacity(count as usize);
        for _ in 0..count {
            list.push(read_u32(bytes, pos)?);
        }
        list
    } else {
        Vec::new()
    };
    // The shared/descriptor markers of later proposals are not in the corpus.
    if bytes.get(*pos) == Some(&0x65) {
        return Err(Error::Unsupported("shared type"));
    }
    let composite = decode_comptype(bytes, pos)?;
    types.push(SubType {
        is_final,
        supertypes,
        composite,
    });
    Ok(())
}

/// A composite type (spec 5.3): a function, struct, or array type.
fn decode_comptype(bytes: &[u8], pos: &mut usize) -> Result<CompositeType, Error> {
    match read_u8(bytes, pos)? {
        0x60 => {
            let params = read_valtype_vec(bytes, pos)?;
            let results = read_valtype_vec(bytes, pos)?;
            Ok(CompositeType::Func(FuncType { params, results }))
        }
        0x5e => Ok(CompositeType::Array(decode_field_type(bytes, pos)?)),
        0x5f => {
            let count = read_u32(bytes, pos)?;
            let mut fields = Vec::with_capacity(count as usize);
            for _ in 0..count {
                fields.push(decode_field_type(bytes, pos)?);
            }
            Ok(CompositeType::Struct(fields))
        }
        0x5d => Err(Error::Unsupported("continuation type")),
        _ => Err(Error::Malformed("malformed composite type")),
    }
}

/// A struct field / array element type: a storage type plus a mutability
/// byte (spec 5.3).
fn decode_field_type(bytes: &[u8], pos: &mut usize) -> Result<FieldType, Error> {
    let ty = decode_storage_type(bytes, pos)?;
    let mutable = match read_u8(bytes, pos)? {
        0x00 => false,
        0x01 => true,
        _ => return Err(Error::Malformed("malformed mutability")),
    };
    Ok(FieldType { ty, mutable })
}

/// A storage type (spec 5.3): the packed i8/i16 forms plus every value type.
fn decode_storage_type(bytes: &[u8], pos: &mut usize) -> Result<StorageType, Error> {
    Ok(match read_u8(bytes, pos)? {
        0x78 => StorageType::I8,
        0x77 => StorageType::I16,
        0x7f => StorageType::I32,
        0x7e => StorageType::I64,
        0x7d => StorageType::F32,
        0x7c => StorageType::F64,
        0x7b => StorageType::V128,
        0x63 => {
            let heap = decode_heap_type(bytes, pos)?;
            return Ok(StorageType::Ref(RefType {
                nullable: true,
                heap,
            }));
        }
        0x64 => {
            let heap = decode_heap_type(bytes, pos)?;
            return Ok(StorageType::Ref(RefType {
                nullable: false,
                heap,
            }));
        }
        byte if abstract_heap_byte(byte).is_some() => StorageType::Ref(RefType {
            nullable: true,
            heap: abstract_heap_byte(byte).expect("checked"),
        }),
        _ => return Err(Error::Malformed("malformed value type")),
    })
}

/// The heap type a single-byte s33 negative encodes, when it is one of the
/// abstract heap types (spec 5.3). A single-byte s33's sign bit is bit 6.
fn abstract_heap_byte(byte: u8) -> Option<HeapType> {
    if byte < 0x80 {
        let value = if byte & 0x40 != 0 {
            i64::from(byte) - 128
        } else {
            i64::from(byte)
        };
        HeapType::from_s33(value)
    } else {
        None
    }
}

fn read_valtype_vec(bytes: &[u8], pos: &mut usize) -> Result<Vec<ValType>, Error> {
    let count = read_u32(bytes, pos)?;
    // No preallocation from an untrusted count: a malformed length would
    // otherwise allocate before the bounds check rejects it.
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(decode_valtype(bytes, pos)?);
    }
    Ok(out)
}

fn decode_valtype(bytes: &[u8], pos: &mut usize) -> Result<ValType, Error> {
    let byte = read_u8(bytes, pos)?;
    match byte {
        0x63 | 0x64 => {
            // `(ref null ht)` / `(ref ht)`.
            let heap = decode_heap_type(bytes, pos)?;
            Ok(ValType::Ref(RefType {
                nullable: byte == 0x63,
                heap,
            }))
        }
        // An abstract heap type byte in value-type position is the nullable
        // reference shorthand (`anyref` = `(ref null any)`, etc.).
        _ => match abstract_heap_byte(byte) {
            Some(heap) => Ok(ValType::Ref(RefType {
                nullable: true,
                heap,
            })),
            None => ValType::from_byte(byte).ok_or(Error::Malformed("malformed value type")),
        },
    }
}

/// A heap type (spec 5.3): a non-negative type index or an abstract heap
/// type encoded as a negative index.
fn decode_heap_type(bytes: &[u8], pos: &mut usize) -> Result<HeapType, Error> {
    let index = read_s33(bytes, pos)?;
    if index < 0 {
        HeapType::from_s33(index).ok_or(Error::Unsupported("abstract heap type"))
    } else {
        Ok(HeapType::Type(index as u32))
    }
}

/// A table's limits (spec 5.3.16): a flags byte whose bit 0 marks a maximum
/// and bit 2 selects a table64 (an i64 index type), then u64 LEB limits; every
/// other flag bit is malformed.
fn decode_limits(bytes: &[u8], pos: &mut usize) -> Result<(Limits, bool), Error> {
    let flags = read_u8(bytes, pos)?;
    if flags & 0xfa != 0 {
        return Err(Error::Malformed("malformed limits flags"));
    }
    let table64 = flags & 0x04 != 0;
    let min = read_leb(bytes, pos, 10)?;
    let max = if flags & 0x01 != 0 {
        Some(read_leb(bytes, pos, 10)?)
    } else {
        None
    };
    Ok((Limits::new(min, max), table64))
}

/// Memory limits (spec 5.3.12): the limit values are u64 LEB128 even for
/// 32-bit memories, so out-of-range page counts (e.g. 2^32) still decode and
/// are rejected by validation instead. The flags byte's bit 0 marks a
/// maximum, bit 2 selects memory64; every other bit is malformed.
fn decode_memory_limits(bytes: &[u8], pos: &mut usize) -> Result<MemType, Error> {
    let flags = read_u8(bytes, pos)?;
    if flags & 0xfa != 0 {
        return Err(Error::Malformed("malformed limits flags"));
    }
    let memory64 = flags & 0x04 != 0;
    let min = read_leb(bytes, pos, 10)?;
    let max = if flags & 0x01 != 0 {
        Some(read_leb(bytes, pos, 10)?)
    } else {
        None
    };
    Ok(MemType {
        limits: Limits::new(min, max),
        memory64,
    })
}

fn decode_table_type(bytes: &[u8], pos: &mut usize) -> Result<TableType, Error> {
    let element = decode_ref_type(bytes, pos)?;
    let (limits, table64) = decode_limits(bytes, pos)?;
    Ok(TableType {
        element,
        limits,
        table64,
    })
}

/// A table-section entry (spec 5.4): a table type, prefixed by `0x40 0x00`
/// and followed by a constant initializer expression when the element type
/// has no default value (non-null element types).
fn decode_table_entry(bytes: &[u8], pos: &mut usize) -> Result<Table, Error> {
    if bytes.get(*pos) == Some(&0x40) && bytes.get(*pos + 1) == Some(&0x00) {
        *pos += 2;
        let ty = decode_table_type(bytes, pos)?;
        let init = Some(decode_expr(bytes, pos)?);
        Ok(Table { ty, init })
    } else {
        let ty = decode_table_type(bytes, pos)?;
        Ok(Table { ty, init: None })
    }
}

fn decode_mem_type(bytes: &[u8], pos: &mut usize) -> Result<MemType, Error> {
    decode_memory_limits(bytes, pos)
}

/// A load/store memarg (spec 5.4.5): a single flags byte holding the
/// alignment exponent (low 6 bits), a memory-index marker (bit 6, which makes
/// an explicit `memidx` follow), then a u64 LEB offset. Flags >= 0x80 are
/// malformed.
fn decode_memarg(bytes: &[u8], pos: &mut usize) -> Result<(u32, u32, u64), Error> {
    let flags = read_u8(bytes, pos)?;
    if flags & 0x80 != 0 {
        return Err(Error::Malformed("malformed memop flags"));
    }
    let align = u32::from(flags & 0x3f);
    let memory = if flags & 0x40 != 0 {
        read_u32(bytes, pos)?
    } else {
        0
    };
    let offset = read_leb(bytes, pos, 10)?;
    Ok((memory, align, offset))
}

fn decode_global_type(bytes: &[u8], pos: &mut usize) -> Result<GlobalType, Error> {
    let value = decode_valtype(bytes, pos)?;
    let mutable = match read_u8(bytes, pos)? {
        0x00 => false,
        0x01 => true,
        _ => return Err(Error::Malformed("malformed mutability")),
    };
    Ok(GlobalType { value, mutable })
}

/// An element type in a table/table-section position: currently the compact
/// `funcref` (0x70) / `externref` (0x6f) forms or a full reference type.
fn decode_ref_type(bytes: &[u8], pos: &mut usize) -> Result<RefType, Error> {
    match decode_valtype(bytes, pos)? {
        ValType::Ref(reference) => Ok(reference),
        _ => Err(Error::Malformed("malformed reference type")),
    }
}

fn decode_block_type(bytes: &[u8], pos: &mut usize) -> Result<BlockType, Error> {
    // Spec 5.3: `0x40`, a full value type, or a type index (s33 >= 0).
    let Some(&byte) = bytes.get(*pos) else {
        return Err(Error::Malformed("unexpected end of section or function"));
    };
    if byte == 0x40 {
        *pos += 1;
        return Ok(BlockType::Empty);
    }
    // Every single-byte value type — the numerics, v128, and the abstract
    // heap-type reference shorthands (`funcref`, `exnref`, `anyref`, ...).
    let single_byte_valtype = matches!(byte, 0x7b..=0x7f | 0x69..=0x74);
    if single_byte_valtype || byte == 0x63 || byte == 0x64 {
        return Ok(BlockType::Val(decode_valtype(bytes, pos)?));
    }
    let index = read_s33(bytes, pos)?;
    if index < 0 {
        return Err(Error::Malformed("malformed block type"));
    }
    Ok(BlockType::Type(index as u32))
}

fn decode_element(bytes: &[u8], pos: &mut usize) -> Result<ElementSegment, Error> {
    let flags = read_u32(bytes, pos)?;
    match flags {
        // Active (table 0): offset expr + `ref.func` indices. The segment
        // element type is non-null `(ref func)`.
        0 => {
            let offset = decode_expr(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_func_indices(bytes, pos, count)?;
            Ok(ElementSegment {
                ty: RefType {
                    nullable: false,
                    heap: HeapType::Func,
                },
                mode: ElementMode::Active { table: 0, offset },
                init,
            })
        }
        // Passive/declarative with an element kind byte + `ref.func` indices.
        1 | 3 => {
            let element_type = decode_elem_kind(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_func_indices(bytes, pos, count)?;
            let mode = if flags == 1 {
                ElementMode::Passive
            } else {
                ElementMode::Declarative
            };
            Ok(ElementSegment {
                ty: element_type,
                mode,
                init,
            })
        }
        // Active (explicit table): table index, offset, kind + indices.
        2 => {
            let table = read_u32(bytes, pos)?;
            let offset = decode_expr(bytes, pos)?;
            let element_type = decode_elem_kind(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_func_indices(bytes, pos, count)?;
            Ok(ElementSegment {
                ty: element_type,
                mode: ElementMode::Active { table, offset },
                init,
            })
        }
        // Active (table 0): offset expr + element expressions.
        4 => {
            let offset = decode_expr(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_expressions(bytes, pos, count)?;
            Ok(ElementSegment {
                ty: RefType::FUNC,
                mode: ElementMode::Active { table: 0, offset },
                init,
            })
        }
        // Passive/declarative with a reference type + element expressions.
        5 | 7 => {
            let element_type = decode_ref_type(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_expressions(bytes, pos, count)?;
            let mode = if flags == 5 {
                ElementMode::Passive
            } else {
                ElementMode::Declarative
            };
            Ok(ElementSegment {
                ty: element_type,
                mode,
                init,
            })
        }
        // Active (explicit table): table index, offset, ref type + exprs.
        6 => {
            let table = read_u32(bytes, pos)?;
            let offset = decode_expr(bytes, pos)?;
            let element_type = decode_ref_type(bytes, pos)?;
            let count = read_u32(bytes, pos)?;
            let init = decode_expressions(bytes, pos, count)?;
            Ok(ElementSegment {
                ty: element_type,
                mode: ElementMode::Active { table, offset },
                init,
            })
        }
        _ => Err(Error::Malformed("malformed element segment flags")),
    }
}

/// The legacy element-kind byte: only `funcref` (0x00) is encodable, as the
/// non-null `(ref func)` form.
fn decode_elem_kind(bytes: &[u8], pos: &mut usize) -> Result<RefType, Error> {
    if read_u8(bytes, pos)? == 0 {
        Ok(RefType {
            nullable: false,
            heap: HeapType::Func,
        })
    } else {
        Err(Error::Malformed("malformed element kind"))
    }
}

fn decode_func_indices(
    bytes: &[u8],
    pos: &mut usize,
    count: u32,
) -> Result<Vec<Vec<Instr>>, Error> {
    let mut init = Vec::new();
    for _ in 0..count {
        let index = read_u32(bytes, pos)?;
        init.push(vec![Instr::RefFunc(index)]);
    }
    Ok(init)
}

fn decode_expressions(bytes: &[u8], pos: &mut usize, count: u32) -> Result<Vec<Vec<Instr>>, Error> {
    let mut init = Vec::new();
    for _ in 0..count {
        init.push(decode_expr(bytes, pos)?);
    }
    Ok(init)
}

fn decode_data(bytes: &[u8], pos: &mut usize) -> Result<DataSegment, Error> {
    let flags = read_u32(bytes, pos)?;
    match flags {
        0 => {
            let offset = decode_expr(bytes, pos)?;
            let len = read_u32(bytes, pos)? as usize;
            let start = *pos;
            let Some(end) = start.checked_add(len) else {
                return Err(Error::Malformed("data segment too large"));
            };
            if end > bytes.len() {
                return Err(Error::Malformed("unexpected end of section or function"));
            }
            *pos = end;
            Ok(DataSegment {
                mode: DataMode::Active { memory: 0, offset },
                bytes: bytes[start..end].to_vec(),
            })
        }
        1 => {
            let len = read_u32(bytes, pos)? as usize;
            let start = *pos;
            let Some(end) = start.checked_add(len) else {
                return Err(Error::Malformed("data segment too large"));
            };
            if end > bytes.len() {
                return Err(Error::Malformed("unexpected end of section or function"));
            }
            *pos = end;
            Ok(DataSegment {
                mode: DataMode::Passive,
                bytes: bytes[start..end].to_vec(),
            })
        }
        2 => {
            let memory = read_u32(bytes, pos)?;
            let offset = decode_expr(bytes, pos)?;
            let len = read_u32(bytes, pos)? as usize;
            let start = *pos;
            let Some(end) = start.checked_add(len) else {
                return Err(Error::Malformed("data segment too large"));
            };
            if end > bytes.len() {
                return Err(Error::Malformed("unexpected end of section or function"));
            }
            *pos = end;
            Ok(DataSegment {
                mode: DataMode::Active { memory, offset },
                bytes: bytes[start..end].to_vec(),
            })
        }
        _ => Err(Error::Unsupported("data segment flags")),
    }
}

fn decode_body(body: &[u8]) -> Result<FuncBody, Error> {
    let mut pos = 0;
    let mut locals = Vec::new();
    let group_count = read_u32(body, &mut pos)?;
    for _ in 0..group_count {
        let count = read_u32(body, &mut pos)?;
        let ty = decode_valtype(body, &mut pos)?;
        // A local group compresses many locals into a few bytes; cap the
        // expansion so a hostile count cannot exhaust memory before
        // validation runs.
        let remaining = locals.len().saturating_add(count as usize);
        if remaining > MAX_LOCALS {
            return Err(Error::Malformed("too many locals"));
        }
        locals.resize(remaining, ty);
    }
    let instructions = decode_expr(body, &mut pos)?;
    Ok(FuncBody {
        locals,
        body: instructions,
    })
}

// ---- expression decoding ----

/// Decode a flat instruction stream terminated by the depth-0 `end`.
/// Structured control keeps its `Block`/`Loop`/`If`/`Else`/`End` markers;
/// the terminating `end` is consumed but not emitted.
fn decode_expr(bytes: &[u8], pos: &mut usize) -> Result<Vec<Instr>, Error> {
    let mut instructions = Vec::new();
    let mut depth = 0usize;
    loop {
        let opcode = read_u8(bytes, pos)?;
        match opcode {
            0x0b => {
                if depth == 0 {
                    return Ok(instructions);
                }
                depth -= 1;
                instructions.push(Instr::End);
            }
            0x02..=0x04 => {
                depth += 1;
                let block_type = decode_block_type(bytes, pos)?;
                instructions.push(match opcode {
                    0x02 => Instr::Block(block_type),
                    0x03 => Instr::Loop(block_type),
                    _ => Instr::If(block_type),
                });
            }
            0x1f => {
                // `try_table` opens a structured construct like a block; its
                // body ends at the matching `end` the depth counter tracks.
                depth += 1;
                let block_type = decode_block_type(bytes, pos)?;
                let catches = decode_catches(bytes, pos)?;
                instructions.push(Instr::TryTable {
                    blocktype: block_type,
                    catches,
                });
            }
            0x05 => {
                // `else` without a matching `if` frame cannot be detected
                // flatly; the malformed check happens at validation. Decode
                // keeps the marker.
                instructions.push(Instr::Else);
            }
            _ => instructions.push(decode_instr(opcode, bytes, pos)?),
        }
    }
}

/// A `try_table` catch-clause vector (spec 5.4.2). Each clause is a kind byte
/// plus its immediates: tag+label for the tag forms, label only for the
/// catch-all forms.
fn decode_catches(bytes: &[u8], pos: &mut usize) -> Result<Vec<Catch>, Error> {
    let count = read_u32(bytes, pos)?;
    let mut catches = Vec::new();
    for _ in 0..count {
        match read_u8(bytes, pos)? {
            0x00 => {
                let tag = read_u32(bytes, pos)?;
                let label = read_u32(bytes, pos)?;
                catches.push(Catch::Tag { tag, label });
            }
            0x01 => {
                let tag = read_u32(bytes, pos)?;
                let label = read_u32(bytes, pos)?;
                catches.push(Catch::TagRef { tag, label });
            }
            0x02 => catches.push(Catch::All {
                label: read_u32(bytes, pos)?,
            }),
            0x03 => catches.push(Catch::AllRef {
                label: read_u32(bytes, pos)?,
            }),
            _ => return Err(Error::Malformed("malformed catch clause")),
        }
    }
    Ok(catches)
}

fn decode_instr(opcode: u8, bytes: &[u8], pos: &mut usize) -> Result<Instr, Error> {
    match opcode {
        0x00 => Ok(Instr::Unreachable),
        0x01 => Ok(Instr::Nop),
        0x08 => Ok(Instr::Throw(read_u32(bytes, pos)?)),
        0x0a => Ok(Instr::ThrowRef),
        0x0c => Ok(Instr::Br(read_u32(bytes, pos)?)),
        0x0d => Ok(Instr::BrIf(read_u32(bytes, pos)?)),
        0x0e => {
            let count = read_u32(bytes, pos)?;
            let mut targets = Vec::new();
            for _ in 0..count {
                targets.push(read_u32(bytes, pos)?);
            }
            let default = read_u32(bytes, pos)?;
            Ok(Instr::BrTable { targets, default })
        }
        0x0f => Ok(Instr::Return),
        0x10 => Ok(Instr::Call(read_u32(bytes, pos)?)),
        0x11 => {
            let type_index = read_u32(bytes, pos)?;
            let table_index = read_u32(bytes, pos)?;
            Ok(Instr::CallIndirect {
                type_index,
                table_index,
            })
        }
        0x12 => Ok(Instr::ReturnCall(read_u32(bytes, pos)?)),
        0x13 => {
            let type_index = read_u32(bytes, pos)?;
            let table_index = read_u32(bytes, pos)?;
            Ok(Instr::ReturnCallIndirect {
                type_index,
                table_index,
            })
        }
        0x14 => Ok(Instr::CallRef(read_u32(bytes, pos)?)),
        0x15 => Ok(Instr::ReturnCallRef(read_u32(bytes, pos)?)),
        0x1a => Ok(Instr::Drop),
        0x1b => Ok(Instr::Select),
        0x1c => {
            let types = read_valtype_vec(bytes, pos)?;
            Ok(Instr::SelectTyped(types))
        }
        0x20 => Ok(Instr::LocalGet(read_u32(bytes, pos)?)),
        0x21 => Ok(Instr::LocalSet(read_u32(bytes, pos)?)),
        0x22 => Ok(Instr::LocalTee(read_u32(bytes, pos)?)),
        0x23 => Ok(Instr::GlobalGet(read_u32(bytes, pos)?)),
        0x24 => Ok(Instr::GlobalSet(read_u32(bytes, pos)?)),
        0x25 => Ok(Instr::TableGet(read_u32(bytes, pos)?)),
        0x26 => Ok(Instr::TableSet(read_u32(bytes, pos)?)),
        0x28..=0x35 => {
            let (op, _align_bytes) = match opcode {
                0x28 => (LoadOp::I32, 4),
                0x29 => (LoadOp::I64, 8),
                0x2a => (LoadOp::F32, 4),
                0x2b => (LoadOp::F64, 8),
                0x2c => (LoadOp::I32Load8S, 1),
                0x2d => (LoadOp::I32Load8U, 1),
                0x2e => (LoadOp::I32Load16S, 2),
                0x2f => (LoadOp::I32Load16U, 2),
                0x30 => (LoadOp::I64Load8S, 1),
                0x31 => (LoadOp::I64Load8U, 1),
                0x32 => (LoadOp::I64Load16S, 2),
                0x33 => (LoadOp::I64Load16U, 2),
                0x34 => (LoadOp::I64Load32S, 4),
                _ => (LoadOp::I64Load32U, 4),
            };
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            Ok(Instr::Load {
                memory,
                op,
                align,
                offset,
            })
        }
        0x36..=0x3e => {
            let op = match opcode {
                0x36 => StoreOp::I32,
                0x37 => StoreOp::I64,
                0x38 => StoreOp::F32,
                0x39 => StoreOp::F64,
                0x3a => StoreOp::I32Store8,
                0x3b => StoreOp::I32Store16,
                0x3c => StoreOp::I64Store8,
                0x3d => StoreOp::I64Store16,
                _ => StoreOp::I64Store32,
            };
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            Ok(Instr::Store {
                memory,
                op,
                align,
                offset,
            })
        }
        0x3f => {
            let memory = read_u32(bytes, pos)?;
            Ok(Instr::MemorySize(memory))
        }
        0x40 => {
            let memory = read_u32(bytes, pos)?;
            Ok(Instr::MemoryGrow(memory))
        }
        0x41 => Ok(Instr::I32Const(read_s32(bytes, pos)?)),
        0x42 => Ok(Instr::I64Const(read_s64(bytes, pos)?)),
        0x43 => Ok(Instr::F32Const(read_f32_bits(bytes, pos)?)),
        0x44 => Ok(Instr::F64Const(read_f64_bits(bytes, pos)?)),
        0xd0 => {
            let heap = decode_heap_type(bytes, pos)?;
            Ok(Instr::RefNull(heap))
        }
        0xd1 => Ok(Instr::RefIsNull),
        0xd2 => Ok(Instr::RefFunc(read_u32(bytes, pos)?)),
        0xd3 => Ok(Instr::RefEq),
        0xd4 => Ok(Instr::RefAsNonNull),
        0xd5 => Ok(Instr::BrOnNull(read_u32(bytes, pos)?)),
        0xd6 => Ok(Instr::BrOnNonNull(read_u32(bytes, pos)?)),
        0xfc => decode_fc(bytes, pos),
        0xfd => decode_simd(bytes, pos),
        0xfb => decode_gc(bytes, pos),
        opcode @ 0x45..=0xc4 => numeric_op(opcode)
            .map(Instr::Num)
            .ok_or(Error::Malformed("illegal opcode")),
        _ => Err(Error::Unsupported("instruction")),
    }
}

/// Map a 0xfd load-form subopcode to its [`VecLoadOp`].
fn vec_load_op(sub: u16) -> VecLoadOp {
    use VecLoadOp::*;
    match sub {
        0x00 => V128,
        0x01 => I8x8S,
        0x02 => I8x8U,
        0x03 => I16x4S,
        0x04 => I16x4U,
        0x05 => I32x2S,
        0x06 => I32x2U,
        0x07 => I8Splat,
        0x08 => I16Splat,
        0x09 => I32Splat,
        0x0a => I64Splat,
        0x5c => I32Zero,
        0x5d => I64Zero,
        _ => V128,
    }
}

/// Decode the 0xfd SIMD prefix (spec 5.4). The wave of opcodes with full
/// decode+validate+exec support is handled; everything else stays
/// [`Error::Unsupported`] so its modules count as pending, not wrong.
fn decode_simd(bytes: &[u8], pos: &mut usize) -> Result<Instr, Error> {
    let sub = read_u32(bytes, pos)?;
    if sub > u32::from(u16::MAX) {
        return Err(Error::Unsupported("simd opcode"));
    }
    let sub = sub as u16;
    match sub {
        0x00..=0x0a => {
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            Ok(Instr::VecLoad {
                memory,
                op: vec_load_op(sub),
                align,
                offset,
            })
        }
        0x0b => {
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            Ok(Instr::VecStore {
                memory,
                align,
                offset,
            })
        }
        0x5c | 0x5d => {
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            Ok(Instr::VecLoad {
                memory,
                op: vec_load_op(sub),
                align,
                offset,
            })
        }
        0x0c => {
            // `v128.const`: sixteen little-endian bytes.
            let mut raw = [0u8; 16];
            for slot in &mut raw {
                *slot = read_u8(bytes, pos)?;
            }
            Ok(Instr::V128Const(u128::from_le_bytes(raw)))
        }
        0x0d => {
            // `i8x16.shuffle`: sixteen lane-index bytes.
            let mut lanes = [0u8; 16];
            for slot in &mut lanes {
                *slot = read_u8(bytes, pos)?;
            }
            Ok(Instr::VecShuffle(lanes))
        }
        0x54..=0x5b => {
            // v128 lane loads (0x54-0x57) and stores (0x58-0x5b).
            let (memory, align, offset) = decode_memarg(bytes, pos)?;
            let lane = read_u8(bytes, pos)?;
            let size = match sub {
                0x54 | 0x58 => 1u8,
                0x55 | 0x59 => 2,
                0x56 | 0x5a => 4,
                _ => 8,
            };
            if sub < 0x58 {
                Ok(Instr::VecLaneLoad {
                    memory,
                    size,
                    align,
                    offset,
                    lane,
                })
            } else {
                Ok(Instr::VecLaneStore {
                    memory,
                    size,
                    align,
                    offset,
                    lane,
                })
            }
        }
        _ if crate::simd::sig(sub).is_some() => {
            if (0x15..=0x22).contains(&sub) {
                let lane = read_u8(bytes, pos)?;
                Ok(Instr::VecLane { op: sub, lane })
            } else {
                Ok(Instr::Vec(sub))
            }
        }
        _ => Err(Error::Unsupported("simd opcode")),
    }
}

fn decode_fc(bytes: &[u8], pos: &mut usize) -> Result<Instr, Error> {
    let sub = read_u32(bytes, pos)?;
    match sub {
        0 => Ok(Instr::Num(NumOp::I32TruncSatF32S)),
        1 => Ok(Instr::Num(NumOp::I32TruncSatF32U)),
        2 => Ok(Instr::Num(NumOp::I32TruncSatF64S)),
        3 => Ok(Instr::Num(NumOp::I32TruncSatF64U)),
        4 => Ok(Instr::Num(NumOp::I64TruncSatF32S)),
        5 => Ok(Instr::Num(NumOp::I64TruncSatF32U)),
        6 => Ok(Instr::Num(NumOp::I64TruncSatF64S)),
        7 => Ok(Instr::Num(NumOp::I64TruncSatF64U)),
        8 => {
            let data_index = read_u32(bytes, pos)?;
            let memory = read_u32(bytes, pos)?;
            Ok(Instr::MemoryInit { data_index, memory })
        }
        9 => Ok(Instr::DataDrop(read_u32(bytes, pos)?)),
        10 => {
            let dst = read_u32(bytes, pos)?;
            let src = read_u32(bytes, pos)?;
            Ok(Instr::MemoryCopy { dst, src })
        }
        11 => {
            let memory = read_u32(bytes, pos)?;
            Ok(Instr::MemoryFill(memory))
        }
        12 => {
            let element_index = read_u32(bytes, pos)?;
            let table = read_u32(bytes, pos)?;
            Ok(Instr::TableInit {
                element_index,
                table,
            })
        }
        13 => Ok(Instr::ElemDrop(read_u32(bytes, pos)?)),
        14 => {
            let dst = read_u32(bytes, pos)?;
            let src = read_u32(bytes, pos)?;
            Ok(Instr::TableCopy { dst, src })
        }
        15 => Ok(Instr::TableGrow(read_u32(bytes, pos)?)),
        16 => Ok(Instr::TableSize(read_u32(bytes, pos)?)),
        17 => Ok(Instr::TableFill(read_u32(bytes, pos)?)),
        _ => Err(Error::Unsupported("0xfc subopcode")),
    }
}

/// Decode the 0xfb GC prefix (spec 5.4.7/5.4.9): aggregate construction and
/// access, casts/tests, and extern/i31 conversions.
fn decode_gc(bytes: &[u8], pos: &mut usize) -> Result<Instr, Error> {
    let sub = read_u32(bytes, pos)?;
    Ok(match sub {
        0 => Instr::StructNew(read_u32(bytes, pos)?),
        1 => Instr::StructNewDefault(read_u32(bytes, pos)?),
        2..=5 => {
            let ty = read_u32(bytes, pos)?;
            let field = read_u32(bytes, pos)?;
            match sub {
                2 => Instr::StructGet { ty, field },
                3 => Instr::StructGetS { ty, field },
                4 => Instr::StructGetU { ty, field },
                _ => Instr::StructSet { ty, field },
            }
        }
        6 => Instr::ArrayNew(read_u32(bytes, pos)?),
        7 => Instr::ArrayNewDefault(read_u32(bytes, pos)?),
        8 => Instr::ArrayNewFixed {
            ty: read_u32(bytes, pos)?,
            n: read_u32(bytes, pos)?,
        },
        9 => Instr::ArrayNewData {
            ty: read_u32(bytes, pos)?,
            data: read_u32(bytes, pos)?,
        },
        10 => Instr::ArrayNewElem {
            ty: read_u32(bytes, pos)?,
            elem: read_u32(bytes, pos)?,
        },
        11..=14 => {
            let ty = read_u32(bytes, pos)?;
            match sub {
                11 => Instr::ArrayGet(ty),
                12 => Instr::ArrayGetS(ty),
                13 => Instr::ArrayGetU(ty),
                _ => Instr::ArraySet(ty),
            }
        }
        15 => Instr::ArrayLen,
        16 => Instr::ArrayFill(read_u32(bytes, pos)?),
        17 => Instr::ArrayCopy {
            dst: read_u32(bytes, pos)?,
            src: read_u32(bytes, pos)?,
        },
        18 => Instr::ArrayInitData {
            ty: read_u32(bytes, pos)?,
            data: read_u32(bytes, pos)?,
        },
        19 => Instr::ArrayInitElem {
            ty: read_u32(bytes, pos)?,
            elem: read_u32(bytes, pos)?,
        },
        20 | 21 => Instr::RefTest {
            nullable: sub == 21,
            heap: decode_heap_type(bytes, pos)?,
        },
        22 | 23 => Instr::RefCast {
            nullable: sub == 23,
            heap: decode_heap_type(bytes, pos)?,
        },
        24 | 25 => {
            let flags = read_u8(bytes, pos)?;
            if flags & !0x03 != 0 {
                return Err(Error::Malformed("malformed cast flags"));
            }
            let label = read_u32(bytes, pos)?;
            let from_heap = decode_heap_type(bytes, pos)?;
            let to_heap = decode_heap_type(bytes, pos)?;
            let from = RefType {
                nullable: flags & 0x01 != 0,
                heap: from_heap,
            };
            let to = RefType {
                nullable: flags & 0x02 != 0,
                heap: to_heap,
            };
            if sub == 24 {
                Instr::BrOnCast { label, from, to }
            } else {
                Instr::BrOnCastFail { label, from, to }
            }
        }
        26 => Instr::AnyConvertExtern,
        27 => Instr::ExternConvertAny,
        28 => Instr::RefI31,
        29 => Instr::I31GetS,
        30 => Instr::I31GetU,
        _ => return Err(Error::Unsupported("GC opcode")),
    })
}

/// Map a numeric/conversion opcode to its [`NumOp`]. The 0x45..=0xc4 block
/// is dense, so this is one flat match; every arm is a bare discriminant.
fn numeric_op(opcode: u8) -> Option<NumOp> {
    Some(match opcode {
        0x45 => NumOp::I32Eqz,
        0x46 => NumOp::I32Eq,
        0x47 => NumOp::I32Ne,
        0x48 => NumOp::I32LtS,
        0x49 => NumOp::I32LtU,
        0x4a => NumOp::I32GtS,
        0x4b => NumOp::I32GtU,
        0x4c => NumOp::I32LeS,
        0x4d => NumOp::I32LeU,
        0x4e => NumOp::I32GeS,
        0x4f => NumOp::I32GeU,
        0x50 => NumOp::I64Eqz,
        0x51 => NumOp::I64Eq,
        0x52 => NumOp::I64Ne,
        0x53 => NumOp::I64LtS,
        0x54 => NumOp::I64LtU,
        0x55 => NumOp::I64GtS,
        0x56 => NumOp::I64GtU,
        0x57 => NumOp::I64LeS,
        0x58 => NumOp::I64LeU,
        0x59 => NumOp::I64GeS,
        0x5a => NumOp::I64GeU,
        0x5b => NumOp::F32Eq,
        0x5c => NumOp::F32Ne,
        0x5d => NumOp::F32Lt,
        0x5e => NumOp::F32Gt,
        0x5f => NumOp::F32Le,
        0x60 => NumOp::F32Ge,
        0x61 => NumOp::F64Eq,
        0x62 => NumOp::F64Ne,
        0x63 => NumOp::F64Lt,
        0x64 => NumOp::F64Gt,
        0x65 => NumOp::F64Le,
        0x66 => NumOp::F64Ge,
        0x67 => NumOp::I32Clz,
        0x68 => NumOp::I32Ctz,
        0x69 => NumOp::I32Popcnt,
        0x6a => NumOp::I32Add,
        0x6b => NumOp::I32Sub,
        0x6c => NumOp::I32Mul,
        0x6d => NumOp::I32DivS,
        0x6e => NumOp::I32DivU,
        0x6f => NumOp::I32RemS,
        0x70 => NumOp::I32RemU,
        0x71 => NumOp::I32And,
        0x72 => NumOp::I32Or,
        0x73 => NumOp::I32Xor,
        0x74 => NumOp::I32Shl,
        0x75 => NumOp::I32ShrS,
        0x76 => NumOp::I32ShrU,
        0x77 => NumOp::I32Rotl,
        0x78 => NumOp::I32Rotr,
        0x79 => NumOp::I64Clz,
        0x7a => NumOp::I64Ctz,
        0x7b => NumOp::I64Popcnt,
        0x7c => NumOp::I64Add,
        0x7d => NumOp::I64Sub,
        0x7e => NumOp::I64Mul,
        0x7f => NumOp::I64DivS,
        0x80 => NumOp::I64DivU,
        0x81 => NumOp::I64RemS,
        0x82 => NumOp::I64RemU,
        0x83 => NumOp::I64And,
        0x84 => NumOp::I64Or,
        0x85 => NumOp::I64Xor,
        0x86 => NumOp::I64Shl,
        0x87 => NumOp::I64ShrS,
        0x88 => NumOp::I64ShrU,
        0x89 => NumOp::I64Rotl,
        0x8a => NumOp::I64Rotr,
        0x8b => NumOp::F32Abs,
        0x8c => NumOp::F32Neg,
        0x8d => NumOp::F32Ceil,
        0x8e => NumOp::F32Floor,
        0x8f => NumOp::F32Trunc,
        0x90 => NumOp::F32Nearest,
        0x91 => NumOp::F32Sqrt,
        0x92 => NumOp::F32Add,
        0x93 => NumOp::F32Sub,
        0x94 => NumOp::F32Mul,
        0x95 => NumOp::F32Div,
        0x96 => NumOp::F32Min,
        0x97 => NumOp::F32Max,
        0x98 => NumOp::F32Copysign,
        0x99 => NumOp::F64Abs,
        0x9a => NumOp::F64Neg,
        0x9b => NumOp::F64Ceil,
        0x9c => NumOp::F64Floor,
        0x9d => NumOp::F64Trunc,
        0x9e => NumOp::F64Nearest,
        0x9f => NumOp::F64Sqrt,
        0xa0 => NumOp::F64Add,
        0xa1 => NumOp::F64Sub,
        0xa2 => NumOp::F64Mul,
        0xa3 => NumOp::F64Div,
        0xa4 => NumOp::F64Min,
        0xa5 => NumOp::F64Max,
        0xa6 => NumOp::F64Copysign,
        0xa7 => NumOp::I32WrapI64,
        0xa8 => NumOp::I32TruncF32S,
        0xa9 => NumOp::I32TruncF32U,
        0xaa => NumOp::I32TruncF64S,
        0xab => NumOp::I32TruncF64U,
        0xac => NumOp::I64ExtendI32S,
        0xad => NumOp::I64ExtendI32U,
        0xae => NumOp::I64TruncF32S,
        0xaf => NumOp::I64TruncF32U,
        0xb0 => NumOp::I64TruncF64S,
        0xb1 => NumOp::I64TruncF64U,
        0xb2 => NumOp::F32ConvertI32S,
        0xb3 => NumOp::F32ConvertI32U,
        0xb4 => NumOp::F32ConvertI64S,
        0xb5 => NumOp::F32ConvertI64U,
        0xb6 => NumOp::F32DemoteF64,
        0xb7 => NumOp::F64ConvertI32S,
        0xb8 => NumOp::F64ConvertI32U,
        0xb9 => NumOp::F64ConvertI64S,
        0xba => NumOp::F64ConvertI64U,
        0xbb => NumOp::F64PromoteF32,
        0xbc => NumOp::I32ReinterpretF32,
        0xbd => NumOp::I64ReinterpretF64,
        0xbe => NumOp::F32ReinterpretI32,
        0xbf => NumOp::F64ReinterpretI64,
        0xc0 => NumOp::I32Extend8S,
        0xc1 => NumOp::I32Extend16S,
        0xc2 => NumOp::I64Extend8S,
        0xc3 => NumOp::I64Extend16S,
        0xc4 => NumOp::I64Extend32S,
        _ => return None,
    })
}

// ---- byte readers ----

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, Error> {
    let byte = bytes
        .get(*pos)
        .copied()
        .ok_or(Error::Malformed("unexpected end of section or function"))?;
    *pos += 1;
    Ok(byte)
}

/// Read an unsigned LEB128 of at most `max_bytes` bytes, advancing `pos`.
/// Non-minimal encodings (trailing zero groups) are legal, but a u64 LEB that
/// needs its full 10 bytes may set only bit 0 of the final byte — any other
/// bit there would exceed 64 bits.
fn read_leb(bytes: &[u8], pos: &mut usize, max_bytes: u32) -> Result<u64, Error> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for index in 0..max_bytes {
        let byte = read_u8(bytes, pos)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if max_bytes == 10 && index + 1 == 10 && byte & 0x7e != 0 {
                // The 10th byte of a u64 LEB carries only bit 63.
                return Err(Error::Malformed("integer too large"));
            }
            return Ok(value);
        }
        shift += 7;
        if index + 1 == max_bytes {
            return Err(Error::Malformed("integer representation too long"));
        }
    }
    unreachable!("loop is bounded by max_bytes")
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, Error> {
    let value = read_leb(bytes, pos, 5)?;
    if value > u64::from(u32::MAX) {
        return Err(Error::Malformed("integer too large"));
    }
    Ok(value as u32)
}

/// Signed LEB128 (s64), used for integer consts.
fn read_s64(bytes: &[u8], pos: &mut usize) -> Result<i64, Error> {
    let mut value: i64 = 0;
    let mut shift: u32 = 0;
    for index in 0..10 {
        let byte = read_u8(bytes, pos)?;
        value |= i64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            // The 10th byte of a u64 LEB carries only bit 63, so its unused
            // bits must be pure sign extension: 0x00 or 0x7f and nothing else.
            if index + 1 == 10 && byte != 0x00 && byte != 0x7f {
                return Err(Error::Malformed("integer too large"));
            }
            if shift < 64 && byte & 0x40 != 0 {
                value |= -1i64 << shift;
            }
            return Ok(value);
        }
        if index + 1 == 10 {
            return Err(Error::Malformed("integer representation too long"));
        }
    }
    unreachable!("loop is bounded by 10 bytes")
}

fn read_s32(bytes: &[u8], pos: &mut usize) -> Result<i32, Error> {
    let value = read_s33(bytes, pos)?;
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&value) {
        return Err(Error::Malformed("integer too large"));
    }
    Ok(value as i32)
}

/// Signed LEB128 limited to five bytes (s33), used for block types and
/// type-index heap types.
fn read_s33(bytes: &[u8], pos: &mut usize) -> Result<i64, Error> {
    let mut value: i64 = 0;
    let mut shift: u32 = 0;
    for index in 0..5 {
        let byte = read_u8(bytes, pos)?;
        value |= i64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if byte & 0x40 != 0 {
                value |= -1i64 << shift;
            }
            return Ok(value);
        }
        if index + 1 == 5 {
            return Err(Error::Malformed("integer representation too long"));
        }
    }
    unreachable!("loop is bounded by 5 bytes")
}

fn read_f32_bits(bytes: &[u8], pos: &mut usize) -> Result<u32, Error> {
    let raw = bytes
        .get(*pos..*pos + 4)
        .ok_or(Error::Malformed("unexpected end of section or function"))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().expect("4 bytes")))
}

fn read_f64_bits(bytes: &[u8], pos: &mut usize) -> Result<u64, Error> {
    let raw = bytes
        .get(*pos..*pos + 8)
        .ok_or(Error::Malformed("unexpected end of section or function"))?;
    *pos += 8;
    Ok(u64::from_le_bytes(raw.try_into().expect("8 bytes")))
}

/// Read a UTF-8 `name` (spec 5.2.4) as a `String`.
fn read_name(bytes: &[u8], pos: &mut usize) -> Result<String, Error> {
    let len = read_u32(bytes, pos)? as usize;
    let start = *pos;
    let Some(end) = start.checked_add(len) else {
        return Err(Error::Malformed("unexpected end of section or function"));
    };
    if end > bytes.len() {
        return Err(Error::Malformed("unexpected end of section or function"));
    }
    *pos = end;
    let raw = &bytes[start..end];
    std::str::from_utf8(raw)
        .map(str::to_string)
        .map_err(|_| Error::Malformed("malformed UTF-8 encoding"))
}

/// Read a custom section's name. The spec requires valid UTF-8 here too
/// (`utf8-custom-section-id.wast` asserts malformed), but the payload may
/// contain anything.
fn read_custom_name(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>, Error> {
    let len = read_u32(bytes, pos)? as usize;
    let start = *pos;
    let Some(end) = start.checked_add(len) else {
        return Err(Error::Malformed("unexpected end of section or function"));
    };
    if end > bytes.len() {
        return Err(Error::Malformed("unexpected end of section or function"));
    }
    *pos = end;
    let raw = &bytes[start..end];
    if std::str::from_utf8(raw).is_err() {
        return Err(Error::Malformed("malformed UTF-8 encoding"));
    }
    Ok(raw.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ValType;

    struct Writer {
        bytes: Vec<u8>,
    }

    impl Writer {
        fn header() -> Self {
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&VERSION.to_le_bytes());
            Writer { bytes }
        }

        fn push_leb(&mut self, mut value: u64) {
            loop {
                let mut byte = (value & 0x7f) as u8;
                value >>= 7;
                if value != 0 {
                    byte |= 0x80;
                }
                self.bytes.push(byte);
                if value == 0 {
                    break;
                }
            }
        }

        fn section(&mut self, id: u8, body: &[u8]) {
            self.bytes.push(id);
            self.push_leb(body.len() as u64);
            self.bytes.extend_from_slice(body);
        }

        fn into_module(self) -> Vec<u8> {
            self.bytes
        }
    }

    fn count(count: u32, out: &mut Vec<u8>) {
        let mut value = count;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    #[test]
    fn header_only_and_garbage() {
        let empty = Writer::header().into_module();
        let decoded = decode(&empty).unwrap();
        assert!(decoded.types.is_empty());

        // Trailing byte that cannot start a section.
        let mut trailing = empty.clone();
        trailing.push(0x01);
        assert!(matches!(
            decode(&trailing),
            Err(Error::Malformed("unexpected end of section or function"))
        ));

        assert_eq!(
            decode(b"\x00asm\x02\x00\x00\x00").unwrap_err(),
            Error::Malformed("unknown binary version")
        );
        assert_eq!(
            decode(b"\x00bsm\x01\x00\x00\x00").unwrap_err(),
            Error::Malformed("magic header not detected")
        );
    }

    #[test]
    fn type_section_decodes_func_types() {
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(1, &mut body);
        body.push(0x60); // func
        count(1, &mut body);
        body.push(0x7f); // (param i32)
        count(1, &mut body);
        body.push(0x7e); // (result i64)
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(
            module.types,
            vec![crate::types::SubType::func(
                vec![ValType::I32],
                vec![ValType::I64]
            )]
        );
    }

    #[test]
    fn type_section_decodes_struct_array_and_packed_fields() {
        use crate::types::{CompositeType, FieldType, StorageType, SubType};
        // struct { (field i32) (field (mut i8)) }
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(1, &mut body);
        body.extend([0x5f, 0x02, 0x7f, 0x00, 0x78, 0x01]);
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(
            module.types,
            vec![SubType {
                is_final: true,
                supertypes: vec![],
                composite: CompositeType::Struct(vec![
                    FieldType {
                        ty: StorageType::I32,
                        mutable: false
                    },
                    FieldType {
                        ty: StorageType::I8,
                        mutable: true
                    }
                ]),
            }]
        );

        // array (mut i16) plus a `sub` non-final func with no supertypes
        // (`0x50` prefix, zero-length supertype list).
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(2, &mut body);
        body.extend([0x5e, 0x77, 0x01]);
        body.extend([0x50, 0x00, 0x60, 0x00, 0x00]);
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.types.len(), 2);
        assert_eq!(
            module.types[0].composite,
            CompositeType::Array(FieldType {
                ty: StorageType::I16,
                mutable: true
            })
        );
        assert!(!module.types[1].is_final);
    }

    #[test]
    fn type_section_decodes_sub_final_and_rec_groups() {
        use crate::types::CompositeType;
        // (type $a (func)) then (type $b (sub final $a (func))):
        // the second subtype is `0x4f` + supertype list `[0]` + a func type.
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(2, &mut body);
        body.extend([0x60, 0x00, 0x00]);
        body.extend([0x4f, 0x01, 0x00, 0x60, 0x00, 0x00]);
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.types.len(), 2);
        assert!(module.types[1].is_final);
        assert_eq!(module.types[1].supertypes, vec![0]);

        // A rec group of two unrolled into two consecutive type indices.
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(1, &mut body);
        body.extend([0x4e, 0x02, 0x5f, 0x00, 0x5e, 0x7e, 0x00]);
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.types.len(), 2);
        assert_eq!(module.types[0].composite, CompositeType::Struct(vec![]));
        assert_eq!(
            module.types[1].composite,
            CompositeType::Array(FieldType {
                ty: StorageType::I64,
                mutable: false
            })
        );
    }

    #[test]
    fn gc_value_and_heap_types_decode() {
        // A func type `(param anyref (ref null 0)) (result (ref i31))` uses
        // the single-byte `anyref` shorthand, a `0x63` nullable ref to type
        // index 0, and `0x64` + the i31 heap byte for the non-null result.
        use crate::types::{CompositeType, RefType};
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(1, &mut body);
        body.extend([0x60, 0x02, 0x6e, 0x63, 0x00, 0x01, 0x64, 0x6c]);
        w.section(1, &body);
        let module = decode(&w.into_module()).unwrap();
        let CompositeType::Func(func) = &module.types[0].composite else {
            panic!("expected a func type");
        };
        assert_eq!(
            func.params,
            vec![
                ValType::Ref(RefType {
                    nullable: true,
                    heap: crate::types::HeapType::Any,
                }),
                ValType::Ref(RefType {
                    nullable: true,
                    heap: crate::types::HeapType::Type(0),
                }),
            ]
        );
        assert_eq!(
            func.results,
            vec![ValType::Ref(RefType {
                nullable: false,
                heap: crate::types::HeapType::I31,
            })]
        );
    }

    #[test]
    fn type_section_decodes_table_element_gc_refs() {
        // A table of `anyref` is a bare `0x6e` element byte.
        let mut w = Writer::header();
        let mut body = Vec::new();
        count(1, &mut body);
        body.extend([0x6e, 0x00, 0x01]); // reftype anyref, limits min 0 max 1
        w.section(4, &body);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.tables.len(), 1);
        assert_eq!(
            module.tables[0].ty.element,
            RefType {
                nullable: true,
                heap: crate::types::HeapType::Any,
            }
        );
    }

    #[test]
    fn malformed_utf8_in_custom_name_is_rejected() {
        let mut w = Writer::header();
        w.section(0, &[0x01, 0x80]);
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("malformed UTF-8 encoding")
        );
    }

    #[test]
    fn custom_sections_are_kept_raw() {
        let mut w = Writer::header();
        w.section(0, &[0x03, b'a', b'b', b'c', 0xde, 0xad]);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.custom.len(), 1);
        assert_eq!(module.custom[0].name, b"abc");
        assert_eq!(module.custom[0].data, vec![0xde, 0xad]);
    }

    #[test]
    fn sections_must_appear_in_order() {
        // Type section id (1) after export section id (7): out of order.
        let mut w = Writer::header();
        w.section(7, &[0x00]); // export section: zero exports
        w.section(1, &[0x00]); // type section: zero types
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("section out of order")
        );
    }

    #[test]
    fn function_and_code_sections_must_agree() {
        let mut w = Writer::header();
        w.section(3, &[0x01, 0x00]); // one function -> type 0
        w.section(10, &[0x00]); // code section with zero bodies
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("function and code section have inconsistent lengths")
        );
    }

    #[test]
    fn global_with_const_initializer() {
        let mut w = Writer::header();
        let mut globals = Vec::new();
        count(1, &mut globals);
        globals.push(0x7f); // i32
        globals.push(0x01); // mutable
        globals.push(0x41); // i32.const
        globals.push(0x2a); // 42
        globals.push(0x0b); // end
        w.section(6, &globals);
        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.globals.len(), 1);
        assert!(module.globals[0].ty.mutable);
        assert_eq!(module.globals[0].init, vec![Instr::I32Const(42)]);
    }

    #[test]
    fn block_types_decode() {
        let mut w = Writer::header();
        // A function with a single i64 local and a body using block/loop.
        let mut type_body = Vec::new();
        count(1, &mut type_body);
        type_body.push(0x60);
        count(0, &mut type_body);
        count(0, &mut type_body);
        w.section(1, &type_body);

        w.section(3, &[0x01, 0x00]);

        let mut code_body = Vec::new();
        count(0, &mut code_body); // no locals
        code_body.push(0x02); // block
        code_body.push(0x40); // empty block type
        code_body.push(0x0b); // end (inner)
        code_body.push(0x0b); // end (function)
        let mut code = Vec::new();
        count(1, &mut code);
        count(code_body.len() as u32, &mut code);
        code.extend_from_slice(&code_body);
        w.section(10, &code);

        let module = decode(&w.into_module()).unwrap();
        assert_eq!(
            module.bodies[0].body,
            vec![Instr::Block(crate::types::BlockType::Empty), Instr::End]
        );
    }

    /// Every baseline zero-immediate opcode must decode to *some* value.
    /// The decoder does not type-check, so a straight run of all of them is
    /// fine as a decoding smoke test.
    #[test]
    fn every_baseline_opcode_decodes() {
        let mut body = Vec::new();
        count(0, &mut body); // no locals

        // Control-flow frames opened below must all be closed before the
        // function terminator.
        let mut opens = 0usize;
        let emit = |bytes: &mut Vec<u8>, code: &[u8]| bytes.extend_from_slice(code);

        emit(&mut body, &[0x00]); // unreachable
        emit(&mut body, &[0x01]); // nop
        emit(&mut body, &[0x02, 0x40]); // block empty
        emit(&mut body, &[0x03, 0x40]); // loop empty
        emit(&mut body, &[0x04, 0x7f]); // if (result i32) — negative blocktype
        emit(&mut body, &[0x05]); // else
        emit(&mut body, &[0x1a]); // drop
        opens += 3;

        emit(&mut body, &[0x0c, 0x00]); // br 0
        emit(&mut body, &[0x0d, 0x00]); // br_if 0
        emit(&mut body, &[0x0e, 0x00, 0x00]); // br_table [] 0
        emit(&mut body, &[0x0f]); // return
        emit(&mut body, &[0x10, 0x00]); // call 0
        emit(&mut body, &[0x12, 0x00]); // return_call 0
        emit(&mut body, &[0x11, 0x00, 0x00]); // call_indirect
        emit(&mut body, &[0x13, 0x00, 0x00]); // return_call_indirect
        emit(&mut body, &[0x14, 0x00]); // call_ref 0
        emit(&mut body, &[0x15, 0x00]); // return_call_ref 0
        emit(&mut body, &[0x1b]); // select
        emit(&mut body, &[0x1c, 0x00]); // select (no types)
        emit(&mut body, &[0x20, 0x00]); // local.get 0
        emit(&mut body, &[0x21, 0x00]); // local.set 0
        emit(&mut body, &[0x22, 0x00]); // local.tee 0
        emit(&mut body, &[0x23, 0x00]); // global.get 0
        emit(&mut body, &[0x24, 0x00]); // global.set 0
        emit(&mut body, &[0x25, 0x00]); // table.get 0
        emit(&mut body, &[0x26, 0x00]); // table.set 0

        for code in 0x28..=0x35 {
            emit(&mut body, &[code, 0x00, 0x00]);
        }
        for code in 0x36..=0x3e {
            emit(&mut body, &[code, 0x00, 0x00]);
        }
        emit(&mut body, &[0x3f, 0x00]); // memory.size
        emit(&mut body, &[0x40, 0x00]); // memory.grow

        emit(&mut body, &[0x41, 0x7f]); // i32.const -1
        emit(&mut body, &[0x42, 0xff, 0x7f]); // i64.const -1
        emit(&mut body, &[0x43, 0, 0, 0x80, 0x3f]); // f32.const
        emit(&mut body, &[0x44, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f]); // f64.const
        for code in 0x45..=0xc4 {
            emit(&mut body, &[code]);
        }
        // Saturating and bulk/table 0xfc subopcodes.
        for sub in [0, 1, 2, 3, 4, 5, 6, 7] {
            emit(&mut body, &[0xfc, sub]);
        }
        emit(&mut body, &[0xfc, 8, 0x00, 0x00]); // memory.init 0 0
        emit(&mut body, &[0xfc, 9, 0x00]); // data.drop 0
        emit(&mut body, &[0xfc, 10, 0x00, 0x00]); // memory.copy
        emit(&mut body, &[0xfc, 11, 0x00]); // memory.fill
        emit(&mut body, &[0xfc, 12, 0x00, 0x00]); // table.init 0 0
        emit(&mut body, &[0xfc, 13, 0x00]); // elem.drop 0
        emit(&mut body, &[0xfc, 14, 0x00, 0x00]); // table.copy
        emit(&mut body, &[0xfc, 15, 0x00]); // table.grow
        emit(&mut body, &[0xfc, 16, 0x00]); // table.size
        emit(&mut body, &[0xfc, 17, 0x00]); // table.fill
        emit(&mut body, &[0xd0, 0x70]); // ref.null func
        emit(&mut body, &[0xd1]); // ref.is_null
        emit(&mut body, &[0xd2, 0x00]); // ref.func 0
        emit(&mut body, &[0xd3]); // ref.eq
        emit(&mut body, &[0xd4]); // ref.as_non_null
        emit(&mut body, &[0xd5, 0x00]); // br_on_null
        emit(&mut body, &[0xd6, 0x00]); // br_on_non_null

        for _ in 0..opens {
            emit(&mut body, &[0x0b]); // close the opened frames
        }
        emit(&mut body, &[0x0b]); // function end

        let decoded = decode_body(&body).unwrap();
        assert!(!decoded.body.is_empty());
    }

    #[test]
    fn module_with_memory_table_export_start_and_data() {
        let mut w = Writer::header();

        // One empty func type.
        let mut types = Vec::new();
        count(1, &mut types);
        types.push(0x60);
        count(0, &mut types);
        count(0, &mut types);
        w.section(1, &types);

        // One function -> type 0.
        w.section(3, &[0x01, 0x00]);

        // One table: funcref, min 1.
        let mut tables = Vec::new();
        count(1, &mut tables);
        tables.push(0x70); // funcref
        tables.push(0x00);
        tables.push(0x01);
        w.section(4, &tables);

        // One memory: min 1 page.
        let mut memories = Vec::new();
        count(1, &mut memories);
        memories.push(0x00);
        memories.push(0x01);
        w.section(5, &memories);

        // Export the function as "run".
        let mut exports = Vec::new();
        count(1, &mut exports);
        exports.extend_from_slice(&[0x03]);
        exports.extend_from_slice(b"run");
        exports.push(0x00);
        exports.push(0x00);
        w.section(7, &exports);

        // Start function = func 0.
        w.section(8, &[0x00]);

        // Code: one empty body.
        let mut body = Vec::new();
        count(0, &mut body);
        body.push(0x0b);
        let mut code = Vec::new();
        count(1, &mut code);
        count(body.len() as u32, &mut code);
        code.extend_from_slice(&body);
        w.section(10, &code);

        // Data segment: active, offset i32.const 0, bytes "hi".
        let mut data = Vec::new();
        count(1, &mut data);
        data.push(0x00); // active, memory 0
        data.extend_from_slice(&[0x41, 0x00, 0x0b]); // offset 0
        data.push(0x02);
        data.extend_from_slice(b"hi");
        w.section(11, &data);

        let module = decode(&w.into_module()).unwrap();
        assert_eq!(module.tables.len(), 1);
        assert_eq!(module.memories[0].limits.min, 1);
        assert_eq!(module.start, Some(0));
        assert_eq!(module.exports[0].name, "run");
        assert_eq!(module.data[0].bytes, b"hi");
        assert_eq!(module.bodies.len(), 1);
    }

    /// A header + type/function/code skeleton whose single function body is
    /// `body` (locals group count 0 prepended, the terminating `end` included
    /// in `body`).
    fn module_with_body(body: &[u8]) -> Vec<u8> {
        let mut w = Writer::header();
        w.section(1, &[0x01, 0x60, 0x00, 0x00]); // (type (func))
        w.section(3, &[0x01, 0x00]); // one function, type 0
        let mut code = Vec::new();
        count(1, &mut code);
        let mut bytes = vec![0x00]; // no local groups
        bytes.extend_from_slice(body);
        count(bytes.len() as u32, &mut code);
        code.extend_from_slice(&bytes);
        w.section(10, &code);
        w.into_module()
    }

    #[test]
    fn decoder_enforces_section_and_leb_strictness() {
        // A section payload that outlives its declared item count (the
        // spec's "section size mismatch" family).
        let mut w = Writer::header();
        w.section(1, &[0x01, 0x60, 0x00, 0x00, 0x60, 0x00, 0x00]);
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("section size mismatch")
        );

        // A function section with no matching code section.
        let mut w = Writer::header();
        w.section(1, &[0x01, 0x60, 0x00, 0x00]);
        w.section(3, &[0x01, 0x00]);
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("function and code section have inconsistent lengths")
        );

        // A non-zero data count without a data section.
        let mut w = Writer::header();
        w.section(12, &[0x01]);
        assert_eq!(
            decode(&w.into_module()).unwrap_err(),
            Error::Malformed("data count and data section have inconsistent lengths")
        );

        // memory.init without a data count section.
        let module = module_with_body(&[0xfc, 0x08, 0x00, 0x00, 0x0b]);
        assert_eq!(
            decode(&module).unwrap_err(),
            Error::Malformed("data count section required")
        );

        // A 10-byte i64 LEB whose final byte is not pure sign extension.
        let module = module_with_body(&[
            0x42, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7e, 0x0b,
        ]);
        assert_eq!(
            decode(&module).unwrap_err(),
            Error::Malformed("integer too large")
        );
    }
}
