//! Loop-invariant code motion over the SSA IR
//! (`.notes/optimizing-tier-impl.md` §2, `pass/licm.rs`).
//!
//! The fused-test slice gave lifted loops a real preheader, which is what LICM
//! needs. For each natural loop (a back edge whose target dominates the source)
//! with a **unique** preheader — the one predecessor of the header outside the
//! loop — this hoists the instructions that are invariant and safe to speculate:
//! a `pure` computation (a narrowed arithmetic op, a constant) or an
//! `Op::FrameLoad` of a slot never stored in the loop. Both cannot trap, so
//! executing them once before a zero-trip loop is unobservable.
//!
//! An entry-header loop (`do`/`while`, whose back edge targets block 0) has no
//! predecessor outside the loop and is skipped — the same shape that made LICM
//! impossible before the fused-test slice. `Op::Check`/`Op::TdzCheck`/
//! `Op::GuardType` are never hoisted (a guard's timing is observable).

use crate::opt::ir::{Function, Imm, Inst, Op, Term, ValueId};

/// Hoist loop-invariant instructions. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.block_count();
    if n < 2 {
        return false;
    }
    let dom = dominators(func, n);
    let def_block = definitions(func);
    let mut changed = false;

    for source in 0..n as u32 {
        for header in successors(func, source) {
            if !dom[source as usize][header as usize] {
                continue;
            }
            let in_loop = natural_loop(func, source, header, &dom);
            let Some(preheader) = preheader_of(func, header, &in_loop) else {
                continue;
            };
            changed |= hoist_loop(func, &in_loop, preheader, &def_block);
        }
    }
    changed
}

/// Move every invariantly-safe instruction of one loop into its preheader.
fn hoist_loop(
    func: &mut Function,
    in_loop: &[bool],
    preheader: u32,
    def_block: &[Option<u32>],
) -> bool {
    let slots_stored = slot_stores(func, in_loop);
    // Fixpoint: an instruction is invariant when it is hoistable and every
    // operand is either defined outside the loop or already invariant.
    let mut invariant = vec![false; func.value_count() as usize];
    loop {
        let mut grew = false;
        for b in 0..func.block_count() as u32 {
            if !in_loop[b as usize] {
                continue;
            }
            for inst in &func.block(b).insts {
                let Some(r) = inst.result else { continue };
                if invariant[r as usize] || !hoistable(inst, &slots_stored) {
                    continue;
                }
                let ready = inst.args.iter().all(|a| {
                    invariant[*a as usize]
                        || def_block[*a as usize].is_some_and(|db| !in_loop[db as usize])
                });
                if ready {
                    invariant[r as usize] = true;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    let hoisted: Vec<bool> = invariant.clone();
    if !hoisted.iter().any(|&x| x) {
        return false;
    }

    // Dependency order: place an invariant only once its invariant operands are
    // placed. Bounded by the number of invariant instructions.
    let mut order: Vec<ValueId> = Vec::new();
    let mut placed = vec![false; func.value_count() as usize];
    loop {
        let mut progress = false;
        for b in 0..func.block_count() as u32 {
            if !in_loop[b as usize] {
                continue;
            }
            for inst in &func.block(b).insts {
                let Some(r) = inst.result else { continue };
                if !hoisted[r as usize] || placed[r as usize] {
                    continue;
                }
                if inst
                    .args
                    .iter()
                    .all(|a| !hoisted[*a as usize] || placed[*a as usize])
                {
                    placed[r as usize] = true;
                    order.push(r);
                    progress = true;
                }
            }
        }
        if !progress {
            break;
        }
    }

    // Pull the instructions out of the loop (in order) and append them to the
    // preheader.
    let mut moved: Vec<Inst> = Vec::with_capacity(order.len());
    for &v in &order {
        for b in 0..func.block_count() as u32 {
            if !in_loop[b as usize] {
                continue;
            }
            let block = func.block_mut(b);
            if let Some(pos) = block.insts.iter().position(|i| i.result == Some(v)) {
                moved.push(block.insts.remove(pos));
                break;
            }
        }
    }
    func.block_mut(preheader).insts.extend(moved);
    true
}

/// Whether an instruction may be speculated before the loop body.
fn hoistable(inst: &Inst, slots_stored: &[bool]) -> bool {
    match inst.op {
        Op::FrameLoad => matches!(
            inst.imm,
            Imm::Slot(s) if !slots_stored.get(s as usize).copied().unwrap_or(false)
        ),
        Op::Check | Op::TdzCheck | Op::GuardType => false,
        _ => inst.effects.is_pure(),
    }
}

/// Which frame slots the loop stores to.
fn slot_stores(func: &Function, in_loop: &[bool]) -> Vec<bool> {
    let mut stored: Vec<bool> = Vec::new();
    for b in 0..func.block_count() as u32 {
        if !in_loop[b as usize] {
            continue;
        }
        for inst in &func.block(b).insts {
            if inst.op == Op::FrameStore
                && let Imm::Slot(s) = inst.imm
            {
                if s as usize >= stored.len() {
                    stored.resize(s as usize + 1, false);
                }
                stored[s as usize] = true;
            }
        }
    }
    stored
}

/// The natural loop of the back edge `source -> header`: `header` plus every
/// block that reaches `source` without passing through `header` (equivalently,
/// every such block the header dominates — the gate excludes the header's own
/// outside predecessor, which the plain walk would absorb for a self-loop).
fn natural_loop(func: &Function, source: u32, header: u32, dom: &[Vec<bool>]) -> Vec<bool> {
    let mut in_loop = vec![false; func.block_count()];
    in_loop[header as usize] = true;
    // The back-edge source is in the loop by definition; then walk up through
    // every predecessor the header dominates. The dominance gate is what
    // excludes the header's own outside predecessor (and keeps a self-loop from
    // absorbing its preheader).
    let mut stack = vec![source];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut in_loop[b as usize], true) {
            continue;
        }
        for p in predecessors(func, b) {
            if !in_loop[p as usize] && dom[p as usize][header as usize] {
                stack.push(p);
            }
        }
    }
    in_loop
}

/// The unique predecessor of `header` outside the loop, if there is exactly one.
fn preheader_of(func: &Function, header: u32, in_loop: &[bool]) -> Option<u32> {
    let outside: Vec<u32> = predecessors(func, header)
        .into_iter()
        .filter(|p| !in_loop[*p as usize])
        .collect();
    if outside.len() == 1 {
        Some(outside[0])
    } else {
        None
    }
}

fn successors(func: &Function, b: u32) -> Vec<u32> {
    match func.block(b).term.as_ref() {
        Some(Term::Jump { target, .. }) => vec![*target],
        Some(Term::Branch {
            then_block,
            else_block,
            ..
        }) => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

fn predecessors(func: &Function, b: u32) -> Vec<u32> {
    let mut preds = Vec::new();
    for p in 0..func.block_count() as u32 {
        if successors(func, p).contains(&b) {
            preds.push(p);
        }
    }
    preds
}

/// Each value's defining block (`None` for a value that is never defined).
fn definitions(func: &Function) -> Vec<Option<u32>> {
    let mut def = vec![None; func.value_count() as usize];
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for &p in &block.params {
            def[p as usize] = Some(b);
        }
        for inst in &block.insts {
            if let Some(r) = inst.result {
                def[r as usize] = Some(b);
            }
        }
    }
    def
}

/// `dom[b][d]` = block `d` dominates block `b`.
fn dominators(func: &Function, n: usize) -> Vec<Vec<bool>> {
    let entry = func.entry() as usize;
    let mut dom = vec![vec![true; n]; n];
    for (i, slot) in dom[entry].iter_mut().enumerate() {
        *slot = i == entry;
    }
    loop {
        let mut changed = false;
        for b in 0..n {
            if b == entry {
                continue;
            }
            let preds = predecessors(func, b as u32);
            if preds.is_empty() {
                continue;
            }
            let mut next = vec![true; n];
            for (i, slot) in next.iter_mut().enumerate() {
                *slot = preds.iter().all(|&p| dom[p as usize][i]);
            }
            next[b] = true;
            if next != dom[b] {
                dom[b] = next;
                changed = true;
            }
        }
        if !changed {
            return dom;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Term, Type};

    /// A `while`-style loop with a dedicated preheader and an invariant `2 * 5`
    /// computed inside the header. LICM must move it to the preheader.
    #[test]
    fn hoists_an_invariant_computation_into_the_preheader() {
        let mut func = Function::new();
        let entry = func.entry();
        let pre;
        let header;
        {
            let mut b = Builder::new(&mut func);
            let three = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(3.0),
            );
            pre = b.block();
            header = b.block();
            let exit = b.block();
            b.term(
                entry,
                Term::Jump {
                    target: pre,
                    args: vec![],
                },
            );
            let two = b.emit(
                pre,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            let five = b.emit(
                pre,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(5.0),
            );
            b.term(
                pre,
                Term::Jump {
                    target: header,
                    args: vec![],
                },
            );
            let k = b.param(header, Type::Number);
            let inv = b.emit(
                header,
                Op::Mul,
                &[two, five],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let _ = b.emit(
                header,
                Op::Add,
                &[k, inv],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let one = b.emit(
                header,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            let k2 = b.emit(
                header,
                Op::Add,
                &[k, one],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let cond = b.emit(
                header,
                Op::Lt,
                &[k2, three],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: header,
                    then_args: vec![k2],
                    else_block: exit,
                    else_args: vec![k2],
                },
            );
            let r = b.param(exit, Type::Number);
            b.term(exit, Term::Return(Some(r)));
        }
        assert!(run(&mut func));
        assert!(
            func.block(pre).insts.iter().any(|i| i.op == Op::Mul),
            "the invariant multiply moved to the preheader"
        );
        assert!(
            !func.block(header).insts.iter().any(|i| i.op == Op::Mul),
            "and out of the loop header"
        );
    }

    /// A loop whose back edge's source is a SEPARATE block
    /// (`entry -> pre -> header -> body -> header`). `natural_loop` must include
    /// that source block, or the body is never treated as in-loop: the header's
    /// outside predecessors would then be `{pre, body}`, no unique preheader
    /// exists, and nothing hoists (the bug this test pins).
    #[test]
    fn hoists_from_a_non_self_loop() {
        let mut func = Function::new();
        let entry = func.entry();
        let pre;
        let header;
        let body;
        {
            let mut b = Builder::new(&mut func);
            let three = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(3.0),
            );
            pre = b.block();
            header = b.block();
            body = b.block();
            let exit = b.block();
            b.term(
                entry,
                Term::Jump {
                    target: pre,
                    args: vec![],
                },
            );
            let two = b.emit(
                pre,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            let five = b.emit(
                pre,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(5.0),
            );
            b.term(
                pre,
                Term::Jump {
                    target: header,
                    args: vec![],
                },
            );
            let k = b.param(header, Type::Number);
            let cond = b.emit(
                header,
                Op::Lt,
                &[k, three],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: body,
                    then_args: vec![k],
                    else_block: exit,
                    else_args: vec![k],
                },
            );
            let kb = b.param(body, Type::Number);
            let inv = b.emit(
                body,
                Op::Mul,
                &[two, five],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let k2 = b.emit(
                body,
                Op::Add,
                &[kb, inv],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                body,
                Term::Jump {
                    target: header,
                    args: vec![k2],
                },
            );
            let r = b.param(exit, Type::Number);
            b.term(exit, Term::Return(Some(r)));
        }
        assert!(run(&mut func));
        assert!(
            func.block(pre).insts.iter().any(|i| i.op == Op::Mul),
            "the invariant multiply moved to the preheader"
        );
        assert!(
            !func.block(body).insts.iter().any(|i| i.op == Op::Mul),
            "and out of the loop body"
        );
    }

    /// An entry-header loop (a `do`/`while` back edge to block 0) has no
    /// preheader, so nothing is hoisted.
    #[test]
    fn an_entry_header_loop_has_no_preheader() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let two = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            let five = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(5.0),
            );
            let inv = b.emit(
                entry,
                Op::Mul,
                &[two, five],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let exit = b.block();
            let cond = b.emit(
                entry,
                Op::Lt,
                &[inv, five],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                entry,
                Term::Branch {
                    cond,
                    then_block: entry,
                    then_args: vec![],
                    else_block: exit,
                    else_args: vec![],
                },
            );
            b.term(exit, Term::Return(None));
        }
        assert!(!run(&mut func));
    }
}
