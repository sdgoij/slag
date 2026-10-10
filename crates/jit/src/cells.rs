//! Shared member-value-cell addressing and validation.
//!
//! Both the per-step lowerer (`compiler.rs`) and the optimizing tier
//! (`opt_lower.rs`) must compute the same cell address and validate it the same
//! way; keeping the arithmetic here is the "single-source the probe" rule from
//! `.notes/tier-guarded-read.md` §6 (a fork would drift from the store paths
//! that keep the cell current).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::FunctionBuilder;

use runtime::ir::{MEMBER_CELLS, MemberMapCell, MemberValueCell};

/// The `JsObject` data pointer from a NaN-boxed Object `Value`: the payload
/// shifted past the NaN tag plus the `GcBox` header offset. Only valid for an
/// Object `Value` (callers gate on [`is_plain_object`]).
pub(crate) fn object_data_ptr(builder: &mut FunctionBuilder, object: Value) -> Value {
    let ptr = builder.ins().band_imm_u(object, crux::PAYLOAD_MASK as i64);
    let ptr = builder.ins().ishl_imm_u(ptr, 4);
    builder
        .ins()
        .iadd_imm_s(ptr, crux::heap::GCBOX_DATA_OFFSET as i64)
}

/// Whether a NaN-boxed `Value` is a plain Object (`TAG_OBJECT`), the receiver
/// shape the member-value-cell probe serves.
pub(crate) fn is_plain_object(builder: &mut FunctionBuilder, value: Value) -> Value {
    let masked = builder.ins().band_imm_u(value, crux::TAG_MASK as i64);
    let is_heap = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, masked, crux::TAG_PREFIX as i64);
    let tag = builder.ins().ushr_imm_u(value, 44);
    let tag = builder.ins().band_imm_u(tag, 0xF);
    let tag_obj = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
    builder.ins().band(is_heap, tag_obj)
}

/// The address of the member-value cell for `(object_id, name_imm)`, mirroring
/// `Vm::member_cell_index` (`(id ^ atom) & (MEMBER_CELLS - 1)`).
pub(crate) fn member_value_cell_addr(
    builder: &mut FunctionBuilder,
    cells: Value,
    object_id: Value,
    name_imm: Value,
) -> Value {
    let slot = builder.ins().bxor(object_id, name_imm);
    let slot = builder.ins().band_imm_u(slot, (MEMBER_CELLS - 1) as i64);
    let index_bytes = builder
        .ins()
        .imul_imm_s(slot, std::mem::size_of::<MemberValueCell>() as i64);
    builder.ins().iadd(cells, index_bytes)
}

/// The address of the map-keyed member cell for `(map_id, name_imm)`, mirroring
/// `Vm::member_map_cell_index` (`(map_id ^ atom) & (MEMBER_CELLS - 1)`). A map
/// id pins the descriptor layout for every instance of the shape, so this cell
/// serves any object count with no per-object identity or generation.
pub(crate) fn member_map_cell_addr(
    builder: &mut FunctionBuilder,
    cells: Value,
    map_id: Value,
    name_imm: Value,
) -> Value {
    let slot = builder.ins().bxor(map_id, name_imm);
    let slot = builder.ins().band_imm_u(slot, (MEMBER_CELLS - 1) as i64);
    let index_bytes = builder
        .ins()
        .imul_imm_s(slot, std::mem::size_of::<MemberMapCell>() as i64);
    builder.ins().iadd(cells, index_bytes)
}

/// Whether `cell` holds `(live_id, name)` at the receiver's `live_gen` — the
/// cell's id, name and generation must all match, so a hit means the own data
/// property is unchanged since the cell was recorded.
pub(crate) fn member_value_cell_valid(
    builder: &mut FunctionBuilder,
    cell: Value,
    live_id: Value,
    name: crux::AtomId,
    live_gen: Value,
) -> Value {
    let cell_id = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(MemberValueCell, id) as i32),
    );
    let cell_name = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(MemberValueCell, name) as i32),
    );
    let cell_gen = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(MemberValueCell, generation) as i32),
    );
    let id_ok = builder.ins().icmp(IntCC::Equal, cell_id, live_id);
    let name_ok = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, cell_name, name as i64);
    let gen_ok = builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
    let id_name_ok = builder.ins().band(id_ok, name_ok);
    builder.ins().band(id_name_ok, gen_ok)
}

/// The cached value bits of a member-value `cell`.
pub(crate) fn member_value_cell_value(builder: &mut FunctionBuilder, cell: Value) -> Value {
    builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(MemberValueCell, value) as i32),
    )
}
