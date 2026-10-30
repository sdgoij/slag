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

use crate::opt::ir::{BlockId, Effects, Function, Heap, Imm, Inst, Op, Term, Type, ValueId};

/// Promote every storable frame slot in `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    promote(func, None)
}

/// Promote only the slots `allow` names (`.notes/opt-per-op-overhead.md` §8).
/// The top-level pipeline uses it to promote a sealed body's slots, restricted
/// to those that never hold a heap value: a frame slot is a GC root, so
/// promoting a heap value into an untraced register would unroot it.
pub fn run_restricted(func: &mut Function, allow: &HashSet<u32>) -> bool {
    promote(func, Some(allow))
}

fn promote(func: &mut Function, allow: Option<&HashSet<u32>>) -> bool {
    let n = func.block_count();
    if n == 0 {
        return false;
    }
    let preds = predecessors(func, n);
    let idom = immediate_dominators(&preds, n);
    let children = dom_children(&idom, n);
    let df = dominance_frontiers(&preds, &idom, n);

    let (candidates, tdz) = slots_with_stores(func);
    let trace = trace_mem2reg();
    let mut changed = false;
    for slot in candidates {
        if tdz.contains(&slot) {
            continue;
        }
        let allowed = allow.is_none_or(|a| a.contains(&slot));
        let promoted = allowed && promote_slot(func, slot, &preds, &df, &children);
        if trace {
            eprintln!("mem2reg slot {slot} allowed {allowed} promoted {promoted}");
        }
        if promoted {
            changed = true;
        }
    }
    changed
}

fn trace_mem2reg() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("SLAG_MEM2REG_TRACE").is_some())
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
fn promote_slot(
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

    let mut state = Rename::default();
    rename(func, entry, slot, &phi, children, &mut state);
    retype_phis(func, &phi, preds);
    let removed = apply(func, slot, &state.repl);
    insert_flushes(func, slot, &state.flushes, &removed);
    true
}

/// Materialize the slot's promoted value into its frame slot before every
/// guard, because a guard that fails resumes the interpreter, and the
/// interpreter reads the frame (`.notes/opt-per-op-overhead.md` §8, Cut B).
/// Without this a promoted slot is invisible to the resumed code.
///
/// `flushes` holds `(block, instruction index, value)` positions as they were
/// before `apply` dropped the slot's loads and stores, so `removed` (the
/// indices `apply` deleted, per block) shifts each one back into place.
fn insert_flushes(
    func: &mut Function,
    slot: u32,
    flushes: &[(BlockId, usize, ValueId)],
    removed: &HashMap<BlockId, Vec<usize>>,
) {
    if flushes.is_empty() {
        return;
    }
    let mut by_block: HashMap<BlockId, Vec<(usize, ValueId)>> = HashMap::new();
    for &(b, at, value) in flushes {
        by_block.entry(b).or_default().push((at, value));
    }
    for (b, mut list) in by_block {
        list.sort_unstable_by_key(|&(at, _)| at);
        let gone = removed.get(&b).map_or(&[][..], |v| v.as_slice());
        for (offset, (at, value)) in list.into_iter().enumerate() {
            let shifted = at - gone.iter().filter(|&&r| r < at).count();
            let position = (shifted + offset).min(func.block(b).insts.len());
            func.block_mut(b).insts.insert(
                position,
                Inst {
                    op: Op::FrameStore,
                    args: vec![value],
                    result: None,
                    ty: Type::Never,
                    effects: Effects::write(Heap::Slots),
                    imm: Imm::Slot(slot),
                },
            );
        }
    }
}

/// Re-type each promoted phi as the join of its incoming values' types.
///
/// The placeholder is created before the rename fills the edges, when the
/// incoming types are not yet known, so it starts `Unknown`. `narrow` ran before
/// promotion and never sees the new parameter, so without this the lowering
/// cannot recognize a loop-carried value (an `Int` accumulator) as `Int` — the
/// representation work depends on it.
///
/// A promoted loop can have several phis feeding each other (a `for` header and
/// its latch both carry the accumulator), and `join(Unknown, t)` is `Unknown`, so
/// a single round would leave every phi of that cycle untyped — which then
/// un-types the arithmetic consuming it. Join from `Never` (the identity of
/// `join`) and iterate instead: the phis of one promotion are a finite lattice
/// chain, so this converges in at most one round per phi.
fn retype_phis(func: &mut Function, phi: &HashMap<BlockId, usize>, preds: &[Vec<BlockId>]) {
    let mut params: Vec<(BlockId, usize, ValueId)> = phi
        .iter()
        .map(|(&m, &index)| (m, index, func.block(m).params[index]))
        .collect();
    params.sort_unstable_by_key(|&(m, index, _)| (m, index));
    let mut types: HashMap<ValueId, Type> =
        params.iter().map(|&(_, _, v)| (v, Type::Never)).collect();
    for _ in 0..=params.len() {
        let mut changed = false;
        for &(m, index, param) in &params {
            let mut ty = Type::Never;
            // Whether every incoming value is an int32: an `Int`-typed one, or a
            // constant whose value is integral and inside the i32 range. A loop
            // counter's preheader init is the usual `Const 0`, which `narrow`
            // types `Number` (its only uses are a comparison and this edge), so
            // the plain join kept the counter in a `Value` word and
            // `loop_counter`'s `IntAdd` increment then round-tripped i32 -> i64
            // -> i32 every iteration. A phi that is not resolved yet (`Never`)
            // must not break the claim for the phi it feeds.
            let mut all_int = true;
            for &p in &preds[m as usize] {
                let Some(v) = edge_arg(func, p, m, index) else {
                    continue;
                };
                let incoming = types.get(&v).copied().unwrap_or_else(|| func.value_type(v));
                ty = ty.join(incoming);
                all_int &= matches!(incoming, Type::Int | Type::Never) || is_int32_const(func, v);
            }
            if ty == Type::Never {
                continue;
            }
            if all_int {
                ty = Type::Int;
            }
            if types[&param] != ty {
                types.insert(param, ty);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for &(_, _, param) in &params {
        if let Some(&ty) = types.get(&param) {
            func.set_value_type(param, ty);
        }
    }
}

/// Whether `v` is a constant instruction holding an integral value inside the
/// i32 range.
fn is_int32_const(func: &Function, v: ValueId) -> bool {
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.result == Some(v) {
                return inst.op == Op::Const && crate::opt::pass::narrow::is_int_const(&inst.imm);
            }
        }
    }
    false
}

/// The argument a `from -> to` edge passes for the target's parameter `index`.
fn edge_arg(func: &Function, from: BlockId, to: BlockId, index: usize) -> Option<ValueId> {
    match &func.block(from).term {
        Some(Term::Jump { target, args }) if *target == to => args.get(index).copied(),
        Some(Term::Branch {
            then_block,
            then_args,
            else_block,
            else_args,
            ..
        }) => {
            if *then_block == to {
                then_args.get(index).copied()
            } else if *else_block == to {
                else_args.get(index).copied()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The mutable state the dominator-tree walk carries: the reaching-definition
/// stack, the load rewrite map, and the guard flush positions.
#[derive(Default)]
struct Rename {
    stack: Vec<ValueId>,
    repl: HashMap<ValueId, ValueId>,
    flushes: Vec<(BlockId, usize, ValueId)>,
}

/// The dominator-tree walk that rewrites the slot's loads to their reaching
/// definitions and fills the phi edge arguments.
fn rename(
    func: &mut Function,
    b: BlockId,
    slot: u32,
    phi: &HashMap<BlockId, usize>,
    children: &[Vec<BlockId>],
    state: &mut Rename,
) {
    let mut pushed = 0;
    if let Some(&index) = phi.get(&b) {
        state.stack.push(func.block(b).params[index]);
        pushed += 1;
    }
    let len = func.block(b).insts.len();
    // The value this block last flushed for the slot: a run of guards needs one
    // materialization, not one each (a guarded element read deopts from inside
    // its own lowering, so a read followed by its type guard is two adjacent
    // deopt points over the same frame).
    let mut flushed: Option<ValueId> = None;
    for i in 0..len {
        let inst = &func.block(b).insts[i];
        // A guard may deopt to the interpreter, which reads this slot's frame
        // word: record the value to materialize there. A guard before the
        // slot's first store on this path leaves the initial `undefined`,
        // which is exactly what the frame already holds. A guarded element read
        // (`Imm::Guard`) deopts from inside its own lowering, so it is a
        // materialization point too.
        let is_deopt_point = matches!(inst.op, Op::Check | Op::GuardType | Op::GuardCallee)
            || (inst.op == Op::ElementLoad && matches!(inst.imm, Imm::Guard { .. }));
        if is_deopt_point
            && let Some(&current) = state.stack.last()
            && flushed != Some(current)
        {
            state.flushes.push((b, i, current));
            flushed = Some(current);
        }
        if inst.op == Op::FrameStore && inst.imm == Imm::Slot(slot) {
            let operand = resolve(&state.repl, inst.args[0]);
            state.stack.push(operand);
            flushed = None;
            pushed += 1;
        } else if inst.op == Op::FrameLoad && inst.imm == Imm::Slot(slot) {
            let result = inst.result.expect("a store-reached load has a result");
            let reaching = *state
                .stack
                .last()
                .expect("a store-reached load has a reaching definition");
            state.repl.insert(result, reaching);
        }
    }
    let reach = state.stack.last().copied();
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
        rename(func, c, slot, phi, children, state);
    }
    for _ in 0..pushed {
        state.stack.pop();
    }
}

/// Redirect every use of a rewritten load and drop the slot's loads and stores.
/// Returns the dropped instruction indices per block, so a flush recorded
/// against the pre-drop layout can be placed back at the right spot.
fn apply(
    func: &mut Function,
    slot: u32,
    repl: &HashMap<ValueId, ValueId>,
) -> HashMap<BlockId, Vec<usize>> {
    let mut removed: HashMap<BlockId, Vec<usize>> = HashMap::new();
    for b in 0..func.block_count() as u32 {
        for (i, inst) in func.block(b).insts.iter().enumerate() {
            if matches!(inst.op, Op::FrameLoad | Op::FrameStore) && inst.imm == Imm::Slot(slot) {
                removed.entry(b).or_default().push(i);
            }
        }
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
    removed
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
///
/// This is a forward *must* analysis, so it is the GREATEST fixpoint of
/// "in = AND over predecessors' out". Iterating upward from `out = false` (the
/// least fixpoint) reports every block of a loop as not store-reached whenever
/// the header has a back edge, because the back edge's `out` depends on the
/// header's own `in`: a counting loop's accumulator, stored once before the
/// loop and reloaded inside it, is never promoted. Starting from `out = true`
/// and shrinking converges to the same answer the discarded-least version gives
/// for a store-dominated chain, plus the loop case the semantics allow.
fn store_reach(func: &Function, slot: u32, preds: &[Vec<BlockId>]) -> Vec<bool> {
    let n = func.block_count();
    let entry = func.entry() as usize;
    let reachable = reachable_blocks(func, n);
    let mut reach_in = vec![true; n];
    let mut reach_out = vec![true; n];
    for b in 0..n {
        if !reachable[b] {
            reach_in[b] = false;
            reach_out[b] = false;
        }
    }
    loop {
        let mut changed = false;
        for b in 0..n {
            let rin = if b == entry || !reachable[b] {
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

/// The blocks reachable from the entry over terminator edges. A block outside
/// this set has no definition to promote: `rename` walks the dominator tree
/// from the entry, so it never assigns one, and `apply` would drop the block's
/// loads without a replacement.
fn reachable_blocks(func: &Function, n: usize) -> Vec<bool> {
    let mut seen = vec![false; n];
    let mut work = vec![func.entry()];
    seen[func.entry() as usize] = true;
    while let Some(b) = work.pop() {
        let Some(term) = func.block(b).term.as_ref() else {
            continue;
        };
        for to in term_successors(term) {
            if (to as usize) < n && !seen[to as usize] {
                seen[to as usize] = true;
                work.push(to);
            }
        }
    }
    seen
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
