//! The decoded instruction stream (spec ch. 2.4 / 5.4).
//!
//! Numeric and conversion instructions share one flat [`NumOp`] enum; the
//! immediate-bearing structural instructions are their own [`Instr`]
//! variants. Validation (Cut 2) and execution (Cut 3) match on these.

use crate::types::{BlockType, ValType};

/// Load instructions (spec 2.4.5): the value type, width/extend form, and
/// the natural alignment (bytes), which validation re-checks against the
/// decoded `align` exponent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOp {
    I32,
    I64,
    F32,
    F64,
    I32Load8S,
    I32Load8U,
    I32Load16S,
    I32Load16U,
    I64Load8S,
    I64Load8U,
    I64Load16S,
    I64Load16U,
    I64Load32S,
    I64Load32U,
}

/// Store instructions (spec 2.4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    I32,
    I64,
    F32,
    F64,
    I32Store8,
    I32Store16,
    I64Store8,
    I64Store16,
    I64Store32,
}

/// Every non-structural numeric/conversion opcode. Grouped per type so
/// validation and the executor can dispatch by category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumOp {
    // Comparisons and tests.
    I32Eqz,
    I32Eq,
    I32Ne,
    I32LtS,
    I32LtU,
    I32GtS,
    I32GtU,
    I32LeS,
    I32LeU,
    I32GeS,
    I32GeU,
    I64Eqz,
    I64Eq,
    I64Ne,
    I64LtS,
    I64LtU,
    I64GtS,
    I64GtU,
    I64LeS,
    I64LeU,
    I64GeS,
    I64GeU,
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    // i32 arithmetic.
    I32Clz,
    I32Ctz,
    I32Popcnt,
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32RemS,
    I32RemU,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    I32Rotl,
    I32Rotr,
    // i64 arithmetic.
    I64Clz,
    I64Ctz,
    I64Popcnt,
    I64Add,
    I64Sub,
    I64Mul,
    I64DivS,
    I64DivU,
    I64RemS,
    I64RemU,
    I64And,
    I64Or,
    I64Xor,
    I64Shl,
    I64ShrS,
    I64ShrU,
    I64Rotl,
    I64Rotr,
    // f32 arithmetic.
    F32Abs,
    F32Neg,
    F32Ceil,
    F32Floor,
    F32Trunc,
    F32Nearest,
    F32Sqrt,
    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Min,
    F32Max,
    F32Copysign,
    // f64 arithmetic.
    F64Abs,
    F64Neg,
    F64Ceil,
    F64Floor,
    F64Trunc,
    F64Nearest,
    F64Sqrt,
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Min,
    F64Max,
    F64Copysign,
    // Conversions (spec 2.4.8).
    I32WrapI64,
    I32TruncF32S,
    I32TruncF32U,
    I32TruncF64S,
    I32TruncF64U,
    I64ExtendI32S,
    I64ExtendI32U,
    I64TruncF32S,
    I64TruncF32U,
    I64TruncF64S,
    I64TruncF64U,
    F32ConvertI32S,
    F32ConvertI32U,
    F32ConvertI64S,
    F32ConvertI64U,
    F32DemoteF64,
    F64ConvertI32S,
    F64ConvertI32U,
    F64ConvertI64S,
    F64ConvertI64U,
    F64PromoteF32,
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,
    // Sign extension (spec 2.4.8).
    I32Extend8S,
    I32Extend16S,
    I64Extend8S,
    I64Extend16S,
    I64Extend32S,
    // Saturating float-to-int (0xfc 0-7).
    I32TruncSatF32S,
    I32TruncSatF32U,
    I32TruncSatF64S,
    I32TruncSatF64U,
    I64TruncSatF32S,
    I64TruncSatF32U,
    I64TruncSatF64S,
    I64TruncSatF64U,
}

/// A `try_table` catch clause (spec 2.4.2 / exceptions). The tag variants
/// carry the tag index and the branch-target label of the *enclosing*
/// construct the payload is delivered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catch {
    /// `catch x y`: tag match; the branch payload is the tag's parameters.
    Tag { tag: u32, label: u32 },
    /// `catch_ref x y`: tag match; the payload is the parameters plus the
    /// caught exception reference on top.
    TagRef { tag: u32, label: u32 },
    /// `catch_all y`: any exception; no payload.
    All { label: u32 },
    /// `catch_all_ref y`: any exception; the payload is the exception ref.
    AllRef { label: u32 },
}

/// The v128 memory load forms (spec 2.4.5): plain, sign/zero-extending
/// narrow loads, splat loads, and zero loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecLoadOp {
    V128,
    I8x8S,
    I8x8U,
    I16x4S,
    I16x4U,
    I32x2S,
    I32x2U,
    I8Splat,
    I16Splat,
    I32Splat,
    I64Splat,
    I32Zero,
    I64Zero,
}

/// A decoded instruction. The list produced by the code-section decoder is
/// flat: structured control keeps its `Block`/`Loop`/`If`/`TryTable`/`Else`/`End`
/// markers so validation can re-derive nesting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instr {
    Unreachable,
    Nop,
    Block(BlockType),
    Loop(BlockType),
    If(BlockType),
    TryTable {
        blocktype: BlockType,
        catches: Vec<Catch>,
    },
    Else,
    End,
    Br(u32),
    BrIf(u32),
    BrTable {
        targets: Vec<u32>,
        default: u32,
    },
    Return,
    Call(u32),
    ReturnCall(u32),
    CallIndirect {
        type_index: u32,
        table_index: u32,
    },
    ReturnCallIndirect {
        type_index: u32,
        table_index: u32,
    },
    CallRef(u32),
    ReturnCallRef(u32),
    Drop,
    Select,
    SelectTyped(Vec<ValType>),
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    TableGet(u32),
    TableSet(u32),
    Load {
        memory: u32,
        op: LoadOp,
        align: u32,
        offset: u64,
    },
    Store {
        memory: u32,
        op: StoreOp,
        align: u32,
        offset: u64,
    },
    MemorySize(u32),
    MemoryGrow(u32),
    I32Const(i32),
    I64Const(i64),
    F32Const(u32),
    F64Const(u64),
    Num(NumOp),
    RefNull(crate::types::HeapType),
    RefIsNull,
    RefFunc(u32),
    RefEq,
    RefAsNonNull,
    BrOnNull(u32),
    BrOnNonNull(u32),
    Throw(u32),
    ThrowRef,
    /// `v128.const` (0xfd 0x0c): the 16 little-endian bytes of the vector.
    V128Const(u128),
    /// A v128 load (0xfd 0x00-0x0a, 0x5c-0x5d).
    VecLoad {
        memory: u32,
        op: VecLoadOp,
        align: u32,
        offset: u64,
    },
    /// `v128.store` (0xfd 0x0b).
    VecStore {
        memory: u32,
        align: u32,
        offset: u64,
    },
    /// A v128 lane load/store (0xfd 0x54-0x5b): `size` is the lane's byte
    /// width.
    VecLaneLoad {
        memory: u32,
        size: u8,
        align: u32,
        offset: u64,
        lane: u8,
    },
    VecLaneStore {
        memory: u32,
        size: u8,
        align: u32,
        offset: u64,
        lane: u8,
    },
    /// `i8x16.shuffle` (0xfd 0x0d): the 16 lane indices (0-31).
    VecShuffle([u8; 16]),
    /// A pure register-to-register v128 op (0xfd), keyed by its subopcode;
    /// `simd::sig` types it and the executor dispatches it.
    Vec(u16),
    /// A v128 lane op with a lane immediate: extract/replace forms
    /// (0xfd 0x15-0x22).
    VecLane {
        op: u16,
        lane: u8,
    },
    MemoryInit {
        data_index: u32,
        memory: u32,
    },
    DataDrop(u32),
    MemoryCopy {
        dst: u32,
        src: u32,
    },
    MemoryFill(u32),
    TableInit {
        element_index: u32,
        table: u32,
    },
    ElemDrop(u32),
    TableCopy {
        dst: u32,
        src: u32,
    },
    TableGrow(u32),
    TableSize(u32),
    TableFill(u32),
    // GC aggregate instructions (0xfb prefix, spec 2.4.7/2.4.9).
    StructNew(u32),
    StructNewDefault(u32),
    StructGet {
        ty: u32,
        field: u32,
    },
    StructGetS {
        ty: u32,
        field: u32,
    },
    StructGetU {
        ty: u32,
        field: u32,
    },
    StructSet {
        ty: u32,
        field: u32,
    },
    ArrayNew(u32),
    ArrayNewDefault(u32),
    ArrayNewFixed {
        ty: u32,
        n: u32,
    },
    ArrayNewData {
        ty: u32,
        data: u32,
    },
    ArrayNewElem {
        ty: u32,
        elem: u32,
    },
    ArrayGet(u32),
    ArrayGetS(u32),
    ArrayGetU(u32),
    ArraySet(u32),
    ArrayLen,
    ArrayFill(u32),
    ArrayCopy {
        dst: u32,
        src: u32,
    },
    ArrayInitData {
        ty: u32,
        data: u32,
    },
    ArrayInitElem {
        ty: u32,
        elem: u32,
    },
    /// `ref.test` — `nullable` is the target type's nullability (the 0x14
    /// non-null form tests nulls as failing; 0x15 lets nulls pass).
    RefTest {
        nullable: bool,
        heap: crate::types::HeapType,
    },
    RefCast {
        nullable: bool,
        heap: crate::types::HeapType,
    },
    BrOnCast {
        label: u32,
        from: crate::types::RefType,
        to: crate::types::RefType,
    },
    BrOnCastFail {
        label: u32,
        from: crate::types::RefType,
        to: crate::types::RefType,
    },
    AnyConvertExtern,
    ExternConvertAny,
    RefI31,
    I31GetS,
    I31GetU,
}
