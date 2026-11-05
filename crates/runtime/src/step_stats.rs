//! Executed-step census for the interpreter.
//!
//! The interpreter's cost is `steps executed x cost per step`. The dispatch
//! loop's cost per step has been measured and driven down; the step COUNT has
//! not, and a workload that runs two extra stack steps per operation looks the
//! same as one that runs an expensive step when only wall time is measured.
//! This counter separates the two: join it with time from a default build to
//! get `ns per step` for a row, and read the bucket histogram to see which
//! family of steps the count is made of.
//!
//! It is a cargo feature rather than an environment switch because the
//! increment sits in the hottest loop of the engine. An atomic per step would
//! dominate the time it exists to explain, so it is compiled in only for a
//! counting run (`--features step-stats`) and disappears entirely otherwise.

#[cfg(feature = "step-stats")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ir::Step;

/// The step families worth distinguishing. Coarse on purpose: the question is
/// "is the count made of real work or of bookkeeping", not a per-variant
/// profile. The four bookkeeping families are separate because they have very
/// different elision safety: a statement-list/completion step is unobservable
/// in a certified function body, while a try entry is not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// `Push`/`Pop`/`Dup`/`PushAcc` — pure operand-stack traffic.
    Stack,
    /// Frame-slot reads and writes (a certified body's locals).
    Local,
    /// Identifier and Reference machinery: resolving a binding by name instead
    /// of reaching a slot.
    IdentRef,
    /// Global and context slots.
    EnvSlot,
    /// Arithmetic and comparison.
    Arith,
    /// Property reads.
    MemberRead,
    /// Property writes.
    MemberWrite,
    /// Calls and construct.
    Call,
    /// Jumps, branch tests, switch tests and the fused loop head.
    Control,
    /// `ListBegin`/`ListEnd` and the completion register.
    ListCompletion,
    /// `EnterBlock`/`LeaveBlock`/`EnterLoopEnv`/`EnterWith`.
    BlockEnv,
    /// `EnterTry`/`Exit`/`FinallyEnd`/`CatchBind`.
    TryEnv,
    /// `EnterPerIteration`/`PerIteration` — the fresh-binding machinery.
    PerIterEnv,
    /// `for-in`/`for-of` machinery.
    Iteration,
    /// Literals and allocation.
    Literal,
    /// The fused register body.
    RegBody,
    /// Anything not named above.
    Other,
}

const COUNT: usize = 17;

impl Bucket {
    pub const ALL: [Bucket; COUNT] = [
        Bucket::Stack,
        Bucket::Local,
        Bucket::IdentRef,
        Bucket::EnvSlot,
        Bucket::Arith,
        Bucket::MemberRead,
        Bucket::MemberWrite,
        Bucket::Call,
        Bucket::Control,
        Bucket::ListCompletion,
        Bucket::BlockEnv,
        Bucket::TryEnv,
        Bucket::PerIterEnv,
        Bucket::Iteration,
        Bucket::Literal,
        Bucket::RegBody,
        Bucket::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Bucket::Stack => "stack",
            Bucket::Local => "local",
            Bucket::IdentRef => "ident-ref",
            Bucket::EnvSlot => "env-slot",
            Bucket::Arith => "arith",
            Bucket::MemberRead => "member-read",
            Bucket::MemberWrite => "member-write",
            Bucket::Call => "call",
            Bucket::Control => "control",
            Bucket::ListCompletion => "list-completion",
            Bucket::BlockEnv => "block-env",
            Bucket::TryEnv => "try-env",
            Bucket::PerIterEnv => "per-iter-env",
            Bucket::Iteration => "iteration",
            Bucket::Literal => "literal",
            Bucket::RegBody => "reg-body",
            Bucket::Other => "other",
        }
    }

    #[cfg(feature = "step-stats")]
    fn index(self) -> usize {
        match self {
            Bucket::Stack => 0,
            Bucket::Local => 1,
            Bucket::IdentRef => 2,
            Bucket::EnvSlot => 3,
            Bucket::Arith => 4,
            Bucket::MemberRead => 5,
            Bucket::MemberWrite => 6,
            Bucket::Call => 7,
            Bucket::Control => 8,
            Bucket::ListCompletion => 9,
            Bucket::BlockEnv => 10,
            Bucket::TryEnv => 11,
            Bucket::PerIterEnv => 12,
            Bucket::Iteration => 13,
            Bucket::Literal => 14,
            Bucket::RegBody => 15,
            Bucket::Other => 16,
        }
    }
}

/// The family a step belongs to.
pub fn tag(step: &Step) -> Bucket {
    match step {
        Step::Push(_) | Step::PushStr(_) | Step::Pop | Step::Dup | Step::Dup2 => Bucket::Stack,
        Step::PushAcc | Step::PopAcc => Bucket::Control,

        Step::LoadLocal { .. }
        | Step::StoreLocal { .. }
        | Step::FusedStoreLocal { .. }
        | Step::InitLocal { .. }
        | Step::Inc { .. }
        | Step::Dec { .. } => Bucket::Local,

        Step::LoadIdent { .. }
        | Step::StoreIdent { .. }
        | Step::ResolveVarIdent { .. }
        | Step::ResolveIdentStore { .. }
        | Step::GetVarReference
        | Step::GetVarReferenceThis
        | Step::PutVarReference
        | Step::PutVarReferenceOp { .. }
        | Step::UpdateVarReference { .. }
        | Step::PopVarReference
        | Step::ResolveMemberRefName { .. }
        | Step::ResolveMemberRefComputed
        | Step::ResolveSuperRefName { .. }
        | Step::ResolveSuperRefComputed
        | Step::ResolvePrivateRef { .. } => Bucket::IdentRef,

        Step::LoadContextSlot { .. }
        | Step::StoreContextSlot { .. }
        | Step::InitContextSlot { .. }
        | Step::LoadPerIteration { .. }
        | Step::StorePerIteration { .. }
        | Step::UpdatePerIteration { .. }
        | Step::StoreGlobal { .. }
        | Step::FusedStoreGlobal { .. } => Bucket::EnvSlot,

        Step::Binary(_) | Step::BinaryImm { .. } | Step::Unary(_) => Bucket::Arith,

        Step::GetMemberName { .. }
        | Step::GetMemberComputed
        | Step::GetMemberComputedKeep
        | Step::GetPrivate { .. }
        | Step::GetSuperName { .. }
        | Step::GetSuperComputed
        | Step::GetSuperComputedKeep
        | Step::GetSuperBase => Bucket::MemberRead,

        Step::AssignMemberName { .. }
        | Step::AssignMemberComputed { .. }
        | Step::AssignPrivate { .. }
        | Step::AssignSuperName { .. }
        | Step::AssignSuperComputed { .. }
        | Step::UpdateMemberName { .. }
        | Step::UpdateMemberComputed { .. }
        | Step::UpdatePrivate { .. }
        | Step::UpdateSuperName { .. }
        | Step::UpdateSuperComputed { .. }
        | Step::AssignIdent { .. }
        | Step::DeleteMemberName { .. }
        | Step::DeleteMemberComputed => Bucket::MemberWrite,

        Step::CallFast { .. }
        | Step::CallFastSlot { .. }
        | Step::CallFastGlobal { .. }
        | Step::CallIntrinsic { .. }
        | Step::CallFastSlotStore { .. }
        | Step::CallFastGlobalStore { .. }
        | Step::Call { .. }
        | Step::Construct { .. }
        | Step::CallApply { .. }
        | Step::TaggedTemplate { .. } => Bucket::Call,

        Step::Jump(_)
        | Step::JumpIfFalse(_)
        | Step::JumpIfTrue(_)
        | Step::JumpIfFalseKeep(_)
        | Step::JumpIfTrueKeep(_)
        | Step::JumpIfNullishKeep(_)
        | Step::JumpIfNotNullishKeep(_)
        | Step::JumpIfChainShort(_)
        | Step::JumpIfLtImm { .. }
        | Step::JumpIfLeImm { .. }
        | Step::JumpIfGtImm { .. }
        | Step::JumpIfGeImm { .. }
        | Step::JumpIfEqImm { .. }
        | Step::JumpIfNeqImm { .. }
        | Step::JumpIfLtGlobalImm { .. }
        | Step::JumpIfLeGlobalImm { .. }
        | Step::JumpIfGtGlobalImm { .. }
        | Step::JumpIfGeGlobalImm { .. }
        | Step::JumpIfRelLimit { .. }
        | Step::SwitchDisc
        | Step::SwitchTest { .. }
        | Step::Break { .. }
        | Step::Continue { .. }
        | Step::Return
        | Step::FastLoopHead { .. }
        | Step::IncGlobal { .. }
        | Step::DecGlobal { .. }
        | Step::IncAcc
        | Step::DecAcc
        | Step::Yield { .. } => Bucket::Control,

        Step::ListBegin
        | Step::ListEnd
        | Step::SetCompletion
        | Step::ResetCompletion
        | Step::NormalizeCompletion => Bucket::ListCompletion,

        Step::EnterBlock { .. }
        | Step::LeaveBlock
        | Step::EnterLoopEnv { .. }
        | Step::EnterWith => Bucket::BlockEnv,

        Step::EnterTry { .. } | Step::Exit { .. } | Step::FinallyEnd | Step::CatchBind { .. } => {
            Bucket::TryEnv
        }

        Step::EnterPerIteration { .. } | Step::PerIteration { .. } => Bucket::PerIterEnv,

        Step::ForInBegin { .. }
        | Step::ForInNext { .. }
        | Step::ForOfBegin { .. }
        | Step::ForOfNext { .. }
        | Step::ForOfNextBindLocal { .. }
        | Step::ForOfBindLocal { .. }
        | Step::ForOfBindGlobal { .. }
        | Step::ForOfClose => Bucket::Iteration,

        Step::ArrayBegin
        | Step::ObjectBegin
        | Step::RegExpLiteral { .. }
        | Step::CreateFunction { .. }
        | Step::CreateArrow { .. } => Bucket::Literal,

        Step::RunRegBody { .. } => Bucket::RegBody,

        _ => Bucket::Other,
    }
}

#[cfg(feature = "step-stats")]
static STEPS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "step-stats")]
static BUCKETS: [AtomicU64; COUNT] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Record one dispatched interpreter step.
#[inline(always)]
pub fn count_step(step: &Step) {
    #[cfg(feature = "step-stats")]
    {
        STEPS.fetch_add(1, Ordering::Relaxed);
        BUCKETS[tag(step).index()].fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(not(feature = "step-stats"))]
    {
        let _ = step;
    }
}

/// Steps dispatched since process start (always 0 when the census is off).
#[inline]
pub fn steps() -> u64 {
    #[cfg(feature = "step-stats")]
    {
        STEPS.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "step-stats"))]
    {
        0
    }
}

/// Per-bucket counts since process start (empty when the census is off).
pub fn buckets() -> Vec<(Bucket, u64)> {
    #[cfg(feature = "step-stats")]
    {
        Bucket::ALL
            .iter()
            .map(|b| (*b, BUCKETS[b.index()].load(Ordering::Relaxed)))
            .collect()
    }
    #[cfg(not(feature = "step-stats"))]
    {
        Vec::new()
    }
}

/// Print the bucket histogram to stderr (a diagnostic, not a result).
pub fn dump_buckets() {
    let counts = buckets();
    if counts.is_empty() {
        return;
    }
    let total: u64 = counts.iter().map(|(_, n)| *n).sum();
    let _ = total;
    for (bucket, n) in counts {
        if n == 0 {
            continue;
        }
        eprintln!("  step-bucket\t{}\t{n}", bucket.name());
    }
}

/// Whether the census was compiled in.
pub const fn enabled() -> bool {
    cfg!(feature = "step-stats")
}
