//! Numeric effect narrowing (`.notes/optimizing-tier-impl.md` §2, `pass/`).
//!
//! An arithmetic op's *sound* default effects are `call()` — `+` may run
//! `valueOf`, `-` may throw on a BigInt/Symbol. That default is what stops CSE
//! and load elimination: every `+` "writes the world", so no cached load
//! survives it. This pass proves the cases where coercion cannot happen and
//! widens those ops to `pure()`.
//!
//! A frame slot is numeric when it is stored at least once and every store
//! stores a numeric value (a slot with no store is a parameter or `undefined`,
//! never a number). A value is numeric when it is a numeric constant, a load of
//! a numeric slot, or an arithmetic or bit op on numeric operands. The result is the
//! **greatest fixpoint** of a monotone (shrinking) operator, which is the sound
//! over-approximation for the "must be numeric" property.
//!
//! A second, independent fixpoint tracks the stronger "is an int32" property
//! (a bit-op result, an in-range integral constant, or a slot stored only such
//! values). A load of an `Int` slot is typed `Int`, so the lowering can skip the
//! `|x| < 2^63` range guard on that operand (`opt_lower::call_int_binary`).

use crate::opt::ir::{Effects, Function, Imm, Inst, Op, Type, ValueId};
use std::collections::HashMap;

/// Widen the provably-numeric arithmetic ops to `pure()`. Returns whether the
/// IR changed.
pub fn run(func: &mut Function) -> bool {
    let slots = slot_count(func);
    let mut slot_numeric = vec![true; slots];
    let mut slot_int = vec![true; slots];
    loop {
        let values = value_numerics(func, &slot_numeric);
        let ints = restrict_int_to_arith_use(func, value_ints(func, &values, &slot_int), slots);
        let next = slot_flags(func, &values, slots);
        let next_int = slot_flags(func, &ints, slots);
        if next == slot_numeric && next_int == slot_int {
            return rewrite(
                func,
                &values,
                &ints,
                &slot_numeric,
                &slot_int,
                widen_effects(),
            );
        }
        slot_numeric = next;
        slot_int = next_int;
    }
}

/// The number of frame slots the body names.
fn slot_count(func: &Function) -> usize {
    let mut max = 0usize;
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let Imm::Slot(s) = inst.imm {
                max = max.max(s as usize + 1);
            }
        }
    }
    max
}

/// Each value's "is a Number" flag, given the current per-slot flags.
fn value_numerics(func: &Function, slot_numeric: &[bool]) -> Vec<bool> {
    let mut numeric = vec![false; func.value_count() as usize];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let Some(r) = inst.result {
                numeric[r as usize] = is_numeric_inst(inst, &numeric, slot_numeric);
            }
        }
    }
    numeric
}

/// Whether an instruction produces a Number, given its operands' flags.
fn is_numeric_inst(inst: &Inst, values: &[bool], slot_numeric: &[bool]) -> bool {
    let arg_numeric = |a: &ValueId| values.get(*a as usize).copied().unwrap_or(false);
    match inst.op {
        Op::Const => matches!(inst.imm, Imm::Float(_) | Imm::Int(_)),
        // A guard asserts its value's type, so a `Number` guard produces a
        // Number (and lets a slot stored only guarded reads stay numeric).
        Op::GuardType => inst.ty.is_numeric(),
        Op::FrameLoad => matches!(
            inst.imm,
            Imm::Slot(s) if slot_numeric.get(s as usize).copied().unwrap_or(false)
        ),
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow | Op::Neg | Op::ToNumber => {
            !inst.args.is_empty() && inst.args.iter().all(arg_numeric)
        }
        // A bit op on genuine Numbers is a Number (`ToInt32`/`ToUint32` of a
        // Number is an integral Number). The arg-numeric guard is required: a
        // BigInt operand makes the result a BigInt, not a Number. This is a
        // *type* fact only — `rewrite` widens the arithmetic ops, not these —
        // but it lets a slot stored a `&`/`|`/`<<` be proven numeric, so its
        // later reads lower tag-free.
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot => {
            !inst.args.is_empty() && inst.args.iter().all(arg_numeric)
        }
        // A wrapping i32 op is an integral Number.
        Op::IntAdd | Op::IntSub | Op::IntMul => true,
        _ => false,
    }
}

/// Each slot's flag: stored at least once, and every store is a member of the
/// lattice (`values` carries the value flag for that lattice).
fn slot_flags(func: &Function, values: &[bool], slots: usize) -> Vec<bool> {
    let mut stored = vec![false; slots];
    let mut all = vec![true; slots];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.op != Op::FrameStore {
                continue;
            }
            let Imm::Slot(s) = inst.imm else { continue };
            let Some(&value) = inst.args.first() else {
                continue;
            };
            let s = s as usize;
            if s >= slots {
                continue;
            }
            stored[s] = true;
            all[s] &= values.get(value as usize).copied().unwrap_or(false);
        }
    }
    (0..slots).map(|s| stored[s] && all[s]).collect()
}

/// Each value's "is an int32" flag, given the numeric-value flags and the
/// per-slot int flags. An int32 is a Number, so the numeric flags gate the
/// bit-op case (`ToInt32` of a BigInt is a BigInt, not an int32).
fn value_ints(func: &Function, values: &[bool], slot_int: &[bool]) -> Vec<bool> {
    let mut ints = vec![false; func.value_count() as usize];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let Some(r) = inst.result {
                ints[r as usize] = is_int_inst(inst, values, slot_int);
            }
        }
    }
    ints
}

/// Whether `imm` is an int32 — an `Imm::Int`, or an integral `f64` inside the
/// i32 range. These are the constants whose `|x| < 2^63` guard can be skipped.
fn is_int_const(imm: &Imm) -> bool {
    match imm {
        Imm::Int(_) => true,
        Imm::Float(f) => f.fract() == 0.0 && f.abs() < 2_147_483_648.0,
        _ => false,
    }
}

/// Whether an instruction produces an int32, given its operands' numeric flags
/// and the per-slot int flags.
fn is_int_inst(inst: &Inst, values: &[bool], slot_int: &[bool]) -> bool {
    let arg_numeric = |a: &ValueId| values.get(*a as usize).copied().unwrap_or(false);
    match inst.op {
        Op::Const => is_int_const(&inst.imm),
        Op::FrameLoad => matches!(
            inst.imm,
            Imm::Slot(s) if slot_int.get(s as usize).copied().unwrap_or(false)
        ),
        // `ToInt32` of a Number is a signed int32, whatever the operand's
        // magnitude — so the range guard is unnecessary on it. `>>>` is excluded:
        // its `ToUint32` result (`0..2^32-1`) needs an unsigned conversion, so it
        // stays a `Number` (see `lift::binary_type`).
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::BitNot => {
            !inst.args.is_empty() && inst.args.iter().all(arg_numeric)
        }
        // A wrapping i32 op is an int32 by construction.
        Op::IntAdd | Op::IntSub | Op::IntMul => true,
        _ => false,
    }
}

/// Restrict the forward int32 lattice to the values that *benefit* from the i32
/// representation.
///
/// A value that is an int32 but whose only uses are boundaries — a frame store,
/// a helper argument, an element key — need not carry the representation: the
/// lowering would convert it at every crossing, and the frame slot it feeds is
/// a `Value` word the interpreter and the deopt resume read regardless. Keep
/// `Int` only for a value with an arithmetic/bit use, or for the store of a slot
/// that is read into such a use (the register body's accumulator slot, whose
/// loads feed its own `IntAdd`). `IntAdd`/`IntSub`/`IntMul` results are
/// inherently i32 — their lowering emits an i32 op — and are always kept.
fn restrict_int_to_arith_use(func: &Function, mut ints: Vec<bool>, slots: usize) -> Vec<bool> {
    let is_arith = |op: Op| {
        matches!(
            op,
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::Pow
                | Op::Neg
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::BitNot
                | Op::IntAdd
                | Op::IntSub
                | Op::IntMul
        )
    };
    let mut arith = vec![false; ints.len()];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if is_arith(inst.op) {
                for &a in &inst.args {
                    if let Some(flag) = arith.get_mut(a as usize) {
                        *flag = true;
                    }
                }
            }
        }
    }
    // A slot is arith-read when one of its loads is used by an arithmetic/bit op.
    let mut slot_arith = vec![false; slots];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.op == Op::FrameLoad
                && let Imm::Slot(s) = inst.imm
                && inst
                    .result
                    .is_some_and(|r| arith.get(r as usize).copied().unwrap_or(false))
                && let Some(flag) = slot_arith.get_mut(s as usize)
            {
                *flag = true;
            }
        }
    }
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.op == Op::FrameStore
                && let Imm::Slot(s) = inst.imm
                && slot_arith.get(s as usize).copied().unwrap_or(false)
                && let Some(&v) = inst.args.first()
                && let Some(flag) = arith.get_mut(v as usize)
            {
                *flag = true;
            }
        }
    }
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let Some(r) = inst.result
                && ints.get(r as usize).copied().unwrap_or(false)
                && !arith.get(r as usize).copied().unwrap_or(false)
                && !matches!(inst.op, Op::IntAdd | Op::IntSub | Op::IntMul)
            {
                ints[r as usize] = false;
            }
        }
    }
    ints
}

/// Whether the bit-op and numeric-comparison effect widening runs
/// (`SLAG_WIDEN=0` disables it, the same-binary seam for measuring its marginal
/// effect).
fn widen_effects() -> bool {
    std::env::var("SLAG_WIDEN")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Widen the effects (and tighten the type) of the now-provably-numeric ops, and
/// mark a load of a numeric slot as a `Number` (`Int` for an int slot) so the
/// lowering can trust it.
fn rewrite(
    func: &mut Function,
    values: &[bool],
    values_int: &[bool],
    slot_numeric: &[bool],
    slot_int: &[bool],
    widen: bool,
) -> bool {
    let mut changed = false;
    // The lowered code reads each value's `Function::value_type`, so a narrowed
    // `Inst::ty` alone is invisible to it — collect and apply the value-table
    // retypes after the instruction walk (which holds `func` mutably).
    let mut retype: Vec<(ValueId, Type)> = Vec::new();
    for b in 0..func.block_count() as u32 {
        for inst in &mut func.block_mut(b).insts {
            if inst.op == Op::FrameLoad {
                let Imm::Slot(s) = inst.imm else { continue };
                let s = s as usize;
                let want = if slot_int.get(s).copied().unwrap_or(false) {
                    Type::Int
                } else if slot_numeric.get(s).copied().unwrap_or(false) {
                    Type::Number
                } else {
                    continue;
                };
                if inst.ty != want {
                    inst.ty = want;
                    changed = true;
                }
                if let Some(r) = inst.result {
                    retype.push((r, want));
                }
                continue;
            }
            // An int32-producing instruction (a bit op, an in-range integral
            // constant) is typed `Int`, so the lowering skips its range guard.
            if let Some(r) = inst.result
                && values_int.get(r as usize).copied().unwrap_or(false)
            {
                if inst.ty != Type::Int {
                    inst.ty = Type::Int;
                    changed = true;
                }
                // A bit op on provable Numbers runs no user code and cannot
                // throw (`ToInt32` of a Number is pure), so its `call()`
                // effects widen to `pure()`.
                if widen && !inst.effects.is_pure() {
                    inst.effects = Effects::pure();
                    changed = true;
                }
                retype.push((r, Type::Int));
                continue;
            }
            // A comparison of two provable Numbers likewise runs no user code
            // and cannot throw; only its effects change (its result is `Bool`).
            if matches!(inst.op, Op::Lt | Op::Le | Op::Gt | Op::Ge)
                && !inst.args.is_empty()
                && inst
                    .args
                    .iter()
                    .all(|a| values.get(*a as usize).copied().unwrap_or(false))
                && widen
                && !inst.effects.is_pure()
            {
                inst.effects = Effects::pure();
                changed = true;
            }
            // A bit op on provable Numbers is pure whatever its result type —
            // this reaches `>>>`, whose `Number` result the branch above (which
            // retypes to `Int`) skipped.
            if matches!(
                inst.op,
                Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot
            ) && !inst.args.is_empty()
                && inst
                    .args
                    .iter()
                    .all(|a| values.get(*a as usize).copied().unwrap_or(false))
                && widen
                && !inst.effects.is_pure()
            {
                inst.effects = Effects::pure();
                changed = true;
            }
            // The lift types every bit op `Int` unconditionally
            // (`lift::binary_type`); the prover either did not confirm this
            // value is an int32 (its operands are not both proven Numbers), or
            // confirmed it but the value has only boundary uses (see
            // `restrict_int_to_arith_use`). Keep it in the `Value`-word
            // representation rather than an i32 register. A numeric value is a
            // `Number`; anything else (a bit op on a non-Number operand) is left
            // `Unknown` so the lowering takes the guarded path.
            if inst.ty == Type::Int
                && matches!(
                    inst.op,
                    Op::BitAnd
                        | Op::BitOr
                        | Op::BitXor
                        | Op::Shl
                        | Op::Shr
                        | Op::UShr
                        | Op::BitNot
                        | Op::Const
                )
            {
                let ty = if inst
                    .result
                    .is_some_and(|r| values.get(r as usize).copied().unwrap_or(false))
                {
                    Type::Number
                } else {
                    Type::Unknown
                };
                inst.ty = ty;
                if let Some(r) = inst.result {
                    retype.push((r, ty));
                }
                changed = true;
            }
            if !matches!(
                inst.op,
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow | Op::Neg | Op::ToNumber
            ) {
                continue;
            }
            if inst.args.is_empty()
                || !inst
                    .args
                    .iter()
                    .all(|a| values.get(*a as usize).copied().unwrap_or(false))
            {
                continue;
            }
            if !inst.effects.is_pure() {
                inst.effects = Effects::pure();
                changed = true;
            }
            if inst.ty != Type::Number {
                inst.ty = Type::Number;
                changed = true;
            }
            if let Some(r) = inst.result {
                retype.push((r, Type::Number));
            }
        }
    }
    for (v, ty) in retype {
        if func.value_type(v) != ty {
            func.set_value_type(v, ty);
            changed = true;
        }
    }
    let fused = fuse_wrapping_arith(func, values_int);
    changed || fused
}

/// Fuse a `ToInt32(a op b)` — a `| 0`/`^ 0` on a `+`/`-` of two int32s — into a
/// wrapping i32 op (`IntAdd`/`IntSub`). The typer types the arithmetic `Number`
/// (a sum of int32s can exceed `2^31`) but the `ToInt32` truncates it, and
/// `ToInt32(a + b)` of two int32s IS the wrapping i32 add — so the whole chain
/// lowers to one `iadd`, no f64 round trip. The original arithmetic op stays
/// (DCE removes it if the fused use was its only use).
///
/// `*` is *excluded*: an int32 product can exceed `2^53`, so the spec's f64
/// multiply rounds before `ToInt32`, and the wrapping i32 multiply would differ
/// (the register-body plan's separate `Mul` bound is what makes its `IntMul`
/// safe).
fn fuse_wrapping_arith(func: &mut Function, values_int: &[bool]) -> bool {
    let mut def: HashMap<ValueId, (Op, Imm, Vec<ValueId>)> = HashMap::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let Some(r) = inst.result {
                def.insert(r, (inst.op, inst.imm.clone(), inst.args.clone()));
            }
        }
    }
    let is_zero = |v: ValueId| match def.get(&v) {
        Some((Op::Const, Imm::Int(0), _)) => true,
        Some((Op::Const, Imm::Float(f), _)) => *f == 0.0,
        _ => false,
    };
    let mut changed = false;
    for b in 0..func.block_count() as u32 {
        for inst in &mut func.block_mut(b).insts {
            // `x | 0` and `x ^ 0` are both `ToInt32(x)`.
            if !matches!(inst.op, Op::BitOr | Op::BitXor) || inst.args.len() != 2 {
                continue;
            }
            let other = if is_zero(inst.args[0]) {
                inst.args[1]
            } else if is_zero(inst.args[1]) {
                inst.args[0]
            } else {
                continue;
            };
            let Some((op, _, args)) = def.get(&other) else {
                continue;
            };
            let int = match op {
                Op::Add => Op::IntAdd,
                Op::Sub => Op::IntSub,
                _ => continue,
            };
            if args.len() != 2
                || !values_int.get(args[0] as usize).copied().unwrap_or(false)
                || !values_int.get(args[1] as usize).copied().unwrap_or(false)
            {
                continue;
            }
            inst.op = int;
            inst.args = args.clone();
            inst.ty = Type::Int;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Term};

    /// `var s = 0; var i = 0; s = s + i * 2;` — both slots always numeric, so
    /// the `Mul` and `Add` narrow.
    fn numeric_body() -> Function {
        let mut func = Function::new();
        let entry = func.entry();
        let mut b = Builder::new(&mut func);
        let zero = b.emit(
            entry,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(0.0),
        );
        b.emit_void(
            entry,
            Op::FrameStore,
            &[zero],
            Effects::write(crate::opt::ir::Heap::Slots),
            Imm::Slot(0),
        );
        b.emit_void(
            entry,
            Op::FrameStore,
            &[zero],
            Effects::write(crate::opt::ir::Heap::Slots),
            Imm::Slot(1),
        );
        let i = b.emit(
            entry,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(crate::opt::ir::Heap::Slots),
            Imm::Slot(1),
        );
        let two = b.emit(
            entry,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(2.0),
        );
        let prod = b.emit(
            entry,
            Op::Mul,
            &[i, two],
            Type::Unknown,
            Effects::call(),
            Imm::None,
        );
        let s = b.emit(
            entry,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(crate::opt::ir::Heap::Slots),
            Imm::Slot(0),
        );
        let sum = b.emit(
            entry,
            Op::Add,
            &[s, prod],
            Type::Unknown,
            Effects::call(),
            Imm::None,
        );
        b.emit_void(
            entry,
            Op::FrameStore,
            &[sum],
            Effects::write(crate::opt::ir::Heap::Slots),
            Imm::Slot(0),
        );
        b.term(entry, Term::Return(Some(sum)));
        func
    }

    fn op_effects(func: &Function, op: Op) -> Option<Effects> {
        func.block(func.entry())
            .insts
            .iter()
            .find(|i| i.op == op)
            .map(|i| i.effects)
    }

    #[test]
    fn narrows_arithmetic_on_numeric_slots() {
        let mut func = numeric_body();
        assert!(run(&mut func));
        assert!(op_effects(&func, Op::Mul).expect("mul").is_pure());
        assert!(op_effects(&func, Op::Add).expect("add").is_pure());
    }

    #[test]
    fn narrowed_values_carry_the_number_type() {
        // The lowering reads each operand's `Function::value_type` (not
        // `Inst::ty`), so a narrowed value must be re-typed in the value table
        // too — otherwise the type the pass proves never reaches the tag-free
        // arithmetic path.
        let mut func = numeric_body();
        run(&mut func);
        let Some(Term::Return(Some(sum))) = func.block(func.entry()).term else {
            panic!("the body returns the sum");
        };
        assert_eq!(func.value_type(sum), Type::Number);
    }

    #[test]
    fn a_number_guard_makes_its_operand_numeric() {
        // `s + guarded` where `guarded` is a `Number`-typed type guard: the
        // guard's result counts as numeric, so the `Add` narrows `pure` and the
        // slot storing it stays numeric (which is what lets the guard make the
        // consuming arithmetic lower tag-free).
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let zero = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(0.0),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[zero],
                Effects::write(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let s = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let read = b.emit(
                entry,
                Op::MemberGuard,
                &[s, s],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let guarded = b.emit(
                entry,
                Op::GuardType,
                &[read, read],
                Type::Number,
                Effects::pure(),
                Imm::Int(1),
            );
            let add = b.emit(
                entry,
                Op::Add,
                &[s, guarded],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[add],
                Effects::write(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            b.term(entry, Term::Return(Some(add)));
        }
        run(&mut func);
        assert!(op_effects(&func, Op::Add).expect("add").is_pure());
    }

    #[test]
    fn does_not_narrow_when_a_slot_may_hold_a_string() {
        // slot 1 is stored a string, so the `i * 2` is not provably numeric.
        let mut func = Function::new();
        let entry = func.entry();
        let mul;
        {
            let mut b = Builder::new(&mut func);
            let text = b.emit(
                entry,
                Op::Const,
                &[],
                Type::String,
                Effects::pure(),
                Imm::Str("x".into()),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[text],
                Effects::write(crate::opt::ir::Heap::Slots),
                Imm::Slot(1),
            );
            let i = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(1),
            );
            let two = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            mul = b.emit(
                entry,
                Op::Mul,
                &[i, two],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(mul)));
        }
        run(&mut func);
        assert!(!op_effects(&func, Op::Mul).expect("mul").is_pure());
    }
}
