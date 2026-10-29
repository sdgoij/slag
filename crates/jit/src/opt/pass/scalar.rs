//! Scalar replacement of non-escaping object literals.
//!
//! A whole-literal `NewObject` (`Step::ObjectFast`) or `NewArray`
//! (`Step::ArrayFast`) whose result is only ever read through named members or
//! constant element indexes is work V8 eliminates (TurboFan's escape analysis
//! and scalar replacement). The object is fresh, its keys are static
//! data properties, and nothing can observe it: every read can be served by
//! the SSA value the literal stored under that key.
//!
//! The literal is usually assigned to a local, so the object's first use is a
//! `FrameStore` into a private slot (mem2reg deliberately does not promote
//! heap-valued slots: a frame slot is a GC root). The pass therefore forwards
//! through such a slot block-locally  -  a `FrameLoad` after the store aliases
//! the object  -  and treats the slot as an escape route only when it cannot
//! account for every load.
//!
//! The pass replaces a read only when:
//!
//! - the object reaches the read through the literal itself or one such slot
//!   alias, with no other use in the function (a call argument, return, second
//!   store, or edge argument is an escape and bails),
//! - each `MemberCellLoad`'s result is used by exactly one `MemberGuard` of
//!   the same object and atom (the shape `emit_named_read` produces), and
//! - the atom maps to a literal value (a read of an inherited property has no
//!   literal value and bails).
//!
//! `Step::ObjectFast` already guarantees static, unique, non-`__proto__` data
//! keys, so a fresh object cannot have an accessor or a shadowed key: the
//! mapped SSA value IS the read. Replacing the guard's result with it makes
//! the cell probe and its fallback helper dead; when every read is replaced,
//! the slot store and the allocation are dead too.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use runtime::ir::{CompiledBody, Step};

use crate::opt::ir::{Block, Effects, Function, Imm, Op, Term, Type, ValueId};

/// The reads this pass has replaced (the opt-tier tests assert the pass ran).
pub(crate) static REPLACED: AtomicU64 = AtomicU64::new(0);

/// Whether the pass runs. On by default; `SLAG_SCALAR=0` disables it  -  the
/// same-binary seam for measuring the pass's marginal effect.
fn enabled() -> bool {
    std::env::var("SLAG_SCALAR")
        .map(|value| value != "0")
        .unwrap_or(true)
}

/// Whether [`promote_loop_carried_field`] runs. On by default;
/// `SLAG_SCALAR_LOOP=0` disables only this half  -  the seam that isolates a
/// change to it from the pass's other rewrites and from the build's layout.
fn loop_carried_enabled() -> bool {
    std::env::var("SLAG_SCALAR_LOOP")
        .map(|value| value != "0")
        .unwrap_or(true)
}

/// Replace non-escaping object literals with their field values. Returns
/// whether the IR changed.
pub fn run(func: &mut Function, body: &CompiledBody) -> bool {
    if !enabled() {
        return false;
    }
    // A replaced read can expose the next literal (an inner object read back
    // through a slot), so iterate to a small fixed point.
    let mut changed = false;
    let loop_carried = loop_carried_enabled();
    for _ in 0..4 {
        if !(one_pass(func, body) || (loop_carried && promote_loop_carried_field(func, body))) {
            break;
        }
        changed = true;
    }
    changed
}

/// Promote a non-escaping object's loop-carried field to a loop phi, then elide
/// the object. The `compound_assign` shape (`o.x += 1; s += o.x` across loop
/// iterations) reads the field before its store, so `one_pass` has no preceding
/// store to forward and declines, leaving the object allocated. The field's
/// value at the loop header is a phi (the initial literal on the entry edge, the
/// stored value on the back-edge).
///
/// The gate is deliberately narrow: a header that is its own only back-edge with
/// exactly one entry predecessor, the field's load and store both in the header
/// with the load first, and [`only_these_sites`] admitting the elision. It
/// returns after one promotion; `run` iterates.
fn promote_loop_carried_field(func: &mut Function, body: &CompiledBody) -> bool {
    let mut objects = literal_objects(func, body);
    if objects.is_empty() {
        return false;
    }
    let arrays = literal_arrays(func, body);
    let roots: HashSet<ValueId> = objects.keys().chain(arrays.keys()).copied().collect();
    let aliases = slot_aliases(func, &roots);
    let escaping = escaping_literals(func, &objects, &arrays, &aliases);
    objects.retain(|object, _| !escaping.contains(object));
    if objects.is_empty() {
        return false;
    }
    let n = func.block_count() as u32;
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n as usize];
    for b in 0..n {
        for p in edge_targets(func, b) {
            preds[p as usize].push(b);
        }
    }
    for h in 0..n {
        // The header must be its own back-edge with exactly one other
        // predecessor (the entry edge from outside the loop).
        if preds[h as usize].len() != 2 || !preds[h as usize].contains(&h) {
            continue;
        }
        let Some(entry) = preds[h as usize].iter().copied().find(|&p| p != h) else {
            continue;
        };
        for (&object, entries) in &objects {
            for &(atom, init) in entries {
                let Some((load, load_at)) =
                    find_member_load(func, h, object, atom, &objects, &arrays, &aliases)
                else {
                    continue;
                };
                let Some((store_at, store_value)) = find_member_store_after(
                    func, h, load_at, object, atom, &objects, &arrays, &aliases,
                ) else {
                    continue;
                };
                let Some(guard) = func.block(h).insts.iter().find_map(|inst| {
                    (inst.op == Op::MemberGuard && inst.args.len() == 2 && inst.args[1] == load)
                        .then_some(inst.result)
                        .flatten()
                }) else {
                    continue;
                };
                if !only_these_sites(
                    func, object, atom, h, load, guard, store_at, &objects, &arrays, &aliases,
                ) {
                    continue;
                }
                let param = func.push_value(Type::Unknown);
                // The guard's result is the value the parameter replaces, so the
                // back-edge arithmetic still reads the guard at this point.
                let carried = carried_type(func, init, guard, store_value);
                func.set_value_type(param, carried);
                func.block_mut(h).params.push(param);
                append_edge_arg(func, entry, h, init);
                append_edge_arg(func, h, h, store_value);
                let mut subst = vec![None; func.value_count() as usize];
                subst[guard as usize] = Some(param);
                rewrite_uses(func, &subst);
                func.block_mut(h).insts.remove(store_at);
                strip_instructions(func, &[guard, load]);
                let replaced: HashSet<ValueId> = std::iter::once(object).collect();
                drop_dead_stores(func, &replaced);
                strip_dead_literals(func, &replaced);
                REPLACED.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
    }
    false
}

/// The type of a loop-carried field: `Number` when the value entering the loop
/// is numeric and the back-edge value is a numeric-preserving arithmetic op over
/// the carried value and numeric operands.
///
/// The proof is an induction. The value entering the loop is numeric, and every
/// arithmetic op returns a Number when all of its operands are Numbers, so a
/// Number can never become a String through the cycle. Typing it is what lets
/// the lowering drop the per-iteration tag checks, and it lets the slot fixpoint
/// type the accumulator the field feeds.
fn carried_type(func: &Function, init: ValueId, carried: ValueId, store_value: ValueId) -> Type {
    if !func.value_type(init).is_numeric() {
        return Type::Unknown;
    }
    let Some((op, args)) = defining_op(func, store_value) else {
        return Type::Unknown;
    };
    if !matches!(
        op,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow | Op::Neg | Op::ToNumber
    ) || args.is_empty()
        || !args
            .iter()
            .all(|&arg| arg == carried || func.value_type(arg).is_numeric())
    {
        return Type::Unknown;
    }
    Type::Number
}

/// The op and arguments of the instruction that produced `value`.
fn defining_op(func: &Function, value: ValueId) -> Option<(Op, Vec<ValueId>)> {
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.result == Some(value) {
                return Some((inst.op, inst.args.clone()));
            }
        }
    }
    None
}

/// Whether promoting `object`'s `atom` field may elide the object: the whole body
/// may hold no reference to it other than the `load`, `guard` and `store` the
/// promotion removes, and no guard op other than that `guard`.
///
/// A guard is a deopt point, and a deopt resumes the interpreter with the frame,
/// where an elided object no longer is. A read that survives the promotion would
/// read a field the removed store left at its old value, so the object could only
/// be elided if every such read were also removed.
#[allow(clippy::too_many_arguments)]
fn only_these_sites(
    func: &Function,
    object: ValueId,
    atom: u32,
    header: u32,
    load: ValueId,
    guard: ValueId,
    store_at: usize,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> bool {
    if uses_of(func, load) != 1 {
        return false;
    }
    for b in 0..func.block_count() as u32 {
        for (i, inst) in func.block(b).insts.iter().enumerate() {
            if matches!(
                inst.op,
                Op::GuardType | Op::Check | Op::GuardCallee | Op::CalleeEq
            ) {
                return false;
            }
            let touches = inst
                .args
                .iter()
                .any(|&arg| root_of_any(arg, objects, arrays, aliases) == Some((object, false)));
            if !touches {
                continue;
            }
            let is_site = match inst.op {
                Op::FrameStore | Op::FrameLoad => true,
                Op::MemberCellLoad => inst.imm == Imm::Atom(atom) && inst.result == Some(load),
                Op::MemberGuard => inst.result == Some(guard),
                Op::MemberStore => b == header && i == store_at,
                _ => false,
            };
            if !is_site {
                return false;
            }
        }
    }
    true
}

/// The successor blocks of `b` (a self-loop listed once).
fn edge_targets(func: &Function, b: u32) -> Vec<u32> {
    match func.block(b).term.as_ref() {
        Some(Term::Jump { target, .. }) => vec![*target],
        Some(Term::Branch {
            then_block,
            else_block,
            ..
        }) => {
            if then_block == else_block {
                vec![*then_block]
            } else {
                vec![*then_block, *else_block]
            }
        }
        _ => Vec::new(),
    }
}

/// Append one argument to every edge `from -> to`.
fn append_edge_arg(func: &mut Function, from: u32, to: u32, v: ValueId) {
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

/// The `MemberCellLoad` of `atom` on `object` in block `b`, with its index.
fn find_member_load(
    func: &Function,
    b: u32,
    object: ValueId,
    atom: u32,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> Option<(ValueId, usize)> {
    func.block(b)
        .insts
        .iter()
        .enumerate()
        .find_map(|(i, inst)| {
            (inst.op == Op::MemberCellLoad
                && inst.imm == Imm::Atom(atom)
                && inst.args.len() == 1
                && root_of_any(inst.args[0], objects, arrays, aliases) == Some((object, false)))
            .then_some(inst.result)
            .flatten()
            .map(|load| (load, i))
        })
}

/// The `MemberStore` of `atom` on `object` in block `b` after `after`, with its
/// index and stored value.
#[allow(clippy::too_many_arguments)]
fn find_member_store_after(
    func: &Function,
    b: u32,
    after: usize,
    object: ValueId,
    atom: u32,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> Option<(usize, ValueId)> {
    func.block(b)
        .insts
        .iter()
        .enumerate()
        .skip(after + 1)
        .find_map(|(i, inst)| {
            (inst.op == Op::MemberStore
                && inst.imm == Imm::Atom(atom)
                && inst.args.len() == 2
                && root_of_any(inst.args[0], objects, arrays, aliases) == Some((object, false)))
            .then(|| (i, inst.args[1]))
        })
}

fn one_pass(func: &mut Function, body: &CompiledBody) -> bool {
    let mut objects = literal_objects(func, body);
    let mut arrays = literal_arrays(func, body);
    if objects.is_empty() && arrays.is_empty() {
        return false;
    }
    let roots: HashSet<ValueId> = objects.keys().chain(arrays.keys()).copied().collect();
    let aliases = slot_aliases(func, &roots);
    let escaping = escaping_literals(func, &objects, &arrays, &aliases);
    objects.retain(|object, _| !escaping.contains(object));
    arrays.retain(|array, _| !escaping.contains(array));
    if objects.is_empty() && arrays.is_empty() {
        return false;
    }

    // Each load must feed exactly one guard of the same object and atom, and
    // each guard's atom must have a literal value.
    let mut subst: Vec<Option<ValueId>> = vec![None; func.value_count() as usize];
    let mut remove: Vec<ValueId> = Vec::new();
    let mut replaced_objects: HashSet<ValueId> = HashSet::new();
    // Array `.length` reads need a fresh `Const` in the (dominating) block; the
    // instruction cannot be inserted while the block is borrowed for the scan.
    let mut length_reads: Vec<(u32, ValueId, usize)> = Vec::new();
    // Fields a `MemberStore` writes anywhere: a read of one must forward from a
    // preceding store in its own block (the literal's value is stale), and a
    // read with no such store bails.
    let mut mutable: HashSet<(ValueId, u32)> = HashSet::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::MemberStore, Imm::Atom(atom)) = (inst.op, &inst.imm) else {
                continue;
            };
            if inst.args.len() != 2 {
                continue;
            }
            let Some((root, false)) = root_of_any(inst.args[0], &objects, &arrays, &aliases) else {
                continue;
            };
            mutable.insert((root, *atom));
        }
    }
    for b in 0..func.block_count() as u32 {
        let block_len = func.block(b).insts.len();
        for i in 0..block_len {
            let inst = &func.block(b).insts[i];
            let (Op::MemberGuard, Some(guard), Imm::Atom(atom)) = (inst.op, inst.result, &inst.imm)
            else {
                continue;
            };
            if inst.args.len() != 2 {
                continue;
            }
            let Some((object, is_array)) = root_of_any(inst.args[0], &objects, &arrays, &aliases)
            else {
                continue;
            };
            let Some((load_object, load_atom, load)) = load_for(func, inst.args[1]) else {
                continue;
            };
            if load_atom != *atom
                || root_of_any(load_object, &objects, &arrays, &aliases) != Some((object, is_array))
            {
                continue;
            }
            if uses_of(func, load) != 1 {
                continue;
            }
            if is_array {
                // A fresh literal array's own `length` is the element count;
                // any other member read (a prototype method) is not replaced.
                if *atom != length_atom() {
                    continue;
                }
                let Some(count) = arrays.get(&object).map(Vec::len) else {
                    continue;
                };
                REPLACED.fetch_add(1, Ordering::Relaxed);
                length_reads.push((b, guard, count));
                remove.push(load);
                remove.push(guard);
                replaced_objects.insert(object);
                continue;
            }
            let Some(entries) = objects.get(&object) else {
                continue;
            };
            // A store earlier in this block is the field's live value; without
            // one, the literal's value is right only when nothing writes the
            // field.
            let forwarded = func.block(b).insts[..i]
                .iter()
                .rposition(|x| x.result == Some(load) && x.op == Op::MemberCellLoad)
                .and_then(|load_index| {
                    forward_store_in_block(
                        func, b, load_index, object, *atom, &objects, &arrays, &aliases,
                    )
                });
            let value = match forwarded {
                Some(value) => value,
                None => {
                    if mutable.contains(&(object, *atom)) {
                        continue;
                    }
                    let Some(&(_, value)) = entries.iter().find(|(key, _)| *key == *atom) else {
                        continue;
                    };
                    value
                }
            };
            REPLACED.fetch_add(1, Ordering::Relaxed);
            subst[guard as usize] = Some(value);
            remove.push(load);
            remove.push(guard);
            replaced_objects.insert(object);
        }
    }
    for (block, guard, count) in length_reads {
        subst[guard as usize] = Some(push_number_const(func, block, count as f64));
    }
    // The length constants are values created after `subst` was sized; the
    // substitution chains and the rewrite index by every value id.
    subst.resize(func.value_count() as usize, None);
    // Constant element reads of a non-escaping literal array: the literal's
    // own element is the read only for an in-range index (an out-of-range read
    // consults the prototype chain, which the pass cannot rule out).
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::ElementLoad, Some(result)) = (inst.op, inst.result) else {
                continue;
            };
            if inst.args.len() != 2 {
                continue;
            }
            let Some((array, true)) = root_of_any(inst.args[0], &objects, &arrays, &aliases) else {
                continue;
            };
            let Some(index) = const_index(func, inst.args[1]) else {
                continue;
            };
            let Some(&value) = arrays.get(&array).and_then(|values| values.get(index)) else {
                continue;
            };
            REPLACED.fetch_add(1, Ordering::Relaxed);
            subst[result as usize] = Some(value);
            remove.push(result);
            replaced_objects.insert(array);
        }
    }
    if remove.is_empty() {
        return false;
    }
    for index in 0..subst.len() {
        let mut value = subst[index];
        while let Some(next) = value.and_then(|v| subst[v as usize]) {
            value = Some(next);
        }
        subst[index] = value;
    }
    rewrite_uses(func, &subst);
    strip_instructions(func, &remove);
    // The allocation is dead only when every read of it was replaced: then its
    // field stores and the slot stores that keep it alive can go too.
    drop_dead_member_stores(func, &replaced_objects, &objects, &arrays, &aliases);
    drop_dead_stores(func, &replaced_objects);
    strip_dead_literals(func, &replaced_objects);
    true
}

/// Drop an unused `NewObject`. Its effects are not pure (the allocation drives
/// the carrier/GC bookkeeping), so DCE keeps it; an unused fresh object is
/// unobservable, so the pass removes it explicitly. Iterates because removing
/// an outer literal can orphan the inner one it held.
fn strip_dead_literals(func: &mut Function, replaced: &HashSet<ValueId>) {
    loop {
        let mut removed = false;
        for b in 0..func.block_count() as u32 {
            let dead: Vec<ValueId> = func
                .block(b)
                .insts
                .iter()
                .filter(|inst| {
                    matches!(inst.op, Op::NewObject | Op::NewArray)
                        && inst.result.is_some_and(|result| {
                            replaced.contains(&result) && uses_of(func, result) == 0
                        })
                })
                .filter_map(|inst| inst.result)
                .collect();
            if dead.is_empty() {
                continue;
            }
            func.block_mut(b)
                .insts
                .retain(|inst| inst.result.is_none_or(|result| !dead.contains(&result)));
            removed = true;
        }
        if !removed {
            return;
        }
    }
}

/// The whole-literal objects this pass may replace: value -> (atom, value)
/// pairs in source order. `Imm::Int(step)` points at the `ObjectFast` payload.
pub(crate) fn literal_objects(
    func: &Function,
    body: &CompiledBody,
) -> HashMap<ValueId, Vec<(u32, ValueId)>> {
    let mut objects = HashMap::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::NewObject, Some(result)) = (inst.op, inst.result) else {
                continue;
            };
            let Imm::Int(step) = &inst.imm else {
                continue;
            };
            let Some(Step::ObjectFast { names }) = body.steps.get(*step as usize) else {
                continue;
            };
            if names.len() != inst.args.len() {
                continue;
            }
            objects.insert(
                result,
                names
                    .iter()
                    .copied()
                    .zip(inst.args.iter().copied())
                    .collect(),
            );
        }
    }
    objects
}

/// The whole-literal arrays this pass may replace: value -> element values in
/// index order. `Imm::Int(step)` points at the `ArrayFast` count.
fn literal_arrays(func: &Function, body: &CompiledBody) -> HashMap<ValueId, Vec<ValueId>> {
    let mut arrays = HashMap::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::NewArray, Some(result)) = (inst.op, inst.result) else {
                continue;
            };
            let Imm::Int(step) = &inst.imm else {
                continue;
            };
            let Some(Step::ArrayFast { count }) = body.steps.get(*step as usize) else {
                continue;
            };
            if *count as usize != inst.args.len() {
                continue;
            }
            arrays.insert(result, inst.args.clone());
        }
    }
    arrays
}

/// Values that alias a candidate through a private frame slot: a `FrameLoad`
/// after a `FrameStore` of the object, within one block and with no
/// slot-writing effect in between (a call may observe the slot through a
/// closure created earlier only if it was captured, which makes it a context
/// slot, not a frame slot  -  so a read here is the stored value).
pub(crate) fn slot_aliases(func: &Function, roots: &HashSet<ValueId>) -> HashMap<ValueId, ValueId> {
    let mut aliases: HashMap<ValueId, ValueId> = HashMap::new();
    for b in 0..func.block_count() as u32 {
        let mut slot_value: HashMap<u32, ValueId> = HashMap::new();
        for inst in &func.block(b).insts {
            // Only a `FrameStore` to the slot invalidates the forwarded value.
            // A call cannot reach this invocation's private frame slot (a
            // captured binding lives in a context slot, and a certified body
            // has no `eval`/`with`), so `Effects::call()` is not a clobber
            // here -- the guard's own effects must not break the chain.
            match inst.op {
                Op::FrameStore => {
                    let Imm::Slot(slot) = inst.imm else { continue };
                    match inst.args.first().copied() {
                        Some(value) if roots.contains(&value) => {
                            slot_value.insert(slot, value);
                        }
                        _ => {
                            slot_value.remove(&slot);
                        }
                    }
                }
                Op::FrameLoad => {
                    let (Imm::Slot(slot), Some(result)) = (&inst.imm, inst.result) else {
                        continue;
                    };
                    if let Some(&object) = slot_value.get(slot) {
                        aliases.insert(result, object);
                    }
                }
                _ => {}
            }
        }
    }
    // A slot stored exactly once, in the entry block, holds that value on every
    // path to any load (the entry dominates every reached block), so a load
    // anywhere aliases it. This is the `var o = {..}; for (...) { o.x }` shape:
    // the store is in the preheader, every read in the loop body. A slot stored
    // more than once, or outside the entry, keeps only the block-local aliases.
    let entry = func.entry();
    let mut single: HashMap<u32, Option<ValueId>> = HashMap::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::FrameStore, Imm::Slot(slot), Some(value)) =
                (inst.op, &inst.imm, inst.args.first().copied())
            else {
                continue;
            };
            let candidate = (b == entry && roots.contains(&value)).then_some(value);
            single
                .entry(*slot)
                .and_modify(|slot_value| *slot_value = None)
                .or_insert(candidate);
        }
    }
    for (&slot, value) in &single {
        let Some(object) = *value else { continue };
        for b in 0..func.block_count() as u32 {
            for inst in &func.block(b).insts {
                if let (Op::FrameLoad, Imm::Slot(load_slot), Some(result)) =
                    (inst.op, &inst.imm, inst.result)
                    && *load_slot == slot
                {
                    aliases.insert(result, object);
                }
            }
        }
    }
    aliases
}

/// The candidates whose every use is a slot store, a slot load, or a named
/// read. A candidate used for anything else (a call argument, a second
/// literal's field value, a return) escapes and is left alone; the pass is
/// per-object, so one escaping literal does not disable the others.
fn escaping_literals(
    func: &Function,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> HashSet<ValueId> {
    let mut escaping: HashSet<ValueId> = HashSet::new();
    let mut value_root: HashMap<ValueId, ValueId> = HashMap::new();
    for &object in objects.keys().chain(arrays.keys()) {
        value_root.insert(object, object);
    }
    for (&alias, &object) in aliases {
        value_root.insert(alias, object);
    }
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for inst in &block.insts {
            for &arg in &inst.args {
                let Some(&object) = value_root.get(&arg) else {
                    continue;
                };
                let allowed = match inst.op {
                    Op::FrameStore => inst.args.first() == Some(&arg),
                    Op::MemberCellLoad => inst.args.len() == 1 && inst.args[0] == arg,
                    Op::MemberGuard => inst.args.len() == 2 && inst.args[0] == arg,
                    Op::ElementLoad => inst.args.len() == 2 && inst.args[0] == arg,
                    // A write to the object's own named field is not an escape
                    // (the rewrite forwards the stored value); a store that uses
                    // the object as the *value* escapes through its receiver.
                    Op::MemberStore => {
                        inst.args.len() == 2 && inst.args[0] == arg && objects.contains_key(&object)
                    }
                    _ => false,
                };
                if !allowed {
                    escaping.insert(object);
                }
            }
        }
        for arg in terminator_args(block) {
            if let Some(&object) = value_root.get(&arg) {
                escaping.insert(object);
            }
        }
    }
    escaping
}

/// The interned `length` atom (the array length read's key).
fn length_atom() -> crux::AtomId {
    use std::sync::OnceLock;
    static LENGTH: OnceLock<crux::AtomId> = OnceLock::new();
    *LENGTH.get_or_init(|| crux::string::intern_utf8("length"))
}

/// Materialize a Number constant at the START of `block`, so it dominates every
/// instruction and every use the block dominates (the same region the replaced
/// guard did).
fn push_number_const(func: &mut Function, block: u32, value: f64) -> ValueId {
    let result = func.push_value(Type::Number);
    func.block_mut(block).insts.insert(
        0,
        crate::opt::ir::Inst {
            op: Op::Const,
            args: Vec::new(),
            result: Some(result),
            ty: Type::Number,
            effects: Effects::pure(),
            imm: Imm::Float(value),
        },
    );
    result
}

/// The literal a value denotes and whether it is an array literal.
pub(crate) fn root_of_any(
    value: ValueId,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> Option<(ValueId, bool)> {
    if objects.contains_key(&value) {
        return Some((value, false));
    }
    if arrays.contains_key(&value) {
        return Some((value, true));
    }
    aliases
        .get(&value)
        .map(|&root| (root, arrays.contains_key(&root)))
}

/// The element index a key value denotes, when it is a non-negative integer
/// constant. Anything else (a runtime key, a negative/fractional number, a
/// string) bails: an out-of-range or non-index read consults the prototype
/// chain.
fn const_index(func: &Function, value: ValueId) -> Option<usize> {
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.result != Some(value) || inst.op != Op::Const {
                continue;
            }
            return match &inst.imm {
                Imm::Int(n) if *n >= 0 => Some(*n as usize),
                Imm::Float(n) if *n >= 0.0 && n.fract() == 0.0 && *n <= usize::MAX as f64 => {
                    Some(*n as usize)
                }
                _ => None,
            };
        }
    }
    None
}

/// The `MemberCellLoad` that produced `value`, with its atom and object.
fn load_for(func: &Function, value: ValueId) -> Option<(ValueId, u32, ValueId)> {
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if inst.result != Some(value) || inst.op != Op::MemberCellLoad {
                continue;
            }
            let (Imm::Atom(atom), Some(object)) = (&inst.imm, inst.args.first().copied()) else {
                continue;
            };
            return Some((object, *atom, value));
        }
    }
    None
}

/// The value a `MemberStore(root, atom, v)` earlier in the same block gave the
/// field, for a read of `atom` on `root`. A computed store (an `ElementStore`)
/// to the same object between the store and the read could write any field, so
/// it bails; a store to a different named field does not clobber this one.
#[allow(clippy::too_many_arguments)]
fn forward_store_in_block(
    func: &Function,
    b: u32,
    before_index: usize,
    root: ValueId,
    atom: u32,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> Option<ValueId> {
    for inst in func.block(b).insts[..before_index].iter().rev() {
        if inst.args.len() < 2 {
            continue;
        }
        match inst.op {
            Op::MemberStore => {
                if root_of_any(inst.args[0], objects, arrays, aliases) != Some((root, false)) {
                    continue;
                }
                match &inst.imm {
                    Imm::Atom(store_atom) if *store_atom == atom => return Some(inst.args[1]),
                    _ => {}
                }
            }
            Op::ElementStore
                if root_of_any(inst.args[0], objects, arrays, aliases) == Some((root, false)) =>
            {
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Remove the named field stores of a fully-replaced object. When every read of
/// the object was replaced it is unobservable, so writing its fields is too;
/// dropping the stores lets [`drop_dead_stores`] and [`strip_dead_literals`]
/// remove the allocation. An object with any live member read keeps its stores.
fn drop_dead_member_stores(
    func: &mut Function,
    replaced: &HashSet<ValueId>,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    arrays: &HashMap<ValueId, Vec<ValueId>>,
    aliases: &HashMap<ValueId, ValueId>,
) {
    if replaced.is_empty() {
        return;
    }
    let mut read_roots: HashSet<ValueId> = HashSet::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            if !matches!(
                inst.op,
                Op::MemberCellLoad | Op::MemberGuard | Op::ElementLoad
            ) {
                continue;
            }
            if let Some(arg) = inst.args.first().copied()
                && let Some((root, _)) = root_of_any(arg, objects, arrays, aliases)
            {
                read_roots.insert(root);
            }
        }
    }
    for b in 0..func.block_count() as u32 {
        func.block_mut(b).insts.retain(|inst| {
            if inst.op != Op::MemberStore || inst.args.len() != 2 {
                return true;
            }
            let Some((root, false)) = root_of_any(inst.args[0], objects, arrays, aliases) else {
                return true;
            };
            !replaced.contains(&root) || read_roots.contains(&root)
        });
    }
}

/// Remove the slot stores that keep a fully-replaced object alive. A store is
/// dead when no live `FrameLoad` of its slot remains  -  every read the pass did
/// not replace still loads through an alias, which keeps its load (and thus the
/// store) alive.
fn drop_dead_stores(func: &mut Function, replaced: &HashSet<ValueId>) {
    if replaced.is_empty() {
        return;
    }
    let mut stored_slots: HashSet<u32> = HashSet::new();
    let mut other_stores: HashSet<u32> = HashSet::new();
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::FrameStore, Imm::Slot(slot), Some(value)) =
                (inst.op, &inst.imm, inst.args.first().copied())
            else {
                continue;
            };
            if replaced.contains(&value) {
                stored_slots.insert(*slot);
            } else {
                other_stores.insert(*slot);
            }
        }
    }
    let dead_slots: HashSet<u32> = stored_slots
        .clone()
        .into_iter()
        .filter(|slot| !other_stores.contains(slot))
        .filter(|slot| {
            (0..func.block_count() as u32).all(|b| {
                func.block(b).insts.iter().all(|inst| {
                    !(inst.op == Op::FrameLoad
                        && inst.imm == Imm::Slot(*slot)
                        && inst.result.is_some_and(|r| uses_of(func, r) > 0))
                })
            })
        })
        .collect();
    if dead_slots.is_empty() {
        return;
    }
    for b in 0..func.block_count() as u32 {
        func.block_mut(b).insts.retain(|inst| {
            let (Op::FrameStore, Imm::Slot(slot), Some(value)) =
                (inst.op, &inst.imm, inst.args.first().copied())
            else {
                return true;
            };
            !(dead_slots.contains(slot) && replaced.contains(&value))
        });
    }
}

fn terminator_args(block: &Block) -> Vec<ValueId> {
    match block.term.as_ref() {
        Some(Term::Jump { args, .. }) => args.clone(),
        Some(Term::Branch {
            cond,
            then_args,
            else_args,
            ..
        }) => std::iter::once(*cond)
            .chain(then_args.iter().copied())
            .chain(else_args.iter().copied())
            .collect(),
        Some(Term::Return(Some(v)) | Term::Throw(v)) => vec![*v],
        _ => Vec::new(),
    }
}

/// The number of places `value` is referenced (instruction args, terminator
/// conditions, and edge arguments).
fn uses_of(func: &Function, value: ValueId) -> usize {
    let mut count = 0;
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for inst in &block.insts {
            count += inst.args.iter().filter(|&&arg| arg == value).count();
        }
        count += terminator_args(block)
            .iter()
            .filter(|&&arg| arg == value)
            .count();
    }
    count
}

/// Rewrite every use of a substituted value.
fn rewrite_uses(func: &mut Function, subst: &[Option<ValueId>]) {
    let resolve = |value: &mut ValueId| {
        while let Some(next) = subst[*value as usize] {
            *value = next;
        }
    };
    for b in 0..func.block_count() as u32 {
        let block = func.block_mut(b);
        for inst in &mut block.insts {
            for arg in &mut inst.args {
                resolve(arg);
            }
        }
        match block.term.as_mut() {
            Some(Term::Jump { args, .. }) => {
                for arg in args {
                    resolve(arg);
                }
            }
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                resolve(cond);
                for arg in then_args.iter_mut().chain(else_args) {
                    resolve(arg);
                }
            }
            Some(Term::Return(Some(value)) | Term::Throw(value)) => resolve(value),
            _ => {}
        }
    }
}

/// Remove the named instructions (matched by result value).
fn strip_instructions(func: &mut Function, remove: &[ValueId]) {
    let dropped: HashSet<ValueId> = remove.iter().copied().collect();
    for b in 0..func.block_count() as u32 {
        func.block_mut(b)
            .insts
            .retain(|inst| inst.result.is_none_or(|result| !dropped.contains(&result)));
    }
}
