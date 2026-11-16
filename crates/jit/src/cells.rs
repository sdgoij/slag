//! Shared member-value-cell addressing and validation.
//!
//! Both the per-step lowerer (`compiler.rs`) and the optimizing tier
//! (`opt_lower.rs`) must compute the same cell address and validate it the same
//! way; keeping the arithmetic here is the "single-source the probe" rule from
//! `.notes/archive/perf/tier-guarded-read.md` §6 (a fork would drift from the store paths
//! that keep the cell current).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::FunctionBuilder;

use runtime::ir::{MEMBER_CELLS, MemberChainCell, MemberMapCell, MemberValueCell};

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

/// Whether `value` is a Function (`TAG_FUNCTION`).
pub(crate) fn is_function(builder: &mut FunctionBuilder, value: Value) -> Value {
    let masked = builder.ins().band_imm_u(value, crux::TAG_MASK as i64);
    let is_heap = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, masked, crux::TAG_PREFIX as i64);
    let tag = builder.ins().ushr_imm_u(value, 44);
    let tag = builder.ins().band_imm_u(tag, 0xF);
    let tag_fn = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, tag, crux::TAG_FUNCTION as i64);
    builder.ins().band(is_heap, tag_fn)
}

/// The machine twin of `Vm::chain_object`: the tag test for a receiver the
/// prototype-chain cache serves (an Object or a Function — a function's own
/// properties and [[Prototype]] live on its *object part*).
pub(crate) fn is_chain_receiver(builder: &mut FunctionBuilder, value: Value) -> Value {
    let plain = is_plain_object(builder, value);
    let function = is_function(builder, value);
    builder.ins().bor(plain, function)
}

/// The `JsObject` data pointer behind a chain receiver: an Object value's own
/// box, or a Function value's object part. A function's `JsObject` is the
/// FIRST field of its box payload (`#[repr(C)]`, `Function`), so both are the
/// payload address itself — the box layout decides this, not a load. Valid
/// only where [`is_chain_receiver`] holds — a non-receiver's payload is not a
/// box at all.
pub(crate) fn chain_object_ptr(builder: &mut FunctionBuilder, value: Value) -> Value {
    object_data_ptr(builder, value)
}

/// Whether a function receiver's own properties are all defined: false while
/// its boilerplate `length`/`name`[/`caller`/`arguments`] are deferred (`the
/// Function::props_pending` flag). A chain probe must decline a deferred
/// function — its own data properties are not part of the chain the probe
/// recorded, and the helper materializes them before answering.
///
/// `function_ptr` is a Function's payload (its object part); call only on the
/// `is_function` edge.
pub(crate) fn function_own_props_ready(
    builder: &mut FunctionBuilder,
    function_ptr: Value,
) -> Value {
    let pending = builder.ins().load(
        types::I8,
        MemFlagsData::new(),
        function_ptr,
        Offset32::new(std::mem::offset_of!(crux::function::Function, props_pending) as i32),
    );
    builder.ins().icmp_imm_u(IntCC::Equal, pending, 0)
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

/// The identity-keyed prototype-chain cell for `(receiver id, name)` — the
/// same slot arithmetic as `Vm::member_chain_index`, so the compiled probe and
/// the runtime's fill agree (a divergence would silently never hit).
pub(crate) fn member_chain_cell_addr(
    builder: &mut FunctionBuilder,
    cells: Value,
    receiver_id: Value,
    name_imm: Value,
) -> Value {
    let slot = builder.ins().bxor(receiver_id, name_imm);
    let slot = builder.ins().band_imm_u(slot, (MEMBER_CELLS - 1) as i64);
    let index_bytes = builder
        .ins()
        .imul_imm_s(slot, std::mem::size_of::<MemberChainCell>() as i64);
    builder.ins().iadd(cells, index_bytes)
}

/// The map-keyed prototype-chain cell for `(map id, name)`, mirroring
/// `Vm::member_map_cell_index`. Note the STRIDE: the table is
/// `Agent::member_chain_map_cells`, an array of `MemberChainCell` — not the
/// `MemberMapCell` table `member_map_cell_addr` addresses, whose cells are a
/// different size. Both slot arithmetic and stride must match the runtime.
pub(crate) fn member_chain_map_cell_addr(
    builder: &mut FunctionBuilder,
    cells: Value,
    map_id: Value,
    name_imm: Value,
) -> Value {
    let slot = builder.ins().bxor(map_id, name_imm);
    let slot = builder.ins().band_imm_u(slot, (MEMBER_CELLS - 1) as i64);
    let index_bytes = builder
        .ins()
        .imul_imm_s(slot, std::mem::size_of::<MemberChainCell>() as i64);
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

/// The shared member-value cell for a member read on a primitive string.
///
/// A primitive string's non-index members resolve on the realm's
/// `%String.prototype%` intrinsic — `ToObject` of a string uses the intrinsic,
/// not the reassignable `String.prototype` property — so the cell keyed by that
/// prototype's id, validated by its live generation, serves the read exactly as
/// an own-property cell does. The interpreter's primitive path populates the
/// cell on the first (cold) read; mutating the prototype bumps its generation
/// and invalidates the entry.
///
/// The caller must have established that the receiver is a string and that
/// [`JitCallContext::string_prototype_bits`] is non-zero. Returns the cell's
/// value bits and whether the cell validated.
pub(crate) fn string_proto_cell(
    builder: &mut FunctionBuilder,
    vm: Value,
    name: crux::AtomId,
    name_imm: Value,
) -> (Value, Value) {
    let proto_bits = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        vm,
        Offset32::new(
            std::mem::offset_of!(runtime::jit::JitCallContext, string_prototype_bits) as i32,
        ),
    );
    let proto = object_data_ptr(builder, proto_bits);
    let proto_id = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        proto,
        Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
    );
    let proto_gen = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        proto,
        Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
    );
    let cells = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        vm,
        Offset32::new(
            std::mem::offset_of!(runtime::jit::JitCallContext, member_value_cells) as i32,
        ),
    );
    let cell = member_value_cell_addr(builder, cells, proto_id, name_imm);
    let value = member_value_cell_value(builder, cell);
    let ok = member_value_cell_valid(builder, cell, proto_id, name, proto_gen);
    (value, ok)
}
