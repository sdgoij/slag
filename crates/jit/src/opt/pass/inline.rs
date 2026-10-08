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

/// A monomorphic call site's inline target: the callee's function id (the
/// resolver's key) and its box (the `Op::GuardCallee` constant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSite {
    pub callee_id: u64,
    pub callee_box: u64,
}

/// A resolved callee: its lifted IR and the input frame layout (`ScopeInfo`)
/// the splice binds.
#[derive(Clone)]
pub struct Callee {
    pub ir: Rc<Function>,
    /// The parameter count; params occupy slots `0..arity` in source order.
    pub arity: usize,
    /// The frame slot holding the call's receiver, if the body reads `this`.
    pub this_slot: Option<usize>,
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
        if call.state() == runtime::feedback::IcState::Specialized && call.first_callee_id() != 0 {
            *site = Some(InlineSite {
                callee_id: call.first_callee_id(),
                callee_box: call.first_callee_box(),
            });
        }
    }
    sites
}

/// Inline every spliceable monomorphic call site. `sites` is indexed by step
/// (the `Op::Call`'s `Imm::Int`); `resolve` yields the callee. Returns whether
/// the IR changed.
pub fn run(
    func: &mut Function,
    sites: &[Option<InlineSite>],
    resolve: &mut dyn FnMut(u64) -> Option<Callee>,
) -> bool {
    let mut changed = false;
    let blocks = func.block_count() as u32;
    for b in 0..blocks {
        if try_inline(func, b, sites, resolve) {
            changed = true;
        }
    }
    changed
}

/// Splice the call at the end of block `b` if it has a spliceable site.
fn try_inline(
    func: &mut Function,
    b: BlockId,
    sites: &[Option<InlineSite>],
    resolve: &mut dyn FnMut(u64) -> Option<Callee>,
) -> bool {
    let (args, result, ty, step, site) = {
        let Some(last) = func.block(b).insts.last() else {
            return false;
        };
        if last.op != Op::Call {
            return false;
        }
        let Imm::Int(step) = last.imm else {
            return false;
        };
        let Some(&Some(site)) = sites.get(step as usize) else {
            return false;
        };
        let Some(result) = last.result else {
            return false;
        };
        (last.args.clone(), result, last.ty, step, site)
    };
    let Some(callee) = resolve(site.callee_id) else {
        return false;
    };
    // Every input slot must have a call operand to bind to.
    if args.len() < 2 + callee.arity || !spliceable(&callee) {
        return false;
    }

    // The call's result is now produced by the continuation, which every
    // successor of `b` passes through (its only edge is `b -> callee -> cont`).
    func.block_mut(b).insts.pop();
    let old_term = func.block_mut(b).term.take();
    let cont = func.push_block();
    let param = func.push_value(ty);
    func.block_mut(cont).params.push(param);
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

    // Guard the assumed callee, then enter it. The guard's live operand stack is
    // the call's operands, so a deopt re-runs the call step exactly.
    let expected = func.push_value(Type::Object);
    func.block_mut(b).insts.push(Inst {
        op: Op::Const,
        args: Vec::new(),
        result: Some(expected),
        ty: Type::Object,
        effects: Effects::pure(),
        imm: Imm::U64(site.callee_box),
    });
    let guarded = func.push_value(Type::Object);
    let mut guard_args = vec![args[1], expected];
    guard_args.extend_from_slice(&args);
    func.block_mut(b).insts.push(Inst {
        op: Op::GuardCallee,
        args: guard_args,
        result: Some(guarded),
        ty: Type::Object,
        effects: Effects::pure(),
        imm: Imm::Int(step),
    });
    func.block_mut(b).term = Some(Term::Jump {
        target: block_map[0],
        args: Vec::new(),
    });

    replace_uses(func, result, param);
    true
}

/// Whether this cut can splice `callee`: call-free, and every remaining slot
/// access is a load of an **input** slot (`< arity` or the `this` slot). The
/// resolver promotes the callee's `var` slots to SSA first (`pass/mem2reg.rs`,
/// I5c-2c-iii-b), so a `FrameStore`, a `TdzCheck`, or a load of any other slot
/// means promotion could not lift it — refused.
fn spliceable(callee: &Callee) -> bool {
    let mut returns_a_value = false;
    for k in 0..callee.ir.block_count() as u32 {
        for inst in &callee.ir.block(k).insts {
            match inst.op {
                Op::Call | Op::FrameStore | Op::TdzCheck => return false,
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

    fn site() -> Vec<Option<InlineSite>> {
        vec![Some(InlineSite {
            callee_id: 7,
            callee_box: 0xabc,
        })]
    }

    #[test]
    fn splices_a_parameter_reading_callee() {
        // The callee's param load becomes the call's argument, so
        // `x + 1` with `x = 10` splices to `11`.
        let mut func = caller_ir();
        let mut resolve = |id: u64| (id == 7).then(param_callee);
        assert!(run(&mut func, &site(), &mut resolve), "the site inlined");
        assert!(!ops(&func).contains(&Op::Call), "the call is gone");
        assert!(!ops(&func).contains(&Op::FrameLoad), "the param is bound");
        assert!(
            ops(&func).contains(&Op::GuardCallee),
            "the callee is guarded"
        );
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
        let mut resolve = |id: u64| (id == 7).then(constant_callee);
        assert!(run(&mut func, &site(), &mut resolve));
        assert!(!ops(&func).contains(&Op::Call));
        assert_eq!(verify(&func), Ok(()));
    }

    #[test]
    fn leaves_a_site_the_resolver_declines() {
        let mut func = caller_ir();
        let mut resolve = |_id: u64| None;
        assert!(!run(&mut func, &site(), &mut resolve));
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
        };
        let mut func = caller_ir();
        let mut resolve = |_id: u64| Some(var_callee.clone());
        assert!(!run(&mut func, &site(), &mut resolve));
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
        };
        let mut func = caller_ir();
        let mut resolve = |_id: u64| Some(callee.clone());
        assert!(run(&mut func, &site(), &mut resolve), "the callee spliced");
        assert!(!ops(&func).contains(&Op::Call), "the call is gone");
        assert_eq!(verify(&func), Ok(()), "the spliced IR verifies");
    }
}
