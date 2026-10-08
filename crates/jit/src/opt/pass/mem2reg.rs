//! Promote a body's storable frame slots to SSA (`.notes/optimizing-tier-impl.md`
//! I5c-2c-iii-b).
//!
//! A callee lifted for a trial splice still reads and writes its own frame slots
//! (`Op::FrameLoad`/`Op::FrameStore`), which have no home in the caller's frame.
//! This pass promotes them: each store becomes a definition, each load a use of
//! the reaching definition, and a merge of two defs materializes a block
//! parameter (the IR's phi, its value passed on the incoming edges). After the
//! pass a callee with locals (`function f(x) { var t = x * 2; return t + 1; }`)
//! has no slot access left, so `pass/inline.rs` can splice it.
//!
//! Two guards keep it sound. A slot with a `TdzCheck` (a `let`/`const`) stays a
//! frame slot — the lexical check has no promotion. And every `FrameLoad` must
//! be **store-reached** (reachable only through a store), because a load that can
//! observe the binding's initial `undefined` has no value to materialize. Either
//! guard leaves the slot a frame slot, which makes the callee unspliceable —
//! never a wrong program.
//!
//! Only frame slots are touched, and a captured binding lives in the capture
//! context rather than the frame (`ScopeInfo::context_names`), so promotion
//! cannot divorce a slot from a closure that captured it.

use std::collections::{HashMap, HashSet};

use crate::opt::ir::{BlockId, Function, Imm, Op, Term, Type, ValueId};

/// Promote every storable frame slot in `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.block_count();
    if n == 0 {
        return false;
    }
    let preds = predecessors(func, n);
    let idom = immediate_dominators(&preds, n);
    let children = dom_children(&idom, n);
    let df = dominance_frontiers(&preds, &idom, n);

    let (candidates, tdz) = slots_with_stores(func);
    let mut changed = false;
    for slot in candidates {
        if tdz.contains(&slot) {
            continue;
        }
        if promote(func, slot, &preds, &df, &children) {
            changed = true;
        }
    }
    changed
}

/// Every slot with a `FrameStore`, sorted, and the set of slots a `TdzCheck`
/// reads (a lexical slot, never promoted).
fn slots_with_stores(func: &Function) -> (Vec<u32>, HashSet<u32>) {
    let mut stores = Vec::new();
    let mut tdz = HashSet::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            match inst.op {
                Op::FrameStore => {
                    if let Imm::Slot(slot) = inst.imm {
                        stores.push(slot);
                    }
                }
                Op::TdzCheck => {
                    if let Imm::Slot(slot) = inst.imm {
                        tdz.insert(slot);
                    }
                }
                _ => {}
            }
        }
    }
    stores.sort_unstable();
    stores.dedup();
    (stores, tdz)
}

/// Promote one slot, or return `false` (leaving the IR untouched) when it is not
/// promotable.
fn promote(
    func: &mut Function,
    slot: u32,
    preds: &[Vec<BlockId>],
    df: &[Vec<BlockId>],
    children: &[Vec<BlockId>],
) -> bool {
    let reach_in = store_reach(func, slot, preds);
    if !loads_are_store_reached(func, slot, &reach_in) {
        return false;
    }
    let entry = func.entry();
    let mut defs: Vec<BlockId> = Vec::new();
    for b in 0..func.block_count() as u32 {
        if block_stores(func, b, slot) {
            defs.push(b);
        }
    }
    if defs.is_empty() {
        return false;
    }

    // Iterated dominance frontier: a phi belongs at a merge of two defs. Only
    // store-reached merges qualify (a merge with an unassigned path would need
    // the `undefined` initial value), and the entry block cannot carry one
    // (`verify` forbids entry parameters) — a slot whose entry load needs the
    // loop-carried value is refused above, so dropping the entry phi is safe.
    let mut phi: HashMap<BlockId, usize> = HashMap::new();
    let mut queued: HashSet<BlockId> = defs.iter().copied().collect();
    let mut work = defs.clone();
    while let Some(x) = work.pop() {
        for &y in &df[x as usize] {
            if y == entry || !reach_in[y as usize] || phi.contains_key(&y) {
                continue;
            }
            let index = func.block(y).params.len();
            let v = func.push_value(Type::Unknown);
            func.block_mut(y).params.push(v);
            phi.insert(y, index);
            if queued.insert(y) {
                work.push(y);
            }
        }
    }

    // Every predecessor edge into a phi block gains one argument slot; the
    // placeholder is overwritten during the rename, but a value id with no
    // definition would fail `verify` rather than pass through silently.
    for (&m, &index) in &phi {
        debug_assert_eq!(func.block(m).params.len(), index + 1);
        let placeholder = func.push_value(Type::Unknown);
        for &p in &preds[m as usize] {
            append_edge_arg(func, p, m, placeholder);
        }
    }

    let mut stack: Vec<ValueId> = Vec::new();
    let mut repl: HashMap<ValueId, ValueId> = HashMap::new();
    rename(func, entry, slot, &phi, children, &mut stack, &mut repl);
    apply(func, slot, &repl);
    true
}

/// The dominator-tree walk that rewrites the slot's loads to their reaching
/// definitions and fills the phi edge arguments.
fn rename(
    func: &mut Function,
    b: BlockId,
    slot: u32,
    phi: &HashMap<BlockId, usize>,
    children: &[Vec<BlockId>],
    stack: &mut Vec<ValueId>,
    repl: &mut HashMap<ValueId, ValueId>,
) {
    let mut pushed = 0;
    if let Some(&index) = phi.get(&b) {
        stack.push(func.block(b).params[index]);
        pushed += 1;
    }
    let len = func.block(b).insts.len();
    for i in 0..len {
        let inst = &func.block(b).insts[i];
        if inst.op == Op::FrameStore && inst.imm == Imm::Slot(slot) {
            let operand = resolve(repl, inst.args[0]);
            stack.push(operand);
            pushed += 1;
        } else if inst.op == Op::FrameLoad && inst.imm == Imm::Slot(slot) {
            let result = inst.result.expect("a store-reached load has a result");
            let reaching = *stack
                .last()
                .expect("a store-reached load has a reaching definition");
            repl.insert(result, reaching);
        }
    }
    let reach = stack.last().copied();
    match func.block_mut(b).term.as_mut() {
        Some(Term::Jump { target, args }) => {
            if let (Some(&index), Some(r)) = (phi.get(target), reach) {
                args[index] = r;
            }
        }
        Some(Term::Branch {
            then_block,
            then_args,
            else_block,
            else_args,
            ..
        }) => {
            if let (Some(&index), Some(r)) = (phi.get(then_block), reach) {
                then_args[index] = r;
            }
            if let (Some(&index), Some(r)) = (phi.get(else_block), reach) {
                else_args[index] = r;
            }
        }
        _ => {}
    }
    for &c in &children[b as usize] {
        rename(func, c, slot, phi, children, stack, repl);
    }
    for _ in 0..pushed {
        stack.pop();
    }
}

/// Redirect every use of a rewritten load and drop the slot's loads and stores.
fn apply(func: &mut Function, slot: u32, repl: &HashMap<ValueId, ValueId>) {
    for b in 0..func.block_count() as u32 {
        for inst in &mut func.block_mut(b).insts {
            for a in &mut inst.args {
                *a = resolve(repl, *a);
            }
        }
        match func.block_mut(b).term.as_mut() {
            Some(Term::Jump { args, .. }) => {
                for a in args.iter_mut() {
                    *a = resolve(repl, *a);
                }
            }
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                *cond = resolve(repl, *cond);
                for a in then_args.iter_mut().chain(else_args) {
                    *a = resolve(repl, *a);
                }
            }
            Some(Term::Return(Some(v)) | Term::Throw(v)) => *v = resolve(repl, *v),
            _ => {}
        }
        func.block_mut(b).insts.retain(|inst| {
            !(matches!(inst.op, Op::FrameLoad | Op::FrameStore) && inst.imm == Imm::Slot(slot))
        });
    }
}

/// Follow load replacements to a canonical value (no chains survive the rename,
/// but the walk is defensive against a cycle).
fn resolve(repl: &HashMap<ValueId, ValueId>, v: ValueId) -> ValueId {
    let mut cur = v;
    let mut guard = 0;
    while let Some(&next) = repl.get(&cur) {
        if next == cur || guard > repl.len() {
            break;
        }
        cur = next;
        guard += 1;
    }
    cur
}

/// Whether `slot` has a store in `b`.
fn block_stores(func: &Function, b: BlockId, slot: u32) -> bool {
    func.block(b)
        .insts
        .iter()
        .any(|i| i.op == Op::FrameStore && i.imm == Imm::Slot(slot))
}

/// The "assigned on every path" dataflow: `reach_in[b]` is true when every path
/// from the entry to `b` passed a store of `slot`.
fn store_reach(func: &Function, slot: u32, preds: &[Vec<BlockId>]) -> Vec<bool> {
    let n = func.block_count();
    let entry = func.entry() as usize;
    let mut reach_in = vec![true; n];
    let mut reach_out = vec![false; n];
    reach_in[entry] = false;
    loop {
        let mut changed = false;
        for b in 0..n {
            let rin = if b == entry || preds[b].is_empty() {
                false
            } else {
                preds[b].iter().all(|&p| reach_out[p as usize])
            };
            let rout = rin || block_stores(func, b as BlockId, slot);
            if rin != reach_in[b] || rout != reach_out[b] {
                reach_in[b] = rin;
                reach_out[b] = rout;
                changed = true;
            }
        }
        if !changed {
            return reach_in;
        }
    }
}

/// Whether every load of `slot` is store-reached, and each store/load has the
/// shape the rename assumes (one operand, a result).
fn loads_are_store_reached(func: &Function, slot: u32, reach_in: &[bool]) -> bool {
    for b in 0..func.block_count() as u32 {
        let mut assigned = reach_in[b as usize];
        for inst in &func.block(b).insts {
            if inst.op == Op::FrameStore && inst.imm == Imm::Slot(slot) {
                if inst.args.len() != 1 {
                    return false;
                }
                assigned = true;
            } else if inst.op == Op::FrameLoad
                && inst.imm == Imm::Slot(slot)
                && (inst.result.is_none() || !assigned)
            {
                return false;
            }
        }
    }
    true
}

fn predecessors(func: &Function, n: usize) -> Vec<Vec<BlockId>> {
    let mut preds = vec![Vec::new(); n];
    for b in 0..n as u32 {
        if let Some(term) = func.block(b).term.as_ref() {
            for to in term_successors(term) {
                if (to as usize) < n {
                    preds[to as usize].push(b);
                }
            }
        }
    }
    preds
}

fn term_successors(term: &Term) -> Vec<BlockId> {
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

/// The immediate dominator of every block, from the dominator sets (the closest
/// strict dominator — the one every other strict dominator dominates).
fn immediate_dominators(preds: &[Vec<BlockId>], n: usize) -> Vec<usize> {
    let entry = 0;
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
                *slot = preds[b].iter().all(|&p| dom[p as usize][i]);
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
    let mut idom = vec![usize::MAX; n];
    idom[entry] = entry;
    for b in 0..n {
        if b == entry || preds[b].is_empty() {
            continue;
        }
        let strict: Vec<usize> = (0..n).filter(|&a| a != b && dom[b][a]).collect();
        // The immediate dominator is every other strict dominator's dominator.
        for &d in &strict {
            if strict.iter().all(|&o| o == d || dom[d][o]) {
                idom[b] = d;
                break;
            }
        }
    }
    idom
}

fn dominance_frontiers(preds: &[Vec<BlockId>], idom: &[usize], n: usize) -> Vec<Vec<BlockId>> {
    let mut df = vec![Vec::new(); n];
    for b in 0..n {
        if preds[b].len() < 2 {
            continue;
        }
        let stop = idom[b];
        for &p in &preds[b] {
            let mut runner = p as usize;
            let mut steps = 0;
            while runner != stop && steps <= n {
                df[runner].push(b as BlockId);
                runner = idom[runner];
                steps += 1;
            }
        }
    }
    df
}

fn dom_children(idom: &[usize], n: usize) -> Vec<Vec<BlockId>> {
    let mut children = vec![Vec::new(); n];
    for (b, &d) in idom.iter().enumerate() {
        if d != usize::MAX && d != b {
            children[d].push(b as BlockId);
        }
    }
    children
}

/// Append one argument to every edge `from -> to`.
fn append_edge_arg(func: &mut Function, from: BlockId, to: BlockId, v: ValueId) {
    match func.block_mut(from).term.as_mut() {
        Some(Term::Jump { target, args }) => {
            if *target == to {
                args.push(v);
            }
        }
        Some(Term::Branch {
            then_block,
            then_args,
            else_block,
            else_args,
            ..
        }) => {
            if *then_block == to {
                then_args.push(v);
            }
            if *else_block == to {
                else_args.push(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Heap};
    use crate::opt::verify::verify;

    fn store(b: &mut Builder<'_>, block: BlockId, slot: u32, v: ValueId) {
        b.emit_void(
            block,
            Op::FrameStore,
            &[v],
            Effects::write(Heap::Slots),
            Imm::Slot(slot),
        );
    }

    fn load(b: &mut Builder<'_>, block: BlockId, slot: u32) -> ValueId {
        b.emit(
            block,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(Heap::Slots),
            Imm::Slot(slot),
        )
    }

    fn has_slot_access(func: &Function, slot: u32) -> bool {
        (0..func.block_count() as u32).any(|b| {
            func.block(b)
                .insts
                .iter()
                .any(|i| matches!(i.op, Op::FrameLoad | Op::FrameStore) && i.imm == Imm::Slot(slot))
        })
    }

    #[test]
    fn promotes_a_straight_line_local() {
        // `var t = 7; return t;` — the load reads the stored constant, so the
        // slot access disappears and the return reads the constant directly.
        let mut func = Function::new();
        let e = func.entry();
        let seven;
        {
            let mut b = Builder::new(&mut func);
            seven = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(7.0),
            );
            store(&mut b, e, 2, seven);
            let t = load(&mut b, e, 2);
            b.term(e, Term::Return(Some(t)));
        }
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        assert!(!has_slot_access(&func, 2));
        match func.block(e).term.as_ref() {
            Some(Term::Return(Some(v))) => assert_eq!(*v, seven),
            other => panic!("unexpected terminator {other:?}"),
        }
    }

    #[test]
    fn leaves_a_load_that_precedes_any_store() {
        // `var t; return t;` — the load can observe `undefined`, which the IR
        // cannot materialize, so the slot stays a frame slot.
        let mut func = Function::new();
        let e = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let t = load(&mut b, e, 2);
            b.term(e, Term::Return(Some(t)));
        }
        assert!(!run(&mut func));
        assert!(has_slot_access(&func, 2));
    }

    #[test]
    fn promotes_a_branchy_local_with_a_phi() {
        // `var t; if (c) { t = 1; } else { t = 2; } return t;` — the merge
        // carries the joined value through a block parameter.
        let mut func = Function::new();
        let e = func.entry();
        let join;
        {
            let mut b = Builder::new(&mut func);
            let cond = b.emit(
                e,
                Op::Const,
                &[],
                Type::Bool,
                Effects::pure(),
                Imm::Bool(true),
            );
            let then = b.block();
            let els = b.block();
            join = b.block();
            b.term(
                e,
                Term::Branch {
                    cond,
                    then_block: then,
                    then_args: vec![],
                    else_block: els,
                    else_args: vec![],
                },
            );
            let one = b.emit(
                then,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            store(&mut b, then, 2, one);
            b.term(
                then,
                Term::Jump {
                    target: join,
                    args: vec![],
                },
            );
            let two = b.emit(
                els,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            store(&mut b, els, 2, two);
            b.term(
                els,
                Term::Jump {
                    target: join,
                    args: vec![],
                },
            );
            let t = load(&mut b, join, 2);
            b.term(join, Term::Return(Some(t)));
        }
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        assert!(!has_slot_access(&func, 2));
        let phi = *func.block(join).params.first().expect("a phi at the join");
        match func.block(join).term.as_ref() {
            Some(Term::Return(Some(v))) => assert_eq!(*v, phi, "the join returns the phi"),
            other => panic!("unexpected terminator {other:?}"),
        }
    }

    #[test]
    fn promotes_a_loop_carried_local() {
        // `var i = 0; while (i < n) { i = i + 1; } return i;` — the loop header
        // merges the initial value and the incremented one through a phi. The
        // body must read the phi, not the entry value (a dominator-tree bug put
        // the body under the entry instead of the header, which `verify` cannot
        // see).
        let mut func = Function::new();
        let e = func.entry();
        let header;
        let body;
        {
            let mut b = Builder::new(&mut func);
            let zero = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(0.0),
            );
            store(&mut b, e, 2, zero);
            header = b.block();
            body = b.block();
            let exit = b.block();
            b.term(
                e,
                Term::Jump {
                    target: header,
                    args: vec![],
                },
            );
            let i = load(&mut b, header, 2);
            let bound = b.emit(
                header,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(10.0),
            );
            let cond = b.emit(
                header,
                Op::Lt,
                &[i, bound],
                Type::Bool,
                Effects::call(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond,
                    then_block: body,
                    then_args: vec![],
                    else_block: exit,
                    else_args: vec![],
                },
            );
            let j = load(&mut b, body, 2);
            let one = b.emit(
                body,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            let inc = b.emit(
                body,
                Op::Add,
                &[j, one],
                Type::Number,
                Effects::call(),
                Imm::None,
            );
            store(&mut b, body, 2, inc);
            b.term(
                body,
                Term::Jump {
                    target: header,
                    args: vec![],
                },
            );
            let out = load(&mut b, exit, 2);
            b.term(exit, Term::Return(Some(out)));
        }
        assert!(run(&mut func));
        assert_eq!(verify(&func), Ok(()));
        assert!(!has_slot_access(&func, 2));
        let phi = *func
            .block(header)
            .params
            .first()
            .expect("a phi at the header");
        let add = func
            .block(body)
            .insts
            .iter()
            .find(|i| i.op == Op::Add)
            .expect("the body's increment");
        assert_eq!(
            add.args[0], phi,
            "the body reads the loop phi, not the entry value"
        );
    }

    #[test]
    fn leaves_a_tdz_slot_alone() {
        // A `let`/`const` slot carries a `TdzCheck`; the check has no promotion.
        let mut func = Function::new();
        let e = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = b.emit(
                e,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(3.0),
            );
            b.emit_void(
                e,
                Op::TdzCheck,
                &[],
                Effects::read(Heap::Slots),
                Imm::Slot(2),
            );
            store(&mut b, e, 2, v);
            let t = load(&mut b, e, 2);
            b.term(e, Term::Return(Some(t)));
        }
        assert!(!run(&mut func));
        assert!(has_slot_access(&func, 2));
    }

    #[test]
    fn promotes_nothing_without_a_store() {
        let mut func = Function::new();
        let e = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let t = load(&mut b, e, 2);
            b.term(e, Term::Return(Some(t)));
        }
        assert!(!run(&mut func));
    }
}
