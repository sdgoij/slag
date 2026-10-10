//! Prove a bounded loop counter's range so its increment can be a wrapping i32
//! op and the counter ride in an i32 register.
//!
//! `narrow` types an increment `i + 1` a `Number` (a sum of Numbers may exceed
//! `2^31`), so `i & MASK` on a `for (i = 0; i < N; i++)` counter keeps the f64
//! range guard the per-step lane drops — its plan proved the counter in range.
//! A counter whose entry is an int32 constant and whose back edge is `i ± c`
//! bounded by an int32 constant is int32 for every iteration, so `i ± c` is
//! exactly `IntAdd`/`IntSub` and the guard is redundant.
//!
//! Runs after `mem2reg` (the counter is then a block parameter, not a slot) and
//! before `normalize_param_types`, which re-types the parameter from the
//! (now `Int`) increment.

use std::collections::HashMap;

use crate::opt::ir::{BlockId, Function, Imm, Inst, Op, Term, Type, ValueId};

/// A proven increment: where its `Add`/`Sub` and its constant operand live, and
/// the wrapping op to rewrite it to.
struct Increment {
    add: (BlockId, usize),
    op: Op,
    kv: ValueId,
    konst: (BlockId, usize),
    delta: i32,
}

/// Run the pass over `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.block_count() as u32;
    let mut def: HashMap<ValueId, (BlockId, usize)> = HashMap::new();
    let mut uses: HashMap<ValueId, u32> = HashMap::new();
    for b in 0..n {
        for (i, inst) in func.block(b).insts.iter().enumerate() {
            if let Some(r) = inst.result {
                def.insert(r, (b, i));
            }
            for &a in &inst.args {
                *uses.entry(a).or_insert(0) += 1;
            }
        }
        match &func.block(b).term {
            Some(Term::Return(Some(v))) => *uses.entry(*v).or_insert(0) += 1,
            Some(Term::Jump { args, .. }) => {
                for &a in args {
                    *uses.entry(a).or_insert(0) += 1;
                }
            }
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                *uses.entry(*cond).or_insert(0) += 1;
                for &a in then_args {
                    *uses.entry(a).or_insert(0) += 1;
                }
                for &a in else_args {
                    *uses.entry(a).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }

    let mut found: Vec<Increment> = Vec::new();
    for h in 0..n {
        let params = func.block(h).params.clone();
        for (pi, &p) in params.iter().enumerate() {
            if let Some(inc) = prove(func, h, pi, p, &def, &uses) {
                found.push(inc);
            }
        }
    }
    if found.is_empty() {
        return false;
    }
    for inc in found {
        let (ab, ai) = inc.add;
        let inst = &mut func.block_mut(ab).insts[ai];
        inst.op = inc.op;
        inst.ty = Type::Int;
        if let Some(r) = inst.result {
            func.set_value_type(r, Type::Int);
        }
        let (kb, ki) = inc.konst;
        let kinst = &mut func.block_mut(kb).insts[ki];
        kinst.imm = Imm::Int(inc.delta);
        kinst.ty = Type::Int;
        if let Some(r) = kinst.result {
            func.set_value_type(r, Type::Int);
        }
    }
    true
}

/// The integral value of a constant instruction, when it is in `i32` range.
fn const_int(inst: &Inst) -> Option<i32> {
    match &inst.imm {
        Imm::Int(i) => Some(*i),
        Imm::Float(f) if f.fract() == 0.0 && f.abs() < 2_147_483_648.0 => Some(*f as i32),
        _ => None,
    }
}

/// Prove the parameter `p` (index `pi` of block `h`) is a bounded counter and
/// return its increment, if it is.
fn prove(
    func: &Function,
    h: BlockId,
    pi: usize,
    p: ValueId,
    def: &HashMap<ValueId, (BlockId, usize)>,
    uses: &HashMap<ValueId, u32>,
) -> Option<Increment> {
    let mut init: Option<i32> = None;
    let mut inc: Option<Increment> = None;
    let mut bound: Option<(i32, bool)> = None;
    for body in 0..func.block_count() as u32 {
        let Some(term) = func.block(body).term.as_ref() else {
            continue;
        };
        let (args, guard) = match term {
            Term::Jump { target, args } if *target == h => (args, None),
            Term::Branch {
                cond,
                then_block,
                then_args,
                else_block,
                else_args,
            } => {
                if *then_block == h {
                    (then_args, Some((*cond, false)))
                } else if *else_block == h {
                    (else_args, Some((*cond, true)))
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let &a = args.get(pi)?;
        // Every edge into `h` must be the initial constant or a bounded `i ± c`;
        // any other incoming value could be outside the proven range.
        let &(db, di) = def.get(&a)?;
        let d = &func.block(db).insts[di];
        if d.op == Op::Const
            && let Some(c) = const_int(d)
        {
            if init.replace(c).is_some() {
                return None;
            }
            continue;
        }
        let (op, konst) = match d.op {
            Op::Add => (Op::IntAdd, d.args.get(1)),
            Op::Sub => (Op::IntSub, d.args.get(1)),
            _ => return None,
        };
        if d.args.first() != Some(&p) {
            return None;
        }
        let &cv = konst?;
        let &(kb, ki) = def.get(&cv)?;
        let delta = const_int(&func.block(kb).insts[ki])?;
        // The bound comes from the guard on *this* edge (an unconditional
        // back-edge has none, so it cannot be bounded).
        let (cond, negate) = guard?;
        let (limit, increasing) = bound_of(func, cond, a, negate, def)?;
        if inc.is_some() {
            return None;
        }
        inc = Some(Increment {
            add: (db, di),
            op,
            kv: cv,
            konst: (kb, ki),
            delta,
        });
        bound = Some((limit, increasing));
    }
    let init = init?;
    let inc = inc?;
    let (limit, increasing) = bound?;
    // The constant must be private to this increment, so re-typing it is local.
    if inc.delta == 0 || uses.get(&inc.kv).copied().unwrap_or(0) != 1 {
        return None;
    }
    // Increasing `i` is bounded above (`Lt/Le(v, N)`), decreasing below.
    if (inc.delta > 0) != increasing {
        return None;
    }
    // Every reachable value — `p` and the increment (which also flows to the
    // exit block on the last iteration) — must be an int32.
    let span = i64::from(inc.delta).abs();
    let lo = i64::from(init).min(i64::from(limit)) - span;
    let hi = i64::from(init).max(i64::from(limit)) + span;
    if lo < i64::from(i32::MIN) || hi > i64::from(i32::MAX) {
        return None;
    }
    Some(inc)
}

/// The `(limit, increasing)` a comparison `c` imposes on the value `x`, when
/// `c` is `Lt/Le(x, N)` (increasing) or its negated/normal form (decreasing).
/// `N` must be an integral constant; `negate` flips the comparison because the
/// edge reaches the header on the branch's *false* side.
fn bound_of(
    func: &Function,
    c: ValueId,
    x: ValueId,
    negate: bool,
    def: &HashMap<ValueId, (BlockId, usize)>,
) -> Option<(i32, bool)> {
    let &(cb, ci) = def.get(&c)?;
    let inst = &func.block(cb).insts[ci];
    if inst.args.len() != 2 {
        return None;
    }
    // `NOT (a < b)` is `a >= b`, and so on.
    let op = if negate {
        match inst.op {
            Op::Lt => Op::Ge,
            Op::Le => Op::Gt,
            Op::Gt => Op::Le,
            Op::Ge => Op::Lt,
            _ => return None,
        }
    } else {
        inst.op
    };
    let (limit_v, increasing) = match op {
        Op::Lt | Op::Le if inst.args[0] == x => (inst.args[1], true),
        Op::Gt | Op::Ge if inst.args[1] == x => (inst.args[0], true),
        Op::Gt | Op::Ge if inst.args[0] == x => (inst.args[1], false),
        Op::Lt | Op::Le if inst.args[1] == x => (inst.args[0], false),
        _ => return None,
    };
    let &(lb, li) = def.get(&limit_v)?;
    let limit = const_int(&func.block(lb).insts[li])?;
    Some((limit, increasing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::Effects;
    use crate::opt::verify::verify;

    /// `entry: jump header(0); header(p): inc = p + 1; if (inc < limit)
    /// header(inc) else exit(p)` — a bounded, increasing counter.
    fn counter_body(limit: f64) -> (Function, BlockId) {
        let mut func = Function::new();
        let e = func.entry();
        let header;
        {
            let mut b = Builder::new(&mut func);
            let zero = b.emit(e, Op::Const, &[], Type::Int, Effects::pure(), Imm::Int(0));
            header = b.block();
            let exit = b.block();
            b.term(
                e,
                Term::Jump {
                    target: header,
                    args: vec![zero],
                },
            );
            let p = b.param(header, Type::Number);
            let one = b.emit(
                header,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            let inc = b.emit(
                header,
                Op::Add,
                &[p, one],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            let lim = b.emit(
                header,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(limit),
            );
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
                    else_args: vec![],
                },
            );
            b.term(exit, Term::Return(None));
        }
        (func, header)
    }

    #[test]
    fn proves_a_bounded_counter() {
        let (mut func, header) = counter_body(100.0);
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        let inc = func
            .block(header)
            .insts
            .iter()
            .find(|i| i.op == Op::IntAdd)
            .expect("the increment became a wrapping i32 op");
        let konst = func
            .block(header)
            .insts
            .iter()
            .find(|i| i.result == Some(inc.args[1]))
            .expect("the increment's constant");
        assert_eq!(konst.imm, Imm::Int(1));
        assert_eq!(konst.ty, Type::Int);
    }

    #[test]
    fn refuses_a_counter_beyond_int32() {
        // A limit above `2^31` cannot bound the counter into an int32.
        let (mut func, _) = counter_body(3.0e9);
        assert!(!run(&mut func));
    }
}
