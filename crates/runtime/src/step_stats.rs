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

/// The census's per-variant axis: every `Step` variant in declaration
/// order, so a count names the instruction that produced it. The
/// interpreter's dispatch is one 3,000-line `match` over these; drawing a
/// structural boundary through it (a hot core plus per-family handlers)
/// needs the per-variant counts per row, which the bucket histogram cannot
/// give.
#[cfg(feature = "step-stats")]
pub const VARIANT_COUNT: usize = 233;

/// The variant names, indexed like [`variant_index`].
#[cfg(feature = "step-stats")]
pub const VARIANT_NAMES: [&str; VARIANT_COUNT] = [
    "DebugCheck",
    "Push",
    "Pop",
    "Dup",
    "Dup2",
    "Unary",
    "Binary",
    "BinaryImm",
    "GetMemberName",
    "GetMemberComputed",
    "GetMemberComputedKeep",
    "GetSuperComputedKeep",
    "GetSuperName",
    "GetSuperComputed",
    "GetPrivate",
    "ThisValue",
    "LoadIdent",
    "LoadLocal",
    "StoreLocal",
    "FusedStoreLocal",
    "FusedStoreGlobal",
    "InitLocal",
    "FusedCompoundLocal",
    "UpdateLocal",
    "Inc",
    "Dec",
    "GetSuperBase",
    "AssignIdent",
    "AssignMemberName",
    "AssignMemberComputed",
    "AssignSuperName",
    "AssignSuperComputed",
    "AssignPrivate",
    "Destructure",
    "DestructureBegin",
    "DestructureNext",
    "DestructureUndef",
    "DestructureRest",
    "DestructureObjCoercible",
    "DestructureObjKey",
    "DestructureObjKeyComputed",
    "DestructureObjKeyStore",
    "DestructureObjKeyGet",
    "DestructureObjRest",
    "DestructureClose",
    "DestructureObjEnd",
    "DeclInit",
    "ResolveVarIdent",
    "ResolvePrivateRef",
    "ResolveMemberRefName",
    "ResolveMemberRefComputed",
    "GetVarReference",
    "GetVarReferenceThis",
    "PutVarReference",
    "PopVarReference",
    "ResolveSuperRefName",
    "ResolveSuperRefComputed",
    "PutVarReferenceOp",
    "UpdateVarReference",
    "UsingInit",
    "SetFunctionName",
    "SetChainShort",
    "ClearChainShort",
    "JumpIfChainShort",
    "UpdateIdent",
    "UpdateMemberName",
    "UpdateMemberComputed",
    "UpdateSuperName",
    "UpdateSuperComputed",
    "UpdatePrivate",
    "DeleteIdent",
    "DeleteMemberName",
    "DeleteMemberComputed",
    "DeleteSuper",
    "TypeofTop",
    "TypeofIdent",
    "PrivateIn",
    "Call",
    "CallFast",
    "CallIntrinsic",
    "CallApply",
    "CallFastGlobal",
    "CallFastSlot",
    "CallFastIdent",
    "CallFastSlotStore",
    "CallFastGlobalStore",
    "TailCallFast",
    "TailCall",
    "TailCallFastGlobal",
    "TailCallFastSlot",
    "TailCallSelf",
    "TailCallSelfCheck",
    "TailCallSelfVector",
    "TailCallSelfCheckVector",
    "TailTaggedTemplate",
    "SuperCall",
    "Construct",
    "TaggedTemplate",
    "CreateFunction",
    "CreateArrow",
    "NewTarget",
    "RegExpLiteral",
    "ArrayBegin",
    "ArrayElement",
    "ArraySpread",
    "ArrayHole",
    "ArrayEnd",
    "ArrayFast",
    "ClassBegin",
    "ClassHeritage",
    "ClassKeyToPropertyKey",
    "ClassFinish",
    "ObjectBegin",
    "ObjectFast",
    "ObjectInitName",
    "ObjectInitComputed",
    "ObjectKeyToPropertyKey",
    "ObjectMethodName",
    "ObjectMethodComputed",
    "ObjectAccessorName",
    "ObjectAccessorComputed",
    "ObjectSpread",
    "PushStr",
    "ConcatStr",
    "ConcatStrConst",
    "ArgsBase",
    "ArgsPush",
    "ArgsSpread",
    "EnterBlock",
    "FunctionDecl",
    "FunctionDeclInit",
    "ResetCompletion",
    "NormalizeCompletion",
    "ListBegin",
    "ListEnd",
    "SaveCompletion",
    "RestoreCompletion",
    "LeaveBlock",
    "EnterTry",
    "Exit",
    "CatchBind",
    "FinallyEnd",
    "EnterWith",
    "PerIteration",
    "EnterLoopEnv",
    "EnterIterTdzEnv",
    "LeaveIterTdzEnv",
    "ForInBegin",
    "ForInNext",
    "ForInBind",
    "ForInRestore",
    "ForOfBegin",
    "ForOfNext",
    "ForOfNextBindLocal",
    "ForOfBind",
    "ForOfBindLocal",
    "ForOfBindGlobal",
    "ForOfRestore",
    "ForOfClose",
    "AsyncForOfBegin",
    "AsyncForOfNext",
    "AsyncForOfTest",
    "AsyncForOfBind",
    "AsyncForOfRestore",
    "AsyncForOfClose",
    "SwitchDisc",
    "SwitchTest",
    "SetCompletion",
    "Jump",
    "JumpIfFalse",
    "JumpIfTrue",
    "JumpIfFalseKeep",
    "JumpIfTrueKeep",
    "JumpIfNullishKeep",
    "JumpIfNotNullishKeep",
    "JumpIfLtImm",
    "JumpIfLeImm",
    "JumpIfGtImm",
    "JumpIfGeImm",
    "JumpIfEqImm",
    "JumpIfNeqImm",
    "JumpIfRelLimit",
    "TypedArrayLengthHoist",
    "HoistMemberGuard",
    "HoistGlobalGuard",
    "LoadGlobal",
    "StoreGlobal",
    "ScriptGlobalFlush",
    "ScriptGlobalReload",
    "ResolveIdentStore",
    "StoreIdent",
    "UpdateGlobal",
    "IncGlobal",
    "DecGlobal",
    "JumpIfLtGlobalImm",
    "JumpIfLeGlobalImm",
    "JumpIfGtGlobalImm",
    "JumpIfGeGlobalImm",
    "FastLoopHead",
    "FastLoopBind",
    "FastLoopStore",
    "BuilderBind",
    "BuilderStore",
    "RunRegBody",
    "PushAcc",
    "PopAcc",
    "IncAcc",
    "DecAcc",
    "LoadContextSlot",
    "StoreContextSlot",
    "InitContextSlot",
    "UpdateContextSlot",
    "EnterPerIteration",
    "LoadPerIteration",
    "StorePerIteration",
    "UpdatePerIteration",
    "CreateArguments",
    "Break",
    "Continue",
    "InvalidAssignmentTarget",
    "Return",
    "Throw",
    "Yield",
    "Await",
    "YieldStarBegin",
    "YieldStarNext",
    "YieldStarResume",
    "AsyncYieldStarBegin",
    "AsyncYieldStarNext",
    "AsyncYieldStarInspect",
    "AsyncYieldStarResume",
    "ImportCall",
    "ImportMeta",
];

/// The index [`count_step`] counts `step` under. The match is exhaustive,
/// so adding a `Step` variant fails the census build until it is named here.
#[cfg(feature = "step-stats")]
pub fn variant_index(step: &Step) -> usize {
    match step {
        Step::DebugCheck { .. } => 0,
        Step::Push(_) => 1,
        Step::Pop => 2,
        Step::Dup => 3,
        Step::Dup2 => 4,
        Step::Unary(_) => 5,
        Step::Binary(_) => 6,
        Step::BinaryImm { .. } => 7,
        Step::GetMemberName { .. } => 8,
        Step::GetMemberComputed => 9,
        Step::GetMemberComputedKeep => 10,
        Step::GetSuperComputedKeep => 11,
        Step::GetSuperName { .. } => 12,
        Step::GetSuperComputed => 13,
        Step::GetPrivate { .. } => 14,
        Step::ThisValue => 15,
        Step::LoadIdent { .. } => 16,
        Step::LoadLocal { .. } => 17,
        Step::StoreLocal { .. } => 18,
        Step::FusedStoreLocal { .. } => 19,
        Step::FusedStoreGlobal { .. } => 20,
        Step::InitLocal { .. } => 21,
        Step::FusedCompoundLocal { .. } => 22,
        Step::UpdateLocal { .. } => 23,
        Step::Inc { .. } => 24,
        Step::Dec { .. } => 25,
        Step::GetSuperBase => 26,
        Step::AssignIdent { .. } => 27,
        Step::AssignMemberName { .. } => 28,
        Step::AssignMemberComputed { .. } => 29,
        Step::AssignSuperName { .. } => 30,
        Step::AssignSuperComputed { .. } => 31,
        Step::AssignPrivate { .. } => 32,
        Step::Destructure { .. } => 33,
        Step::DestructureBegin => 34,
        Step::DestructureNext => 35,
        Step::DestructureUndef { .. } => 36,
        Step::DestructureRest => 37,
        Step::DestructureObjCoercible => 38,
        Step::DestructureObjKey { .. } => 39,
        Step::DestructureObjKeyComputed => 40,
        Step::DestructureObjKeyStore => 41,
        Step::DestructureObjKeyGet => 42,
        Step::DestructureObjRest { .. } => 43,
        Step::DestructureClose => 44,
        Step::DestructureObjEnd => 45,
        Step::DeclInit { .. } => 46,
        Step::ResolveVarIdent { .. } => 47,
        Step::ResolvePrivateRef { .. } => 48,
        Step::ResolveMemberRefName { .. } => 49,
        Step::ResolveMemberRefComputed => 50,
        Step::GetVarReference => 51,
        Step::GetVarReferenceThis => 52,
        Step::PutVarReference => 53,
        Step::PopVarReference => 54,
        Step::ResolveSuperRefName { .. } => 55,
        Step::ResolveSuperRefComputed => 56,
        Step::PutVarReferenceOp { .. } => 57,
        Step::UpdateVarReference { .. } => 58,
        Step::UsingInit { .. } => 59,
        Step::SetFunctionName { .. } => 60,
        Step::SetChainShort => 61,
        Step::ClearChainShort => 62,
        Step::JumpIfChainShort(_) => 63,
        Step::UpdateIdent { .. } => 64,
        Step::UpdateMemberName { .. } => 65,
        Step::UpdateMemberComputed { .. } => 66,
        Step::UpdateSuperName { .. } => 67,
        Step::UpdateSuperComputed { .. } => 68,
        Step::UpdatePrivate { .. } => 69,
        Step::DeleteIdent { .. } => 70,
        Step::DeleteMemberName { .. } => 71,
        Step::DeleteMemberComputed => 72,
        Step::DeleteSuper => 73,
        Step::TypeofTop => 74,
        Step::TypeofIdent { .. } => 75,
        Step::PrivateIn { .. } => 76,
        Step::Call { .. } => 77,
        Step::CallFast { .. } => 78,
        Step::CallIntrinsic { .. } => 79,
        Step::CallApply { .. } => 80,
        Step::CallFastGlobal { .. } => 81,
        Step::CallFastSlot { .. } => 82,
        Step::CallFastIdent { .. } => 83,
        Step::CallFastSlotStore { .. } => 84,
        Step::CallFastGlobalStore { .. } => 85,
        Step::TailCallFast { .. } => 86,
        Step::TailCall { .. } => 87,
        Step::TailCallFastGlobal { .. } => 88,
        Step::TailCallFastSlot { .. } => 89,
        Step::TailCallSelf { .. } => 90,
        Step::TailCallSelfCheck { .. } => 91,
        Step::TailCallSelfVector => 92,
        Step::TailCallSelfCheckVector => 93,
        Step::TailTaggedTemplate(_) => 94,
        Step::SuperCall => 95,
        Step::Construct { .. } => 96,
        Step::TaggedTemplate(_) => 97,
        Step::CreateFunction { .. } => 98,
        Step::CreateArrow { .. } => 99,
        Step::NewTarget => 100,
        Step::RegExpLiteral { .. } => 101,
        Step::ArrayBegin => 102,
        Step::ArrayElement => 103,
        Step::ArraySpread => 104,
        Step::ArrayHole => 105,
        Step::ArrayEnd => 106,
        Step::ArrayFast { .. } => 107,
        Step::ClassBegin { .. } => 108,
        Step::ClassHeritage => 109,
        Step::ClassKeyToPropertyKey => 110,
        Step::ClassFinish { .. } => 111,
        Step::ObjectBegin => 112,
        Step::ObjectFast { .. } => 113,
        Step::ObjectInitName { .. } => 114,
        Step::ObjectInitComputed { .. } => 115,
        Step::ObjectKeyToPropertyKey => 116,
        Step::ObjectMethodName { .. } => 117,
        Step::ObjectMethodComputed { .. } => 118,
        Step::ObjectAccessorName { .. } => 119,
        Step::ObjectAccessorComputed { .. } => 120,
        Step::ObjectSpread => 121,
        Step::PushStr(_) => 122,
        Step::ConcatStr => 123,
        Step::ConcatStrConst(_) => 124,
        Step::ArgsBase => 125,
        Step::ArgsPush => 126,
        Step::ArgsSpread => 127,
        Step::EnterBlock { .. } => 128,
        Step::FunctionDecl { .. } => 129,
        Step::FunctionDeclInit { .. } => 130,
        Step::ResetCompletion => 131,
        Step::NormalizeCompletion => 132,
        Step::ListBegin => 133,
        Step::ListEnd => 134,
        Step::SaveCompletion => 135,
        Step::RestoreCompletion => 136,
        Step::LeaveBlock => 137,
        Step::EnterTry { .. } => 138,
        Step::Exit { .. } => 139,
        Step::CatchBind { .. } => 140,
        Step::FinallyEnd => 141,
        Step::EnterWith => 142,
        Step::PerIteration { .. } => 143,
        Step::EnterLoopEnv { .. } => 144,
        Step::EnterIterTdzEnv { .. } => 145,
        Step::LeaveIterTdzEnv => 146,
        Step::ForInBegin { .. } => 147,
        Step::ForInNext { .. } => 148,
        Step::ForInBind { .. } => 149,
        Step::ForInRestore => 150,
        Step::ForOfBegin { .. } => 151,
        Step::ForOfNext { .. } => 152,
        Step::ForOfNextBindLocal { .. } => 153,
        Step::ForOfBind { .. } => 154,
        Step::ForOfBindLocal { .. } => 155,
        Step::ForOfBindGlobal { .. } => 156,
        Step::ForOfRestore => 157,
        Step::ForOfClose => 158,
        Step::AsyncForOfBegin { .. } => 159,
        Step::AsyncForOfNext => 160,
        Step::AsyncForOfTest { .. } => 161,
        Step::AsyncForOfBind { .. } => 162,
        Step::AsyncForOfRestore => 163,
        Step::AsyncForOfClose => 164,
        Step::SwitchDisc => 165,
        Step::SwitchTest { .. } => 166,
        Step::SetCompletion => 167,
        Step::Jump(_) => 168,
        Step::JumpIfFalse(_) => 169,
        Step::JumpIfTrue(_) => 170,
        Step::JumpIfFalseKeep(_) => 171,
        Step::JumpIfTrueKeep(_) => 172,
        Step::JumpIfNullishKeep(_) => 173,
        Step::JumpIfNotNullishKeep(_) => 174,
        Step::JumpIfLtImm { .. } => 175,
        Step::JumpIfLeImm { .. } => 176,
        Step::JumpIfGtImm { .. } => 177,
        Step::JumpIfGeImm { .. } => 178,
        Step::JumpIfEqImm { .. } => 179,
        Step::JumpIfNeqImm { .. } => 180,
        Step::JumpIfRelLimit { .. } => 181,
        Step::TypedArrayLengthHoist { .. } => 182,
        Step::HoistMemberGuard { .. } => 183,
        Step::HoistGlobalGuard { .. } => 184,
        Step::LoadGlobal { .. } => 185,
        Step::StoreGlobal { .. } => 186,
        Step::ScriptGlobalFlush { .. } => 187,
        Step::ScriptGlobalReload { .. } => 188,
        Step::ResolveIdentStore { .. } => 189,
        Step::StoreIdent { .. } => 190,
        Step::UpdateGlobal { .. } => 191,
        Step::IncGlobal { .. } => 192,
        Step::DecGlobal { .. } => 193,
        Step::JumpIfLtGlobalImm { .. } => 194,
        Step::JumpIfLeGlobalImm { .. } => 195,
        Step::JumpIfGtGlobalImm { .. } => 196,
        Step::JumpIfGeGlobalImm { .. } => 197,
        Step::FastLoopHead { .. } => 198,
        Step::FastLoopBind { .. } => 199,
        Step::FastLoopStore { .. } => 200,
        Step::BuilderBind { .. } => 201,
        Step::BuilderStore { .. } => 202,
        Step::RunRegBody { .. } => 203,
        Step::PushAcc => 204,
        Step::PopAcc => 205,
        Step::IncAcc => 206,
        Step::DecAcc => 207,
        Step::LoadContextSlot { .. } => 208,
        Step::StoreContextSlot { .. } => 209,
        Step::InitContextSlot { .. } => 210,
        Step::UpdateContextSlot { .. } => 211,
        Step::EnterPerIteration { .. } => 212,
        Step::LoadPerIteration { .. } => 213,
        Step::StorePerIteration { .. } => 214,
        Step::UpdatePerIteration { .. } => 215,
        Step::CreateArguments { .. } => 216,
        Step::Break { .. } => 217,
        Step::Continue { .. } => 218,
        Step::InvalidAssignmentTarget => 219,
        Step::Return => 220,
        Step::Throw { .. } => 221,
        Step::Yield { .. } => 222,
        Step::Await { .. } => 223,
        Step::YieldStarBegin => 224,
        Step::YieldStarNext { .. } => 225,
        Step::YieldStarResume { .. } => 226,
        Step::AsyncYieldStarBegin => 227,
        Step::AsyncYieldStarNext { .. } => 228,
        Step::AsyncYieldStarInspect { .. } => 229,
        Step::AsyncYieldStarResume { .. } => 230,
        Step::ImportCall { .. } => 231,
        Step::ImportMeta => 232,
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

/// Per-variant counts, in [`VARIANT_NAMES`] order.
#[cfg(feature = "step-stats")]
static VARIANT_STEPS: [AtomicU64; VARIANT_COUNT] = [const { AtomicU64::new(0) }; VARIANT_COUNT];

/// Record one dispatched interpreter step.
#[inline(always)]
pub fn count_step(step: &Step) {
    #[cfg(feature = "step-stats")]
    {
        STEPS.fetch_add(1, Ordering::Relaxed);
        BUCKETS[tag(step).index()].fetch_add(1, Ordering::Relaxed);
        VARIANT_STEPS[variant_index(step)].fetch_add(1, Ordering::Relaxed);
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

/// Per-variant counts since process start, descending by count (empty when the
/// census is off). This is the list a structural split of the dispatch works
/// from: it says which arms a row actually executes.
pub fn variants() -> Vec<(&'static str, u64)> {
    #[cfg(feature = "step-stats")]
    {
        let mut counts: Vec<(&'static str, u64)> = VARIANT_NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| (*name, VARIANT_STEPS[index].load(Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        counts
    }
    #[cfg(not(feature = "step-stats"))]
    {
        Vec::new()
    }
}

/// Print the top `limit` variants by count to stderr (a diagnostic, not a
/// result).
pub fn dump_variants(limit: usize) {
    let counts = variants();
    let total: u64 = counts.iter().map(|(_, n)| *n).sum();
    for (name, n) in counts.iter().take(limit) {
        let share = if total == 0 {
            0.0
        } else {
            100.0 * *n as f64 / total as f64
        };
        eprintln!("  step-variant	{name}	{n}	{share:.2}%");
    }
}

/// Whether the census was compiled in.
pub const fn enabled() -> bool {
    cfg!(feature = "step-stats")
}
