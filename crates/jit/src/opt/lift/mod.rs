//! The lift: the interpreter's `Step` stream into the SSA IR.
//!
//! Increments I1 (straight line) and I2 (control flow): `Jump`,
//! `JumpIfFalse`/`JumpIfTrue`, joins and loops, the dominant reads
//! (`GetMemberName`, `LoadGlobal`), and the `let`/`const` TDZ checks. A step
//! outside the subset or a body that is not certified returns [`Unsupported`]
//! and keeps the current per-step lowering, so nothing about the engine's
//! behavior changes unless a later increment wires the IR in.
//!
//! The lift is exact by construction: an accepted step means exactly what the
//! interpreter's handler for that step means. It refuses rather than
//! approximates.
//!
//! - **TDZ slots.** A `let`/`const` slot (`scope.tdz_store`) carries an
//!   `is_uninitialized` check the interpreter runs before a load or store (a
//!   read or assignment before initialization is a `ReferenceError`). The IR
//!   models it as an explicit `Op::TdzCheck` emitted before the `FrameLoad`/
//!   `FrameStore`; `InitLocal` (the initializing store) needs none, and a body
//!   whose scope has no lexical slot — parameters and `var`s only — emits no
//!   check at all.
//!
//! Frame slots stay in memory (the IR's `FrameLoad`/`FrameStore` read and write
//! `Heap::Slots`); only the operand stack is SSA, so a join takes its stack as
//! block parameters. Those parameters are sized by a stack-depth fixpoint, not
//! by a predecessor's already-emitted stack, which is what lets a loop header's
//! parameters be known before the header is emitted.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::opt::builder::Builder;
use crate::opt::ir::{BlockId, Effects, Function, Heap, Imm, Op, Term, Type, ValueId};
use syntax::ast::{BinaryOp, UnaryOp};

use runtime::ir::{CompiledBody, RelLimit, Step};

/// Why a body could not be lifted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// The body is not certified (`CompiledBody::scope` is `None`): it has no
    /// frame slots to lift against.
    Uncertified,
    /// A step outside the subset.
    Step(&'static str),
    /// The body reached its end without a `Return`, so the entry block has no
    /// terminator.
    NoReturn,
    /// The operand stack underflowed — a step sequence the interpreter would
    /// not produce.
    Stack,
    /// A malformed body (a jump outside the body, mismatched stack depths at a
    /// join).
    Malformed(&'static str),
    /// The lift produced a graph [`verify`](super::verify::verify) rejects.
    /// This is a lift bug; the caller keeps the current path either way.
    Invalid,
}

/// A diagnostic guard depth (tests only). When non-zero, [`lift`] emits a
/// constant-false `Op::Check` at the first step whose incoming operand stack has
/// this depth, so the body deopts there and the interpreter resumes. It
/// exercises the tier's resume fidelity (`.notes/tier-resume-fidelity.md`);
/// zero (the default) is off and a normal run never sets it. It is ignored for a
/// leaf-eligible body: the leaf lane has no `DISPATCH_DEOPT` handling.
pub(crate) static OPT_PROBE_DEPTH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Whether the typer guards a member read's result in place (the T2 in-loop
/// placement, `.notes/tier-typer.md`). `0` consults `SLAG_TYPER`; `1`/`2` force
/// it on/off (tests). Off by default, so production is unchanged.
pub(crate) static OPT_TYPER: AtomicU8 = AtomicU8::new(0);

fn typer_reads_enabled() -> bool {
    match OPT_TYPER.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => std::env::var("SLAG_TYPER").is_ok_and(|v| v != "0"),
    }
}

/// Lift a certified body into the SSA IR (straight line and control flow,
/// including loops).
pub fn lift(body: &CompiledBody) -> Result<Function, Unsupported> {
    lift_impl(body, typer_reads_enabled())
}

/// [`lift`] with the typer's read guard forced on or off (tests).
fn lift_impl(body: &CompiledBody, guard_reads: bool) -> Result<Function, Unsupported> {
    let scope = body.scope.as_ref().ok_or(Unsupported::Uncertified)?;
    let tdz: &[bool] = &scope.tdz_store;
    let steps = &body.steps;
    let n = steps.len();
    if n == 0 {
        return Err(Unsupported::NoReturn);
    }
    // The typer's read guard needs a deopt lane. Non-leaf bodies resume via
    // `run_jit_body`; leaf bodies resume via `run_jit_leaf` (L1). The machine-
    // code inline lanes cannot resume, so a guard-bearing body is marked
    // `deopts` and refused there (`leaf_call_probe`, `certified_verdict`, the
    // shared-construct lane, the self-call path), falling back to a
    // runtime-driven lane.
    let probe_depth = if body.leaf {
        None
    } else {
        match OPT_PROBE_DEPTH.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            d => Some(d),
        }
    };
    let mut probed = false;

    // A block starts at 0, at every jump target (forward or a back edge), and
    // after every terminator.
    let mut starts = vec![false; n + 1];
    starts[0] = true;
    for (i, step) in steps.iter().enumerate() {
        if let Some(t) = jump_target(step) {
            if t >= n {
                return Err(Unsupported::Malformed("jump out of range"));
            }
            starts[t] = true;
        }
        if is_terminator(step) {
            starts[i + 1] = true;
        }
    }
    let block_starts: Vec<usize> = (0..n).filter(|&i| starts[i]).collect();
    let nb = block_starts.len();
    let mut block_of = vec![usize::MAX; n];
    for (bi, &s) in block_starts.iter().enumerate() {
        block_of[s] = bi;
    }
    let ends: Vec<usize> = (0..nb)
        .map(|bi| block_starts.get(bi + 1).copied().unwrap_or(n))
        .collect();
    let terms: Vec<Option<usize>> = ends.iter().map(|&e| terminator_index(steps, e)).collect();

    // Successors, then each block's entry stack depth. The depth is a fixpoint:
    // a loop header's depth is fed by a back edge from a block not yet visited.
    let mut succs: Vec<Vec<usize>> = Vec::with_capacity(nb);
    for &end in &ends {
        let mut row = Vec::new();
        for succ in block_successors(steps, end, n)? {
            let sb = block_of[succ];
            if sb == usize::MAX {
                return Err(Unsupported::Malformed("jump to a non-block start"));
            }
            row.push(sb);
        }
        succs.push(row);
    }
    let depths = stack_depths(steps, &block_starts, &ends, &terms, &succs)?;

    let mut func = Function::new();
    for _ in 1..nb {
        func.push_block();
    }
    let mut builder = Builder::new(&mut func);
    // Every join takes its stack as parameters, sized by the fixpoint. Uniform
    // (a single predecessor passes its stack too), which is what lets a loop
    // header's parameters be known before the header is emitted.
    for (bi, &depth) in depths.iter().enumerate().skip(1) {
        for _ in 0..depth {
            builder.param(bi as BlockId, Type::Unknown);
        }
    }
    let mut term_rec: Vec<Option<TermRec>> = (0..nb).map(|_| None).collect();
    for bi in 0..nb {
        let mut stack: Vec<ValueId> = builder.func().block(bi as BlockId).params.clone();
        let body_end = terms[bi].unwrap_or(ends[bi]);
        for (off, step) in steps[block_starts[bi]..body_end].iter().enumerate() {
            if !probed && probe_depth == Some(stack.len()) {
                emit_opt_probe(&mut builder, bi as BlockId, &stack, block_starts[bi] + off);
                probed = true;
            }
            emit_step(
                &mut builder,
                bi as BlockId,
                &mut stack,
                tdz,
                block_starts[bi] + off,
                guard_reads,
                step,
            )?;
        }
        term_rec[bi] = Some(match terms[bi] {
            Some(ti) => match &steps[ti] {
                Step::Return => TermRec::Ret(stack.pop().ok_or(Unsupported::Stack)?),
                Step::Jump(_) => TermRec::Jump(stack),
                Step::JumpIfFalse(_) | Step::JumpIfTrue(_) => {
                    TermRec::Branch(stack.pop().ok_or(Unsupported::Stack)?, stack)
                }
                other if is_fused_test(other) => TermRec::FusedTest(stack),
                other => return Err(Unsupported::Step(step_name(other))),
            },
            None => TermRec::Jump(stack),
        });
    }

    for bi in 0..nb {
        let end = ends[bi];
        let rec = term_rec[bi].as_ref().ok_or(Unsupported::Invalid)?;
        match rec {
            TermRec::Ret(v) => builder.term(bi as BlockId, Term::Return(Some(*v))),
            TermRec::Jump(stack) => {
                let target = match terms[bi] {
                    Some(ti) => jump_target(&steps[ti]).ok_or(Unsupported::Invalid)?,
                    None => end,
                };
                let sb = block_of[target] as BlockId;
                let args = edge_args(builder.func(), sb, stack);
                builder.term(bi as BlockId, Term::Jump { target: sb, args });
            }
            TermRec::Branch(cond, stack) => {
                let ti = terms[bi].ok_or(Unsupported::Invalid)?;
                let (target, truthy) = match &steps[ti] {
                    Step::JumpIfTrue(t) => (*t, true),
                    Step::JumpIfFalse(t) => (*t, false),
                    _ => return Err(Unsupported::Invalid),
                };
                let sb = block_of[target] as BlockId;
                let fb = block_of[end] as BlockId;
                let (then_block, else_block) = if truthy { (sb, fb) } else { (fb, sb) };
                let then_args = edge_args(builder.func(), then_block, stack);
                let else_args = edge_args(builder.func(), else_block, stack);
                builder.term(
                    bi as BlockId,
                    Term::Branch {
                        cond: *cond,
                        then_block,
                        then_args,
                        else_block,
                        else_args,
                    },
                );
            }
            TermRec::FusedTest(stack) => {
                let ti = terms[bi].ok_or(Unsupported::Invalid)?;
                let (cond, jump_when_true) =
                    emit_fused_test(&mut builder, bi as BlockId, &steps[ti], tdz)?;
                let target = jump_target(&steps[ti]).ok_or(Unsupported::Invalid)?;
                let sb = block_of[target] as BlockId;
                let fb = block_of[end] as BlockId;
                let (then_block, else_block) = if jump_when_true { (sb, fb) } else { (fb, sb) };
                let then_args = edge_args(builder.func(), then_block, stack);
                let else_args = edge_args(builder.func(), else_block, stack);
                builder.term(
                    bi as BlockId,
                    Term::Branch {
                        cond,
                        then_block,
                        then_args,
                        else_block,
                        else_args,
                    },
                );
            }
        }
    }

    if crate::opt::verify::verify(&func).is_err() {
        return Err(Unsupported::Invalid);
    }
    Ok(func)
}

/// Every block's entry stack depth, as a fixpoint (a loop header's depth is
/// fed by a back edge, so one forward pass is not enough). Every edge into a
/// block must agree; a block with no predecessors is unreachable.
fn stack_depths(
    steps: &[Step],
    starts: &[usize],
    ends: &[usize],
    terms: &[Option<usize>],
    succs: &[Vec<usize>],
) -> Result<Vec<usize>, Unsupported> {
    let nb = ends.len();
    let mut depth: Vec<Option<i32>> = vec![None; nb];
    depth[0] = Some(0);
    loop {
        let mut changed = false;
        for bi in 0..nb {
            let Some(entry) = depth[bi] else {
                continue;
            };
            let mut out = entry;
            for step in &steps[starts[bi]..terms[bi].unwrap_or(ends[bi])] {
                out += stack_delta(step)?;
            }
            // A conditional pops its condition on the way out.
            let edge = match terms[bi] {
                Some(ti) if matches!(steps[ti], Step::JumpIfFalse(_) | Step::JumpIfTrue(_)) => {
                    out - 1
                }
                _ => out,
            };
            if edge < 0 {
                return Err(Unsupported::Malformed("stack underflow"));
            }
            for &s in &succs[bi] {
                match depth[s] {
                    None => {
                        depth[s] = Some(edge);
                        changed = true;
                    }
                    Some(known) if known != edge => {
                        return Err(Unsupported::Malformed("stack depth mismatch at a join"));
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }
    depth
        .into_iter()
        .map(|d| {
            d.map(|d| d as usize)
                .ok_or(Unsupported::Malformed("unreachable block"))
        })
        .collect()
}

/// The operand-stack effect of a step the lift accepts (it must mirror
/// `emit_step`); a step outside the subset is refused here too, so the bail
/// names the step rather than a spurious depth mismatch.
fn stack_delta(step: &Step) -> Result<i32, Unsupported> {
    Ok(match step {
        Step::Push(_)
        | Step::Dup
        | Step::LoadLocal { .. }
        | Step::LoadGlobal { .. }
        | Step::LoadIdent { .. } => 1,
        Step::Pop
        | Step::StoreLocal { .. }
        | Step::InitLocal { .. }
        | Step::FusedStoreLocal { .. }
        | Step::Binary(_)
        | Step::GetMemberComputed
        | Step::SetCompletion => -1,
        // `BinaryImm` pops its left operand and pushes the result, a member
        // read pops the receiver and pushes the value; the rest are net-neutral.
        Step::BinaryImm { .. }
        | Step::GetMemberName { .. }
        | Step::Unary(_)
        | Step::ResetCompletion
        | Step::NormalizeCompletion
        | Step::ListBegin
        | Step::ListEnd
        | Step::SaveCompletion
        | Step::RestoreCompletion => 0,
        other => return Err(Unsupported::Step(step_name(other))),
    })
}

/// The edge payload a block leaves on each successor.
enum TermRec {
    Ret(ValueId),
    Jump(Vec<ValueId>),
    Branch(ValueId, Vec<ValueId>),
    /// A fused test: the condition is emitted from the step at terminator time
    /// (it reads its operands itself), and the stack is passed to both edges.
    FusedTest(Vec<ValueId>),
}

/// The arguments a predecessor passes into `target`: its stack, or nothing
/// when the target has a single predecessor and therefore no parameters.
fn edge_args(func: &Function, target: BlockId, stack: &[ValueId]) -> Vec<ValueId> {
    if func.block(target).params.is_empty() {
        Vec::new()
    } else {
        stack.to_vec()
    }
}

/// The index of the terminating step of a block ending at `end`, if it is a
/// terminator (`end - 1`); `None` for a fall-through block.
fn terminator_index(steps: &[Step], end: usize) -> Option<usize> {
    let last = end.checked_sub(1)?;
    is_terminator(&steps[last]).then_some(last)
}

/// The blocks a block ending at `end` can reach.
fn block_successors(steps: &[Step], end: usize, n: usize) -> Result<Vec<usize>, Unsupported> {
    let Some(last) = end.checked_sub(1) else {
        return Err(Unsupported::Invalid);
    };
    if is_terminator(&steps[last]) {
        if let Some(target) = jump_target(&steps[last]) {
            // An unconditional jump has one successor; every conditional (the
            // `JumpIf*` pair and the fused loop test) also falls through.
            return if matches!(steps[last], Step::Jump(_)) {
                Ok(vec![target])
            } else if end >= n {
                Err(Unsupported::Malformed("conditional with no fall-through"))
            } else {
                Ok(vec![target, end])
            };
        }
        match &steps[last] {
            Step::Return => Ok(Vec::new()),
            Step::Throw { .. } => Err(Unsupported::Step("Throw")),
            _ => Err(Unsupported::Invalid),
        }
    } else if end >= n {
        Err(Unsupported::NoReturn)
    } else {
        Ok(vec![end])
    }
}

fn is_terminator(step: &Step) -> bool {
    jump_target(step).is_some() || matches!(step, Step::Return | Step::Throw { .. })
}

/// Whether `step` is one of the fused loop/strict-equality tests (a conditional
/// jump that reads its operands itself rather than popping a stack condition).
fn is_fused_test(step: &Step) -> bool {
    matches!(
        step,
        Step::JumpIfLtImm { .. }
            | Step::JumpIfLeImm { .. }
            | Step::JumpIfGtImm { .. }
            | Step::JumpIfGeImm { .. }
            | Step::JumpIfEqImm { .. }
            | Step::JumpIfNeqImm { .. }
            | Step::JumpIfRelLimit { .. }
            | Step::JumpIfLtGlobalImm { .. }
            | Step::JumpIfLeGlobalImm { .. }
            | Step::JumpIfGtGlobalImm { .. }
            | Step::JumpIfGeGlobalImm { .. }
    )
}

fn jump_target(step: &Step) -> Option<usize> {
    match step {
        Step::Jump(t) | Step::JumpIfFalse(t) | Step::JumpIfTrue(t) => Some(*t),
        Step::JumpIfLtImm { target, .. }
        | Step::JumpIfLeImm { target, .. }
        | Step::JumpIfGtImm { target, .. }
        | Step::JumpIfGeImm { target, .. }
        | Step::JumpIfEqImm { target, .. }
        | Step::JumpIfNeqImm { target, .. }
        | Step::JumpIfRelLimit { target, .. }
        | Step::JumpIfLtGlobalImm { target, .. }
        | Step::JumpIfLeGlobalImm { target, .. }
        | Step::JumpIfGtGlobalImm { target, .. }
        | Step::JumpIfGeGlobalImm { target, .. } => Some(*target),
        _ => None,
    }
}

/// Lift one `Step` into IR instructions.
/// Emit the diagnostic guard: an `Op::Check` carrying the live operand stack
/// (bottom to top) and the step to resume at. The condition is constant
/// **false** — the guard exits when its condition is falsy, so this forces the
/// deopt — and a constant condition keeps both arms of the guard structurally
/// reachable, so the graph still verifies.
fn emit_opt_probe(builder: &mut Builder<'_>, block: BlockId, stack: &[ValueId], index: usize) {
    let cond = builder.emit(
        block,
        Op::Const,
        &[],
        Type::Bool,
        Effects::pure(),
        Imm::Bool(false),
    );
    let mut args = Vec::with_capacity(stack.len() + 1);
    args.push(cond);
    args.extend_from_slice(stack);
    builder.emit_void(
        block,
        Op::Check,
        &args,
        Effects::pure(),
        Imm::Int(index as i32),
    );
}

/// The typer's in-loop type guard on a just-produced read value
/// (`.notes/tier-typer.md` T2): assert the value is a Number so the arithmetic
/// that consumes it lowers tag-free. The resume step is `index + 1` — the *next*
/// step, since the read has already run once and the interpreter must not re-run
/// it (a getter would fire twice); the mirrored stack is the post-read stack,
/// which is that step's entry stack `[.., value]`.
fn guard_read_value(
    builder: &mut Builder,
    block: BlockId,
    stack: &[ValueId],
    value: ValueId,
    index: usize,
) -> ValueId {
    let mut args = Vec::with_capacity(stack.len() + 2);
    args.push(value);
    args.extend(stack.iter().copied());
    args.push(value);
    builder.emit(
        block,
        Op::GuardType,
        &args,
        Type::Number,
        Effects::pure(),
        Imm::Int(index as i32 + 1),
    )
}

fn emit_step(
    builder: &mut Builder,
    block: BlockId,
    stack: &mut Vec<ValueId>,
    tdz: &[bool],
    index: usize,
    guard_reads: bool,
    step: &Step,
) -> Result<(), Unsupported> {
    let is_tdz = |slot: usize| tdz.get(slot).copied().unwrap_or(false);
    match step {
        Step::Push(value) => {
            let (imm, ty) = constant(value)?;
            let v = builder.emit(block, Op::Const, &[], ty, Effects::pure(), imm);
            stack.push(v);
        }
        Step::Pop => {
            stack.pop().ok_or(Unsupported::Stack)?;
        }
        Step::Dup => {
            let top = *stack.last().ok_or(Unsupported::Stack)?;
            stack.push(top);
        }
        // A read of a name that resolves through the environment chain (a
        // global read from a function body, `BindingLoc::Env`): an opaque effect
        // like `LoadGlobal`, lowered through the same `load_ident` helper the
        // per-step path uses on its slow path. It also makes the body **non-leaf**
        // (`steps_are_leaf` excludes `LoadIdent`), so it runs through
        // `run_jit_body` — the entry where a guard can resume.
        Step::LoadIdent { name } => {
            let v = builder.emit(
                block,
                Op::IdentLoad,
                &[],
                Type::Unknown,
                Effects::call(),
                Imm::Atom(*name),
            );
            stack.push(v);
        }
        Step::LoadLocal { slot } => {
            // A lexical slot carries a TDZ check the interpreter runs before
            // the read; mirror it (`let`/`const` slots only — params and
            // `var`s are never the uninitialized marker).
            if is_tdz(*slot) {
                emit_tdz_check(builder, block, *slot);
            }
            let v = builder.emit(
                block,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
            stack.push(v);
        }
        // A member read (`o.x`): an inline member-value cell probe — a
        // speculative cell load plus its validity guard — replacing the
        // `get_member_name` helper call. The guard falls back to the helper, so
        // a getter, a proxy trap, a throwing receiver or a plain miss is served
        // exactly as before; the per-step path reaches the same shape with its
        // own cell probe, so the tier stops paying a helper call per read.
        Step::GetMemberName { name } => {
            let object = stack.pop().ok_or(Unsupported::Stack)?;
            let cell = builder.emit(
                block,
                Op::MemberCellLoad,
                &[object],
                Type::Unknown,
                Effects::read(Heap::Slots).union(Effects::read(Heap::Members)),
                Imm::Atom(*name),
            );
            let v = builder.emit(
                block,
                Op::MemberGuard,
                &[object, cell],
                Type::Unknown,
                Effects::call(),
                Imm::Atom(*name),
            );
            let value = if guard_reads {
                guard_read_value(builder, block, stack, v, index)
            } else {
                v
            };
            stack.push(value);
        }
        // A computed member read (`o[k]`): the same opaque effect as the name
        // form; the key is a runtime value on the stack.
        Step::GetMemberComputed => {
            let key = stack.pop().ok_or(Unsupported::Stack)?;
            let object = stack.pop().ok_or(Unsupported::Stack)?;
            let v = builder.emit(
                block,
                Op::ElementLoad,
                &[object, key],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let value = if guard_reads {
                guard_read_value(builder, block, stack, v, index)
            } else {
                v
            };
            stack.push(value);
        }
        // A global read through the global object (`LoadGlobal`); the same
        // opaque effect until proven data.
        Step::LoadGlobal { name } => {
            let v = builder.emit(
                block,
                Op::GlobalLoad,
                &[],
                Type::Unknown,
                Effects::call(),
                Imm::Atom(*name),
            );
            stack.push(v);
        }
        Step::StoreLocal { slot } => {
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            // A lexical store checks the *current* binding for the
            // uninitialized marker first (assignment before initialization is
            // a ReferenceError).
            if is_tdz(*slot) {
                emit_tdz_check(builder, block, *slot);
            }
            builder.emit_void(
                block,
                Op::FrameStore,
                &[value],
                Effects::write(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
        }
        // `let x = v` initializes the binding: no TDZ check.
        Step::InitLocal { slot } => {
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            builder.emit_void(
                block,
                Op::FrameStore,
                &[value],
                Effects::write(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
        }
        Step::FusedStoreLocal { slot } => {
            // A statement-position assignment: check (when lexical), store,
            // then set the completion register to the stored value (spec
            // 6.2.2.4).
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            if is_tdz(*slot) {
                emit_tdz_check(builder, block, *slot);
            }
            builder.emit_void(
                block,
                Op::FrameStore,
                &[value],
                Effects::write(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
            builder.emit_void(
                block,
                Op::CompletionStore,
                &[value],
                Op::CompletionStore.default_effects(),
                Imm::None,
            );
        }
        Step::SetCompletion => {
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            builder.emit_void(
                block,
                Op::CompletionStore,
                &[value],
                Op::CompletionStore.default_effects(),
                Imm::None,
            );
        }
        Step::ResetCompletion => {
            builder.emit_void(
                block,
                Op::CompletionReset,
                &[],
                Op::CompletionReset.default_effects(),
                Imm::None,
            );
        }
        // The compiled model treats the normalize and save/restore as no-ops
        // (the completion register is unobservable within a certified body).
        Step::NormalizeCompletion
        | Step::ListBegin
        | Step::ListEnd
        | Step::SaveCompletion
        | Step::RestoreCompletion => {}
        Step::Binary(op) => {
            let right = stack.pop().ok_or(Unsupported::Stack)?;
            let left = stack.pop().ok_or(Unsupported::Stack)?;
            emit_binary(builder, block, stack, *op, left, right)?;
        }
        Step::BinaryImm { op, imm } => {
            let left = stack.pop().ok_or(Unsupported::Stack)?;
            let right = builder.emit(
                block,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(*imm),
            );
            emit_binary(builder, block, stack, *op, left, right)?;
        }
        Step::Unary(op) => {
            let operand = stack.pop().ok_or(Unsupported::Stack)?;
            let op = unary_op(*op)?;
            let v = builder.emit(
                block,
                op,
                &[operand],
                unary_type(op),
                op.default_effects(),
                Imm::None,
            );
            stack.push(v);
        }
        other => return Err(Unsupported::Step(step_name(other))),
    }
    Ok(())
}

/// A TDZ guard on `slot`: throw a `ReferenceError` when the slot still holds
/// the uninitialized marker. Mirrors the per-step lowerer's `emit_tdz_check`.
fn emit_tdz_check(builder: &mut Builder, block: BlockId, slot: usize) {
    builder.emit_void(
        block,
        Op::TdzCheck,
        &[],
        Effects::read(Heap::Slots),
        Imm::Slot(slot as u32),
    );
}

/// Emit the comparison of a fused test step and return the condition value plus
/// whether the step jumps to its target when that condition is *true* (only
/// `JumpIfNeqImm` does; every other fused test jumps when it is false). Mirrors
/// `jump_if_rel_imm`/`jump_if_rel_limit`/`jump_if_rel_global` and
/// `jump_if_strict_eq_imm`.
fn emit_fused_test(
    builder: &mut Builder,
    block: BlockId,
    step: &Step,
    tdz: &[bool],
) -> Result<(ValueId, bool), Unsupported> {
    let slot_load = |b: &mut Builder, slot: usize| -> ValueId {
        if tdz.get(slot).copied().unwrap_or(false) {
            emit_tdz_check(b, block, slot);
        }
        b.emit(
            block,
            Op::FrameLoad,
            &[],
            Type::Unknown,
            Effects::read(Heap::Slots),
            Imm::Slot(slot as u32),
        )
    };
    let num = |b: &mut Builder, v: f64| -> ValueId {
        b.emit(
            block,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(v),
        )
    };
    let global = |b: &mut Builder, name: crux::AtomId| -> ValueId {
        b.emit(
            block,
            Op::GlobalLoad,
            &[],
            Type::Unknown,
            Effects::call(),
            Imm::Atom(name),
        )
    };
    let cmp = |b: &mut Builder, op: Op, l: ValueId, r: ValueId| -> ValueId {
        b.emit(
            block,
            op,
            &[l, r],
            Type::Bool,
            op.default_effects(),
            Imm::None,
        )
    };
    Ok(match step {
        Step::JumpIfLtImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::Lt, l, r), false)
        }
        Step::JumpIfLeImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::Le, l, r), false)
        }
        Step::JumpIfGtImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::Gt, l, r), false)
        }
        Step::JumpIfGeImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::Ge, l, r), false)
        }
        Step::JumpIfEqImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::StrictEq, l, r), false)
        }
        Step::JumpIfNeqImm { slot, imm, .. } => {
            let l = slot_load(builder, *slot);
            let r = num(builder, *imm);
            (cmp(builder, Op::StrictEq, l, r), true)
        }
        Step::JumpIfRelLimit {
            op, slot, limit, ..
        } => {
            let l = slot_load(builder, *slot);
            let r = match limit {
                RelLimit::Imm(i) => num(builder, *i),
                RelLimit::Slot(s) => slot_load(builder, *s),
                RelLimit::Global(name) => global(builder, *name),
            };
            (cmp(builder, binary_op(*op)?, l, r), false)
        }
        Step::JumpIfLtGlobalImm { name, imm, .. } => {
            let l = global(builder, *name);
            let r = num(builder, *imm);
            (cmp(builder, Op::Lt, l, r), false)
        }
        Step::JumpIfLeGlobalImm { name, imm, .. } => {
            let l = global(builder, *name);
            let r = num(builder, *imm);
            (cmp(builder, Op::Le, l, r), false)
        }
        Step::JumpIfGtGlobalImm { name, imm, .. } => {
            let l = global(builder, *name);
            let r = num(builder, *imm);
            (cmp(builder, Op::Gt, l, r), false)
        }
        Step::JumpIfGeGlobalImm { name, imm, .. } => {
            let l = global(builder, *name);
            let r = num(builder, *imm);
            (cmp(builder, Op::Ge, l, r), false)
        }
        other => return Err(Unsupported::Step(step_name(other))),
    })
}

fn emit_binary(
    builder: &mut Builder,
    block: BlockId,
    stack: &mut Vec<ValueId>,
    op: BinaryOp,
    left: ValueId,
    right: ValueId,
) -> Result<(), Unsupported> {
    let op = binary_op(op)?;
    let v = builder.emit(
        block,
        op,
        &[left, right],
        binary_type(op),
        op.default_effects(),
        Imm::None,
    );
    stack.push(v);
    Ok(())
}

fn constant(value: &crux::Value) -> Result<(Imm, Type), Unsupported> {
    if let Some(n) = value.as_number() {
        return Ok((Imm::Float(n), Type::Number));
    }
    if let Some(b) = value.as_boolean() {
        return Ok((Imm::Bool(b), Type::Bool));
    }
    Err(Unsupported::Step("Push"))
}

fn binary_op(op: BinaryOp) -> Result<Op, Unsupported> {
    Ok(match op {
        BinaryOp::Exp => Op::Pow,
        BinaryOp::Mul => Op::Mul,
        BinaryOp::Div => Op::Div,
        BinaryOp::Rem => Op::Mod,
        BinaryOp::Add => Op::Add,
        BinaryOp::Sub => Op::Sub,
        BinaryOp::LeftShift => Op::Shl,
        BinaryOp::RightShift => Op::Shr,
        BinaryOp::UnsignedRightShift => Op::UShr,
        BinaryOp::LessThan => Op::Lt,
        BinaryOp::GreaterThan => Op::Gt,
        BinaryOp::LessEqual => Op::Le,
        BinaryOp::GreaterEqual => Op::Ge,
        BinaryOp::Equal => Op::Eq,
        BinaryOp::StrictEqual => Op::StrictEq,
        BinaryOp::BitAnd => Op::BitAnd,
        BinaryOp::BitXor => Op::BitXor,
        BinaryOp::BitOr => Op::BitOr,
        BinaryOp::In | BinaryOp::Instanceof | BinaryOp::NotEqual | BinaryOp::StrictNotEqual => {
            return Err(Unsupported::Step("Binary"));
        }
    })
}

fn unary_op(op: UnaryOp) -> Result<Op, Unsupported> {
    Ok(match op {
        UnaryOp::Plus => Op::ToNumber,
        UnaryOp::Minus => Op::Neg,
        UnaryOp::BitNot => Op::BitNot,
        UnaryOp::Not => Op::Not,
        UnaryOp::Delete | UnaryOp::Void | UnaryOp::Typeof => {
            return Err(Unsupported::Step("Unary"));
        }
    })
}

fn binary_type(op: Op) -> Type {
    match op {
        Op::Eq | Op::StrictEq | Op::Lt | Op::Le | Op::Gt | Op::Ge => Type::Bool,
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr => Type::Int,
        _ => Type::Unknown,
    }
}

fn unary_type(op: Op) -> Type {
    match op {
        Op::Not => Type::Bool,
        Op::BitNot => Type::Int,
        _ => Type::Number,
    }
}

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Jump(_) | Step::JumpIfFalse(_) | Step::JumpIfTrue(_) => "control",
        Step::Return => "Return",
        _ => "step",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::ir::{Imm, Op, Term, Type};
    use crux::Value;
    use runtime::ir::{CompiledBody, ScopeInfo};

    /// A certified-style scope for the hand-built test bodies: `frame_size`
    /// slots, all `var`-like (no TDZ), nothing captured.
    fn scope(frame_size: usize) -> ScopeInfo {
        ScopeInfo {
            frame_size,
            arity: 0,
            slots: Default::default(),
            tdz_store: vec![false; frame_size],
            shadow_slots: Default::default(),
            shadowed_catch_params: Default::default(),
            context_names: Vec::new(),
            context_tdz: Vec::new(),
            context_const: Vec::new(),
            context_param: Vec::new(),
            context_slots: Default::default(),
            arguments_slot: None,
            arguments_formals: None,
            this_slot: None,
            captured_this: None,
            args_alias: false,
            annex_b: Vec::new(),
            statement_fns: Vec::new(),
        }
    }

    fn body(steps: Vec<Step>, frame_size: usize) -> CompiledBody {
        let max_stack = runtime::ir::max_stack_usage(&steps);
        CompiledBody {
            steps,
            handlers: Vec::new(),
            strict: false,
            scope: Some(scope(frame_size)),
            env_constant: true,
            leaf: false,
            leaf_needs_env: false,
            leaf_uses_env: false,
            leaf_ops: None,
            script_globals: None,
            jit_info: std::cell::Cell::new(0),
            jit_calls: std::cell::Cell::new(0),
            jit_evictions: std::cell::Cell::new(0),
            ident_names: Vec::new(),
            has_loop: false,
            has_call_apply: false,
            has_call_intrinsic: false,
            max_stack,
            nested_gate: std::cell::Cell::new(None),
            feedback: std::cell::RefCell::new(None),
        }
    }

    fn ops(func: &Function, block: BlockId) -> Vec<Op> {
        func.block(block).insts.iter().map(|i| i.op).collect()
    }

    #[test]
    fn lifts_a_straight_line_arithmetic_body() {
        // return (1 + 2) - f[0];
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(BinaryOp::Add),
                Step::LoadLocal { slot: 0 },
                Step::Binary(BinaryOp::Sub),
                Step::Return,
            ],
            1,
        );
        let func = lift(&b).expect("lifts");
        assert_eq!(func.block_count(), 1);
        let entry = func.entry();
        assert_eq!(
            ops(&func, entry),
            vec![Op::Const, Op::Const, Op::Add, Op::FrameLoad, Op::Sub]
        );
        let add = func.block(entry).insts[2].result.unwrap();
        let load = func.block(entry).insts[3].result.unwrap();
        assert_eq!(func.block(entry).insts[4].args, vec![add, load]);
        assert!(matches!(
            func.block(entry).term,
            Some(Term::Return(Some(_)))
        ));
    }

    #[test]
    fn lifts_a_ternary_branch() {
        // return f[0] ? 1 : 2;
        //  0 LoadLocal 0
        //  1 JumpIfFalse 4
        //  2 Push 1
        //  3 Jump 5
        //  4 Push 2
        //  5 Return
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::JumpIfFalse(4),
                Step::Push(Value::Number(1.0)),
                Step::Jump(5),
                Step::Push(Value::Number(2.0)),
                Step::Return,
            ],
            1,
        );
        let func = lift(&b).expect("lifts");
        // Blocks: [0,2) branch, [2,4) then, [4,5) else, [5,6) join.
        assert_eq!(func.block_count(), 4);
        assert!(matches!(func.block(0).term, Some(Term::Branch { .. })));
        // The join block takes the two branch values as its parameter.
        assert_eq!(func.block(3).params.len(), 1);
        assert!(matches!(func.block(3).term, Some(Term::Return(Some(_)))));
        assert!(ops(&func, 2).contains(&Op::Const));
        assert!(ops(&func, 1).contains(&Op::Const));
    }

    #[test]
    fn lifts_push_pop_dup_and_binary_imm() {
        // A dead push/pop pair and a dup-ed multiply, then `f[1] + 4`.
        let b = body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::Pop,
                Step::Push(Value::Number(3.0)),
                Step::Dup,
                Step::Binary(BinaryOp::Mul),
                Step::LoadLocal { slot: 1 },
                Step::BinaryImm {
                    op: BinaryOp::Add,
                    imm: 4.0,
                },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(func.block(entry).insts[4].imm, Imm::Float(4.0));
    }

    #[test]
    fn lifts_stores_and_unary() {
        // var x = f[0]; x = -x; return x;
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::InitLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::Unary(UnaryOp::Minus),
                Step::StoreLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(
            ops(&func, entry),
            vec![
                Op::FrameLoad,
                Op::FrameStore,
                Op::FrameLoad,
                Op::Neg,
                Op::FrameStore,
                Op::FrameLoad,
            ]
        );
        assert_eq!(func.block(entry).insts[1].imm, Imm::Slot(1));
    }

    #[test]
    fn an_uncertified_body_is_unsupported() {
        let mut b = body(vec![Step::Push(Value::Number(1.0)), Step::Return], 0);
        b.scope = None;
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Uncertified);
    }

    #[test]
    fn a_lexical_slot_gets_a_tdz_check() {
        // A `let` slot is TDZ-checked on load and store; `InitLocal`
        // (the initializing store) is not.
        let mut b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::InitLocal { slot: 0 },
                Step::LoadLocal { slot: 0 },
                Step::Return,
            ],
            1,
        );
        b.scope = Some(ScopeInfo {
            tdz_store: vec![true],
            ..scope(1)
        });
        let func = lift(&b).expect("lifts now");
        let insts = &func.block(func.entry()).insts;
        // Push, the InitLocal store (no check), then a TdzCheck before the load.
        assert_eq!(insts[0].op, Op::Const);
        assert_eq!(insts[1].op, Op::FrameStore);
        assert_eq!(insts[2].op, Op::TdzCheck);
        assert_eq!(insts[3].op, Op::FrameLoad);
    }

    #[test]
    fn lifts_a_loop() {
        // var s = 0; while (s < 3) { s = s + 1; } return s;
        //  0 Push 0        5 LoadLocal 1
        //  1 InitLocal 1   6 BinaryImm + 1
        //  2 LoadLocal 1   7 StoreLocal 1
        //  3 BinaryImm < 3 8 Jump 2      (back edge)
        //  4 JumpIfFalse 9 9 LoadLocal 1
        //                 10 Return
        let b = body(
            vec![
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::BinaryImm {
                    op: BinaryOp::LessThan,
                    imm: 3.0,
                },
                Step::JumpIfFalse(9),
                Step::LoadLocal { slot: 1 },
                Step::BinaryImm {
                    op: BinaryOp::Add,
                    imm: 1.0,
                },
                Step::StoreLocal { slot: 1 },
                Step::Jump(2),
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        // Blocks: [0,2) entry, [2,5) header, [5,9) body, [9,11) exit.
        assert_eq!(func.block_count(), 4);
        // The loop body (block 2) jumps back to the header (block 1).
        assert!(matches!(
            func.block(2).term,
            Some(Term::Jump { target: 1, .. })
        ));
    }

    #[test]
    fn lifts_a_fused_relational_test() {
        // var i = 0; var s = 0; while (i < 10) { s = s + i; i = i + 1; } return s;
        // The loop test is the fused `JumpIfLtImm` step.
        let b = body(
            vec![
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 0 },
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 1 },
                Step::JumpIfLtImm {
                    slot: 0,
                    imm: 10.0,
                    target: 13,
                },
                Step::LoadLocal { slot: 1 },
                Step::LoadLocal { slot: 0 },
                Step::Binary(BinaryOp::Add),
                Step::StoreLocal { slot: 1 },
                Step::LoadLocal { slot: 0 },
                Step::BinaryImm {
                    op: BinaryOp::Add,
                    imm: 1.0,
                },
                Step::StoreLocal { slot: 0 },
                Step::Jump(4),
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        // Some block ends in a `Branch` whose condition is the `i < 10` compare.
        let has_test = (0..func.block_count() as u32).any(|bi| {
            let block = func.block(bi);
            matches!(block.term, Some(Term::Branch { .. }))
                && block.insts.iter().any(|i| i.op == Op::Lt)
        });
        assert!(has_test, "the fused test lifted to a comparison + branch");
    }

    #[test]
    fn lifts_member_and_global_reads() {
        let member = crux::intern(&[u16::from(b'x')]);
        let global = crux::intern(&[u16::from(b'g')]);
        // LoadLocal 0; GetMemberName {x}; LoadGlobal {g}; Return
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::GetMemberName { name: member },
                Step::LoadGlobal { name: global },
                Step::Return,
            ],
            1,
        );
        let func = lift_impl(&b, false).expect("lifts");
        let insts = &func.block(func.entry()).insts;
        // A member read is a speculative cell load plus its validity guard.
        assert_eq!(insts[1].op, Op::MemberCellLoad);
        assert_eq!(insts[1].imm, Imm::Atom(member));
        assert_eq!(insts[2].op, Op::MemberGuard);
        assert_eq!(insts[2].imm, Imm::Atom(member));
        assert_eq!(insts[3].op, Op::GlobalLoad);
        assert_eq!(insts[3].imm, Imm::Atom(global));
    }

    #[test]
    fn the_typer_guards_a_member_read_when_enabled() {
        let member = crux::intern(&[u16::from(b'x')]);
        // LoadLocal 0; GetMemberName {x}; Return
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::GetMemberName { name: member },
                Step::Return,
            ],
            1,
        );
        // Off (and for a leaf body, whose lane cannot resume a deopt): the read
        // is the cell load + its validity guard, untyped.
        let off = lift_impl(&b, false).expect("lifts");
        let off_insts = &off.block(off.entry()).insts;
        assert_eq!(off_insts[1].op, Op::MemberCellLoad);
        assert_eq!(off_insts[2].op, Op::MemberGuard);
        assert!(!off_insts.iter().any(|i| i.op == Op::GuardType));

        // On: a `GuardType` types the read result `Number` so the arithmetic
        // that consumes it lowers tag-free, resuming at the step AFTER the read
        // (the read has already run once, so the interpreter must not re-run it).
        let on = lift_impl(&b, true).expect("lifts");
        let on_insts = &on.block(on.entry()).insts;
        let guard = on_insts
            .iter()
            .find(|i| i.op == Op::GuardType)
            .expect("the typer guards the read");
        assert_eq!(guard.ty, Type::Number);
        assert_eq!(guard.imm, Imm::Int(2));
    }

    #[test]
    fn lifts_a_computed_read() {
        // LoadLocal 0; Push 1; GetMemberComputed; Return
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::Push(crux::Value::Number(1.0)),
                Step::GetMemberComputed,
                Step::Return,
            ],
            1,
        );
        let func = lift_impl(&b, false).expect("lifts");
        let insts = &func.block(func.entry()).insts;
        assert_eq!(insts[0].op, Op::FrameLoad);
        assert_eq!(insts[1].op, Op::Const);
        assert_eq!(insts[2].op, Op::ElementLoad);
        assert_eq!(insts[2].args.len(), 2);
    }

    #[test]
    fn a_throw_is_unsupported() {
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Throw {
                    span: crux::Span::empty(0),
                },
            ],
            0,
        );
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Throw"));
    }

    #[test]
    fn a_non_numeric_constant_is_unsupported() {
        // A bare `return;` pushes `undefined`, which the constant set omits.
        let b = body(vec![Step::Push(Value::Undefined), Step::Return], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Push"));
    }

    #[test]
    fn a_body_without_a_return_is_unsupported() {
        let b = body(vec![Step::Push(Value::Number(1.0))], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::NoReturn);
    }

    #[test]
    fn constant_folds_nothing_but_types_the_result() {
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(BinaryOp::StrictEqual),
                Step::Push(Value::Number(3.0)),
                Step::Binary(BinaryOp::BitOr),
                Step::Return,
            ],
            0,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(func.block(entry).insts[2].ty, Type::Bool);
        assert_eq!(func.block(entry).insts[4].ty, Type::Int);
    }
}
