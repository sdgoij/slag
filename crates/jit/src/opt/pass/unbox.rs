//! Keep a proven-Number arithmetic chain in f64 registers.
//!
//! The lowering canonicalizes every arithmetic result against the NaN-box tag
//! region (`canon_double`), so an accumulator like `n += 1` leaves the f64
//! domain, round-trips through a general-purpose register, and pays a mask,
//! compare and select per operation. The per-step lane does not: it keeps the
//! value in an f64 register and canonicalizes only when the value is stored.
//!
//! This pass reproduces that for the optimizing tier. A value whose operands
//! are proven Numbers (`narrow`) — including a loop-carried block parameter —
//! is retyped [`Type::Float`], the raw f64 word. Only an arithmetic or
//! ordering op may consume it (the lowering works in f64 directly); every
//! other use gets an [`Op::Canon`] inserted in front of it, so the tag-region
//! canonicalization happens where the value leaves the chain rather than on
//! every iteration.
//!
//! Runs last: no later pass must see a raw float, and the canonicalization
//! points depend on the final control flow.

use crate::opt::ir::{BlockId, Effects, Function, Imm, Inst, Op, Term, Type, ValueId};

/// Whether an instruction is one the lowering can feed a raw f64.
fn is_arith(op: Op) -> bool {
    matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div)
}

/// Whether an instruction can consume a raw f64 without canonicalizing first:
/// the bare-f64 lowering works in f64 registers, but it is only taken when
/// both operands are known numerics.
fn accepts_raw(func: &Function, inst: &Inst, raw: &[bool]) -> bool {
    inst.args.len() == 2
        && matches!(
            inst.op,
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Lt | Op::Le | Op::Gt | Op::Ge
        )
        && inst.args.iter().all(|&a| raw_operand(func, a, raw))
}

/// Whether `v` can stand in for a raw f64 in a bare op: a canonical Number or
/// int32 word is already a valid f64 bit pattern, and a raw value is by
/// definition one.
fn raw_operand(func: &Function, v: ValueId, raw: &[bool]) -> bool {
    matches!(func.value_type(v), Type::Number | Type::Int | Type::Float) || raw[v as usize]
}

/// Run the pass over `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.block_count() as u32;
    let mut raw = vec![false; func.value_count() as usize];

    // Incoming edge arguments per block, for the loop-carried fixpoint.
    let mut incoming: Vec<Vec<Vec<ValueId>>> = vec![Vec::new(); n as usize];
    for b in 0..n {
        match &func.block(b).term {
            Some(Term::Jump { target, args }) => incoming[*target as usize].push(args.clone()),
            Some(Term::Branch {
                then_block,
                then_args,
                else_block,
                else_args,
                ..
            }) => {
                incoming[*then_block as usize].push(then_args.clone());
                incoming[*else_block as usize].push(else_args.clone());
            }
            _ => {}
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for b in 0..n {
            for inst in &func.block(b).insts {
                let Some(r) = inst.result else {
                    continue;
                };
                if raw[r as usize] || inst.ty != Type::Number || !is_arith(inst.op) {
                    continue;
                }
                if accepts_raw(func, inst, &raw) {
                    raw[r as usize] = true;
                    changed = true;
                }
            }
        }
        // A parameter that receives a raw value in any edge must hold one
        // itself, or the edge needs a canonicalization inside the loop.
        for b in 0..n {
            for &p in &func.block(b).params {
                if raw[p as usize] || func.value_type(p) != Type::Number {
                    continue;
                }
                if incoming[b as usize]
                    .iter()
                    .any(|args| args.iter().any(|a| raw[*a as usize]))
                {
                    raw[p as usize] = true;
                    changed = true;
                }
            }
        }
    }

    if !raw.iter().any(|&r| r) {
        return false;
    }

    for b in 0..n {
        let params = func.block(b).params.clone();
        for p in params {
            if raw[p as usize] && func.value_type(p) == Type::Number {
                func.set_value_type(p, Type::Float);
            }
        }
        for i in 0..func.block(b).insts.len() {
            let Some(r) = func.block(b).insts[i].result else {
                continue;
            };
            if !raw[r as usize] {
                continue;
            }
            func.block_mut(b).insts[i].ty = Type::Float;
            func.set_value_type(r, Type::Float);
        }
    }

    // Canonicalize at every use that observes the value word. Instructions
    // are visited bottom-up so an insertion at `i` cannot shift an index not
    // yet processed.
    for b in 0..n {
        for i in (0..func.block(b).insts.len()).rev() {
            let needs: Vec<(usize, ValueId)> = {
                let inst = &func.block(b).insts[i];
                if accepts_raw(func, inst, &raw) {
                    continue;
                }
                inst.args
                    .iter()
                    .enumerate()
                    .filter(|&(_, &a)| raw[a as usize])
                    .map(|(k, &a)| (k, a))
                    .collect()
            };
            if needs.is_empty() {
                continue;
            }
            let created = insert_canons(func, b, i, &needs);
            let at = i + created.len();
            for &(k, value) in &needs {
                let canon = created
                    .iter()
                    .find(|(v, _)| *v == value)
                    .map(|&(_, c)| c)
                    .expect("every needed value has a canon");
                func.block_mut(b).insts[at].args[k] = canon;
            }
        }

        // Terminator operands: a value passed to a raw parameter is fine, but
        // a return, throw, branch condition or non-raw edge argument is a
        // `Value` boundary.
        let mut needs: Vec<ValueId> = Vec::new();
        match &func.block(b).term {
            Some(Term::Return(Some(v))) | Some(Term::Throw(v)) => needs.push(*v),
            Some(Term::Branch { cond, .. }) => needs.push(*cond),
            _ => {}
        }
        match &func.block(b).term {
            Some(Term::Jump { target, args }) => {
                edge_needs(func, *target, args, &raw, &mut needs);
            }
            Some(Term::Branch {
                then_block,
                then_args,
                else_block,
                else_args,
                ..
            }) => {
                edge_needs(func, *then_block, then_args, &raw, &mut needs);
                edge_needs(func, *else_block, else_args, &raw, &mut needs);
            }
            _ => {}
        }
        needs.retain(|&v| raw[v as usize]);
        if needs.is_empty() {
            continue;
        }
        let at = func.block(b).insts.len();
        let pairs: Vec<(usize, ValueId)> = needs.iter().enumerate().map(|(i, &v)| (i, v)).collect();
        let created = insert_canons(func, b, at, &pairs);
        let replace = |v: &mut ValueId| {
            if let Some(&(_, canon)) = created.iter().find(|(old, _)| old == v) {
                *v = canon;
            }
        };
        match &mut func.block_mut(b).term {
            Some(Term::Return(Some(v))) | Some(Term::Throw(v)) => replace(v),
            Some(Term::Branch { cond, .. }) => replace(cond),
            _ => {}
        }
        match &mut func.block_mut(b).term {
            Some(Term::Jump { args, .. }) => {
                for a in args.iter_mut() {
                    replace(a);
                }
            }
            Some(Term::Branch {
                then_args,
                else_args,
                ..
            }) => {
                for a in then_args.iter_mut().chain(else_args.iter_mut()) {
                    replace(a);
                }
            }
            _ => {}
        }
    }
    true
}

/// Insert one `Canon` per distinct raw value in `needs`, starting at `at`, and
/// return the `(value, canon)` pairs.
fn insert_canons(
    func: &mut Function,
    block: BlockId,
    at: usize,
    needs: &[(usize, ValueId)],
) -> Vec<(ValueId, ValueId)> {
    let mut created: Vec<(ValueId, ValueId)> = Vec::new();
    for &(_, value) in needs {
        if created.iter().any(|(v, _)| *v == value) {
            continue;
        }
        let canon = func.push_value(Type::Number);
        func.block_mut(block).insts.insert(
            at + created.len(),
            Inst {
                op: Op::Canon,
                args: vec![value],
                result: Some(canon),
                ty: Type::Number,
                effects: Effects::pure(),
                imm: Imm::None,
            },
        );
        created.push((value, canon));
    }
    created
}

/// Collect raw arguments of an edge whose target parameter is not raw.
fn edge_needs(
    func: &Function,
    target: BlockId,
    args: &[ValueId],
    raw: &[bool],
    needs: &mut Vec<ValueId>,
) {
    for (i, &a) in args.iter().enumerate() {
        if !raw[a as usize] {
            continue;
        }
        let raw_param = func
            .block(target)
            .params
            .get(i)
            .is_some_and(|p| func.value_type(*p) == Type::Float);
        if !raw_param {
            needs.push(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::verify::verify;

    fn num(b: &mut Builder<'_>, block: BlockId, v: f64) -> ValueId {
        b.emit(
            block,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(v),
        )
    }

    /// `entry: jump header(0); header(p): inc = p + 1; if (inc < 100)
    /// header(inc) else exit(inc); exit(q): return q` — the accumulator shape
    /// the lowering previously boxed on every iteration.
    fn accumulator_loop() -> (Function, BlockId, BlockId) {
        let mut func = Function::new();
        let e = func.entry();
        let header;
        let exit;
        {
            let mut b = Builder::new(&mut func);
            let zero = num(&mut b, e, 0.0);
            header = b.block();
            exit = b.block();
            b.term(
                e,
                Term::Jump {
                    target: header,
                    args: vec![zero],
                },
            );
            let p = b.param(header, Type::Number);
            let one = num(&mut b, header, 1.0);
            let inc = b.emit(
                header,
                Op::Add,
                &[p, one],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let lim = num(&mut b, header, 100.0);
            let cond = b.emit(
                header,
                Op::Lt,
                &[inc, lim],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: header,
                    then_args: vec![inc],
                    else_block: exit,
                    else_args: vec![inc],
                },
            );
            let q = b.param(exit, Type::Number);
            b.term(exit, Term::Return(Some(q)));
        }
        (func, header, exit)
    }

    #[test]
    fn a_loop_accumulator_stays_in_an_f64_register() {
        let (mut func, header, exit) = accumulator_loop();
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        assert_eq!(func.value_type(func.block(header).params[0]), Type::Float);
        assert_eq!(func.value_type(func.block(exit).params[0]), Type::Float);
        assert!(
            func.block(header)
                .insts
                .iter()
                .all(|inst| inst.op != Op::Canon),
            "the loop-carried value must not be canonicalized per iteration"
        );
        let ret = match &func.block(exit).term {
            Some(Term::Return(Some(v))) => *v,
            other => panic!("unexpected terminator {other:?}"),
        };
        let canon = func
            .block(exit)
            .insts
            .iter()
            .find(|inst| inst.result == Some(ret))
            .expect("the returned value's definition");
        assert_eq!(canon.op, Op::Canon, "the return boundary canonicalizes");
        assert_eq!(canon.args, vec![func.block(exit).params[0]]);
    }

    #[test]
    fn a_raw_value_stored_to_a_frame_slot_is_canonicalized() {
        let mut func = Function::new();
        let e = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let one = num(&mut b, e, 1.0);
            let two = num(&mut b, e, 2.0);
            let sum = b.emit(
                e,
                Op::Add,
                &[one, two],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            b.emit(
                e,
                Op::FrameStore,
                &[sum],
                Type::Never,
                Effects::pure(),
                Imm::Slot(0),
            );
            b.term(e, Term::Return(None));
        }
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        let store = func
            .block(e)
            .insts
            .iter()
            .find(|inst| inst.op == Op::FrameStore)
            .expect("the store");
        let arg = func
            .block(e)
            .insts
            .iter()
            .find(|inst| inst.result == Some(store.args[0]))
            .expect("the stored value's definition");
        assert_eq!(arg.op, Op::Canon, "a frame slot stores a Value word");
        assert_eq!(arg.ty, Type::Number);
    }

    #[test]
    fn an_unproven_operand_keeps_the_result_canonical() {
        // `p` is a raw loop parameter, but `u` is not a proven number, so the
        // mixed add cannot be a bare f64 op and `p` is canonicalized for it.
        let mut func = Function::new();
        let e = func.entry();
        let header;
        let exit;
        {
            let mut b = Builder::new(&mut func);
            let zero = num(&mut b, e, 0.0);
            let obj = b.emit(
                e,
                Op::NewObject,
                &[],
                Type::Unknown,
                Effects::pure(),
                Imm::None,
            );
            header = b.block();
            exit = b.block();
            b.term(
                e,
                Term::Jump {
                    target: header,
                    args: vec![zero, obj],
                },
            );
            let p = b.param(header, Type::Number);
            let u = b.param(header, Type::Unknown);
            let one = num(&mut b, header, 1.0);
            let inc = b.emit(
                header,
                Op::Add,
                &[p, one],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let mixed = b.emit(
                header,
                Op::Add,
                &[p, u],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let lim = num(&mut b, header, 100.0);
            let cond = b.emit(
                header,
                Op::Lt,
                &[inc, lim],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: header,
                    then_args: vec![inc, u],
                    else_block: exit,
                    else_args: vec![mixed],
                },
            );
            let q = b.param(exit, Type::Number);
            b.term(exit, Term::Return(Some(q)));
        }
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        let adds: Vec<_> = func
            .block(header)
            .insts
            .iter()
            .filter(|inst| inst.op == Op::Add)
            .collect();
        assert_eq!(adds.len(), 2);
        assert_eq!(adds[0].ty, Type::Float, "the proven chain stays raw");
        assert_eq!(adds[1].ty, Type::Number, "the unproven add stays boxed");
        let canon = func
            .block(header)
            .insts
            .iter()
            .find(|inst| inst.op == Op::Canon)
            .expect("the raw parameter is canonicalized for the mixed add");
        assert_eq!(canon.args, vec![func.block(header).params[0]]);
    }
}
