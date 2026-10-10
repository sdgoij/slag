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

/// Widen the provably-numeric arithmetic ops to `pure()`. Returns whether the
/// IR changed.
pub fn run(func: &mut Function) -> bool {
    let slots = slot_count(func);
    let mut slot_numeric = vec![true; slots];
    let mut slot_int = vec![true; slots];
    loop {
        let values = value_numerics(func, &slot_numeric);
        let ints = value_ints(func, &values, &slot_int);
        let next = slot_flags(func, &values, slots);
        let next_int = slot_flags(func, &ints, slots);
        if next == slot_numeric && next_int == slot_int {
            return rewrite(func, &values, &ints, &slot_numeric, &slot_int);
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
        // `ToInt32`/`ToUint32` of a Number is an int32, whatever the operand's
        // magnitude — so the range guard is unnecessary on it.
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot => {
            !inst.args.is_empty() && inst.args.iter().all(arg_numeric)
        }
        _ => false,
    }
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
                retype.push((r, Type::Int));
                continue;
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
