//! The invariants every pass may assume.
//!
//! `verify` is run after a producer builds a graph (the lift, or a pass) and
//! before any other pass consumes it. It is exact and total: a graph that
//! verifies has dense value ids, a single definition per value, a terminator
//! per block, edge arities that match the target's parameters, every value use
//! dominated by its definition, and no unreachable block.

use super::ir::{BlockId, Function, Term, ValueId};

/// A well-formedness violation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A block has no terminator.
    MissingTerminator(BlockId),
    /// A block cannot be reached from the entry.
    UnreachableBlock(BlockId),
    /// An edge names a block that does not exist.
    EdgeOutOfRange { from: BlockId, to: BlockId },
    /// An edge passes the wrong number of arguments for the target's
    /// parameters.
    EdgeArity {
        from: BlockId,
        to: BlockId,
        expected: usize,
        got: usize,
    },
    /// The entry block has parameters, but nothing can pass them.
    EntryHasParams(u32),
    /// A value referenced by an instruction or terminator is not defined.
    UndefinedValue { block: BlockId, value: ValueId },
    /// A value is defined more than once.
    DuplicateDefinition { value: ValueId },
    /// A value id is outside the function's value range.
    ValueOutOfRange(ValueId),
    /// A value is used in a block its definition does not dominate.
    UseNotDominated { block: BlockId, value: ValueId },
}

/// Verify `func`.
///
/// # Errors
/// Returns the first violation found, in block order.
#[allow(clippy::too_many_lines)]
pub fn verify(func: &Function) -> Result<(), Error> {
    let n = func.block_count();
    let entry = func.entry() as usize;

    if !func.block(func.entry()).params.is_empty() {
        return Err(Error::EntryHasParams(
            func.block(func.entry()).params.len() as u32
        ));
    }

    let mut def_block: Vec<Option<BlockId>> = vec![None; func.value_count() as usize];
    let mut def_pos: Vec<Option<u32>> = vec![None; func.value_count() as usize];

    // Pass 1: definitions and terminators. A value is defined by a block
    // parameter (position `None`, i.e. at block entry) or an instruction
    // (position = its index). A second definition is a violation.
    for b in 0..n as BlockId {
        let block = func.block(b);
        for &p in &block.params {
            define(&mut def_block, &mut def_pos, p, b, None)?;
        }
        for (i, inst) in block.insts.iter().enumerate() {
            if let Some(r) = inst.result {
                define(&mut def_block, &mut def_pos, r, b, Some(i as u32))?;
            }
        }
        if block.term.is_none() {
            return Err(Error::MissingTerminator(b));
        }
    }

    // Pass 2: reachability from the entry.
    let mut reachable = vec![false; n];
    let mut stack = vec![entry];
    reachable[entry] = true;
    while let Some(b) = stack.pop() {
        if let Some(term) = &func.block(b as BlockId).term {
            for to in successors(term) {
                if (to as usize) < n && !reachable[to as usize] {
                    reachable[to as usize] = true;
                    stack.push(to as usize);
                }
            }
        }
    }
    for (b, ok) in reachable.iter().enumerate() {
        if !ok {
            return Err(Error::UnreachableBlock(b as BlockId));
        }
    }

    // Pass 3: dominance.
    let dom = dominators(func, n);

    // Pass 4: edges, arities and uses.
    for b in 0..n as BlockId {
        let block = func.block(b);
        for (i, inst) in block.insts.iter().enumerate() {
            for &arg in &inst.args {
                check_use(b, i as u32, arg, &def_block, &def_pos, &dom)?;
            }
        }
        let term_pos = block.insts.len() as u32;
        match block.term.as_ref() {
            Some(Term::Jump { target, args }) => {
                check_edge(func, b, *target, args.len())?;
                for &arg in args {
                    check_use(b, term_pos, arg, &def_block, &def_pos, &dom)?;
                }
            }
            Some(Term::Branch {
                cond,
                then_block,
                then_args,
                else_block,
                else_args,
            }) => {
                check_use(b, term_pos, *cond, &def_block, &def_pos, &dom)?;
                check_edge(func, b, *then_block, then_args.len())?;
                check_edge(func, b, *else_block, else_args.len())?;
                for &arg in then_args {
                    check_use(b, term_pos, arg, &def_block, &def_pos, &dom)?;
                }
                for &arg in else_args {
                    check_use(b, term_pos, arg, &def_block, &def_pos, &dom)?;
                }
            }
            Some(Term::Return(Some(v))) => {
                check_use(b, term_pos, *v, &def_block, &def_pos, &dom)?;
            }
            Some(Term::Throw(v)) => {
                check_use(b, term_pos, *v, &def_block, &def_pos, &dom)?;
            }
            Some(Term::Return(None) | Term::Unreachable) => {}
            // Pass 1 rejected a missing terminator.
            None => unreachable!("terminator checked in pass 1"),
        }
    }

    Ok(())
}

fn define(
    def_block: &mut [Option<BlockId>],
    def_pos: &mut [Option<u32>],
    v: ValueId,
    block: BlockId,
    pos: Option<u32>,
) -> Result<(), Error> {
    let Some(slot) = def_block.get_mut(v as usize) else {
        return Err(Error::ValueOutOfRange(v));
    };
    if slot.is_some() {
        return Err(Error::DuplicateDefinition { value: v });
    }
    *slot = Some(block);
    def_pos[v as usize] = pos;
    Ok(())
}

fn check_use(
    block: BlockId,
    pos: u32,
    value: ValueId,
    def_block: &[Option<BlockId>],
    def_pos: &[Option<u32>],
    dom: &[Vec<bool>],
) -> Result<(), Error> {
    let Some(&db) = def_block.get(value as usize) else {
        return Err(Error::ValueOutOfRange(value));
    };
    let Some(db) = db else {
        return Err(Error::UndefinedValue { block, value });
    };
    if db == block {
        return match def_pos[value as usize] {
            None => Ok(()),
            Some(dp) if dp < pos => Ok(()),
            Some(_) => Err(Error::UseNotDominated { block, value }),
        };
    }
    if dom[block as usize][db as usize] {
        Ok(())
    } else {
        Err(Error::UseNotDominated { block, value })
    }
}

fn check_edge(func: &Function, from: BlockId, to: BlockId, got: usize) -> Result<(), Error> {
    if to as usize >= func.block_count() {
        return Err(Error::EdgeOutOfRange { from, to });
    }
    let expected = func.block(to).params.len();
    if got != expected {
        return Err(Error::EdgeArity {
            from,
            to,
            expected,
            got,
        });
    }
    Ok(())
}

fn successors(term: &Term) -> Vec<BlockId> {
    match term {
        Term::Jump { target, .. } => vec![*target],
        Term::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        Term::Return(_) | Term::Throw(_) | Term::Unreachable => Vec::new(),
    }
}

fn dominators(func: &Function, n: usize) -> Vec<Vec<bool>> {
    let entry = func.entry() as usize;
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 0..n {
        if let Some(term) = &func.block(b as BlockId).term {
            for to in successors(term) {
                if (to as usize) < n {
                    preds[to as usize].push(b);
                }
            }
        }
    }

    let mut dom = vec![vec![true; n]; n];
    for (i, slot) in dom[entry].iter_mut().enumerate() {
        *slot = i == entry;
    }
    loop {
        let mut changed = false;
        for b in 0..n {
            if b == entry || preds[b].is_empty() {
                continue;
            }
            let mut next = vec![true; n];
            for (i, slot) in next.iter_mut().enumerate() {
                *slot = preds[b].iter().all(|&p| dom[p][i]);
            }
            next[b] = true;
            if next != dom[b] {
                dom[b] = next;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    dom
}

#[cfg(test)]
mod tests {
    use super::super::builder::Builder;
    use super::*;
    use crate::opt::ir::{Effects, Imm, Inst, Op, Type};

    fn const_int(b: &mut Builder<'_>, block: BlockId, v: i32) -> ValueId {
        b.emit(
            block,
            Op::Const,
            &[],
            Type::Int,
            Effects::pure(),
            Imm::Int(v),
        )
    }

    #[test]
    fn a_well_formed_loop_verifies() {
        let mut func = Function::new();
        let entry = func.entry();
        let header;
        let exit;
        {
            let mut b = Builder::new(&mut func);
            let zero = const_int(&mut b, entry, 0);
            let bound = const_int(&mut b, entry, 10);
            header = b.block();
            exit = b.block();
            let acc = b.param(header, Type::Int);
            let one = const_int(&mut b, header, 1);
            let sum = b.emit(
                header,
                Op::Add,
                &[acc, one],
                Type::Int,
                Effects::pure(),
                Imm::None,
            );
            let cond = b.emit(
                header,
                Op::Lt,
                &[acc, bound],
                Type::Bool,
                Effects::call(),
                Imm::None,
            );
            let res = b.param(exit, Type::Int);
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: header,
                    then_args: vec![sum],
                    else_block: exit,
                    else_args: vec![sum],
                },
            );
            b.term(exit, Term::Return(Some(res)));
            b.term(
                entry,
                Term::Jump {
                    target: header,
                    args: vec![zero],
                },
            );
        }
        assert_eq!(verify(&func), Ok(()));
    }

    #[test]
    fn a_missing_terminator_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = const_int(&mut b, entry, 1);
            b.term(entry, Term::Return(Some(v)));
            b.block();
        }
        assert_eq!(verify(&func), Err(Error::MissingTerminator(1)));
    }

    #[test]
    fn a_wrong_edge_arity_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let target = b.block();
            let _p = b.param(target, Type::Int);
            b.term(
                entry,
                Term::Jump {
                    target,
                    args: vec![],
                },
            );
            b.term(target, Term::Return(None));
        }
        assert_eq!(
            verify(&func),
            Err(Error::EdgeArity {
                from: 0,
                to: 1,
                expected: 1,
                got: 0
            })
        );
    }

    #[test]
    fn an_undefined_value_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let undef = b.value(Type::Int);
            b.term(entry, Term::Return(Some(undef)));
        }
        assert_eq!(
            verify(&func),
            Err(Error::UndefinedValue { block: 0, value: 0 })
        );
    }

    #[test]
    fn a_use_not_dominated_by_its_definition_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        let v;
        {
            let mut b = Builder::new(&mut func);
            let c = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Bool,
                Effects::pure(),
                Imm::Bool(true),
            );
            let left = b.block();
            let right = b.block();
            let join = b.block();
            b.term(
                entry,
                Term::Branch {
                    cond: c,
                    then_block: left,
                    then_args: vec![],
                    else_block: right,
                    else_args: vec![],
                },
            );
            v = const_int(&mut b, left, 1);
            b.term(
                left,
                Term::Jump {
                    target: join,
                    args: vec![],
                },
            );
            b.term(
                right,
                Term::Jump {
                    target: join,
                    args: vec![],
                },
            );
            b.term(join, Term::Return(Some(v)));
        }
        assert_eq!(
            verify(&func),
            Err(Error::UseNotDominated { block: 3, value: v })
        );
    }

    #[test]
    fn a_duplicate_definition_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        let v;
        {
            let mut b = Builder::new(&mut func);
            v = const_int(&mut b, entry, 1);
            b.term(entry, Term::Return(Some(v)));
        }
        func.block_mut(entry).insts.push(Inst {
            op: Op::Const,
            args: vec![],
            result: Some(v),
            ty: Type::Int,
            effects: Effects::pure(),
            imm: Imm::Int(2),
        });
        assert_eq!(verify(&func), Err(Error::DuplicateDefinition { value: v }));
    }

    #[test]
    fn an_unreachable_block_is_rejected() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            b.term(entry, Term::Return(None));
            let dead = b.block();
            b.term(dead, Term::Return(None));
        }
        assert_eq!(verify(&func), Err(Error::UnreachableBlock(1)));
    }
}
