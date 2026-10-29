//! Trial inlining over the SSA IR (`.notes/optimizing-tier-impl.md` I5c-2c).
//!
//! At a monomorphic call site, replace the call with the callee's IR — the same
//! exact `lift`, cloned into the caller. This cut handles only a call that is its
//! block's **last instruction** (so the call's result is used only by the
//! terminator or by blocks it dominates, and no block split is needed) and a
//! callee whose slots are all **inputs** (params and `this`, bound to the call's
//! operands) — a callee that writes a `var` still needs mem2reg (I5c-2c-iii-b)
//! and recursion is I5c-2c-iv.
//!
//! The site map and the resolver are injected, so the pass is agent-free and
//! testable. The pipeline re-verifies after the pass and bails to the per-step
//! path on a violation, so a splice bug is a refusal, never a wrong program.

use std::rc::Rc;

use crate::opt::ir::{BlockId, Effects, Function, Imm, Inst, Op, Term, Type, ValueId};

/// The creation-site splices this process has performed (the opt-tier tests
/// assert the static route fired, since no feedback-derived site exists for a
/// loop-bodied caller).
pub(crate) static CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the splice assumes about the callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineTarget {
    /// The callee the site was observed calling: `Op::CalleeEq` compares the
    /// loaded callee against `callee_box` before the region runs, and a mismatch
    /// takes the ordinary call.
    Guarded { callee_id: u64, callee_box: u64 },
    /// The closure a hoisted declaration in the caller writes into the callee
    /// slot at body entry, which nothing else writes
    /// (`CompiledBody::declared_calls`): the slot at the call is that closure by
    /// construction, so no comparison is needed.
    Declared { init_step: usize },
    /// The closure a `Step::CreateFunction` in the caller creates, which the
    /// call's callee operand provably is (the `NewClosure` itself, or a frame
    /// slot stored once in the entry block with it): no comparison is needed.
    Created { create_step: usize },
}

/// A call site's inline target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSite {
    pub target: InlineTarget,
}

/// A resolved callee: its lifted IR (its `var` slots promoted by
/// `pass/mem2reg.rs`) and the input frame layout (`ScopeInfo`) the splice binds.
#[derive(Clone)]
pub struct Callee {
    pub ir: Rc<Function>,
    /// The parameter count; params occupy slots `0..arity` in source order.
    pub arity: usize,
    /// The frame slot holding the call's receiver, if the body reads `this`.
    pub this_slot: Option<usize>,
    /// The callee's own monomorphic call sites, indexed by its step index, so the
    /// splice can recurse into the callee's calls (I5c-2c-iv).
    pub sites: Rc<Vec<Option<InlineSite>>>,
    /// The callee body's step count, the inline budget's unit.
    pub steps: usize,
}

/// The monomorphic call sites of `body`, indexed by step: a site whose
/// `CallSite` is specialized and whose first callee had a compiled body. Reads
/// only the feedback record (a plain u64 read — no agent), so it is called from
/// `JitEngine::compile` before the resolver exists.
#[must_use]
pub fn sites_from_feedback(body: &runtime::ir::CompiledBody) -> Vec<Option<InlineSite>> {
    let mut sites = vec![None; body.steps.len()];
    let Ok(feedback) = body.feedback.try_borrow() else {
        return sites;
    };
    let Some(store) = feedback.as_ref() else {
        return sites;
    };
    for (ip, site) in sites.iter_mut().enumerate() {
        let Some(runtime::feedback::SiteRecord::Call(call)) = store.site(ip) else {
            continue;
        };
        if std::env::var("SLAG_INLINE_TRACE").is_ok() {
            eprintln!(
                "inline-trace: step {ip} = {:?}, hits {}, first id {}",
                call.state(),
                call.hits(),
                call.first_callee_id()
            );
        }
        if call.state() == runtime::feedback::IcState::Specialized && call.first_callee_id() != 0 {
            *site = Some(InlineSite {
                target: InlineTarget::Guarded {
                    callee_id: call.first_callee_id(),
                    callee_box: call.first_callee_box(),
                },
            });
        }
    }
    sites
}

/// The call sites whose callee is provably a declaration in the same body
/// (`CompiledBody::declared_calls`), indexed by step.
#[must_use]
pub fn sites_from_declarations(body: &runtime::ir::CompiledBody) -> Vec<Option<InlineSite>> {
    let mut sites = vec![None; body.steps.len()];
    for &(call, init) in &body.declared_calls {
        if let Some(slot) = sites.get_mut(call as usize) {
            *slot = Some(InlineSite {
                target: InlineTarget::Declared {
                    init_step: init as usize,
                },
            });
        }
    }
    sites
}

/// The call sites whose callee operand is provably a closure `func` creates,
/// with the value to compare that operand against. Two shapes:
///
/// - the callee operand *is* the `NewClosure` value, directly or through a frame
///   slot stored once in the entry block with it: no comparison is needed;
/// - the operand is a member read of a literal object the body builds, whose
///   field holds such a closure: the comparison against that literal value is
///   what makes the splice sound (the field may have been written, and then the
///   ordinary call runs).
///
/// This is the static route to the splice, and the only one available to a loop
/// body: such a body is compiled on its first consult, so it never runs an
/// interpreted step and `sites_from_feedback` is always empty for it
/// (`.notes/perf.md`, 2026-10-28).
#[must_use]
pub fn sites_from_creations(
    func: &Function,
    body: &runtime::ir::CompiledBody,
    steps: usize,
) -> (Vec<Option<InlineSite>>, Vec<Option<ValueId>>) {
    let mut sites = vec![None; steps];
    let mut expected = vec![None; steps];
    let Some(slots) = single_closure_slots(func) else {
        return (sites, expected);
    };
    let objects = super::scalar::literal_objects(func, body);
    let roots: std::collections::HashSet<ValueId> = objects.keys().copied().collect();
    let aliases = super::scalar::slot_aliases(func, &roots);
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::Call, Imm::Int(step)) = (inst.op, &inst.imm) else {
                continue;
            };
            if inst.args.len() < 2 {
                continue;
            }
            let Some(site) = sites.get_mut(*step as usize) else {
                continue;
            };
            if site.is_some() {
                continue;
            }
            let Some((expected_value, create_step)) =
                closure_of_callee(func, inst.args[1], &slots, &objects, &aliases)
            else {
                continue;
            };
            *site = Some(InlineSite {
                target: InlineTarget::Created { create_step },
            });
            expected[*step as usize] = Some(expected_value);
        }
    }
    (sites, expected)
}

/// The closure a call's callee operand is, and the `CreateFunction` step that
/// creates it: the `NewClosure` value itself, a slot load of one, or a member
/// read of a literal object's field that holds one.
fn closure_of_callee(
    func: &Function,
    callee: ValueId,
    slots: &std::collections::HashMap<u32, ValueId>,
    objects: &std::collections::HashMap<ValueId, Vec<(u32, ValueId)>>,
    aliases: &std::collections::HashMap<ValueId, ValueId>,
) -> Option<(ValueId, usize)> {
    let (expected, closure) = match defining_inst(func, callee) {
        Some(inst) if inst.op == Op::NewClosure => (callee, callee),
        Some(inst) if inst.op == Op::FrameLoad => match inst.imm {
            // The slot holds the closure on every path, so the operand needs no
            // comparison; the expected value is the operand itself.
            Imm::Slot(slot) => (callee, *slots.get(&slot)?),
            _ => return None,
        },
        Some(inst) if matches!(inst.op, Op::MemberGuard | Op::MemberCellLoad) => {
            // The cell's value is the receiver's field, and the literal's own
            // value for that atom is the closure the body created. The caller
            // compares the operand against it, so a written field takes the
            // ordinary call.
            let load = if inst.op == Op::MemberGuard {
                *inst.args.get(1)?
            } else {
                callee
            };
            let load = defining_inst(func, load)?;
            if load.op != Op::MemberCellLoad {
                return None;
            }
            let Imm::Atom(atom) = load.imm else {
                return None;
            };
            let receiver = *load.args.first()?;
            let (object, false) =
                super::scalar::root_of_any(receiver, objects, &Default::default(), aliases)?
            else {
                return None;
            };
            let &(_, value) = objects.get(&object)?.iter().find(|(key, _)| *key == atom)?;
            (value, value)
        }
        _ => return None,
    };
    let step = match defining_inst(func, closure)?.imm {
        Imm::Int(step) if defines_closure(func, closure) => step as usize,
        _ => return None,
    };
    Some((expected, step))
}

/// The frame slots that hold a closure on every path: stored exactly once, in the
/// entry block (which dominates every reached block), with a `NewClosure`. `None`
/// when the body writes a slot without naming it (the fused declaration and
/// loop-binding steps), which makes no slot provable.
fn single_closure_slots(func: &Function) -> Option<std::collections::HashMap<u32, ValueId>> {
    let mut slots: std::collections::HashMap<u32, Option<ValueId>> =
        std::collections::HashMap::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if matches!(
                inst.op,
                Op::FunctionDecl | Op::ForOfBindLocal | Op::ForOfNextBindLocal
            ) {
                return None;
            }
            let (Op::FrameStore, Imm::Slot(slot)) = (inst.op, &inst.imm) else {
                continue;
            };
            let candidate = inst
                .args
                .first()
                .copied()
                .filter(|&value| b == func.entry() && defines_closure(func, value));
            slots
                .entry(*slot)
                .and_modify(|value| *value = None)
                .or_insert(candidate);
        }
    }
    Some(
        slots
            .into_iter()
            .filter_map(|(slot, value)| value.map(|value| (slot, value)))
            .collect(),
    )
}

/// The frame slots that hold a closure on every path: stored exactly once, in the
/// entry block (which dominates every reached block), with a `NewClosure`. `None`
/// when the body writes a slot without naming it (the fused declaration and
/// loop-binding steps), which makes no slot provable.
/// Whether `value` is produced by a `NewClosure`.
fn defines_closure(func: &Function, value: ValueId) -> bool {
    defining_inst(func, value).is_some_and(|inst| inst.op == Op::NewClosure)
}

/// The instruction that produced `value`.
fn defining_inst(func: &Function, value: ValueId) -> Option<&Inst> {
    for b in 0..func.block_count() as u32 {
        if let Some(inst) = func
            .block(b)
            .insts
            .iter()
            .find(|inst| inst.result == Some(value))
        {
            return Some(inst);
        }
    }
    None
}

/// Every spliceable site of `body`: the ones a declaration proves, plus — when
/// `with_feedback` — the feedback-recorded callees. A declared site wins when
/// both apply, because it needs no comparison.
#[must_use]
pub fn sites_for(body: &runtime::ir::CompiledBody, with_feedback: bool) -> Vec<Option<InlineSite>> {
    let mut sites = if with_feedback {
        sites_from_feedback(body)
    } else {
        vec![None; body.steps.len()]
    };
    for (slot, declared) in sites.iter_mut().zip(sites_from_declarations(body)) {
        if declared.is_some() {
            *slot = declared;
        }
    }
    sites
}

/// The default total step budget for one top-level splice (its recursive
/// expansion): a bound on code growth and on self-recursion. `SLAG_INLINE_BUDGET`
/// overrides it.
const DEFAULT_INLINE_BUDGET: usize = 256;

fn inline_budget() -> usize {
    std::env::var("SLAG_INLINE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INLINE_BUDGET)
}

/// Whether the static creation sites are derived (`SLAG_INLINE_CREATED=0`
/// disables them) — the same-binary seam for measuring this route alone.
fn created_sites_enabled() -> bool {
    std::env::var("SLAG_INLINE_CREATED")
        .map(|value| value != "0")
        .unwrap_or(true)
}

/// Inline every spliceable monomorphic call site, recursing into a callee's own
/// sites under a step budget. `sites` is indexed by step (the `Op::Call`'s
/// `Imm::Int`); `resolve` yields the callee. Returns whether the IR changed.
pub fn run(
    func: &mut Function,
    body: Option<&runtime::ir::CompiledBody>,
    sites: &[Option<InlineSite>],
    resolve: &mut dyn FnMut(&InlineTarget) -> Option<Callee>,
) -> bool {
    let mut changed = false;
    // The static sites (a callee the body itself creates) are derived from the
    // IR, so they do not depend on feedback. They are merged only here: a
    // spliced-in callee's IR names the callee's own step list, so its sites are
    // the ones the resolver computed for it.
    let (created, expected) = match (created_sites_enabled(), body) {
        (true, Some(body)) => sites_from_creations(func, body, sites.len()),
        _ => (vec![None; sites.len()], vec![None; sites.len()]),
    };
    let sites: Vec<Option<InlineSite>> = sites
        .iter()
        .zip(created)
        .map(|(site, created)| site.or(created))
        .collect();
    let blocks = func.block_count() as u32;
    for b in 0..blocks {
        // The budget is per top-level site; the recursion shares it, so a
        // self-recursive callee stops expanding and leaves a real call.
        let mut budget = inline_budget();
        if try_inline(func, b, &sites, &expected, resolve, &mut budget) {
            changed = true;
        }
    }
    changed
}

/// Splice the call at the end of block `b` if it has a spliceable site, then
/// recurse into the callee's own sites. `root` is the outermost call's operands
/// and step (`None` at the top level): every guard in the expanding region is
/// rewritten to it, so a deopt resumes the outer call as a call and discards the
/// region rather than resuming at a spliced-in step.
fn try_inline(
    func: &mut Function,
    b: BlockId,
    sites: &[Option<InlineSite>],
    expected: &[Option<ValueId>],
    resolve: &mut dyn FnMut(&InlineTarget) -> Option<Callee>,
    budget: &mut usize,
) -> bool {
    // The splice is not restricted to a call that ends its block: a call whose
    // result feeds a later operation (`s += f(x)`) is the common shape, and the
    // remaining instructions move to the continuation below. The last call with
    // a refused site is tried first, so a block with several calls splices the
    // one nearest its terminator and leaves the rest alone.
    let (at, args, result, ty, step, site) = {
        let insts = &func.block(b).insts;
        let mut found = None;
        for (at, inst) in insts.iter().enumerate().rev() {
            if inst.op != Op::Call {
                continue;
            }
            let Imm::Int(step) = inst.imm else {
                continue;
            };
            let Some(&Some(site)) = sites.get(step as usize) else {
                continue;
            };
            let Some(result) = inst.result else {
                continue;
            };
            found = Some((at, inst.args.clone(), result, inst.ty, step, site));
            break;
        }
        let Some(found) = found else {
            return false;
        };
        found
    };
    let Some(callee) = resolve(&site.target) else {
        return false;
    };
    // Every input slot must have a call operand to bind to, the callee must be
    // spliceable, and it must fit the remaining budget.
    if args.len() < 2 + callee.arity || !spliceable(&callee) || callee.steps > *budget {
        return false;
    }
    *budget -= callee.steps;

    // The call's result is now produced by the continuation, which every
    // successor of `b` passes through (its only edge is `b -> callee -> cont`).
    // The instructions after the call move to the continuation: they read values
    // defined in `b` before the call, so `b` still dominates them.
    let tail = {
        let insts = &mut func.block_mut(b).insts;
        let tail = insts.split_off(at + 1);
        insts.pop();
        tail
    };
    let old_term = func.block_mut(b).term.take();
    let cont = func.push_block();
    let param = func.push_value(ty);
    func.block_mut(cont).params.push(param);
    func.block_mut(cont).insts = tail;
    func.block_mut(cont).term = old_term;

    // Clone the callee's blocks with fresh ids, binding its input slots to the
    // call's receiver (`args[0]`) and arguments (`args[2 + slot]`).
    let block_map: Vec<BlockId> = (0..callee.ir.block_count())
        .map(|_| func.push_block())
        .collect();
    let mut value_map: Vec<Option<ValueId>> = vec![None; callee.ir.value_count() as usize];
    for k in 0..callee.ir.block_count() as u32 {
        let src = callee.ir.block(k);
        let dst = block_map[k as usize];
        for &p in &src.params {
            let nv = func.push_value(callee.ir.value_type(p));
            func.block_mut(dst).params.push(nv);
            value_map[p as usize] = Some(nv);
        }
    }
    for k in 0..callee.ir.block_count() as u32 {
        let src = callee.ir.block(k);
        let dst = block_map[k as usize];
        let mut insts = Vec::with_capacity(src.insts.len());
        for inst in &src.insts {
            // An input-slot load is the bound operand, not a frame read.
            if inst.op == Op::FrameLoad {
                let Imm::Slot(slot) = inst.imm else {
                    return false;
                };
                let bound = if Some(slot as usize) == callee.this_slot {
                    args[0]
                } else {
                    args[2 + slot as usize]
                };
                value_map[inst.result.expect("a load has a result") as usize] = Some(bound);
                continue;
            }
            let result = inst.result.map(|r| {
                let nv = func.push_value(inst.ty);
                value_map[r as usize] = Some(nv);
                nv
            });
            let args = inst
                .args
                .iter()
                .map(|&a| value_map[a as usize].expect("callee value cloned before use"))
                .collect();
            insts.push(Inst {
                op: inst.op,
                args,
                result,
                ty: inst.ty,
                effects: inst.effects,
                imm: inst.imm.clone(),
            });
        }
        func.block_mut(dst).insts = insts;
        let map = |v: ValueId| value_map[v as usize].expect("callee value cloned");
        let term = match src.term.as_ref().expect("spliceable has a terminator") {
            Term::Return(Some(v)) => Term::Jump {
                target: cont,
                args: vec![map(*v)],
            },
            Term::Jump { target, args } => Term::Jump {
                target: block_map[*target as usize],
                args: args.iter().map(|&a| map(a)).collect(),
            },
            Term::Branch {
                cond,
                then_block,
                then_args,
                else_block,
                else_args,
            } => Term::Branch {
                cond: map(*cond),
                then_block: block_map[*then_block as usize],
                then_args: then_args.iter().map(|&a| map(a)).collect(),
                else_block: block_map[*else_block as usize],
                else_args: else_args.iter().map(|&a| map(a)).collect(),
            },
            // `spliceable` refused every other terminator.
            _ => return false,
        };
        func.block_mut(dst).term = Some(term);
    }

    // Enter the callee when the site's assumption holds, and take the ordinary
    // call otherwise. A deopt is *not* sound here: it would resume the
    // interpreter at this call step, whose operand stack also holds every value
    // the enclosing expression evaluated before the call (`fib(n-1)` in
    // `fib(n-1) + fib(n-2)`), and the splice has no way to mirror that state. A
    // fallback call keeps the region deopt-free as well, so the spliced body
    // stays eligible for the certified-callee lane.
    match site.target {
        InlineTarget::Guarded { callee_box, .. } => {
            let expected = func.push_value(Type::Object);
            func.block_mut(b).insts.push(Inst {
                op: Op::Const,
                args: Vec::new(),
                result: Some(expected),
                ty: Type::Object,
                effects: Effects::pure(),
                imm: Imm::U64(callee_box),
            });
            let ok = func.push_value(Type::Bool);
            func.block_mut(b).insts.push(Inst {
                op: Op::CalleeEq,
                args: vec![args[1], expected],
                result: Some(ok),
                ty: Type::Bool,
                effects: Effects::pure(),
                imm: Imm::None,
            });
            let fallback = func.push_block();
            let called = func.push_value(ty);
            func.block_mut(fallback).insts.push(Inst {
                op: Op::Call,
                args: args.clone(),
                result: Some(called),
                ty,
                effects: Effects::call(),
                imm: Imm::Int(step),
            });
            func.block_mut(fallback).term = Some(Term::Jump {
                target: cont,
                args: vec![called],
            });
            func.block_mut(b).term = Some(Term::Branch {
                cond: ok,
                then_block: block_map[0],
                then_args: Vec::new(),
                else_block: fallback,
                else_args: Vec::new(),
            });
        }
        // The declaration owns the slot: nothing writes it after the hoisted
        // init, so the callee at this call IS the closure whose body was
        // spliced and no check can fail. A creation site's operand is that
        // closure too when it is the `NewClosure` value (or its slot load); a
        // member read of the same field may have been written since, so it is
        // compared against the literal value and falls back to the call.
        InlineTarget::Declared { .. } => {
            func.block_mut(b).term = Some(Term::Jump {
                target: block_map[0],
                args: Vec::new(),
            });
        }
        InlineTarget::Created { .. } => {
            CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match expected.get(step as usize).copied().flatten() {
                Some(value) if value != args[1] => {
                    emit_callee_guard(func, b, &args, ty, step, cont, block_map[0], value);
                }
                _ => {
                    func.block_mut(b).term = Some(Term::Jump {
                        target: block_map[0],
                        args: Vec::new(),
                    });
                }
            }
        }
    }

    replace_uses(func, result, param);

    // I5c-2c-iv: recurse into the callee's own calls, under the shared budget.
    // The spliced-in IR names the callee's step list, so only the sites the
    // resolver computed for it apply, and no creation site is derived here.
    for &dst in &block_map {
        try_inline(func, dst, callee.sites.as_slice(), &[], resolve, budget);
    }
    true
}

/// Branch to the spliced region when `args[1]` is `expected`, else run the call.
#[allow(clippy::too_many_arguments)]
fn emit_callee_guard(
    func: &mut Function,
    b: BlockId,
    args: &[ValueId],
    ty: Type,
    step: i32,
    cont: BlockId,
    entry: BlockId,
    expected: ValueId,
) {
    let ok = func.push_value(Type::Bool);
    func.block_mut(b).insts.push(Inst {
        op: Op::CalleeEq,
        args: vec![args[1], expected],
        result: Some(ok),
        ty: Type::Bool,
        effects: Effects::pure(),
        imm: Imm::None,
    });
    let fallback = func.push_block();
    let called = func.push_value(ty);
    func.block_mut(fallback).insts.push(Inst {
        op: Op::Call,
        args: args.to_vec(),
        result: Some(called),
        ty,
        effects: Effects::call(),
        imm: Imm::Int(step),
    });
    func.block_mut(fallback).term = Some(Term::Jump {
        target: cont,
        args: vec![called],
    });
    func.block_mut(b).term = Some(Term::Branch {
        cond: ok,
        then_block: entry,
        then_args: Vec::new(),
        else_block: fallback,
        else_args: Vec::new(),
    });
}

/// Whether this cut can splice `callee`: every remaining slot access is a load
/// of an **input** slot (`< arity` or the `this` slot), and no guard. The
/// resolver promotes the callee's `var` slots to SSA first (`pass/mem2reg.rs`,
/// I5c-2c-iii-b), so a `FrameStore`, a `TdzCheck`, or a load of any other slot
/// means promotion could not lift it — refused.
///
/// A guard (`Op::GuardType`/`Op::Check`) is refused too (the design's guard-free
/// callee, I5c-1): its deopt resumes the `imm` step, which for a spliced callee
/// is the *callee's* step, and the read a guard follows may have fired a getter —
/// re-running the outer call to discard the region would fire it twice. An
/// `Op::Call` is allowed: the splice recurses into it (I5c-2c-iv), and a call it
/// declines simply stays a call.
fn spliceable(callee: &Callee) -> bool {
    let mut returns_a_value = false;
    for k in 0..callee.ir.block_count() as u32 {
        for inst in &callee.ir.block(k).insts {
            match inst.op {
                Op::FrameStore | Op::TdzCheck | Op::GuardType | Op::Check => return false,
                Op::FrameLoad => {
                    let Imm::Slot(slot) = inst.imm else {
                        return false;
                    };
                    let slot = slot as usize;
                    if slot >= callee.arity && Some(slot) != callee.this_slot {
                        return false;
                    }
                }
                _ => {}
            }
        }
        match callee.ir.block(k).term.as_ref() {
            Some(Term::Return(Some(_))) => returns_a_value = true,
            Some(Term::Jump { .. } | Term::Branch { .. }) => {}
            // `Return(None)` (undefined), a throw, and an unreachable point are
            // not handled by this cut.
            _ => return false,
        }
    }
    returns_a_value
}

/// Redirect every use of `from` to `to` (the call's result to the
/// continuation's parameter).
fn replace_uses(func: &mut Function, from: ValueId, to: ValueId) {
    for b in 0..func.block_count() as u32 {
        for inst in &mut func.block_mut(b).insts {
            for a in &mut inst.args {
                if *a == from {
                    *a = to;
                }
            }
        }
        match &mut func.block_mut(b).term {
            Some(Term::Jump { args, .. }) => {
                for a in args {
                    if *a == from {
                        *a = to;
                    }
                }
            }
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                if *cond == from {
                    *cond = to;
                }
                for a in then_args.iter_mut().chain(else_args) {
                    if *a == from {
                        *a = to;
                    }
                }
            }
            Some(Term::Return(Some(v)) | Term::Throw(v)) if *v == from => *v = to,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::Heap;
    use crate::opt::verify::verify;

    /// A callee returning a constant.
    fn constant_callee() -> Callee {
        let mut f = Function::new();
        let e = f.entry();
        let mut b = Builder::new(&mut f);
        let c = b.emit(
            e,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(42.0),
        );
        b.term(e, Term::Return(Some(c)));
        Callee {
            ir: Rc::new(f),
            arity: 0,
            this_slot: None,
            sites: Rc::new(Vec::new()),
            steps: 1,
        }
    }

    /// `function (x) { return x + 1; }` — one param, read only.
    fn param_callee() -> Callee {
        let mut f = Function::new();
        let e = f.entry();
        let mut b = Builder::new(&mut f);
        let x = b.emit(
            e,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(Heap::Slots),
            Imm::Slot(0),
        );
        let one = b.emit(
            e,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(1.0),
        );
        let sum = b.emit(
            e,
            Op::Add,
            &[x, one],
            Type::Number,
            Effects::call(),
            Imm::None,
        );
        b.term(e, Term::Return(Some(sum)));
        Callee {
            ir: Rc::new(f),
            arity: 1,
            this_slot: None,
            sites: Rc::new(Vec::new()),
            steps: 1,
        }
    }

    /// `call(this, callee, a1); return result` — one argument.
    fn caller_ir() -> Function {
        let mut f = Function::new();
        let e = f.entry();
        let mut b = Builder::new(&mut f);
        let this = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(0x1111),
        );
        let callee = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(0xabc),
        );
        let a1 = b.emit(
            e,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(10.0),
        );
        let r = b.emit(
            e,
            Op::Call,
            &[this, callee, a1],
            Type::Unknown,
            Effects::call(),
            Imm::Int(0),
        );
        b.term(e, Term::Return(Some(r)));
        f
    }

    fn ops(func: &Function) -> Vec<Op> {
        (0..func.block_count() as u32)
            .flat_map(|b| func.block(b).insts.iter().map(|i| i.op).collect::<Vec<_>>())
            .collect()
    }

    /// The number of `Op::Call`s left in the IR: every splice leaves exactly one
    /// (the mismatch fallback), so a fully spliced site still contains a call.
    fn call_count(func: &Function) -> usize {
        ops(func).iter().filter(|o| **o == Op::Call).count()
    }

    fn site() -> Vec<Option<InlineSite>> {
        vec![Some(InlineSite {
            target: InlineTarget::Guarded {
                callee_id: 7,
                callee_box: 0xabc,
            },
        })]
    }

    /// Adapt a callee-id resolver to the site-target signature the pass takes.
    fn by_id(f: impl Fn(u64) -> Option<Callee>) -> impl FnMut(&InlineTarget) -> Option<Callee> {
        move |target| match target {
            InlineTarget::Guarded { callee_id, .. } => f(*callee_id),
            InlineTarget::Declared { .. } | InlineTarget::Created { .. } => None,
        }
    }

    #[test]
    fn splices_a_parameter_reading_callee() {
        // The callee's param load becomes the call's argument, so
        // `x + 1` with `x = 10` splices to `11`.
        let mut func = caller_ir();
        let mut resolve = by_id(|id| (id == 7).then(param_callee));
        assert!(
            run(&mut func, None, &site(), &mut resolve),
            "the site inlined"
        );
        assert_eq!(call_count(&func), 1, "only the mismatch fallback remains");
        assert!(!ops(&func).contains(&Op::FrameLoad), "the param is bound");
        assert!(ops(&func).contains(&Op::CalleeEq), "the callee is compared");
        assert_eq!(verify(&func), Ok(()), "the spliced IR verifies");
        // The bound value flows: the callee's `Add` now reads the caller's
        // argument (`Const(Float(10.0))`).
        let mut ten = None;
        let mut add_args = None;
        for b in 0..func.block_count() as u32 {
            for inst in &func.block(b).insts {
                if inst.op == Op::Const && inst.imm == Imm::Float(10.0) {
                    ten = inst.result;
                }
                if inst.op == Op::Add {
                    add_args = Some(inst.args.clone());
                }
            }
        }
        assert_eq!(
            add_args.expect("the callee's Add")[0],
            ten.expect("the caller's argument"),
            "the param load became the call's argument"
        );
    }

    #[test]
    fn splices_a_constant_callee() {
        let mut func = caller_ir();
        let mut resolve = by_id(|id| (id == 7).then(constant_callee));
        assert!(run(&mut func, None, &site(), &mut resolve));
        assert_eq!(call_count(&func), 1);
        assert_eq!(verify(&func), Ok(()));
    }

    /// `r = call(this, callee, a1); return r + 1` — the call is not its
    /// block's terminator.
    fn caller_ir_mid_block() -> Function {
        let mut f = Function::new();
        let e = f.entry();
        let mut b = Builder::new(&mut f);
        let this = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(0x1111),
        );
        let callee = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(0xabc),
        );
        let a1 = b.emit(
            e,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(10.0),
        );
        let r = b.emit(
            e,
            Op::Call,
            &[this, callee, a1],
            Type::Unknown,
            Effects::call(),
            Imm::Int(0),
        );
        let one = b.emit(
            e,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(1.0),
        );
        let sum = b.emit(
            e,
            Op::Add,
            &[r, one],
            Type::Number,
            Effects::call(),
            Imm::None,
        );
        b.term(e, Term::Return(Some(sum)));
        f
    }

    #[test]
    fn a_declared_callee_splices_without_a_comparison() {
        // A hoisted declaration owns the callee slot, so the splice needs no
        // runtime check and leaves no fallback call.
        let mut func = caller_ir_mid_block();
        let sites = vec![Some(InlineSite {
            target: InlineTarget::Declared { init_step: 3 },
        })];
        let mut resolve = |target: &InlineTarget| match target {
            InlineTarget::Declared { init_step: 3 } => Some(param_callee()),
            _ => None,
        };
        assert!(
            run(&mut func, None, &sites, &mut resolve),
            "the site inlined"
        );
        assert_eq!(call_count(&func), 0, "no fallback call");
        assert!(!ops(&func).contains(&Op::CalleeEq), "no comparison");
        assert_eq!(verify(&func), Ok(()), "the spliced IR verifies");
    }

    #[test]
    fn splices_a_call_that_is_not_its_blocks_terminator() {
        // `r = call(...); return r + 1`: the instruction after the call moves
        // to the continuation and its operand becomes the continuation's
        // parameter.
        let mut func = caller_ir_mid_block();
        let mut resolve = by_id(|id| (id == 7).then(param_callee));
        assert!(
            run(&mut func, None, &site(), &mut resolve),
            "the site inlined"
        );
        assert_eq!(call_count(&func), 1, "only the mismatch fallback remains");
        assert_eq!(verify(&func), Ok(()), "the spliced IR verifies");
        let cont = (0..func.block_count() as u32)
            .find(|&b| {
                func.block(b)
                    .insts
                    .iter()
                    .any(|i| i.op == Op::Add && i.args.len() == 2)
            })
            .expect("the add moved to a block");
        let add = func
            .block(cont)
            .insts
            .iter()
            .find(|i| i.op == Op::Add && i.args.len() == 2)
            .expect("the add");
        assert_eq!(
            add.args[0],
            func.block(cont).params[0],
            "the call's result is the continuation's parameter"
        );
    }

    #[test]
    fn leaves_a_site_the_resolver_declines() {
        let mut func = caller_ir();
        let mut resolve = by_id(|_id| None);
        assert!(!run(&mut func, None, &site(), &mut resolve));
        assert!(ops(&func).contains(&Op::Call));
    }

    #[test]
    fn leaves_an_unpromoted_var_writing_callee_alone() {
        // This pass sees the resolver's output, after `pass::mem2reg`; a callee
        // whose `var` still lives in the frame (a `FrameStore` or a non-input
        // load) is refused — the slot has no home in the caller's frame.
        let mut f = Function::new();
        let e = f.entry();
        {
            let mut b = Builder::new(&mut f);
            let zero = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(0.0),
            );
            b.emit_void(
                e,
                Op::FrameStore,
                &[zero],
                Effects::write(Heap::Slots),
                Imm::Slot(1),
            );
            let t = b.emit(
                e,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(Heap::Slots),
                Imm::Slot(1),
            );
            b.term(e, Term::Return(Some(t)));
        }
        let var_callee = Callee {
            ir: Rc::new(f),
            arity: 0,
            this_slot: None,
            sites: Rc::new(Vec::new()),
            steps: 1,
        };
        let mut func = caller_ir();
        let mut resolve = by_id(|_id| Some(var_callee.clone()));
        assert!(!run(&mut func, None, &site(), &mut resolve));
        assert!(ops(&func).contains(&Op::Call));
    }

    #[test]
    fn splices_a_local_writing_callee_once_promoted() {
        // I5c-2c-iii-b: `pass::mem2reg` promotes the callee's `var` slots to SSA,
        // after which a callee with locals (`var t = 7; return t;`) splices.
        let mut f = Function::new();
        let e = f.entry();
        {
            let mut b = Builder::new(&mut f);
            let seven = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(7.0),
            );
            b.emit_void(
                e,
                Op::FrameStore,
                &[seven],
                Effects::write(Heap::Slots),
                Imm::Slot(1),
            );
            let t = b.emit(
                e,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(Heap::Slots),
                Imm::Slot(1),
            );
            b.term(e, Term::Return(Some(t)));
        }
        assert!(crate::opt::pass::mem2reg::run(&mut f), "the var promoted");
        assert!(
            !f.block(e).insts.iter().any(|i| i.op == Op::FrameStore),
            "no frame store survives promotion"
        );
        let callee = Callee {
            ir: Rc::new(f),
            arity: 0,
            this_slot: None,
            sites: Rc::new(Vec::new()),
            steps: 1,
        };
        let mut func = caller_ir();
        let mut resolve = by_id(|_id| Some(callee.clone()));
        assert!(
            run(&mut func, None, &site(), &mut resolve),
            "the callee spliced"
        );
        assert_eq!(call_count(&func), 1, "only the mismatch fallback remains");
        assert_eq!(verify(&func), Ok(()), "the spliced IR verifies");
    }

    /// `function (x) { return next(x); }` — one param read, one block-last call.
    fn chain_callee(id: u64, box_bits: u64) -> Callee {
        let mut f = Function::new();
        let e = f.entry();
        let mut b = Builder::new(&mut f);
        let this = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(0),
        );
        let callee = b.emit(
            e,
            Op::Const,
            &[],
            Type::Object,
            Effects::pure(),
            Imm::U64(box_bits),
        );
        let arg = b.emit(
            e,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(Heap::Slots),
            Imm::Slot(0),
        );
        let r = b.emit(
            e,
            Op::Call,
            &[this, callee, arg],
            Type::Unknown,
            Effects::call(),
            Imm::Int(0),
        );
        b.term(e, Term::Return(Some(r)));
        Callee {
            ir: Rc::new(f),
            arity: 1,
            this_slot: None,
            sites: Rc::new(vec![Some(InlineSite {
                target: InlineTarget::Guarded {
                    callee_id: id,
                    callee_box: box_bits,
                },
            })]),
            steps: 3,
        }
    }

    #[test]
    fn recurses_into_a_callees_own_call() {
        // The caller calls A (id 7); A's own site 0 calls B (id 8). Both splice,
        // so the region is call-free and guards twice.
        let mut func = caller_ir();
        let mut resolve = by_id(|id| match id {
            7 => Some(chain_callee(8, 0xdef)),
            8 => Some(constant_callee()),
            _ => None,
        });
        assert!(run(&mut func, None, &site(), &mut resolve));
        assert_eq!(
            call_count(&func),
            2,
            "one fallback per spliced level, and nothing else"
        );
        assert_eq!(
            ops(&func).iter().filter(|o| **o == Op::CalleeEq).count(),
            2,
            "one compare per splice level"
        );
        assert_eq!(verify(&func), Ok(()));
    }

    #[test]
    fn a_self_recursive_callee_stops_at_the_budget() {
        // A's own site calls A: the recursion expands until the budget is spent,
        // then leaves the innermost call real. A hang would fail the test.
        let mut func = caller_ir();
        let mut resolve = by_id(|id| (id == 7).then(|| chain_callee(7, 0xabc)));
        assert!(run(&mut func, None, &site(), &mut resolve));
        assert_eq!(verify(&func), Ok(()));
        assert!(
            ops(&func).contains(&Op::Call),
            "the budget stopped the self-recursion"
        );
    }

    #[test]
    fn leaves_a_guard_bearing_callee_alone() {
        // A guard's deopt resumes the callee's step; spliced, that step is wrong
        // in the caller, and re-running the outer call could double a getter. So
        // a guard-bearing callee is refused.
        let mut f = Function::new();
        let e = f.entry();
        {
            let mut b = Builder::new(&mut f);
            let v = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            b.emit(
                e,
                Op::GuardType,
                &[v, v],
                Type::Number,
                Effects::pure(),
                Imm::Int(0),
            );
            b.term(e, Term::Return(Some(v)));
        }
        let guarded = Callee {
            ir: Rc::new(f),
            arity: 0,
            this_slot: None,
            sites: Rc::new(Vec::new()),
            steps: 1,
        };
        let mut func = caller_ir();
        let mut resolve = by_id(|_id| Some(guarded.clone()));
        assert!(!run(&mut func, None, &site(), &mut resolve));
        assert!(ops(&func).contains(&Op::Call));
    }
}
