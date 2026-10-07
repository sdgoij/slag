//! Common-subexpression elimination over the SSA IR
//! (`.notes/optimizing-tier-impl.md` §2, `pass/cse.rs`).
//!
//! Block-local: two instructions with the same op, immediate and (already
//! resolved) arguments produce the same value, so the second's uses are
//! rewritten to the first and the second becomes dead (DCE removes it). Two
//! kinds are eligible — a *pure* instruction (a store-free, deterministic
//! computation) and a `FrameLoad`, which reads a private frame slot. A
//! `FrameLoad`'s availability is invalidated by any instruction that can write
//! `Slots` (a store, or a `call()`-effects op); a pure instruction's is not,
//! because a pure op cannot change another pure op's result.
//!
//! Block-local is what keeps it sound with no dominator analysis: within one
//! block the order is straight-line, so the first occurrence dominates the
//! second, and every rewritten use is dominated by the value it now reads.

use crate::opt::ir::{Effects, Function, Heap, Imm, Op, Term, ValueId};

/// A previously-seen, still-available computation.
struct Avail {
    op: Op,
    imm: Imm,
    args: Vec<ValueId>,
    value: ValueId,
    /// Whether this entry must be dropped when a frame slot may change.
    is_load: bool,
}

/// Eliminate redundant computations. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let mut subst: Vec<Option<ValueId>> = vec![None; func.value_count() as usize];
    let mut changed = false;

    for b in 0..func.block_count() as u32 {
        let mut avail: Vec<Avail> = Vec::new();
        let insts = func.block(b).insts.clone();
        for inst in &insts {
            if inst
                .effects
                .may_clobber_reads_of(Effects::read(Heap::Slots))
            {
                avail.retain(|a| !a.is_load);
            }
            let Some(result) = inst.result else { continue };
            if inst.op == Op::Check || inst.op == Op::TdzCheck {
                continue;
            }
            let is_load = inst.op == Op::FrameLoad;
            if !is_load && !inst.effects.is_pure() {
                continue;
            }
            let args: Vec<ValueId> = inst.args.iter().map(|&a| resolve_one(&subst, a)).collect();
            match avail
                .iter()
                .find(|a| a.op == inst.op && a.imm == inst.imm && a.args == args)
            {
                Some(a) if a.value != result => {
                    subst[result as usize] = Some(a.value);
                    changed = true;
                }
                Some(_) => {}
                None => avail.push(Avail {
                    op: inst.op,
                    imm: inst.imm.clone(),
                    args,
                    value: result,
                    is_load,
                }),
            }
        }
    }

    if changed {
        resolve(&mut subst);
        rewrite_uses(func, &subst);
    }
    changed
}

/// Follow a single substitution chain (acyclic: a later value maps to an
/// earlier one).
fn resolve_one(subst: &[Option<ValueId>], mut v: ValueId) -> ValueId {
    while let Some(next) = subst[v as usize] {
        v = next;
    }
    v
}

/// Resolve every substitution to its fixed point.
fn resolve(subst: &mut [Option<ValueId>]) {
    for i in 0..subst.len() {
        if let Some(v) = subst[i] {
            subst[i] = Some(resolve_one(subst, v));
        }
    }
}

/// Rewrite every use (instruction argument, branch condition, edge argument) to
/// its substituted value.
fn rewrite_uses(func: &mut Function, subst: &[Option<ValueId>]) {
    let map = |v: &mut ValueId| {
        if let Some(r) = subst[*v as usize] {
            *v = r;
        }
    };
    for b in 0..func.block_count() as u32 {
        let block = func.block_mut(b);
        for inst in &mut block.insts {
            inst.args.iter_mut().for_each(map);
        }
        match block.term.as_mut() {
            Some(Term::Jump { args, .. }) => args.iter_mut().for_each(map),
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                map(cond);
                then_args.iter_mut().for_each(map);
                else_args.iter_mut().for_each(map);
            }
            Some(Term::Return(Some(v)) | Term::Throw(v)) => map(v),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::Type;

    #[test]
    fn eliminates_a_redundant_frame_load() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let one = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[one],
                Effects::write(Heap::Slots),
                Imm::Slot(0),
            );
            let first = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Number,
                Effects::read(Heap::Slots),
                Imm::Slot(0),
            );
            let second = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Number,
                Effects::read(Heap::Slots),
                Imm::Slot(0),
            );
            let sum = b.emit(
                entry,
                Op::Add,
                &[first, second],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(run(&mut func));
        // The `sum`'s two operands are now the same value.
        let add = func
            .block(entry)
            .insts
            .iter()
            .find(|i| i.op == Op::Add)
            .expect("add");
        assert_eq!(add.args[0], add.args[1]);
    }

    #[test]
    fn a_store_invalidates_the_cached_load() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let one = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[one],
                Effects::write(Heap::Slots),
                Imm::Slot(0),
            );
            let first = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Number,
                Effects::read(Heap::Slots),
                Imm::Slot(0),
            );
            let two = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[two],
                Effects::write(Heap::Slots),
                Imm::Slot(0),
            );
            let second = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Number,
                Effects::read(Heap::Slots),
                Imm::Slot(0),
            );
            let sum = b.emit(
                entry,
                Op::Add,
                &[first, second],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(!run(&mut func));
    }
}
