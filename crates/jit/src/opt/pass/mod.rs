//! The pass pipeline over the optimizing SSA IR (`.notes/optimizing-tier-impl.md`
//! §2, `pass/`).
//!
//! The pipeline is folding, numeric effect narrowing, read-guard pruning, trial
//! inlining, CSE, LICM, then dead-code elimination. The order is chosen so a
//! single round reaches the fixpoint — a later pass never enables an earlier one
//! (folding creates constants, narrowing gives arithmetic pure effects, guard
//! pruning only removes, CSE reuses them, DCE removes the leftovers). Guard
//! pruning runs after narrowing because narrowing's numeric-slot proof *reads* a
//! guard (a slot stored only a guarded value is numeric); running it before would
//! let a store that feeds nothing type-sensitive lose the guard narrowing counted
//! on. Trial inlining is the one pass that can *create* work for an earlier one
//! (a cloned callee body may hold foldable arithmetic), so a body that inlines
//! may benefit from a second round — a missed fold in the inlined region is a
//! missed optimization, never a wrong program. Verified
//! empirically (`.notes/pass-pipeline-fixpoint.md`): iterating the pipeline to a
//! fixpoint performs no additional work on any sampled body, so the single round
//! is kept. The caller re-verifies the graph after the pipeline, because a pass
//! bug must be a refusal (the per-step path), never a wrong program.
//!
//! `mem2reg` runs on a callee's lifted IR inside the trial-inline resolver
//! (I5c-2c-iii-b) — and, since `.notes/opt-per-op-overhead.md` §8, also on the
//! TOP-LEVEL body when its frame slots are unobservable and it holds no guard,
//! promoting only non-heap slots (a frame slot is a GC root). `SLAG_MEM2REG=0`
//! disables the top-level promotion.

use std::collections::HashSet;

use crate::opt::ir::{BlockId, Function, Heap, Imm, Op, Term, Type, ValueId};

pub mod cse;
pub mod dce;
pub mod fold;
pub mod guard;
pub mod inline;
pub mod licm;
pub mod mem2reg;
pub mod narrow;

/// Whether the read-guard pruning pass runs. On by default; `SLAG_GUARD=0`
/// disables it — the same-binary seam for measuring the pass's marginal effect
/// (one executable, so no code-layout confound).
fn guard_enabled() -> bool {
    std::env::var("SLAG_GUARD")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Whether the top-level frame-slot promotion runs (`SLAG_MEM2REG=0` disables
/// it, mirroring `SLAG_GUARD`).
fn mem2reg_enabled() -> bool {
    std::env::var("SLAG_MEM2REG")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Run the pass pipeline over `func`. Returns whether the IR changed. `sites`
/// is the caller's monomorphic call-site map and `resolve` yields a callee's
/// lifted IR (I5c-2c); an empty map makes the inline pass a no-op.
pub fn run(
    func: &mut Function,
    sites: &[Option<inline::InlineSite>],
    resolve: &mut dyn FnMut(u64) -> Option<inline::Callee>,
) -> bool {
    let folded = fold::run(func);
    let narrowed = narrow::run(func);
    let promoted = mem2reg_enabled() && promote_frame_slots(func);
    let guarded = guard_enabled() && guard::run(func);
    let inlined = inline::run(func, sites, resolve);
    let cse = cse::run(func);
    let licm = licm::run(func);
    let dropped = dce::run(func);
    let retyped = normalize_param_types(func);
    folded || narrowed || promoted || guarded || inlined || cse || licm || dropped || retyped
}

/// Type every block parameter as the join of its incoming edge arguments' types.
///
/// The lift types a merge block parameter from context and often only `Unknown`
/// (e.g. a ternary's join); the lowering keys the register representation on the
/// type, so a parameter whose declared type disagrees with its arguments would
/// lower to mismatched representations. The join keeps an edge and its target
/// parameter in the same register class.
fn normalize_param_types(func: &mut Function) -> bool {
    let n = func.block_count() as u32;
    let mut retype: Vec<(ValueId, Type)> = Vec::new();
    for b in 0..n {
        let params = func.block(b).params.clone();
        if params.is_empty() {
            continue;
        }
        let mut joined = vec![None::<Type>; params.len()];
        for p in 0..n {
            for (to, args) in edge_args(func, p) {
                if to != b {
                    continue;
                }
                for (i, a) in args.iter().enumerate() {
                    if i >= joined.len() {
                        break;
                    }
                    let t = func.value_type(*a);
                    joined[i] = Some(match joined[i] {
                        Some(j) => j.join(t),
                        None => t,
                    });
                }
            }
        }
        for (i, j) in joined.into_iter().enumerate() {
            if let Some(j) = j
                && func.value_type(params[i]) != j
            {
                retype.push((params[i], j));
            }
        }
    }
    let changed = !retype.is_empty();
    for (v, t) in retype {
        func.set_value_type(v, t);
    }
    changed
}

/// The `(target, args)` edges leaving `from` (empty for a non-branching
/// terminator).
fn edge_args(func: &Function, from: BlockId) -> Vec<(BlockId, Vec<ValueId>)> {
    match &func.block(from).term {
        Some(Term::Jump { target, args }) => vec![(*target, args.clone())],
        Some(Term::Branch {
            then_block,
            then_args,
            else_block,
            else_args,
            ..
        }) => vec![
            (*then_block, then_args.clone()),
            (*else_block, else_args.clone()),
        ],
        _ => Vec::new(),
    }
}

/// Promote the top-level body's frame slots to SSA when nothing can observe them
/// (`.notes/opt-per-op-overhead.md` §8). A promoted slot must also never hold a
/// heap value: a frame slot is a GC root, so promoting a heap value into an
/// untraced register would unroot it.
fn promote_frame_slots(func: &mut Function) -> bool {
    if !slots_unobserved(func) {
        return false;
    }
    let allowed = non_heap_slots(func);
    if allowed.is_empty() {
        return false;
    }
    mem2reg::run_restricted(func, &allowed)
}

/// Whether no instruction other than a `FrameLoad`/`FrameStore` reads or writes
/// frame slots, and the body holds no speculation guard — a guard deopts into
/// the interpreter, which reads the frame.
fn slots_unobserved(func: &Function) -> bool {
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            match inst.op {
                Op::FrameLoad | Op::FrameStore => continue,
                Op::Check | Op::GuardType | Op::GuardCallee => return false,
                _ => {}
            }
            if inst.effects.may_read(Heap::Slots) || inst.effects.may_write(Heap::Slots) {
                return false;
            }
        }
    }
    true
}

/// The frame slots whose every stored value is provably non-heap (a NaN-boxed
/// immediate: `Number`/`Int`/`Bool`/`Undefined`/`Null`). A heap-typed or
/// `Unknown` store disqualifies the slot.
fn non_heap_slots(func: &Function) -> HashSet<u32> {
    let mut stored: Vec<u32> = Vec::new();
    let mut heap: HashSet<u32> = HashSet::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.op != Op::FrameStore {
                continue;
            }
            let Imm::Slot(slot) = inst.imm else { continue };
            stored.push(slot);
            let heap_value = inst
                .args
                .first()
                .is_some_and(|&v| type_is_heap(func.value_type(v)));
            if heap_value {
                heap.insert(slot);
            }
        }
    }
    stored.retain(|s| !heap.contains(s));
    stored.into_iter().collect()
}

/// Whether `ty` can be a heap pointer (the GC must trace the value).
fn type_is_heap(ty: Type) -> bool {
    matches!(ty, Type::String | Type::Object | Type::Unknown)
}

/// Run the pipeline without trial inlining over a single body. The resolver
/// applies it to a callee's lifted IR, so the callee is optimized like the
/// caller's body — in particular a guard that pruning removes (its value feeds a
/// call, not arithmetic) does not spuriously refuse the splice.
pub fn optimize(func: &mut Function) {
    fold::run(func);
    narrow::run(func);
    if guard_enabled() {
        guard::run(func);
    }
    cse::run(func);
    licm::run(func);
    dce::run(func);
    normalize_param_types(func);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Imm, Op, Term, Type};

    #[test]
    fn the_pipeline_reaches_its_fixpoint_in_one_round() {
        // `1 + 2` folds, so round 1 changes the IR; a second round must find
        // nothing — the ordering invariant the module doc claims (no later pass
        // enables an earlier one). A body where a second round *did* change
        // something would refute the claim.
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
            let two = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            let sum = b.emit(
                entry,
                Op::Add,
                &[one, two],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(
            run(&mut func, &[], &mut |_: u64| None),
            "the first round folds `1 + 2`"
        );
        assert!(
            !run(&mut func, &[], &mut |_: u64| None),
            "the second round finds nothing"
        );
    }
}
