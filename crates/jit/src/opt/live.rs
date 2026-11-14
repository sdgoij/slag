//! Live-value analysis over the optimizing IR (C2).
//!
//! A collection can happen inside any call, and the collector finds a compiled
//! frame's roots through the stack map Cranelift recorded for that safepoint.
//! Cranelift does not work out what belongs in one — the producer spills the
//! values and declares the entries — so this module answers the producer's
//! question: which values are live *after* a call, and which of those may be a
//! GC reference. The spill and the `append_user_stack_map_entry` call are the
//! lowering's job; deciding the set is this module's.

use super::ir::{BlockId, Function, Op, Term, Type, ValueId};

/// One safepoint and the values that must survive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointRoots {
    pub block: BlockId,
    /// The index of the call within its block's `insts`.
    pub index: usize,
    /// The heap-typed values live after the call, excluding the call's own
    /// result (which does not exist until the call returns).
    pub roots: Vec<ValueId>,
}

/// Whether a value of `ty` may be a GC reference at a safepoint.
///
/// The representations this tier keeps in a register without a box (`Int`,
/// `Float`, `Bool`) and the immediates (`Undefined`, `Null`, `Never`) cannot be.
/// `Number` can — it is either an int32 or a heap number — so it is included,
/// as is every unknown.
#[must_use]
pub fn may_be_heap(ty: Type) -> bool {
    matches!(
        ty,
        Type::Unknown | Type::Object | Type::String | Type::Number | Type::Hole
    )
}

/// Whether lowering this op emits a call, i.e. whether it is a safepoint.
///
/// This must stay in step with the ops whose lowering calls a helper in
/// `opt_lower`: a safepoint missing here is a frame whose roots the collector
/// cannot see. `Op::Call` is `call_slow` plus the leaf lanes, `Op::Intrinsic`
/// the intrinsic fast path with the same fallback.
fn is_safepoint(op: Op) -> bool {
    matches!(op, Op::Call | Op::Intrinsic)
}

fn successors(f: &Function, block: BlockId, out: &mut Vec<BlockId>) {
    out.clear();
    match f.block(block).term.as_ref() {
        Some(Term::Jump { target, .. }) => out.push(*target),
        Some(Term::Branch {
            then_block,
            else_block,
            ..
        }) => {
            out.push(*then_block);
            out.push(*else_block);
        }
        _ => {}
    }
}

fn terminator_uses(term: Option<&Term>, out: &mut Vec<ValueId>) {
    out.clear();
    match term {
        Some(Term::Jump { args, .. }) => out.extend_from_slice(args),
        Some(Term::Branch {
            cond,
            then_args,
            else_args,
            ..
        }) => {
            out.push(*cond);
            out.extend_from_slice(then_args);
            out.extend_from_slice(else_args);
        }
        Some(Term::Return(Some(value))) | Some(Term::Throw(value)) => out.push(*value),
        _ => {}
    }
}

/// The heap-typed values live across each call, in block and instruction order.
#[must_use]
pub fn safepoint_roots(f: &Function) -> Vec<SafepointRoots> {
    let values = f.value_count() as usize;
    let blocks = f.block_count();
    let empty = vec![false; values];
    let mut live_in: Vec<Vec<bool>> = vec![empty.clone(); blocks];
    let mut succ = Vec::new();
    let mut uses = Vec::new();

    // Backward data flow: `live_in[b]` is what is live on entry to `b`, so a
    // successor's entry set flows into this block's exit. Loops need the
    // fixpoint; straight-line bodies settle in one pass.
    loop {
        let mut changed = false;
        for b in 0..blocks as u32 {
            let mut live = empty.clone();
            successors(f, b, &mut succ);
            for &s in &succ {
                for (dst, src) in live.iter_mut().zip(&live_in[s as usize]) {
                    *dst |= *src;
                }
            }
            terminator_uses(f.block(b).term.as_ref(), &mut uses);
            for &value in &uses {
                live[value as usize] = true;
            }
            for inst in f.block(b).insts.iter().rev() {
                if let Some(result) = inst.result {
                    live[result as usize] = false;
                }
                for &arg in &inst.args {
                    live[arg as usize] = true;
                }
            }
            // A block parameter is defined by its incoming edge, so it is not
            // live on entry.
            for &param in &f.block(b).params {
                live[param as usize] = false;
            }
            if live != live_in[b as usize] {
                live_in[b as usize] = live;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut out = Vec::new();
    for b in 0..blocks as u32 {
        let block = f.block(b);
        if !block.insts.iter().any(|inst| is_safepoint(inst.op)) {
            continue;
        }
        // Replay the block backwards to get the set live after each
        // instruction.
        let mut live = empty.clone();
        successors(f, b, &mut succ);
        for &s in &succ {
            for (dst, src) in live.iter_mut().zip(&live_in[s as usize]) {
                *dst |= *src;
            }
        }
        terminator_uses(block.term.as_ref(), &mut uses);
        for &value in &uses {
            live[value as usize] = true;
        }
        let mut after: Vec<Vec<bool>> = vec![empty.clone(); block.insts.len()];
        for (index, inst) in block.insts.iter().enumerate().rev() {
            after[index] = live.clone();
            if let Some(result) = inst.result {
                live[result as usize] = false;
            }
            for &arg in &inst.args {
                live[arg as usize] = true;
            }
        }
        for (index, inst) in block.insts.iter().enumerate() {
            if !is_safepoint(inst.op) {
                continue;
            }
            let roots: Vec<ValueId> = after[index]
                .iter()
                .enumerate()
                .filter(|(value, live)| {
                    **live
                        && Some(*value as ValueId) != inst.result
                        && may_be_heap(f.value_type(*value as ValueId))
                })
                .map(|(value, _)| value as ValueId)
                .collect();
            if !roots.is_empty() {
                out.push(SafepointRoots {
                    block: b,
                    index,
                    roots,
                });
            }
        }
    }
    out
}
