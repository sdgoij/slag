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

use crate::opt::ir::{Block, Function, Imm, Op, Term, ValueId};

/// The reads this pass has replaced (the opt-tier tests assert the pass ran).
pub(crate) static REPLACED: AtomicU64 = AtomicU64::new(0);

/// Whether the pass runs. On by default; `SLAG_SCALAR=0` disables it  -  the
/// same-binary seam for measuring the pass's marginal effect.
fn enabled() -> bool {
    std::env::var("SLAG_SCALAR")
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
    for _ in 0..4 {
        if !one_pass(func, body) {
            break;
        }
        changed = true;
    }
    changed
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
    for b in 0..func.block_count() as u32 {
        for inst in &func.block(b).insts {
            let (Op::MemberGuard, Some(guard), Imm::Atom(atom)) = (inst.op, inst.result, &inst.imm)
            else {
                continue;
            };
            if inst.args.len() != 2 {
                continue;
            }
            let Some(object) = root_of(inst.args[0], &objects, &aliases) else {
                continue;
            };
            let Some((load_object, load_atom, load)) = load_for(func, inst.args[1]) else {
                continue;
            };
            if load_atom != *atom
                || root_of(load_object, &objects, &aliases) != Some(object)
            {
                continue;
            }
            if uses_of(func, load) != 1 {
                continue;
            }
            let Some(entries) = objects.get(&object) else {
                continue;
            };
            let Some(&(_, value)) = entries.iter().find(|(key, _)| *key == *atom) else {
                continue;
            };
            REPLACED.fetch_add(1, Ordering::Relaxed);
            subst[guard as usize] = Some(value);
            remove.push(load);
            remove.push(guard);
            replaced_objects.insert(object);
        }
    }
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
    // The allocation is dead only when every read of it was replaced: then the
    // slot stores that keep it alive can go too.
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
                        && inst
                            .result
                            .is_some_and(|result| replaced.contains(&result) && uses_of(func, result) == 0)
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
fn literal_objects(func: &Function, body: &CompiledBody) -> HashMap<ValueId, Vec<(u32, ValueId)>> {
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
fn slot_aliases(func: &Function, roots: &HashSet<ValueId>) -> HashMap<ValueId, ValueId> {
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

/// The literal a value denotes: itself, or the candidate an alias forwards.
fn root_of(
    value: ValueId,
    objects: &HashMap<ValueId, Vec<(u32, ValueId)>>,
    aliases: &HashMap<ValueId, ValueId>,
) -> Option<ValueId> {
    if objects.contains_key(&value) {
        return Some(value);
    }
    aliases.get(&value).copied()
}

/// The literal a value denotes and whether it is an array literal.
fn root_of_any(
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
