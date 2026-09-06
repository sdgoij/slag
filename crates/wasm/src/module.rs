//! The decoded module model (spec ch. 2.5).
//!
//! Produced by the binary decoder (`binary`); validated in Cut 2 and
//! instantiated/executed in later cuts.

use crate::instr::Instr;
use crate::types::{
    CompositeType, FuncType, GlobalType, Limits, MemType, RefType, SubType, TableType, ValType,
};

/// An import description (spec 2.5.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDesc {
    Func(u32),
    Table(TableType),
    Memory(MemType),
    Global(GlobalType),
    /// A tag import: the type index of its function type (exceptions).
    Tag(u32),
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
    Tag,
}

/// A global definition: its type plus a constant initializer expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    pub ty: GlobalType,
    pub init: Vec<Instr>,
}

/// A defined table: its type plus an optional constant initializer from the
/// newer table-section encoding (absent means the `ref.null` default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub ty: TableType,
    pub init: Option<Vec<Instr>>,
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
    /// The module's type space: one entry per subtype (a rec group of `n`
    /// members occupies `n` consecutive indices).
    pub types: Vec<SubType>,
    /// The length of each recursion group in the type section, in order
    /// (their members occupy consecutive indices of `types`; zero-length
    /// groups are dropped). Empty when the module was built without a type
    /// section (each type index is then its own singleton group).
    pub rec_groups: Vec<u32>,
    pub imports: Vec<Import>,
    /// Type index of each module-defined function (not imports).
    pub functions: Vec<u32>,
    pub tables: Vec<Table>,
    pub memories: Vec<MemType>,
    pub globals: Vec<Global>,
    /// Type index of each module-defined tag (exceptions; tags import like
    /// funcs/memories/globals).
    pub tags: Vec<u32>,
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

    /// Resolve a type index to its function type, if it is one.
    pub fn func_at(&self, index: u32) -> Option<&FuncType> {
        self.types
            .get(index as usize)
            .and_then(|sub| match &sub.composite {
                CompositeType::Func(func) => Some(func),
                _ => None,
            })
    }

    /// Resolve a type index to its function type by value, if it is one.
    pub fn func_at_cloned(&self, index: u32) -> Option<FuncType> {
        self.func_at(index).cloned()
    }

    /// The rec group containing `index`: its first index and member count. A
    /// module built without a type section records no groups, in which case
    /// each type index is its own singleton group.
    pub fn rec_group_of(&self, index: u32) -> (u32, u32) {
        rec_group_of(&self.rec_groups, index)
    }

    /// The first index past the rec group containing `index`: the scope bound
    /// for a type definition, whose references may only reach its own group or
    /// earlier groups.
    pub fn rec_group_end(&self, index: u32) -> u32 {
        let (start, len) = self.rec_group_of(index);
        start + len
    }

    /// Whether two type indices of this module denote equivalent types
    /// (isorecursive rec-group equivalence, spec 3.3.10).
    pub fn type_indices_equivalent(&self, a: u32, b: u32) -> bool {
        type_indices_equivalent(
            &self.types,
            &self.rec_groups,
            a,
            &self.types,
            &self.rec_groups,
            b,
        )
    }

    /// Whether `start <: goal` in this module: the two are equivalent, or
    /// `start` follows a declared supertype edge to one that is.
    pub fn type_is_subtype(&self, start: u32, goal: u32) -> bool {
        let mut current = start;
        for _ in 0..=self.types.len() {
            if self.type_indices_equivalent(current, goal) {
                return true;
            }
            let Some(parent) = self
                .types
                .get(current as usize)
                .and_then(|st| st.supertypes.first())
            else {
                return false;
            };
            current = *parent;
        }
        false
    }

    /// Whether the function types are equivalent, with `a`'s concrete type
    /// references resolved in this module's type space and `b`'s in `other`'s
    /// (cross-module import matching).
    pub fn func_type_equivalent(&self, a: &FuncType, other: &Module, b: &FuncType) -> bool {
        func_types_equivalent(
            Ctx {
                types: &self.types,
                groups: &self.rec_groups,
            },
            a,
            Ctx {
                types: &other.types,
                groups: &other.rec_groups,
            },
            b,
        )
    }
}

/// The rec group containing `index`, given the group-length list.
fn rec_group_of(groups: &[u32], index: u32) -> (u32, u32) {
    if groups.is_empty() {
        return (index, 1);
    }
    let mut start = 0u64;
    for &len in groups {
        if u64::from(index) >= start && u64::from(index) < start + u64::from(len) {
            return (start as u32, len);
        }
        start += u64::from(len);
    }
    (index, 1)
}

/// One side of a cross-context type comparison: a module's type space plus
/// its rec-group boundaries.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    types: &'a [SubType],
    groups: &'a [u32],
}

impl Ctx<'_> {
    fn group_of(&self, index: u32) -> (u32, u32) {
        rec_group_of(self.groups, index)
    }
}

/// Isorecursive type equivalence (spec 3.3.10): two type indices are
/// equivalent iff their rec groups have equal length and the groups are
/// structurally identical member-for-member. A type reference inside a group
/// maps by offset into the other group; a reference to an earlier (closed)
/// group compares that referenced type. Recursion only ever steps to an
/// earlier group on both sides, so the comparison terminates.
pub fn type_indices_equivalent(
    types_a: &[SubType],
    groups_a: &[u32],
    a: u32,
    types_b: &[SubType],
    groups_b: &[u32],
    b: u32,
) -> bool {
    indices_equivalent(
        Ctx {
            types: types_a,
            groups: groups_a,
        },
        a,
        Ctx {
            types: types_b,
            groups: groups_b,
        },
        b,
    )
}

fn indices_equivalent(ctx_a: Ctx<'_>, a: u32, ctx_b: Ctx<'_>, b: u32) -> bool {
    let (start_a, len_a) = ctx_a.group_of(a);
    let (start_b, len_b) = ctx_b.group_of(b);
    if len_a != len_b || a - start_a != b - start_b {
        return false;
    }
    for offset in 0..len_a {
        if !subtype_equivalent(
            ctx_a,
            (start_a, len_a),
            start_a + offset,
            ctx_b,
            (start_b, len_b),
            start_b + offset,
        ) {
            return false;
        }
    }
    true
}

/// Whether the two function types are equivalent under their own type spaces
/// (concrete `Type` references resolve per side).
fn func_types_equivalent(ctx_a: Ctx<'_>, a: &FuncType, ctx_b: Ctx<'_>, b: &FuncType) -> bool {
    a.params.len() == b.params.len()
        && a.results.len() == b.results.len()
        && a.params
            .iter()
            .zip(&b.params)
            .all(|(x, y)| valtypes_equivalent(ctx_a, None, *x, ctx_b, None, *y))
        && a.results
            .iter()
            .zip(&b.results)
            .all(|(x, y)| valtypes_equivalent(ctx_a, None, *x, ctx_b, None, *y))
}

/// Compare one subtype definition of each aligned group (same length and
/// position checked by the caller).
fn subtype_equivalent(
    ctx_a: Ctx<'_>,
    aligned_a: (u32, u32),
    a: u32,
    ctx_b: Ctx<'_>,
    aligned_b: (u32, u32),
    b: u32,
) -> bool {
    let (Some(sub_a), Some(sub_b)) = (ctx_a.types.get(a as usize), ctx_b.types.get(b as usize))
    else {
        return false;
    };
    if sub_a.is_final != sub_b.is_final || sub_a.supertypes.len() != sub_b.supertypes.len() {
        return false;
    }
    for (&sup_a, &sup_b) in sub_a.supertypes.iter().zip(&sub_b.supertypes) {
        if !indices_equivalent(ctx_a, sup_a, ctx_b, sup_b) {
            return false;
        }
    }
    match (&sub_a.composite, &sub_b.composite) {
        (CompositeType::Func(fa), CompositeType::Func(fb)) => {
            fa.params.len() == fb.params.len()
                && fa.results.len() == fb.results.len()
                && fa.params.iter().zip(&fb.params).all(|(x, y)| {
                    valtypes_equivalent(ctx_a, Some(aligned_a), *x, ctx_b, Some(aligned_b), *y)
                })
                && fa.results.iter().zip(&fb.results).all(|(x, y)| {
                    valtypes_equivalent(ctx_a, Some(aligned_a), *x, ctx_b, Some(aligned_b), *y)
                })
        }
        (CompositeType::Struct(fa), CompositeType::Struct(fb)) => {
            fa.len() == fb.len()
                && fa.iter().zip(fb).all(|(x, y)| {
                    fields_equivalent(ctx_a, Some(aligned_a), x, ctx_b, Some(aligned_b), y)
                })
        }
        (CompositeType::Array(fa), CompositeType::Array(fb)) => {
            fields_equivalent(ctx_a, Some(aligned_a), fa, ctx_b, Some(aligned_b), fb)
        }
        _ => false,
    }
}

fn fields_equivalent(
    ctx_a: Ctx<'_>,
    aligned_a: Option<(u32, u32)>,
    a: &crate::types::FieldType,
    ctx_b: Ctx<'_>,
    aligned_b: Option<(u32, u32)>,
    b: &crate::types::FieldType,
) -> bool {
    a.mutable == b.mutable
        && match (a.ty, b.ty) {
            (crate::types::StorageType::Ref(ra), crate::types::StorageType::Ref(rb)) => {
                ra.nullable == rb.nullable
                    && heaps_equivalent(ctx_a, aligned_a, ra.heap, ctx_b, aligned_b, rb.heap)
            }
            _ => a.ty == b.ty,
        }
}

fn valtypes_equivalent(
    ctx_a: Ctx<'_>,
    aligned_a: Option<(u32, u32)>,
    a: ValType,
    ctx_b: Ctx<'_>,
    aligned_b: Option<(u32, u32)>,
    b: ValType,
) -> bool {
    match (a, b) {
        (ValType::Ref(ra), ValType::Ref(rb)) => {
            ra.nullable == rb.nullable
                && heaps_equivalent(ctx_a, aligned_a, ra.heap, ctx_b, aligned_b, rb.heap)
        }
        _ => a == b,
    }
}

/// Heap-type equality under an aligned group pair: a reference into the
/// aligned group maps by offset; any other `Type` reference compares the
/// (closed, earlier) referenced types.
fn heaps_equivalent(
    ctx_a: Ctx<'_>,
    aligned_a: Option<(u32, u32)>,
    a: crate::types::HeapType,
    ctx_b: Ctx<'_>,
    aligned_b: Option<(u32, u32)>,
    b: crate::types::HeapType,
) -> bool {
    use crate::types::HeapType::*;
    match (a, b) {
        (Type(x), Type(y)) => {
            let inside_a = aligned_a.is_some_and(|(start, len)| x >= start && x < start + len);
            let inside_b = aligned_b.is_some_and(|(start, len)| y >= start && y < start + len);
            match (inside_a, inside_b) {
                // Both reference the aligned groups: offsets must coincide.
                (true, true) => {
                    let (start_a, _) = aligned_a.expect("inside_a");
                    let (start_b, _) = aligned_b.expect("inside_b");
                    x - start_a == y - start_b
                }
                // A group member cannot reach into the other group's space.
                (true, false) | (false, true) => false,
                (false, false) => indices_equivalent(ctx_a, x, ctx_b, y),
            }
        }
        _ => a == b,
    }
}
