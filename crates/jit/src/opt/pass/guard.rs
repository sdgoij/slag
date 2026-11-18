//! Prune the typer's read guards that pay nothing.
//!
//! The lift emits `Op::GuardType` on a member/element read's result for every
//! read (`.notes/archive/perf/tier-typer.md` T2). A guard only pays where a *type-sensitive*
//! op consumes the value — the ops whose lowering reads the operand type to pick
//! the cheaper path (`emit_bare_numeric`, or the known-operand tag-check skip).
//! Everywhere else (the read flows to a call argument, a `return`, a member
//! store) the guard is a check with no payoff, and a wrong-type read deopts for
//! nothing — measured as a ~1% corpus-wide cost that offsets the read-loop win.
//!
//! A `FrameStore` keeps its guard: `narrow` reads the *stored value's* type to
//! decide a slot is numeric, so a guarded store is what makes a later arithmetic
//! read of that slot tag-free. An edge argument keeps it too — the value may
//! cross a block boundary and be consumed type-sensitively there.
//!
//! Dropping a guard is pure type erasure: the guard's result *is* its operand's
//! runtime value, so every use forwards to `args[0]`. Sound because the guard
//! only removes a type annotation, and a pruned guard is one no type-sensitive
//! op reads that annotation from.

use crate::opt::ir::{Function, Inst, Op, Term, Type, ValueId};

/// Whether an op reads its operand's *type* to choose a cheaper lowering.
///
/// The bitwise/shift ops do not (their lowering is type-blind), nor `Eq`/`!`
/// (which take the slow helper regardless), nor the coercing unaries.
fn type_sensitive(op: Op) -> bool {
    matches!(
        op,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Lt | Op::Le | Op::Gt | Op::Ge
    )
}

/// Whether `inst` is an `Add` that is certainly a string concatenation because
/// one operand is a value known to be a string.
///
/// Such an `Add` lowers to a concatenation whatever the other operand's type,
/// so a guard on that operand is never read. Keeping one is not free: a member
/// field that is a number on some objects and a string on others (ordinary
/// application data) makes the numeric speculation fail on every string-valued
/// read, and a deopt in a nested body resumes interpreted for the rest of the
/// call with no way to rebuild (see `JitCache::lookup`'s in-flight rule).
fn is_certain_concat(inst: &Inst, is_string: &[bool]) -> bool {
    inst.op == Op::Add
        && inst
            .args
            .iter()
            .any(|&a| is_string.get(a as usize) == Some(&true))
}

/// Drop every read guard no type-sensitive op consumes. Returns whether the IR
/// changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.value_count() as usize;
    // A value is "kept" when some use reads its type: a type-sensitive op, a
    // slot store (narrow's numeric-slot proof feeds on it), or an edge argument
    // (the value may be consumed type-sensitively in the target block).
    let mut keep = vec![false; n];
    // Which values are certainly strings at runtime: a `String`-typed value, or
    // the result of an `Add` that is itself a concatenation. Propagated to a
    // fixed point because a concatenation chains — `a + ":" + b`'s second
    // `Add` consumes the first `Add`'s result, which is a string without being
    // `Type::String` in the IR (an `Add`'s result is typed `Unknown`).
    let mut is_string = vec![false; n];
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if let (Some(r), Type::String) = (inst.result, inst.ty) {
                is_string[r as usize] = true;
            }
        }
    }
    loop {
        let mut changed = false;
        for b in 0..func.block_count() as u32 {
            for inst in &func.block(b).insts {
                let Some(r) = inst.result else { continue };
                if is_string[r as usize] || !is_certain_concat(inst, &is_string) {
                    continue;
                }
                is_string[r as usize] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for inst in &block.insts {
            if (type_sensitive(inst.op) && !is_certain_concat(inst, &is_string))
                || inst.op == Op::FrameStore
            {
                for &a in &inst.args {
                    keep[a as usize] = true;
                }
            }
        }
        match block.term.as_ref() {
            Some(Term::Jump { args, .. }) => {
                for &a in args {
                    keep[a as usize] = true;
                }
            }
            Some(Term::Branch {
                then_args,
                else_args,
                ..
            }) => {
                for &a in then_args.iter().chain(else_args) {
                    keep[a as usize] = true;
                }
            }
            _ => {}
        }
    }

    let mut subst: Vec<Option<ValueId>> = vec![None; n];
    let mut changed = false;
    for b in 0..func.block_count() as u32 {
        func.block_mut(b).insts.retain(|inst| {
            if inst.op != Op::GuardType {
                return true;
            }
            let Some(r) = inst.result else { return true };
            if keep[r as usize] {
                return true;
            }
            let Some(&operand) = inst.args.first() else {
                return true;
            };
            subst[r as usize] = Some(operand);
            changed = true;
            false
        });
    }
    if !changed {
        return false;
    }

    let resolve = |subst: &[Option<ValueId>], mut v: ValueId| {
        while let Some(next) = subst[v as usize] {
            v = next;
        }
        v
    };
    for b in 0..func.block_count() as u32 {
        let block = func.block_mut(b);
        for inst in &mut block.insts {
            for a in &mut inst.args {
                *a = resolve(&subst, *a);
            }
        }
        if let Some(term) = &mut block.term {
            match term {
                Term::Jump { args, .. } => {
                    for a in args {
                        *a = resolve(&subst, *a);
                    }
                }
                Term::Branch {
                    cond,
                    then_args,
                    else_args,
                    ..
                } => {
                    *cond = resolve(&subst, *cond);
                    for a in then_args.iter_mut().chain(else_args) {
                        *a = resolve(&subst, *a);
                    }
                }
                Term::Return(Some(v)) | Term::Throw(v) => *v = resolve(&subst, *v),
                _ => {}
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Imm, Type};

    /// `guard = GuardType(read); consumer(guard)` with a chosen consumer.
    fn body(consumer: Op, consumer_ty: Type) -> Function {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let obj = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Object,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let read = b.emit(
                entry,
                Op::MemberGuard,
                &[obj, obj],
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
            let out = b.emit(
                entry,
                consumer,
                &[guarded],
                consumer_ty,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(out)));
        }
        func
    }

    fn guards(func: &Function) -> usize {
        func.block(func.entry())
            .insts
            .iter()
            .filter(|i| i.op == Op::GuardType)
            .count()
    }

    #[test]
    fn keeps_a_guard_an_arithmetic_op_consumes() {
        let mut func = body(Op::Add, Type::Number);
        assert!(!run(&mut func), "an Add consumes the guard's type");
        assert_eq!(guards(&func), 1);
    }

    #[test]
    fn drops_a_guard_a_call_consumes() {
        // A read passed as a call argument: the guard's type is never read, so
        // it is pure overhead (and would deopt for nothing on a wrong type).
        let mut func = body(Op::Call, Type::Number);
        assert!(run(&mut func));
        assert_eq!(guards(&func), 0);
        // The call now reads the read's value directly.
        let insts = &func.block(func.entry()).insts;
        let call = insts.iter().find(|i| i.op == Op::Call).expect("call");
        assert_eq!(call.args[0], insts[1].result.expect("read result"));
    }

    /// `read = MemberGuard(); guard = GuardType(read); out = Add(guard, rhs)`,
    /// where `rhs` is a string constant when `string_rhs`, else a second read.
    fn body_add(string_rhs: bool) -> Function {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let obj = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Object,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let read = b.emit(
                entry,
                Op::MemberGuard,
                &[obj, obj],
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
            let rhs = if string_rhs {
                b.emit(
                    entry,
                    Op::Const,
                    &[],
                    Type::String,
                    Effects::pure(),
                    Imm::None,
                )
            } else {
                b.emit(
                    entry,
                    Op::MemberGuard,
                    &[obj, obj],
                    Type::Unknown,
                    Effects::call(),
                    Imm::None,
                )
            };
            let out = b.emit(
                entry,
                Op::Add,
                &[guarded, rhs],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(out)));
        }
        func
    }

    #[test]
    fn drops_a_guard_a_string_concat_consumes() {
        // `field + "lit"` is a concatenation whatever the field's type, so the
        // numeric speculation is never read — and it fails on every string
        // value, which is the ordinary heterogeneous-field shape.
        let mut func = body_add(true);
        assert!(run(&mut func));
        assert_eq!(guards(&func), 0);
    }

    #[test]
    fn drops_a_guard_behind_a_concatenation_chain() {
        // `a + ":" + b`: the guard on `b` consumes the first `Add`'s result,
        // which is a string even though the IR types every `Add` `Unknown`.
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let obj = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Object,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let left = b.emit(
                entry,
                Op::MemberGuard,
                &[obj, obj],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let lit = b.emit(
                entry,
                Op::Const,
                &[],
                Type::String,
                Effects::pure(),
                Imm::None,
            );
            let concat = b.emit(
                entry,
                Op::Add,
                &[left, lit],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let right = b.emit(
                entry,
                Op::MemberGuard,
                &[obj, obj],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let guarded = b.emit(
                entry,
                Op::GuardType,
                &[right, right],
                Type::Number,
                Effects::pure(),
                Imm::Int(1),
            );
            let out = b.emit(
                entry,
                Op::Add,
                &[concat, guarded],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(out)));
        }
        assert!(run(&mut func));
        assert_eq!(guards(&func), 0);
    }

    #[test]
    fn keeps_a_guard_an_add_with_an_untyped_operand_consumes() {
        // An `Add` whose other operand is not known to be a string can still
        // lower to tag-free arithmetic, so the guard pays there.
        let mut func = body_add(false);
        assert!(!run(&mut func));
        assert_eq!(guards(&func), 1);
    }
}
