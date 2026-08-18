//! The resumable-function IR (PLAN §4.5): generator and async function bodies
//! compile to a linear `Vec<Step>` with an explicit instruction pointer, so
//! `yield`/`await` suspend and resume exactly like the spec's generator
//! resume algorithm. This is the seed of the Phase 18 bytecode VM.
//!
//! Every expression and statement compiles to `Step`s; suspension is just
//! more bytecode (`Yield`/`Await`), so generators and async functions are
//! ordinary compiled bodies. Control flow is explicit jumps; lexical
//! environments mirror the compiler's scope stack on the VM side;
//! `try`/`catch`/`finally` use a handler table plus a runtime try stack and
//! a pending-control slot.

use std::collections::HashMap;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};
use syntax::ast::{
    Argument, ArrayBindingElement, ArrayElement, AssignOp, BinaryOp, BindingElement,
    BindingPattern, Class, ClassElement, ClassElementName, Expr, ExprKind, ForBinding, ForInit,
    Function, LogicalOp, MemberProperty, ObjectLiteral, ObjectProperty, PropertyName, Stmt,
    StmtKind, SwitchCase, UnaryOp, UpdateOp, VarDeclKind, VarDeclarator,
};

use crate::agent::Agent;
use crate::context::{
    get_new_target, get_super_base, get_super_constructor, get_this_environment,
    resolve_this_binding,
};
use crate::env::{EnvRef, new_declarative_environment};
use crate::eval::block_declaration_instantiation;
use crate::expr::{get_iterator, iterator_close, iterator_step};
use crate::flow::Completion;
use crate::function::EcmaFunction;

/// One resumable-function instruction.
#[derive(Debug, Clone)]
pub enum Step {
    // ----- stack -----
    Push(Value),
    Pop,
    Dup,
    /// Duplicate the top two values in order: `[a, b] -> [a, b, a, b]` — a
    /// computed member/super compound assignment needs the (base, key) pair
    /// twice: once for the read, once for the write.
    Dup2,
    // ----- expression continuation steps -----
    Unary(UnaryOp),
    Binary(BinaryOp),
    GetMemberName {
        name: crux::AtomId,
    },
    GetMemberComputed,
    GetSuperName {
        name: crux::AtomId,
    },
    GetSuperComputed,
    GetPrivate {
        atom: crux::AtomId,
    },
    ThisValue,
    /// Resolve a bare identifier reference (TDZ check included) and push its
    /// current value (spec 13.1.2).
    LoadIdent {
        name: crux::AtomId,
    },
    GetSuperBase,
    AssignIdent {
        name: crux::AtomId,
        op: AssignOp,
        set_name: bool,
    },
    AssignMemberName {
        name: crux::AtomId,
        op: AssignOp,
    },
    AssignMemberComputed {
        op: AssignOp,
    },
    AssignSuperName {
        name: crux::AtomId,
        op: AssignOp,
    },
    AssignSuperComputed {
        op: AssignOp,
    },
    AssignPrivate {
        atom: crux::AtomId,
        op: AssignOp,
    },
    Destructure {
        pattern: BindingPattern,
    },
    // ----- step-based destructuring (suspension-capable) -----
    /// Pop the value and GetIterator, pushing the record on the destructure
    /// stack.
    DestructureBegin,
    /// Step the innermost destructure iterator, pushing the value; when
    /// exhausted the element still receives *undefined* (a default
    /// initializer must run and may suspend) and the iterator is marked done
    /// so the trailing close is skipped.
    DestructureNext,
    /// Pop the value; if undefined, jump to `use_default` (the value is
    /// consumed); otherwise push it back for the assignment.
    DestructureUndef {
        use_default: usize,
    },
    /// Collect the remaining values of the innermost destructure iterator
    /// into a fresh array, push it, and pop the iterator (no close).
    DestructureRest,
    /// Pop the value; if it is null or undefined throw a TypeError, else
    /// push it back (RequireObjectCoercible of an object pattern).
    DestructureObjCoercible,
    /// Pop the object (a dup) and push the value of the constant property
    /// key.
    DestructureObjKey {
        key: crux::property::PropertyKey,
    },
    /// Pop the key then the object (a dup) and push the property value.
    DestructureObjKeyComputed,
    /// Pop the object (a dup), CopyDataProperties into a fresh rest object,
    /// and push it.
    DestructureObjRest {
        excluded: Vec<crux::property::PropertyKey>,
    },
    /// Pop the innermost destructure iterator and close it (a normal
    /// completion: the pattern consumed fewer values than the iterator
    /// held).
    DestructureClose,
    /// Pop the object pattern's base object off the object stack.
    DestructureObjEnd,
    /// Initialize a let/const/using declaration's binding in the current
    /// lexical environment; the binding was created uninitialized by
    /// declaration instantiation (unlike `Destructure`, which puts a value
    /// into an already-initialized binding).
    DeclInit {
        pattern: BindingPattern,
    },
    /// Resolve a `var` declaration's binding before its initializer runs
    /// (spec 14.3.2 step 2): a `with` object's property is the assignment
    /// target even when the initializer mutates the object.
    ResolveVarIdent {
        name: crux::AtomId,
    },
    /// Resolve a private member reference: pop the receiver, resolve the
    /// private name, and push the reference — the base stays off the value
    /// stack, so a short-circuited `&&=`/`||=`/`??=` leaves only the old
    /// value as the result.
    ResolvePrivateRef {
        atom: crux::AtomId,
    },
    /// Resolve a named member reference the same way: pop the receiver and
    /// push the reference.
    ResolveMemberRefName {
        name: crux::AtomId,
    },
    /// Resolve a computed member reference: pop the key (converting it to a
    /// property key once, spec 13.15.4: ToPropertyKey runs before the read)
    /// and the receiver, then push the reference.
    ResolveMemberRefComputed,
    /// Push the value of the reference on top of the var-reference stack —
    /// the assignment target resolved before its RHS evaluated (spec
    /// 13.15.3 steps 1-5: PutValue uses the initially created reference even
    /// if the binding is gone by then).
    GetVarReference,
    /// Assign a `var` declaration's value to the pre-resolved reference.
    PutVarReference,
    /// Discard the pre-resolved reference without assigning (a
    /// short-circuited `&&=`/`||=`/`??=` never reaches PutVarReference).
    PopVarReference,
    /// Resolve a super property reference: GetThisBinding then GetSuperBase
    /// (spec 13.3.7.1), the receiver recorded in [[ThisValue]].
    ResolveSuperRefName {
        name: crux::AtomId,
    },
    /// Resolve a computed super property reference: the base copy is already
    /// on the stack (GetSuperBase ran the this-check first), the converted
    /// key on top; both are consumed and the reference recorded.
    ResolveSuperRefComputed,
    /// Apply a compound assignment (`+=`, ...) to the pre-resolved
    /// reference: pop the new value and the old value, compute the result,
    /// and put it.
    PutVarReferenceOp {
        op: AssignOp,
    },
    /// Apply a `++`/`--` to the pre-resolved reference.
    UpdateVarReference {
        op: UpdateOp,
        prefix: bool,
    },
    /// Evaluate a `using`/`await using` initializer: register the value as a
    /// disposable resource on the current lexical environment, then bind it
    /// (spec 9.3.1 / 14.2.2).
    UsingInit {
        pattern: BindingPattern,
        is_await: bool,
    },
    /// Set the `name` property of the value on top of the stack (NamedEvaluation,
    /// spec 14.2.2 step 2.d): pop, set, push.
    SetFunctionName {
        name: crux::AtomId,
    },
    /// An upstream `?.` short-circuited: the rest of the chain is skipped
    /// (spec 13.4.3 optional chains propagate undefined without evaluating
    /// the remaining links).
    SetChainShort,
    ClearChainShort,
    JumpIfChainShort(usize),
    UpdateIdent {
        name: crux::AtomId,
        op: UpdateOp,
        prefix: bool,
    },
    UpdateMemberName {
        name: crux::AtomId,
        op: UpdateOp,
        prefix: bool,
    },
    UpdateMemberComputed {
        op: UpdateOp,
        prefix: bool,
    },
    UpdateSuperName {
        name: crux::AtomId,
        op: UpdateOp,
        prefix: bool,
    },
    UpdateSuperComputed {
        op: UpdateOp,
        prefix: bool,
    },
    UpdatePrivate {
        atom: crux::AtomId,
        op: UpdateOp,
        prefix: bool,
    },
    DeleteIdent {
        name: crux::AtomId,
    },
    DeleteMemberName {
        name: crux::AtomId,
    },
    DeleteMemberComputed,
    /// `delete super.x` / `delete super[x]`: a ReferenceError before the key
    /// is evaluated (spec 13.5.1.2 step 4.b).
    DeleteSuper,
    /// Pop a value and push its property key (ToPropertyKey, spec 7.3.21) as
    /// a string/symbol value: a computed member compound assignment converts
    /// the key once, before the read and the write each consume a copy.
    ToPropertyKey,
    TypeofTop,
    /// `typeof <identifier>`: an unresolvable reference yields "undefined"
    /// instead of throwing (spec 13.5.3.2 step 1).
    TypeofIdent {
        name: crux::AtomId,
    },
    PrivateIn {
        atom: crux::AtomId,
    },
    /// Call with `[this, callee]` on the stack and the arguments in the
    /// VM's argument slot.
    Call {
        direct_eval: bool,
    },
    SuperCall,
    Construct,
    TaggedTemplate(syntax::ast::TemplateLiteral),
    /// Create a function expression's closure against the current lexical
    /// environment (spec 15.2.5).
    CreateFunction {
        function: Box<syntax::ast::Function>,
    },
    /// Create an arrow function's closure: `[[ThisMode]]` is lexical and
    /// there is no `prototype` (spec 15.3.2).
    CreateArrow {
        is_async: bool,
        params: Vec<syntax::ast::BindingElement>,
        body: syntax::ast::ArrowBody,
    },
    /// `new.target` (spec 13.3.5.3): the active constructor, or *undefined*
    /// at the script level.
    NewTarget,
    /// A `RegExp` literal: construct a fresh RegExp object at runtime — a
    /// literal creates a new object per evaluation (spec 13.2.4.4).
    RegExpLiteral {
        pattern: JsString,
        flags: JsString,
    },
    // ----- literals -----
    ArrayBegin,
    ArrayElement,
    ArraySpread,
    ArrayHole,
    /// Finish the array literal: set its length to the element count (spec
    /// 13.2.4.1 step 6).
    ArrayEnd,
    // ----- class definitions with suspending heritage/computed names -----
    ClassBegin {
        class: Box<Class>,
        binding: Option<crux::string::AtomId>,
        key_count: usize,
    },
    ClassHeritage,
    ClassKeyToPropertyKey,
    ClassFinish {
        class: Box<Class>,
        binding: Option<crux::string::AtomId>,
        key_count: usize,
    },
    ObjectBegin,
    ObjectInitName {
        name: crux::AtomId,
        set_name: bool,
        /// A shorthand property (`{ x }`): never the `__proto__` setter
        /// (Annex B.3.1).
        shorthand: bool,
    },
    ObjectInitComputed {
        set_name: bool,
    },
    ObjectMethodName {
        name: crux::AtomId,
        function: Function,
    },
    ObjectMethodComputed {
        function: Function,
    },
    ObjectAccessorName {
        name: crux::AtomId,
        get: bool,
        param: Option<BindingElement>,
        body: syntax::ast::Block,
    },
    ObjectAccessorComputed {
        get: bool,
        param: Option<BindingElement>,
        body: syntax::ast::Block,
    },
    ObjectSpread,
    PushStr(JsString),
    ConcatStr,
    ConcatStrConst(JsString),
    // ----- arguments -----
    /// Record the current argument-vector length: a call's arguments are
    /// appended after it and the call takes exactly the appended slice, so
    /// nested calls (an argument that is itself a call) keep their vectors
    /// separate.
    ArgsBase,
    ArgsPush,
    ArgsSpread,
    // ----- statements -----
    /// Create the block environment and instantiate its declarations.
    EnterBlock {
        decls: Vec<Stmt>,
    },
    /// Evaluate a function declaration: instantiate the closure and bind the
    /// hoisted name (spec 15.2.6, incl. the Annex B statement-position form).
    FunctionDecl {
        stmt: Box<Stmt>,
    },
    /// Reset the statement-completion register to an empty completion (spec
    /// 6.2.2.3): the current statement's completion starts empty.
    ResetCompletion,
    /// UpdateEmpty with *undefined* (spec 6.2.2.4): the statement always
    /// completes normally, so an empty register becomes *undefined*.
    NormalizeCompletion,
    /// Enter a statement list (spec 14.2.1): save the enclosing list's
    /// completion state and start the new list empty.
    ListBegin,
    /// Leave a statement list: an empty list restores the enclosing list's
    /// state (UpdateEmpty of the statement list), a valued list keeps its
    /// value as the new accumulation.
    ListEnd,
    /// Save the statement-completion register before a `finally` block runs:
    /// the finally's statements must not clobber the completion of the
    /// statement whose control passes through it (spec 14.15.5).
    SaveCompletion,
    /// Restore the statement-completion register after a `finally` block
    /// completed normally (an abrupt finally replaces the completion anyway).
    RestoreCompletion,
    LeaveBlock,
    EnterTry {
        handler: usize,
    },
    Exit {
        after: usize,
    },
    CatchBind {
        param: Option<BindingPattern>,
        decls: Vec<Stmt>,
    },
    FinallyEnd,
    EnterWith,
    PerIteration {
        names: Vec<JsString>,
    },
    EnterLoopEnv {
        kind: VarDeclKind,
        decls: Vec<VarDeclarator>,
    },
    /// A for-in/for-of lexical head's temporary environment: the head names
    /// are uninitialized (in TDZ) while the RHS evaluates (spec 14.7.5.4 /
    /// 14.7.6.1).
    EnterIterTdzEnv {
        names: Vec<JsString>,
    },
    LeaveIterTdzEnv,
    ForInBegin,
    ForInNext {
        done: usize,
    },
    ForInBind {
        left: ForBinding,
    },
    ForInRestore,
    ForOfBegin,
    ForOfNext {
        done: usize,
    },
    ForOfBind {
        left: ForBinding,
    },
    ForOfRestore,
    ForOfClose,
    AsyncForOfBegin,
    AsyncForOfNext,
    AsyncForOfTest {
        done: usize,
    },
    AsyncForOfBind {
        left: ForBinding,
    },
    AsyncForOfRestore,
    AsyncForOfClose,
    SwitchDisc,
    SwitchTest {
        case: usize,
    },
    SetCompletion,
    // ----- control flow -----
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    JumpIfFalseKeep(usize),
    JumpIfTrueKeep(usize),
    JumpIfNullishKeep(usize),
    JumpIfNotNullishKeep(usize),
    /// A `break` statement: route through any pending finallys, then jump to
    /// `target` (spec 14.14.4).
    Break {
        target: usize,
    },
    /// A `continue` statement, routed through pending finallys.
    Continue {
        target: usize,
    },
    Return,
    Throw,
    // ----- suspension -----
    Yield {
        delegate: bool,
    },
    Await,
    YieldStarBegin,
    YieldStarNext {
        done: usize,
    },
    YieldStarResume {
        loop_top: usize,
        done: usize,
        yield_at: usize,
    },
    // ----- async-generator `yield*` (the inner iterator's results are
    // promises; each step awaits them through the driver) -----
    AsyncYieldStarBegin,
    AsyncYieldStarNext {
        done: usize,
    },
    AsyncYieldStarInspect {
        done: usize,
    },
    AsyncYieldStarResume {
        loop_top: usize,
        done: usize,
        inspect: usize,
    },
    // ----- modules -----
    ImportCall {
        has_options: bool,
        phase: syntax::ast::ImportPhase,
    },
    ImportMeta,
}

/// A `try` region's handler: the covered ranges and the catch/finally
/// targets (step indices).
#[derive(Debug, Clone, Copy)]
pub struct Handler {
    pub start: usize,
    pub try_end: usize,
    pub catch: Option<CatchHandler>,
    pub finally: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct CatchHandler {
    pub start: usize,
    pub end: usize,
}

/// A compiled resumable function body.
#[derive(Debug, Clone)]
pub struct CompiledBody {
    pub steps: Vec<Step>,
    pub handlers: Vec<Handler>,
    pub strict: bool,
}

/// A runtime `try` frame.
#[derive(Debug)]
pub struct TryFrame {
    pub handler: usize,
    pub saved_env: EnvRef,
    pub env_depth: usize,
}

/// A pending control transfer a `finally` finishes after it runs.
#[derive(Debug)]
pub enum PendingControl {
    Normal {
        after: usize,
        env: EnvRef,
        depth: usize,
    },
    Break {
        target: usize,
        env: EnvRef,
        depth: usize,
    },
    Continue {
        target: usize,
        env: EnvRef,
        depth: usize,
    },
    Return {
        value: Value,
        env: EnvRef,
        depth: usize,
    },
    Throw {
        value: Value,
        env: EnvRef,
        depth: usize,
    },
}

/// How the VM suspended, for the driver (generator/async machinery).
#[derive(Debug, Clone)]
pub enum Suspension {
    Yield {
        value: Value,
        delegate: bool,
    },
    Await(Value),
    /// The `yield*` delegation has no `return` method: the received value is
    /// awaited and the body is resumed with a return completion of it (spec
    /// 15.5.5 return case step b.ii). The driver resumes with
    /// `Resume::Return` instead of `Resume::Normal`.
    AwaitReturn(Value),
}

/// What a resume hands to the VM.
#[derive(Debug, Clone)]
pub enum Resume {
    Normal(Value),
    Throw(Value),
    Return(Value),
}

/// An abrupt resume delivered to a `yield*` delegation.
#[derive(Debug, Clone)]
pub enum ResumeAbrupt {
    Throw(Value),
    Return(Value),
}

/// The result of a VM run.
#[derive(Debug)]
pub enum VmOutcome {
    Completed(Completion),
    Suspended(Suspension),
}

/// A property key for member steps.
#[derive(Debug, Clone)]
pub enum PropertyKeyName {
    Name(crux::AtomId),
    Key(PropertyKey),
}

/// The control-transfer payload for the try/finally machinery.
#[derive(Debug, Clone)]
pub enum Ctl {
    Normal { after: usize },
    Break { target: usize },
    Continue { target: usize },
    Return { value: Value },
    Throw { value: Value },
}

#[derive(Debug)]
pub enum CtlResult {
    Continue,
    Done(VmOutcome),
}

/// A for-in enumeration in progress: the base object, the
/// (prototype-level, key) pairs, and the next index.
type ForInState = (Handle<crux::object::JsObject>, Vec<(usize, Value)>, usize);

/// The resumable VM state. Saved across suspension by the driver.
#[derive(Debug)]
pub struct Vm {
    pub ip: usize,
    pub stack: Vec<Value>,
    pub args: Vec<Value>,
    pub lexical_env: EnvRef,
    pub env_stack: Vec<EnvRef>,
    pub completion: Value,
    pub try_stack: Vec<TryFrame>,
    pub pending: Option<PendingControl>,
    pub thrown: Option<Value>,
    pub resume_abrupt: Option<ResumeAbrupt>,
    /// Whether the innermost for-of is stepping its iterator (`ForOfNext`):
    /// a `next()` error propagates without closing the iterator (spec
    /// 14.7.6.2 uses `?`), unlike a body or head-binding error.
    pub for_of_stepping: bool,
    /// In-progress for-in enumerations: the (prototype-level, key) pairs and
    /// the enumerated base object, so a key deleted during enumeration can be
    /// re-checked against its own level at visit time (spec
    /// EnumerateObjectProperties).
    pub for_in_stack: Vec<ForInState>,
    pub for_of_stack: Vec<crate::expr::IteratorRecord>,
    pub async_for_of_stack: Vec<crate::expr::IteratorRecord>,
    pub destructure_stack: Vec<crate::expr::IteratorRecord>,
    /// Whether each destructure's iterator was exhausted by an element step;
    /// an exhausted iterator is not closed (spec 13.15.5.2 step 5).
    pub destructure_done: Vec<bool>,
    pub destructure_obj_stack: Vec<Value>,
    pub yield_star_stack: Vec<YieldStarState>,
    /// Pending class definitions whose heritage/computed names suspend.
    pub class_stack: Vec<ClassEvalState>,
    pub switch_disc: Option<Value>,
    /// An upstream `?.` short-circuited: the rest of the chain (keys, args,
    /// further links) must not evaluate, and the chain is `undefined` (spec
    /// 13.4.3). Cleared when the outermost chain node finishes.
    pub chain_short: bool,
    /// The in-flight disposal of a scope's `using` resources at an
    /// async-disposal suspension: the VM suspends with `Suspension::Await`
    /// per async dispose, and `run_abrupt` resumes the driver.
    pub pending_disposal: Option<PendingDisposal>,
    /// A caught throw that discarded the try block's envs: `CatchBind`
    /// disposes them (folding into the thrown value, spec 9.4.3) before
    /// binding the parameter. `(saved_env, env_depth)` of the try frame.
    pub pending_catch_disposal: Option<(EnvRef, usize)>,
    pub strict: bool,
    /// Whether the statement-completion register holds an empty completion
    /// (spec 6.2.2.3): no value-producing statement has run since the last
    /// reset. Only the top-level script path observes it.
    pub completion_is_empty: bool,
    /// Completion-register saves for the `finally` blocks currently running
    /// (nested finallys push in order).
    completion_stack: Vec<(Value, bool)>,
    /// Completion-state saves for the statement lists currently running
    /// (nested blocks/try bodies push in order).
    list_stack: Vec<(Value, bool)>,
    /// Pre-resolved `var` declaration references awaiting their initializer's
    /// value (spec 14.3.2 step 2).
    var_ref_stack: Vec<crate::context::Reference>,
    /// The next element index of each in-progress array literal (nested
    /// literals push in order); the length is set once at `ArrayEnd`.
    array_index_stack: Vec<usize>,
    /// The argument-vector boundary of each in-progress call (nested calls
    /// push in order); the call step pops it and takes the appended slice.
    args_base_stack: Vec<usize>,
}

/// The per-class-definition state while the resumable VM evaluates a
/// suspending heritage or computed element names.
#[derive(Debug)]
pub struct ClassEvalState {
    pub class_env: EnvRef,
    pub class_private_env: crux::handle::Handle<crate::context::PrivateEnvironment>,
    pub outer_private_env: Option<crux::handle::Handle<crate::context::PrivateEnvironment>>,
    pub outer_env: EnvRef,
    pub heritage: Option<Value>,
}

/// The per-`yield*` delegation state.
#[derive(Debug)]
pub struct YieldStarState {
    pub iterator: crate::expr::IteratorRecord,
    pub received: Value,
    /// Whether the current delegation pass was resumed with a return
    /// completion: a done result then completes the body with a return
    /// completion of its value instead of continuing (spec 15.5.5 return
    /// case step viii).
    pub resumed_return: bool,
}

/// An in-flight scope disposal (spec 9.4.3 DisposeResources): the remaining
/// resources in disposal order, the completion being folded, and how to
/// resume once the stack drains.
#[derive(Debug)]
pub struct PendingDisposal {
    pub resources: Vec<crate::env::DisposableResource>,
    pub index: usize,
    pub completion: Completion,
    pub resume: DisposalResume,
}

/// How a finished scope disposal delivers its folded completion.
#[derive(Debug)]
pub enum DisposalResume {
    /// Scope-exit style: a normal completion continues at the current ip; a
    /// throw propagates through the handler table.
    ApplyCompletion,
    /// The try-catch path: the folded value is delivered to the catch (the
    /// `CatchBind` step re-runs with `self.thrown` set and the env stack
    /// restored).
    DeliverCatch { saved_env: EnvRef, env_depth: usize },
}

impl Vm {
    pub fn new(lexical_env: EnvRef, strict: bool) -> Self {
        Self {
            ip: 0,
            stack: Vec::new(),
            args: Vec::new(),
            lexical_env: lexical_env.clone(),
            env_stack: vec![lexical_env],
            completion: Value::Undefined,
            try_stack: Vec::new(),
            pending: None,
            thrown: None,
            resume_abrupt: None,
            for_of_stepping: false,
            for_in_stack: Vec::new(),
            for_of_stack: Vec::new(),
            async_for_of_stack: Vec::new(),
            destructure_stack: Vec::new(),
            destructure_done: Vec::new(),
            destructure_obj_stack: Vec::new(),
            yield_star_stack: Vec::new(),
            class_stack: Vec::new(),
            switch_disc: None,
            chain_short: false,
            pending_disposal: None,
            pending_catch_disposal: None,
            strict,
            completion_is_empty: true,
            completion_stack: Vec::new(),
            list_stack: Vec::new(),
            var_ref_stack: Vec::new(),
            array_index_stack: Vec::new(),
            args_base_stack: Vec::new(),
        }
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Undefined)
    }

    fn array_index(&mut self) -> Result<&mut usize, JsError> {
        self.array_index_stack.last_mut().ok_or_else(|| {
            JsError::new(
                ErrorKind::SyntaxError,
                "Array step without an array literal".into(),
            )
        })
    }

    fn restore_env(&mut self, env: EnvRef, depth: usize) {
        self.env_stack.truncate(depth);
        self.lexical_env = env;
    }

    /// Run until suspension or completion. `resume` delivers the value of the
    /// last suspension: a normal value for `yield`/`await` continuations, or
    /// an abrupt completion that the driver has decided reaches this VM (the
    /// `yield*` delegate case; plain `yield`/`await` abrupts are handled by
    /// the driver).
    pub fn run(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        resume: Resume,
    ) -> Result<VmOutcome, JsError> {
        match resume {
            Resume::Normal(value) => self.stack.push(value),
            Resume::Throw(value) => {
                self.resume_abrupt = Some(ResumeAbrupt::Throw(value.clone()));
                self.stack.push(value);
            }
            Resume::Return(value) => {
                self.resume_abrupt = Some(ResumeAbrupt::Return(value.clone()));
                self.stack.push(value);
            }
        }
        self.run_inner(agent, body)
    }

    /// Run from the start of the body (no resume value).
    pub fn start(&mut self, agent: &mut Agent, body: &CompiledBody) -> Result<VmOutcome, JsError> {
        self.run_inner(agent, body)
    }

    /// The throw machinery entry used by await-rejection resumes: run the
    /// throw through the handler table.
    pub fn run_abrupt(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        resume: Resume,
    ) -> Result<VmOutcome, JsError> {
        match resume {
            Resume::Normal(value) => {
                if self.pending_disposal.is_some() {
                    // The awaited async-dispose settled: keep driving the
                    // scope disposal.
                    return self.resume_pending_disposal(agent, body, Ok(value));
                }
                self.stack.push(value);
                self.run_inner(agent, body)
            }
            Resume::Throw(value) => {
                if self.pending_disposal.is_some() {
                    // A rejected async-dispose is a throwing disposal, folded
                    // into the disposal completion — never routed through the
                    // body's handler table.
                    return self.resume_pending_disposal(agent, body, Err(value));
                }
                // A resumed `yield`/`await` inside a destructure propagates
                // the abrupt completion through the pattern, closing its
                // iterators (spec 13.15.5.2 step 5 + 7.4.11).
                self.close_destructures_abrupt(agent, false)?;
                match self.throw_machinery(agent, body, value)? {
                    CtlResult::Continue => self.run_inner(agent, body),
                    CtlResult::Done(outcome) => Ok(outcome),
                }
            }
            Resume::Return(value) => {
                match self.close_destructures_abrupt(agent, true) {
                    Ok(()) => match self.control_transfer(agent, body, Ctl::Return { value })? {
                        CtlResult::Continue => self.run_inner(agent, body),
                        CtlResult::Done(outcome) => Ok(outcome),
                    },
                    // A throwing or non-object `return` replaces the return
                    // completion with that error (spec 7.4.11 steps 6-8).
                    Err(error) => self.throw_error(agent, body, error),
                }
            }
        }
    }

    /// Close every active destructure iterator when a suspended `yield`/
    /// `await` inside a pattern is aborted (spec 13.15.5.2/13.15.5.5: an
    /// abrupt element or rest evaluation closes the not-done iterator with
    /// the abrupt completion). With a return completion the innermost close
    /// runs first and a throwing or non-object `return` becomes the new
    /// completion, so the remaining iterators close with the throw flavor
    /// (spec 7.4.11 steps 4-8); with a throw completion all closes swallow.
    fn close_destructures_abrupt(
        &mut self,
        agent: &mut Agent,
        completion_is_return: bool,
    ) -> Result<(), JsError> {
        let mut first_error: Option<JsError> = None;
        while let Some(index) = self.destructure_stack.len().checked_sub(1) {
            let iterator = self.destructure_stack[index].clone();
            let done = self.destructure_done.get(index).copied().unwrap_or(false);
            if !done {
                if completion_is_return && first_error.is_none() {
                    if let Err(error) = iterator_close(agent, &iterator) {
                        first_error = Some(error);
                    }
                } else {
                    crate::expr::iterator_close_throw(agent, &iterator)?;
                }
            }
            self.destructure_stack.pop();
            self.destructure_done.pop();
        }
        self.destructure_obj_stack.clear();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Close the active for-of iterators with a throw completion: the
    /// original error wins, a throwing `return` (or `return` lookup) is
    /// swallowed (spec 7.4.11 steps 6-7, mirroring the tree-walker's
    /// eval_for_of error paths).
    fn close_for_of_throw(&mut self, agent: &mut Agent) {
        while let Some(iterator) = self.for_of_stack.pop() {
            let _ = crate::expr::iterator_close_throw(agent, &iterator);
        }
    }

    /// Close the active for-of iterators with a return completion: a throwing
    /// `return` replaces the return (spec 7.4.6), later closes swallow.
    fn close_for_of_return(&mut self, agent: &mut Agent) -> Result<(), JsError> {
        let mut first_error: Option<JsError> = None;
        while let Some(iterator) = self.for_of_stack.pop() {
            if first_error.is_none() {
                if let Err(e) = crate::expr::iterator_close(agent, &iterator) {
                    first_error = Some(e);
                }
            } else {
                let _ = crate::expr::iterator_close_throw(agent, &iterator);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn run_inner(&mut self, agent: &mut Agent, body: &CompiledBody) -> Result<VmOutcome, JsError> {
        // Record the agent for the duration so crux-side ECMAScript calls
        // (proxy traps reached through property access) can find the runtime.
        crux::function::with_agent(agent as *mut Agent as *mut (), || {
            match self.run_inner_inner(agent, body) {
                Ok(outcome) => Ok(outcome),
                // A step's engine error (a TypeError from a property access, a
                // ReferenceError from an unresolved identifier, ...) inside a
                // `try` is a thrown completion: route it through the handler
                // table so `catch` sees it, like the interpreter path. Without
                // a covering handler the original error propagates untouched.
                Err(error) => {
                    // An error escaping a step while a destructuring pattern is
                    // in progress closes the not-done iterators (spec
                    // 13.15.5.2 step 5): the first throwing `return` replaces
                    // the error (spec 7.4.11). The tree-walker's
                    // array_assignment does the same on its error path.
                    let mut error = error;
                    if !self.destructure_stack.is_empty() {
                        let mut close_error: Option<JsError> = None;
                        while let Some(index) = self.destructure_stack.len().checked_sub(1) {
                            let iterator = self.destructure_stack[index].clone();
                            let done = self.destructure_done.get(index).copied().unwrap_or(false);
                            if !done
                                && let Err(e) = crate::expr::iterator_close_throw(agent, &iterator)
                                && close_error.is_none()
                            {
                                close_error = Some(e);
                            }
                            self.destructure_stack.pop();
                            self.destructure_done.pop();
                        }
                        self.destructure_obj_stack.clear();
                        if let Some(e) = close_error {
                            error = e;
                        }
                    }
                    // An error escaping a for-of body or head closes the
                    // active iterators (spec 14.7.6.2: a head-binding error or
                    // abrupt body completion returns IteratorClose). A `next()`
                    // error is exempt: it propagates with the iterator open.
                    if !self.for_of_stepping {
                        self.close_for_of_throw(agent);
                    }
                    let covered = self.try_stack.iter().any(|frame| {
                        body.handlers.get(frame.handler).is_some_and(|handler| {
                            self.ip >= handler.start && self.ip < handler.try_end
                        })
                    });
                    if covered {
                        self.throw_error(agent, body, error)
                    } else {
                        Err(error)
                    }
                }
            }
        })
    }

    fn run_inner_inner(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
    ) -> Result<VmOutcome, JsError> {
        loop {
            let steps = &body.steps;
            if self.ip >= steps.len() {
                // Fell off the end: the body completes with the statement-list
                // completion — empty when the last statement produced no value.
                return Ok(VmOutcome::Completed(if self.completion_is_empty {
                    Completion::Empty
                } else {
                    Completion::Normal(self.completion.clone())
                }));
            }
            let step = steps.get(self.ip).ok_or_else(|| {
                JsError::new(
                    ErrorKind::SyntaxError,
                    "Instruction pointer out of bounds".into(),
                )
            })?;
            self.ip += 1;
            // Steps that read the running context (resolve_binding, member
            // helpers, closure creation) see the VM's lexical environment
            // through the context; the sync is skipped when it has not
            // changed since the last step (an `Rc` pointer comparison).
            if let Ok(context) = agent.running_context_mut()
                && !std::rc::Rc::ptr_eq(&context.lexical_environment, &self.lexical_env)
            {
                context.lexical_environment = self.lexical_env.clone();
            }
            match step {
                Step::LoadIdent { name } => {
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    let value = crate::context::get_value(agent, &reference)?;
                    self.stack.push(value);
                }
                Step::CreateFunction { function } => {
                    let env = agent.running_context()?.lexical_environment.clone();
                    let value = crate::function::instantiate_function_expression(
                        agent,
                        function,
                        env,
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::CreateArrow {
                    is_async,
                    params,
                    body,
                } => {
                    let env = agent.running_context()?.lexical_environment.clone();
                    let value = crate::function::instantiate_arrow(
                        agent,
                        *is_async,
                        params.clone(),
                        body.clone(),
                        env,
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::NewTarget => {
                    let value = get_new_target(agent)?;
                    self.stack.push(value);
                }
                Step::RegExpLiteral { pattern, flags } => {
                    let value = crate::expr::eval_regexp_literal(agent, pattern, flags)?;
                    self.stack.push(value);
                }
                Step::FunctionDecl { stmt } => {
                    let StmtKind::FunctionDecl(function) = &stmt.kind else {
                        unreachable!("FunctionDecl step carries a function declaration");
                    };
                    if function.statement_position && !self.strict {
                        crate::eval::eval_statement_position_function(
                            agent,
                            function,
                            stmt,
                            self.strict,
                        )?;
                    } else {
                        crate::eval::eval_function_declaration(agent, function, self.strict)?;
                    }
                }
                Step::Push(value) => self.stack.push(value.clone()),
                Step::Pop => {
                    self.pop();
                }
                Step::Dup => {
                    let top = self.pop();
                    self.stack.push(top.clone());
                    self.stack.push(top);
                }
                Step::Dup2 => {
                    let b = self.pop();
                    let a = self.pop();
                    self.stack.push(a.clone());
                    self.stack.push(b.clone());
                    self.stack.push(a);
                    self.stack.push(b);
                }
                Step::Unary(op) => {
                    let operand = self.pop();
                    let value = crate::expr::eval_unary_value(agent, op, operand)?;
                    self.stack.push(value);
                }
                Step::Binary(op) => {
                    let right = self.pop();
                    let left = self.pop();
                    let value = crate::expr::apply_binary(agent, *op, &left, &right)?;
                    self.stack.push(value);
                }
                Step::GetMemberName { name } => {
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot read properties of null"));
                    }
                    let value = crate::context::get_property(
                        agent,
                        &object,
                        &crux::lookup(*name),
                        object.clone(),
                    )?;
                    self.stack.push(value);
                }
                Step::GetMemberComputed => {
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot read properties of null"));
                    }
                    let key = crate::context::to_property_key(agent, &key)?;
                    let value =
                        crate::context::get_property_key(agent, &object, &key, object.clone())?;
                    self.stack.push(value);
                }
                Step::GetSuperName { name } => {
                    let base = self.pop();
                    let this = resolve_this_binding(agent)?;
                    let value =
                        crate::context::get_property(agent, &base, &crux::lookup(*name), this)?;
                    self.stack.push(value);
                }
                Step::GetSuperComputed => {
                    let key = self.pop();
                    let base = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    let this = resolve_this_binding(agent)?;
                    let value = crate::context::get_property_key(agent, &base, &key, this)?;
                    self.stack.push(value);
                }
                Step::GetPrivate { atom } => {
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                    let value = crate::context::private_get(agent, &object, name_id)?;
                    self.stack.push(value);
                }
                Step::ThisValue => {
                    let this = resolve_this_binding(agent)?;
                    self.stack.push(this);
                }
                Step::GetSuperBase => {
                    // SuperProperty evaluation checks GetThisBinding first
                    // (spec 13.3.7.1): an uninitialized `this` in a derived
                    // constructor throws before the base is read or the key
                    // evaluates.
                    resolve_this_binding(agent)?;
                    let base = get_super_base(agent)?;
                    self.stack.push(base);
                }
                Step::AssignIdent { name, op, set_name } => {
                    self.assign_ident(agent, *name, *op, *set_name)?;
                }
                Step::AssignMemberName { name, op } => {
                    let value = self.pop();
                    let old = if is_compound_assign(op) {
                        Some(self.pop())
                    } else {
                        None
                    };
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    self.assign_member(
                        agent,
                        object,
                        PropertyKeyName::Name(*name),
                        old,
                        value,
                        *op,
                    )?;
                }
                Step::AssignMemberComputed { op } => {
                    let value = self.pop();
                    let old = if is_compound_assign(op) {
                        Some(self.pop())
                    } else {
                        None
                    };
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let key = crate::context::to_property_key(agent, &key)?;
                    self.assign_member(
                        agent,
                        object,
                        PropertyKeyName::Key(key.clone()),
                        old,
                        value,
                        *op,
                    )?;
                }
                Step::AssignSuperName { name, op } => {
                    let value = self.pop();
                    let old = if is_compound_assign(op) {
                        Some(self.pop())
                    } else {
                        None
                    };
                    let base = self.pop();
                    self.assign_super(agent, base, PropertyKeyName::Name(*name), old, value, *op)?;
                }
                Step::AssignSuperComputed { op } => {
                    let value = self.pop();
                    let old = if is_compound_assign(op) {
                        Some(self.pop())
                    } else {
                        None
                    };
                    let key = self.pop();
                    let base = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    self.assign_super(agent, base, PropertyKeyName::Key(key), old, value, *op)?;
                }
                Step::AssignPrivate { atom, op } => {
                    let value = self.pop();
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                    // Only a compound assignment reads the old value; a plain
                    // `=` must not invoke the getter (PrivateSet on an
                    // accessor runs the setter, spec 10.2.12.2).
                    let new = match *op {
                        AssignOp::Assign
                        | AssignOp::AndAssign
                        | AssignOp::OrAssign
                        | AssignOp::NullishAssign => value.clone(),
                        _ => {
                            let old = crate::context::private_get(agent, &object, name_id)?;
                            crate::expr::apply_compound(agent, *op, &old, &value)?
                        }
                    };
                    crate::context::private_set(agent, &object, name_id, new.clone())?;
                    self.stack.push(new);
                }
                Step::Destructure { pattern } => {
                    let value = self.pop();
                    crate::binding::binding_initialization(
                        agent,
                        pattern,
                        value.clone(),
                        None,
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::DestructureBegin => {
                    let value = self.pop();
                    let iterator = get_iterator(agent, &value)?;
                    self.destructure_stack.push(iterator);
                    self.destructure_done.push(false);
                }
                Step::DestructureNext => {
                    let index = self.destructure_stack.len().checked_sub(1).ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "DestructureNext without a destructure".into(),
                        )
                    })?;
                    let iterator = self.destructure_stack[index].clone();
                    match iterator_step(agent, &iterator)? {
                        Some(value) => self.stack.push(value),
                        None => {
                            // The iterator is exhausted but the element still
                            // receives *undefined*: a default initializer must
                            // run (and may suspend) even after exhaustion.
                            self.destructure_done[index] = true;
                            self.stack.push(Value::Undefined);
                        }
                    }
                }
                Step::DestructureUndef { use_default } => {
                    let value = self.pop();
                    if matches!(value.kind(), ValueKind::Undefined) {
                        self.ip = *use_default;
                    } else {
                        self.stack.push(value);
                    }
                }
                Step::DestructureRest => {
                    let index = self.destructure_stack.len().checked_sub(1).ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "DestructureRest without a destructure".into(),
                        )
                    })?;
                    let iterator = self.destructure_stack[index].clone();
                    let mut collected = Vec::new();
                    while let Some(value) = iterator_step(agent, &iterator)? {
                        collected.push(value);
                    }
                    self.destructure_stack.pop();
                    self.destructure_done.pop();
                    let array = crate::builtins::array::array_from_values(agent, &collected)?;
                    self.stack.push(array);
                }
                Step::DestructureObjCoercible => {
                    let value = self.pop();
                    if matches!(value.kind(), ValueKind::Undefined | ValueKind::Null) {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "Cannot destructure null or undefined".into(),
                        ));
                    }
                    self.destructure_obj_stack.push(value);
                }
                Step::DestructureObjKey { key } => {
                    let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "DestructureObjKey without an object".into(),
                        )
                    })?;
                    let value =
                        crate::context::get_property_key(agent, &object, key, object.clone())?;
                    self.stack.push(value);
                }
                Step::DestructureObjKeyComputed => {
                    let key = self.pop();
                    let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "DestructureObjKeyComputed without an object".into(),
                        )
                    })?;
                    let key = crate::context::to_property_key(agent, &key)?;
                    let value =
                        crate::context::get_property_key(agent, &object, &key, object.clone())?;
                    self.stack.push(value);
                }
                Step::DestructureObjRest { excluded } => {
                    let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "DestructureObjRest without an object".into(),
                        )
                    })?;
                    let rest = crate::binding::rest_object(agent)?;
                    crate::binding::copy_data_properties_excluding(
                        agent, &rest, &object, excluded,
                    )?;
                    self.stack.push(Value::Object(rest));
                }
                Step::DestructureClose => {
                    if let Some(index) = self.destructure_stack.len().checked_sub(1) {
                        let iterator = self.destructure_stack[index].clone();
                        if !self.destructure_done[index] {
                            iterator_close(agent, &iterator)?;
                        }
                        self.destructure_stack.pop();
                        self.destructure_done.pop();
                    }
                }
                Step::DestructureObjEnd => {
                    self.destructure_obj_stack.pop();
                }
                Step::DeclInit { pattern } => {
                    let value = self.pop();
                    crate::binding::binding_initialization(
                        agent,
                        pattern,
                        value.clone(),
                        Some(&self.lexical_env),
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::ResolveVarIdent { name } => {
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    self.var_ref_stack.push(reference);
                }
                Step::ResolvePrivateRef { atom } => {
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                    self.var_ref_stack.push(crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(object),
                        name: PropertyKey::from_utf8(""),
                        strict: self.strict,
                        this_value: None,
                        private_name: Some(name_id),
                    });
                }
                Step::ResolveMemberRefName { name } => {
                    let object = self.pop();
                    let reference =
                        member_reference(&object, &PropertyKeyName::Name(*name), self.strict);
                    self.var_ref_stack.push(reference);
                }
                Step::ResolveMemberRefComputed => {
                    let key = self.pop();
                    let object = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    let reference =
                        member_reference(&object, &PropertyKeyName::Key(key), self.strict);
                    self.var_ref_stack.push(reference);
                }
                Step::PutVarReference => {
                    let value = self.pop();
                    let reference = self.var_ref_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "PutVarReference without a resolution".into(),
                        )
                    })?;
                    crate::context::put_value(agent, &reference, value.clone())?;
                    self.stack.push(value);
                }
                Step::PopVarReference => {
                    self.var_ref_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "PopVarReference without a resolution".into(),
                        )
                    })?;
                }
                Step::ResolveSuperRefName { name } => {
                    let this = resolve_this_binding(agent)?;
                    let base = get_super_base(agent)?;
                    self.var_ref_stack.push(crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(base),
                        name: PropertyKey::from_js_string(&crux::lookup(*name)),
                        strict: self.strict,
                        this_value: Some(this),
                        private_name: None,
                    });
                }
                Step::ResolveSuperRefComputed => {
                    let key = self.pop();
                    let base = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    let this = resolve_this_binding(agent)?;
                    self.var_ref_stack.push(crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(base),
                        name: key,
                        strict: self.strict,
                        this_value: Some(this),
                        private_name: None,
                    });
                }
                Step::GetVarReference => {
                    let reference = self.var_ref_stack.last().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "GetVarReference without a resolution".into(),
                        )
                    })?;
                    let value = crate::context::get_value(agent, reference)?;
                    self.stack.push(value);
                }
                Step::PutVarReferenceOp { op } => {
                    let value = self.pop();
                    let old = self.pop();
                    let reference = self.var_ref_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "PutVarReferenceOp without a resolution".into(),
                        )
                    })?;
                    let new = crate::expr::apply_compound(agent, *op, &old, &value)?;
                    crate::context::put_value(agent, &reference, new.clone())?;
                    self.stack.push(new);
                }
                Step::UpdateVarReference { op, prefix } => {
                    let old = self.pop();
                    let new = update_value(agent, op, &old)?;
                    let reference = self.var_ref_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "UpdateVarReference without a resolution".into(),
                        )
                    })?;
                    crate::context::put_value(agent, &reference, new.clone())?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UsingInit { pattern, is_await } => {
                    let value = self.pop();
                    let kind = if *is_await {
                        crate::eval::DisposalKind::Async
                    } else {
                        crate::eval::DisposalKind::Sync
                    };
                    let resource = crate::eval::create_disposable_resource(agent, &value, kind)?;
                    self.lexical_env.add_disposable_resource(resource);
                    crate::binding::binding_initialization(
                        agent,
                        pattern,
                        value.clone(),
                        Some(&self.lexical_env),
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::SetFunctionName { name } => {
                    let value = self.pop();
                    let display =
                        crate::function::default_binding_display_name(Some(crux::lookup(*name)))
                            .unwrap_or_else(|| crux::lookup(*name));
                    crate::function::set_function_name(&value, &display, None)?;
                    self.stack.push(value);
                }
                Step::SetChainShort => {
                    self.chain_short = true;
                }
                Step::ClearChainShort => {
                    self.chain_short = false;
                }
                Step::JumpIfChainShort(target) => {
                    if self.chain_short {
                        self.ip = *target;
                    }
                }
                Step::UpdateIdent { name, op, prefix } => {
                    let old = self.pop();
                    let new = update_value(agent, op, &old)?;
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    crate::context::put_value(agent, &reference, new.clone())?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UpdateMemberName { name, op, prefix } => {
                    let old = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let new = update_value(agent, op, &old)?;
                    crate::context::put_value(
                        agent,
                        &member_reference(&object, &PropertyKeyName::Name(*name), self.strict),
                        new.clone(),
                    )?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UpdateMemberComputed { op, prefix } => {
                    let old = self.pop();
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let new = update_value(agent, op, &old)?;
                    let key = crate::context::to_property_key(agent, &key)?;
                    crate::context::put_value(
                        agent,
                        &member_reference(&object, &PropertyKeyName::Key(key), self.strict),
                        new.clone(),
                    )?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UpdateSuperName { name, op, prefix } => {
                    let old = self.pop();
                    let base = self.pop();
                    let new = update_value(agent, op, &old)?;
                    self.put_super(agent, base, PropertyKeyName::Name(*name), new.clone())?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UpdateSuperComputed { op, prefix } => {
                    let old = self.pop();
                    let key = self.pop();
                    let base = self.pop();
                    let new = update_value(agent, op, &old)?;
                    let key = crate::context::to_property_key(agent, &key)?;
                    self.put_super(agent, base, PropertyKeyName::Key(key), new.clone())?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::UpdatePrivate { atom, op, prefix } => {
                    let old = self.pop();
                    let object = self.pop();
                    let new = update_value(agent, op, &old)?;
                    let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                    crate::context::private_set(agent, &object, name_id, new.clone())?;
                    self.stack.push(if *prefix { new } else { old });
                }
                Step::DeleteIdent { name } => {
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    let deleted = crate::context::delete_property_or_throw(agent, &reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::DeleteMemberName { name } => {
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot convert undefined or null to object"));
                    }
                    let reference = crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(object),
                        name: PropertyKey::from_js_string(&crux::lookup(*name)),
                        strict: self.strict,
                        this_value: None,
                        private_name: None,
                    };
                    let deleted = crate::context::delete_property_or_throw(agent, &reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::DeleteMemberComputed => {
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot convert undefined or null to object"));
                    }
                    let key = crate::context::to_property_key(agent, &key)?;
                    let reference = crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(object),
                        name: key,
                        strict: self.strict,
                        this_value: None,
                        private_name: None,
                    };
                    let deleted = crate::context::delete_property_or_throw(agent, &reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::DeleteSuper => {
                    // `delete super.x` is a ReferenceError before the key is
                    // even evaluated (spec 13.5.1.2 step 4.b).
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Unsupported reference to 'super'".into(),
                    ));
                }
                Step::ToPropertyKey => {
                    let value = self.pop();
                    let key = crate::context::to_property_key(agent, &value)?;
                    let key = match key {
                        PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                        PropertyKey::Symbol(symbol) => Value::Symbol(Handle::new(symbol)),
                    };
                    self.stack.push(key);
                }
                Step::TypeofTop => {
                    let value = self.pop();
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf8(
                            crux::value::type_of(&value),
                        ))));
                }
                Step::TypeofIdent { name } => {
                    // spec 13.5.3.2 step 1: an unresolvable reference is
                    // "undefined", not a ReferenceError.
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    let value = match &reference.base {
                        crate::context::ReferenceBase::Unresolvable => Value::Undefined,
                        _ => crate::context::get_value(agent, &reference)?,
                    };
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf8(
                            crux::value::type_of(&value),
                        ))));
                }
                Step::PrivateIn { atom } => {
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                    self.stack.push(Value::Boolean(crate::context::private_in(
                        &object, name_id,
                    )?));
                }
                Step::Call { direct_eval } => {
                    self.do_call(agent, *direct_eval)?;
                }
                Step::SuperCall => {
                    let base = self.args_base_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "SuperCall without an argument boundary".into(),
                        )
                    })?;
                    let args = self.args.split_off(base);
                    let new_target = get_new_target(agent)?;
                    let super_ctor = get_super_constructor(agent)?;
                    let result =
                        crate::function::construct(agent, &super_ctor, &args, &new_target)?;
                    let this_env = get_this_environment(agent)?;
                    this_env.bind_this_value(result.clone())?;
                    if let Some(function_value) = agent.running_context()?.function.clone() {
                        crate::function::initialize_instance_elements(
                            agent,
                            &result,
                            &function_value,
                        )?;
                    }
                    self.stack.push(result);
                }
                Step::Construct => {
                    let callee = self.pop();
                    let base = self.args_base_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "Construct without an argument boundary".into(),
                        )
                    })?;
                    let args = self.args.split_off(base);
                    let result = crate::function::construct(agent, &callee, &args, &callee)?;
                    self.stack.push(result);
                }
                Step::TaggedTemplate(template) => {
                    let tag = self.pop();
                    let base = self.args_base_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "TaggedTemplate without an argument boundary".into(),
                        )
                    })?;
                    let substitutions = self.args.split_off(base);
                    let value = tagged_template(agent, tag, template, substitutions)?;
                    self.stack.push(value);
                }
                Step::ArrayBegin => {
                    let array = crate::builtins::array::array_create(agent, 0.0)?;
                    self.array_index_stack.push(0);
                    self.stack.push(Value::Object(array));
                }
                Step::ArrayElement => {
                    let value = self.pop();
                    let array = self.pop();
                    let index = *self.array_index()?;
                    array_set(&array, &index.to_string(), value)?;
                    *self.array_index()? = index + 1;
                    self.stack.push(array);
                }
                Step::ArraySpread => {
                    let iterable = self.pop();
                    let array = self.pop();
                    let iterator = get_iterator(agent, &iterable)?;
                    while let Some(value) = iterator_step(agent, &iterator)? {
                        let index = *self.array_index()?;
                        array_set(&array, &index.to_string(), value)?;
                        *self.array_index()? = index + 1;
                    }
                    self.stack.push(array);
                }
                Step::ArrayHole => {
                    let index = *self.array_index()?;
                    *self.array_index()? = index + 1;
                }
                Step::ArrayEnd => {
                    let array = self.pop();
                    let length = self.array_index_stack.pop().ok_or_else(|| {
                        JsError::new(ErrorKind::SyntaxError, "ArrayEnd without an array".into())
                    })?;
                    let ValueKind::Object(obj) = array.kind() else {
                        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
                    };
                    obj.set(
                        &JsString::from_utf8("length"),
                        Value::Number(length as f64),
                        true,
                    )?;
                    self.stack.push(array);
                }
                Step::ObjectBegin => {
                    let proto = agent
                        .current_realm()?
                        .intrinsics
                        .get("%Object.prototype%")
                        .and_then(|value| crate::context::as_object(&value));
                    let object = crux::object::JsObject::ordinary_object_create(proto);
                    self.stack.push(Value::Object(object));
                }
                Step::ObjectInitName {
                    name,
                    set_name,
                    shorthand,
                } => {
                    let value = self.pop();
                    let object = self.pop();
                    object_init(
                        &object,
                        &PropertyName::Ident(*name),
                        value,
                        *set_name,
                        *shorthand,
                    )?;
                    self.stack.push(object);
                }
                Step::ObjectInitComputed { set_name } => {
                    let value = self.pop();
                    let key = self.pop();
                    let object = self.pop();
                    let ValueKind::Object(obj) = object.kind() else {
                        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
                    };
                    let key = crate::context::to_property_key(agent, &key)?;
                    object_init_key(&obj, key, value, *set_name)?;
                    self.stack.push(object);
                }
                Step::ObjectMethodName { name, function } => {
                    let object = self.pop();
                    object_method(agent, &object, PropertyKey::String(*name), function)?;
                    self.stack.push(object);
                }
                Step::ObjectMethodComputed { function } => {
                    let key = self.pop();
                    let object = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    object_method(agent, &object, key, function)?;
                    self.stack.push(object);
                }
                Step::ObjectAccessorName {
                    name,
                    get,
                    param,
                    body,
                } => {
                    let object = self.pop();
                    object_accessor(
                        agent,
                        &object,
                        PropertyKey::String(*name),
                        *get,
                        param.as_ref(),
                        body,
                    )?;
                    self.stack.push(object);
                }
                Step::ObjectAccessorComputed { get, param, body } => {
                    let key = self.pop();
                    let object = self.pop();
                    let key = crate::context::to_property_key(agent, &key)?;
                    object_accessor(agent, &object, key, *get, param.as_ref(), body)?;
                    self.stack.push(object);
                }
                Step::ObjectSpread => {
                    let from = self.pop();
                    let object = self.pop();
                    let ValueKind::Object(obj) = object.kind() else {
                        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
                    };
                    crate::expr::copy_data_properties(agent, &obj, &from)?;
                    self.stack.push(object);
                }
                Step::ClassBegin {
                    class,
                    binding,
                    key_count: _,
                } => {
                    // Create the class scope and PrivateEnvironment and
                    // activate them, so the heritage and computed names run
                    // with the class name in TDZ and `super`/private names
                    // visible (spec 15.7.14 steps 2-11).
                    let outer_env = self.lexical_env.clone();
                    let class_env = new_declarative_environment(Some(outer_env.clone()));
                    if let Some(binding) = binding {
                        let name = crux::lookup(*binding);
                        class_env.create_immutable_binding(&name, true)?;
                    }
                    let outer_private_env = agent.running_context()?.private_environment.clone();
                    let class_private_env =
                        crate::context::new_private_environment(outer_private_env.clone());
                    {
                        let mut names = class_private_env.names.borrow_mut();
                        for element in &class.elements {
                            let Some(atom) = crate::class::private_element_name(element) else {
                                continue;
                            };
                            let description = JsString::from_utf8(&format!(
                                "#{}",
                                crux::lookup(atom).to_string_lossy()
                            ));
                            if !names.iter().any(|name| name.description == description) {
                                names.push(crate::context::new_private_name(description));
                            }
                        }
                    }
                    if let Ok(context) = agent.running_context_mut() {
                        context.lexical_environment = class_env.clone();
                        context.private_environment = Some(class_private_env.clone());
                    }
                    self.lexical_env = class_env.clone();
                    self.class_stack.push(ClassEvalState {
                        class_env,
                        class_private_env,
                        outer_private_env,
                        outer_env,
                        heritage: None,
                    });
                }
                Step::ClassHeritage => {
                    let value = self.pop();
                    let Some(state) = self.class_stack.last_mut() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "ClassHeritage without a pending class".into(),
                        ));
                    };
                    state.heritage = Some(value);
                }
                Step::ClassKeyToPropertyKey => {
                    // ToPropertyKey runs before the next name is evaluated
                    // (user code ordering, spec 15.7.13), so convert here.
                    let value = self.pop();
                    let key = crate::context::to_property_key(agent, &value)?;
                    let key = match key {
                        PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                        PropertyKey::Symbol(symbol) => Value::Symbol(Handle::new(symbol)),
                    };
                    self.stack.push(key);
                }
                Step::ClassFinish {
                    class,
                    binding,
                    key_count,
                } => {
                    let mut keys: Vec<Option<PropertyKey>> = Vec::with_capacity(*key_count);
                    for _ in 0..*key_count {
                        let value = self.pop();
                        let key = crate::context::to_property_key(agent, &value)?;
                        keys.push(Some(key));
                    }
                    keys.reverse();
                    let state = self.class_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "ClassFinish without a pending class".into(),
                        )
                    })?;
                    self.lexical_env = state.outer_env.clone();
                    if let Ok(context) = agent.running_context_mut() {
                        context.lexical_environment = state.outer_env.clone();
                        context.private_environment = state.outer_private_env.clone();
                    }
                    let class_value = crate::class::class_definition_evaluation_with_keys(
                        agent,
                        class,
                        *binding,
                        state.heritage.clone(),
                        &keys,
                    )?;
                    self.stack.push(class_value);
                }
                Step::PushStr(text) => {
                    self.stack.push(Value::String(Handle::new(text.clone())));
                }
                Step::ConcatStr => {
                    let value = self.pop();
                    let acc = self.pop();
                    let text = crate::context::to_string(agent, &value)?;
                    let mut units = string_units_of(&acc);
                    units.extend_from_slice(text.as_slice());
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf16(&units))));
                }
                Step::ConcatStrConst(text) => {
                    let acc = self.pop();
                    let mut units = string_units_of(&acc);
                    units.extend_from_slice(text.as_slice());
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf16(&units))));
                }
                Step::ArgsBase => {
                    self.args_base_stack.push(self.args.len());
                }
                Step::ArgsPush => {
                    let value = self.pop();
                    self.args.push(value);
                }
                Step::ArgsSpread => {
                    let iterable = self.pop();
                    let iterator = get_iterator(agent, &iterable)?;
                    while let Some(value) = iterator_step(agent, &iterator)? {
                        self.args.push(value);
                    }
                }
                Step::EnterBlock { decls } => {
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    block_declaration_instantiation(agent, decls, &env, self.strict)?;
                    self.lexical_env = env.clone();
                    self.env_stack.push(env);
                }
                Step::LeaveBlock => {
                    let popped = self.env_stack.pop().ok_or_else(|| {
                        JsError::new(ErrorKind::SyntaxError, "Environment stack underflow".into())
                    })?;
                    // spec 9.4.3: the scope's `using` resources are disposed
                    // when it exits, in reverse registration order;
                    // async-dispose hints suspend the VM through the job
                    // queue.
                    let mut resources = popped.drain_disposable_resources();
                    resources.reverse();
                    // Restore to the popped environment's outer, which may
                    // differ from the stack's previous entry (per-iteration
                    // environments live outside the stack).
                    self.lexical_env = popped.outer().unwrap_or(popped);
                    if !resources.is_empty() {
                        let completion = Completion::Normal(self.completion.clone());
                        if let Some(outcome) = self.start_scope_disposal(
                            agent,
                            body,
                            resources,
                            completion,
                            DisposalResume::ApplyCompletion,
                        )? {
                            return Ok(outcome);
                        }
                    }
                }
                Step::EnterTry { handler } => {
                    self.try_stack.push(TryFrame {
                        handler: *handler,
                        saved_env: self.lexical_env.clone(),
                        env_depth: self.env_stack.len(),
                    });
                }
                Step::Exit { after } => {
                    match self.control_transfer(agent, body, Ctl::Normal { after: *after })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::CatchBind { param, decls } => {
                    // A caught throw discarded the try block's envs: dispose
                    // their `using` resources first, folding into the thrown
                    // value (spec 9.4.3 on the try block's abrupt
                    // completion), so the catch observes the folded error.
                    if let Some((saved_env, depth)) = self.pending_catch_disposal.take() {
                        let resources: Vec<crate::env::DisposableResource> = self
                            .env_stack
                            .drain(depth..)
                            .rev()
                            .flat_map(|env| env.drain_disposable_resources().into_iter().rev())
                            .collect();
                        if !resources.is_empty() {
                            let thrown = self.thrown.take().unwrap_or(Value::Undefined);
                            let completion = Completion::Throw(thrown);
                            if let Some(outcome) = self.start_scope_disposal(
                                agent,
                                body,
                                resources,
                                completion,
                                DisposalResume::DeliverCatch {
                                    saved_env,
                                    env_depth: depth,
                                },
                            )? {
                                // The disposal suspended: the ip was already
                                // advanced past this step, so re-enter the
                                // CatchBind step on resume to bind the
                                // parameter with the folded value.
                                self.ip -= 1;
                                return Ok(outcome);
                            }
                        } else {
                            self.restore_env(saved_env, depth);
                        }
                    }
                    let thrown = self.thrown.take().unwrap_or(Value::Undefined);
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    // The catch parameter binds in its own environment, so a
                    // default initializer's closure captures the parameter
                    // (spec 15.1.7 step 7). All the created environments go on
                    // the stack so the body's LeaveBlock(s) unwind back to
                    // `old_env` — otherwise the parameter environment would
                    // stay active after the catch, leaking its bindings.
                    let body_env = match &param {
                        Some(param) => {
                            let param_env = new_declarative_environment(Some(env.clone()));
                            self.env_stack.push(env);
                            let mut names = Vec::new();
                            crate::script::bound_names(param, &mut names);
                            for name in &names {
                                param_env.create_mutable_binding(name, false)?;
                            }
                            // The parameter environment is the running
                            // environment while the default initializers run,
                            // so a closure captures the parameter (spec
                            // 15.1.7 step 7).
                            if let Ok(context) = agent.running_context_mut() {
                                context.lexical_environment = param_env.clone();
                            }
                            crate::binding::binding_initialization(
                                agent,
                                param,
                                thrown,
                                Some(&param_env),
                                self.strict,
                            )?;
                            self.env_stack.push(param_env.clone());
                            new_declarative_environment(Some(param_env))
                        }
                        None => env,
                    };
                    block_declaration_instantiation(agent, decls, &body_env, self.strict)?;
                    self.lexical_env = body_env.clone();
                    self.env_stack.push(body_env);
                }
                Step::FinallyEnd => {
                    let pending = self.pending.take().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "FinallyEnd without a pending control".into(),
                        )
                    })?;
                    let ctl = match pending {
                        PendingControl::Normal { after, env, depth } => {
                            self.restore_env(env, depth);
                            Ctl::Normal { after }
                        }
                        PendingControl::Break { target, env, depth } => {
                            self.restore_env(env, depth);
                            Ctl::Break { target }
                        }
                        PendingControl::Continue { target, env, depth } => {
                            self.restore_env(env, depth);
                            Ctl::Continue { target }
                        }
                        PendingControl::Return { value, env, depth } => {
                            self.restore_env(env, depth);
                            Ctl::Return { value }
                        }
                        PendingControl::Throw { value, env, depth } => {
                            self.restore_env(env, depth);
                            Ctl::Throw { value }
                        }
                    };
                    match self.control_transfer(agent, body, ctl)? {
                        CtlResult::Continue => {}
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::EnterIterTdzEnv { names } => {
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    for name in names {
                        env.create_mutable_binding(name, false)?;
                    }
                    self.lexical_env = env.clone();
                    self.env_stack.push(env);
                }
                Step::LeaveIterTdzEnv => {
                    let popped = self.env_stack.pop().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "LeaveIterTdzEnv without an environment".into(),
                        )
                    })?;
                    self.lexical_env = popped.outer().unwrap_or(popped);
                }
                Step::EnterWith => {
                    let object = self.pop();
                    // spec 14.15.4 step 2: primitives are boxed (ToObject).
                    let object_value = crate::context::to_object(agent, &object)?;
                    let obj = crate::context::as_object(&object_value).ok_or_else(|| {
                        JsError::new(
                            ErrorKind::TypeError,
                            "Cannot use 'with' on a non-object value".into(),
                        )
                    })?;
                    let with_env = crate::env::new_object_environment(
                        obj,
                        true,
                        Some(self.lexical_env.clone()),
                    );
                    self.lexical_env = with_env.clone();
                    self.env_stack.push(with_env);
                }
                Step::PerIteration { names } => {
                    let last = self.lexical_env.clone();
                    let outer = last.outer().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::ReferenceError,
                            "No outer environment for per-iteration bindings".into(),
                        )
                    })?;
                    let env = new_declarative_environment(Some(outer));
                    for name in names {
                        let value = last.get_binding_value(name, false)?;
                        env.create_mutable_binding(name, false)?;
                        env.initialize_binding(name, value)?;
                    }
                    // The per-iteration environment replaces the lexical
                    // environment without joining the stack; the loop's exit
                    // restores the loop environment directly.
                    self.lexical_env = env;
                }
                Step::EnterLoopEnv { kind, decls } => {
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    for decl in decls {
                        let mut names = Vec::new();
                        crate::script::bound_names(&decl.pattern, &mut names);
                        for name in &names {
                            if *kind == VarDeclKind::Const {
                                env.create_immutable_binding(name, true)?;
                            } else {
                                env.create_mutable_binding(name, false)?;
                            }
                        }
                        let value = match &decl.init {
                            Some(init) => crate::expr::eval_expr(agent, init, self.strict)?,
                            None => Value::Undefined,
                        };
                        crate::binding::binding_initialization(
                            agent,
                            &decl.pattern,
                            value,
                            Some(&env),
                            self.strict,
                        )?;
                    }
                    self.lexical_env = env.clone();
                    self.env_stack.push(env);
                }
                Step::ForInBegin => {
                    let rhs = self.pop();
                    let obj = crate::context::to_object(agent, &rhs)?;
                    let obj = crate::context::as_object(&obj).ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "for-in over a non-object".into())
                    })?;
                    let keys = crate::eval::for_in_key_levels(agent, &rhs)?;
                    self.for_in_stack.push((obj, keys, 0));
                }
                Step::ForInNext { done } => {
                    let Some((obj, keys, index)) = self.for_in_stack.last_mut() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "ForInNext without a for-in".into(),
                        ));
                    };
                    let mut pushed = false;
                    while *index < keys.len() {
                        let (level, key) = keys[*index].clone();
                        *index += 1;
                        // A key deleted during enumeration is skipped (spec
                        // EnumerateObjectProperties step 5.a.v).
                        if crate::eval::key_enumerable_at_level(obj, level, &key)? {
                            self.stack.push(key);
                            pushed = true;
                            break;
                        }
                    }
                    if !pushed {
                        self.for_in_stack.pop();
                        self.ip = *done;
                    }
                }
                Step::ForInBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, left, value)?;
                }
                Step::ForInRestore => {
                    self.restore_per_iteration(agent)?;
                }
                Step::ForOfBegin => {
                    let rhs = self.pop();
                    let iterator = get_iterator(agent, &rhs)?;
                    self.for_of_stack.push(iterator);
                }
                Step::ForOfNext { done } => {
                    let Some(iterator) = self.for_of_stack.last() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "ForOfNext without a for-of".into(),
                        ));
                    };
                    let iterator = iterator.clone();
                    // A `next()` error propagates without closing the iterator
                    // (spec 14.7.6.2 uses `?`): the flag suppresses the
                    // error-path close in `run_inner`.
                    self.for_of_stepping = true;
                    let stepped = iterator_step(agent, &iterator);
                    self.for_of_stepping = false;
                    match stepped? {
                        Some(value) => self.stack.push(value),
                        None => {
                            self.for_of_stack.pop();
                            self.ip = *done;
                        }
                    }
                }
                Step::ForOfBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, left, value)?;
                }
                Step::ForOfRestore => {
                    self.restore_per_iteration(agent)?;
                }
                Step::ForOfClose => {
                    if let Some(iterator) = self.for_of_stack.pop() {
                        iterator_close(agent, &iterator)?;
                    }
                }
                Step::AsyncForOfClose => {
                    // AsyncIteratorClose with a normal completion (spec
                    // 14.7.5.7 step 8.b): the `return` method is invoked when
                    // present; its errors propagate. The exhausted path pops
                    // the stack at AsyncForOfTest, so only a break (or other
                    // early exit reaching the end label) closes here.
                    if let Some(iterator) = self.async_for_of_stack.pop() {
                        iterator_close(agent, &iterator)?;
                    }
                }
                Step::AsyncForOfBegin => {
                    let rhs = self.pop();
                    let iterator = async_from_sync_or_async(agent, &rhs)?;
                    self.async_for_of_stack.push(iterator);
                }
                Step::AsyncForOfNext => {
                    let Some(iterator) = self.async_for_of_stack.last() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "AsyncForOfNext without a for-await-of".into(),
                        ));
                    };
                    let iterator = iterator.clone();
                    let next_result = crate::function::call(
                        agent,
                        &iterator.next,
                        iterator.iterator.clone(),
                        &[],
                    )?;
                    return Ok(VmOutcome::Suspended(Suspension::Await(next_result)));
                }
                Step::AsyncForOfTest { done } => {
                    // Resumed with the awaited iterator result object.
                    let result = self.pop();
                    let is_done = iterator_result_done(agent, &result)?;
                    if is_done {
                        self.async_for_of_stack.pop();
                        self.ip = *done;
                    } else {
                        let value = iterator_result_value(agent, &result)?;
                        // The element value is already unwrapped upstream —
                        // AsyncFromSyncIteratorContinuation awaits a sync
                        // iterable's value and an async generator's yield
                        // awaits its own — so the loop body consumes it
                        // directly (spec 14.7.5.7 awaits only the next()
                        // result). An extra await here costs a microtask and
                        // breaks the promise interleaving.
                        self.stack.push(value);
                    }
                }
                Step::AsyncForOfBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, left, value)?;
                }
                Step::AsyncForOfRestore => {
                    self.restore_per_iteration(agent)?;
                }
                Step::SwitchDisc => {
                    let disc = self.pop();
                    self.switch_disc = Some(disc);
                }
                Step::SwitchTest { case } => {
                    let test = self.pop();
                    let disc = self.switch_disc.as_ref().ok_or_else(|| {
                        JsError::new(
                            ErrorKind::SyntaxError,
                            "SwitchTest without a discriminant".into(),
                        )
                    })?;
                    if crux::ops::is_strictly_equal(disc, &test) {
                        self.ip = *case;
                    }
                }
                Step::SetCompletion => {
                    let value = self.pop();
                    self.completion = value;
                    self.completion_is_empty = false;
                }
                Step::ResetCompletion => {
                    self.completion = Value::Undefined;
                    self.completion_is_empty = true;
                }
                Step::NormalizeCompletion => {
                    if self.completion_is_empty {
                        self.completion = Value::Undefined;
                        self.completion_is_empty = false;
                    }
                }
                Step::ListBegin => {
                    self.list_stack
                        .push((self.completion.clone(), self.completion_is_empty));
                    self.completion = Value::Undefined;
                    self.completion_is_empty = true;
                }
                Step::ListEnd => {
                    if let Some((value, empty)) = self.list_stack.pop()
                        && self.completion_is_empty
                    {
                        self.completion = value;
                        self.completion_is_empty = empty;
                    }
                }
                Step::SaveCompletion => {
                    self.completion_stack
                        .push((self.completion.clone(), self.completion_is_empty));
                }
                Step::RestoreCompletion => {
                    if let Some((value, empty)) = self.completion_stack.pop() {
                        self.completion = value;
                        self.completion_is_empty = empty;
                    }
                }
                Step::Jump(target) => self.ip = *target,
                Step::JumpIfFalse(target) => {
                    let value = self.pop();
                    if !crux::convert::to_boolean(&value) {
                        self.ip = *target;
                    }
                }
                Step::JumpIfTrue(target) => {
                    let value = self.pop();
                    if crux::convert::to_boolean(&value) {
                        self.ip = *target;
                    }
                }
                Step::JumpIfFalseKeep(target) => {
                    let value = self.pop();
                    if !crux::convert::to_boolean(&value) {
                        self.stack.push(value);
                        self.ip = *target;
                    } else {
                        // The left operand is not the result: leave it for the
                        // following Pop to discard (it was already popped).
                        self.stack.push(value);
                    }
                }
                Step::JumpIfTrueKeep(target) => {
                    let value = self.pop();
                    if crux::convert::to_boolean(&value) {
                        self.stack.push(value);
                        self.ip = *target;
                    } else {
                        self.stack.push(value);
                    }
                }
                Step::JumpIfNullishKeep(target) => {
                    let value = self.pop();
                    if is_nullish(&value) {
                        self.stack.push(value);
                        self.ip = *target;
                    }
                }
                Step::JumpIfNotNullishKeep(target) => {
                    let value = self.pop();
                    if !is_nullish(&value) {
                        self.stack.push(value);
                        self.ip = *target;
                    } else {
                        self.stack.push(value);
                    }
                }
                Step::Break { target } => {
                    match self.control_transfer(agent, body, Ctl::Break { target: *target })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Continue { target } => {
                    match self.control_transfer(agent, body, Ctl::Continue { target: *target })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Return => {
                    let value = self.pop();
                    match self.control_transfer(agent, body, Ctl::Return { value })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Throw => {
                    let value = self.pop();
                    match self.throw_machinery(agent, body, value)? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Yield { delegate } => {
                    let value = self.pop();
                    return Ok(VmOutcome::Suspended(Suspension::Yield {
                        value,
                        delegate: *delegate,
                    }));
                }
                Step::Await => {
                    let value = self.pop();
                    return Ok(VmOutcome::Suspended(Suspension::Await(value)));
                }
                Step::YieldStarBegin => {
                    let value = self.pop();
                    // GetIterator runs inside the generator body, so its
                    // TypeError is catchable by the body's try/catch (spec
                    // 15.5.5 step 4 uses `?`).
                    let iterator = match get_iterator(agent, &value) {
                        Ok(iterator) => iterator,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        },
                    };
                    self.yield_star_stack.push(YieldStarState {
                        iterator,
                        received: Value::Undefined,
                        resumed_return: false,
                    });
                }
                Step::YieldStarNext { done } => {
                    let Some(state) = self.yield_star_stack.last() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "YieldStarNext without a delegation".into(),
                        ));
                    };
                    let received = state.received.clone();
                    let iterator = state.iterator.clone();
                    let next = match crate::expr::iterator_next_method(agent, &iterator) {
                        Ok(next) => next,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        },
                    };
                    let result = match crate::function::call(
                        agent,
                        &next,
                        iterator.iterator.clone(),
                        &[received],
                    ) {
                        Ok(result) => result,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        },
                    };
                    if !matches!(result.kind(), ValueKind::Object(_)) {
                        // Spec 15.5.5 normal case step a.iii.
                        let error = JsError::new(
                            ErrorKind::TypeError,
                            "yield*: iterator next() result is not an object".into(),
                        );
                        match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        }
                    }
                    let done_flag = match iterator_result_done(agent, &result) {
                        Ok(done_flag) => done_flag,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        },
                    };
                    if done_flag {
                        let value = match iterator_result_value(agent, &result) {
                            Ok(value) => value,
                            Err(error) => match self.throw_js_error(agent, body, error)? {
                                CtlResult::Continue => continue,
                                CtlResult::Done(outcome) => return Ok(outcome),
                            },
                        };
                        self.yield_star_stack.pop();
                        self.stack.push(value);
                        self.ip = *done;
                    } else {
                        // Spec 15.5.5: GeneratorYield(innerResult) yields the
                        // inner iterator result object itself, so the outer
                        // consumer reads its `value`/`done` lazily.
                        self.stack.push(result);
                    }
                }
                Step::YieldStarResume {
                    loop_top,
                    done,
                    yield_at,
                } => {
                    let received = self.pop();
                    match self.resume_abrupt.take() {
                        None => {
                            if let Some(state) = self.yield_star_stack.last_mut() {
                                state.received = received;
                            }
                            self.ip = *loop_top;
                        }
                        Some(ResumeAbrupt::Throw(value)) => {
                            let Some(state) = self.yield_star_stack.last() else {
                                return Err(JsError::new(
                                    ErrorKind::SyntaxError,
                                    "YieldStarResume without a delegation".into(),
                                ));
                            };
                            let iterator = state.iterator.clone();
                            // GetMethod(iterator, "throw"); errors propagate
                            // through the generator body's handlers (spec
                            // 15.5.5 throw case).
                            let throw_method = match crate::context::get_property(
                                agent,
                                &iterator.iterator,
                                &JsString::from_utf8("throw"),
                                iterator.iterator.clone(),
                            ) {
                                Ok(method) => method,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => continue,
                                    CtlResult::Done(outcome) => return Ok(outcome),
                                },
                            };
                            if is_callable(&throw_method) {
                                let inner = match crate::function::call(
                                    agent,
                                    &throw_method,
                                    iterator.iterator.clone(),
                                    &[value],
                                ) {
                                    Ok(inner) => inner,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    },
                                };
                                if !matches!(inner.kind(), ValueKind::Object(_)) {
                                    // Spec 15.5.5 throw case step b.iii.
                                    let error = JsError::new(
                                        ErrorKind::TypeError,
                                        "yield*: iterator throw() result is not an object".into(),
                                    );
                                    match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    }
                                }
                                let done_flag = match iterator_result_done(agent, &inner) {
                                    Ok(done_flag) => done_flag,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    },
                                };
                                if done_flag {
                                    let value = match iterator_result_value(agent, &inner) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            match self.throw_js_error(agent, body, error)? {
                                                CtlResult::Continue => continue,
                                                CtlResult::Done(outcome) => return Ok(outcome),
                                            }
                                        }
                                    };
                                    self.yield_star_stack.pop();
                                    self.stack.push(value);
                                    self.ip = *done;
                                } else {
                                    self.stack.push(inner);
                                    self.ip = *yield_at;
                                }
                            } else {
                                // No throw method: IteratorClose with a normal
                                // completion, then a protocol-violation
                                // TypeError. Errors from the close propagate
                                // (spec 15.5.5 throw case, no-throw branch).
                                if let Err(error) = iterator_close(agent, &iterator) {
                                    match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    }
                                }
                                self.yield_star_stack.pop();
                                let violation = JsError::new(
                                    ErrorKind::TypeError,
                                    "yield* protocol violation: iterator has no throw method"
                                        .into(),
                                );
                                match self.throw_js_error(agent, body, violation)? {
                                    CtlResult::Continue => continue,
                                    CtlResult::Done(outcome) => return Ok(outcome),
                                }
                            }
                        }
                        Some(ResumeAbrupt::Return(value)) => {
                            let Some(state) = self.yield_star_stack.last() else {
                                return Err(JsError::new(
                                    ErrorKind::SyntaxError,
                                    "YieldStarResume without a delegation".into(),
                                ));
                            };
                            let iterator = state.iterator.clone();
                            // GetMethod(iterator, "return"); errors propagate
                            // through the generator body's handlers (spec
                            // 15.5.5 return case).
                            let return_method = match crate::context::get_property(
                                agent,
                                &iterator.iterator,
                                &JsString::from_utf8("return"),
                                iterator.iterator.clone(),
                            ) {
                                Ok(method) => method,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => continue,
                                    CtlResult::Done(outcome) => return Ok(outcome),
                                },
                            };
                            if is_callable(&return_method) {
                                let inner = match crate::function::call(
                                    agent,
                                    &return_method,
                                    iterator.iterator.clone(),
                                    &[value],
                                ) {
                                    Ok(inner) => inner,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    },
                                };
                                if !matches!(inner.kind(), ValueKind::Object(_)) {
                                    // Spec 15.5.5 return case step c.vi.
                                    let error = JsError::new(
                                        ErrorKind::TypeError,
                                        "yield*: iterator return() result is not an object".into(),
                                    );
                                    match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    }
                                }
                                let done_flag = match iterator_result_done(agent, &inner) {
                                    Ok(done_flag) => done_flag,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    },
                                };
                                if done_flag {
                                    // A done return result completes the
                                    // generator with ReturnCompletion of its
                                    // value (spec 15.5.5 return case).
                                    let value = match iterator_result_value(agent, &inner) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            match self.throw_js_error(agent, body, error)? {
                                                CtlResult::Continue => continue,
                                                CtlResult::Done(outcome) => return Ok(outcome),
                                            }
                                        }
                                    };
                                    self.yield_star_stack.pop();
                                    match self.control_transfer(
                                        agent,
                                        body,
                                        Ctl::Return { value },
                                    )? {
                                        CtlResult::Continue => continue,
                                        CtlResult::Done(outcome) => return Ok(outcome),
                                    }
                                } else {
                                    self.stack.push(inner);
                                    self.ip = *yield_at;
                                }
                            } else {
                                // No return method: the delegation completes
                                // with the return completion carrying the
                                // received value (spec 15.5.5 return case).
                                self.yield_star_stack.pop();
                                match self.control_transfer(agent, body, Ctl::Return { value })? {
                                    CtlResult::Continue => continue,
                                    CtlResult::Done(outcome) => return Ok(outcome),
                                }
                            }
                        }
                    }
                }
                Step::AsyncYieldStarBegin => {
                    let value = self.pop();
                    let iterator = get_async_iterator_record(agent, &value)?;
                    self.yield_star_stack.push(YieldStarState {
                        iterator,
                        received: Value::Undefined,
                        resumed_return: false,
                    });
                }
                Step::AsyncYieldStarNext { done: _ } => {
                    let Some(state) = self.yield_star_stack.last() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "AsyncYieldStarNext without a delegation".into(),
                        ));
                    };
                    let received = state.received.clone();
                    let iterator = state.iterator.clone();
                    let next = crate::expr::iterator_next_method(agent, &iterator)?;
                    let result = crate::function::call(
                        agent,
                        &next,
                        iterator.iterator.clone(),
                        &[received],
                    )?;
                    // The await resume pushes the fulfilled iterator result.
                    return Ok(VmOutcome::Suspended(Suspension::Await(result)));
                }
                Step::AsyncYieldStarInspect { done } => {
                    let result = self.pop();
                    if !matches!(result.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                        // spec 15.5.5: the awaited inner result must be an
                        // object (the check runs after the await, so a
                        // non-object value's `then` is never consulted).
                        let error = JsError::new(
                            ErrorKind::TypeError,
                            "yield*: iterator result is not an object".into(),
                        );
                        match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        }
                    }
                    let done_flag = match iterator_result_done(agent, &result) {
                        Ok(done_flag) => done_flag,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => continue,
                            CtlResult::Done(outcome) => return Ok(outcome),
                        },
                    };
                    if done_flag {
                        // Getter errors are catchable by the body (spec
                        // 15.5.5 uses `?` on IteratorValue).
                        let value = match iterator_result_value(agent, &result) {
                            Ok(value) => value,
                            Err(error) => match self.throw_js_error(agent, body, error)? {
                                CtlResult::Continue => continue,
                                CtlResult::Done(outcome) => return Ok(outcome),
                            },
                        };
                        let resumed_return = self
                            .yield_star_stack
                            .last()
                            .is_some_and(|state| state.resumed_return);
                        self.yield_star_stack.pop();
                        if resumed_return {
                            // spec 15.5.5 return case: a done return result
                            // completes the body with a return completion of
                            // its value.
                            match self.control_transfer(agent, body, Ctl::Return { value })? {
                                CtlResult::Continue => continue,
                                CtlResult::Done(outcome) => return Ok(outcome),
                            }
                        } else {
                            // The loop's done case: the yield* expression
                            // completes with the value and the body continues.
                            self.stack.push(value);
                            self.ip = *done;
                        }
                    } else {
                        let value = match iterator_result_value(agent, &result) {
                            Ok(value) => value,
                            Err(error) => match self.throw_js_error(agent, body, error)? {
                                CtlResult::Continue => continue,
                                CtlResult::Done(outcome) => return Ok(outcome),
                            },
                        };
                        self.stack.push(value);
                    }
                }
                Step::AsyncYieldStarResume {
                    loop_top,
                    done: _,
                    inspect,
                } => {
                    let received = self.pop();
                    match self.resume_abrupt.take() {
                        None => {
                            if let Some(state) = self.yield_star_stack.last_mut() {
                                state.received = received;
                                state.resumed_return = false;
                            }
                            self.ip = *loop_top;
                        }
                        Some(ResumeAbrupt::Throw(value)) => {
                            let (iterator,) = {
                                let Some(state) = self.yield_star_stack.last() else {
                                    return Err(JsError::new(
                                        ErrorKind::SyntaxError,
                                        "AsyncYieldStarResume without a delegation".into(),
                                    ));
                                };
                                (state.iterator.clone(),)
                            };
                            if let Some(state) = self.yield_star_stack.last_mut() {
                                state.resumed_return = false;
                            }
                            let throw_method = crate::context::get_property(
                                agent,
                                &iterator.iterator,
                                &JsString::from_utf8("throw"),
                                iterator.iterator.clone(),
                            )?;
                            if is_callable(&throw_method) {
                                let inner = crate::function::call(
                                    agent,
                                    &throw_method,
                                    iterator.iterator.clone(),
                                    &[value],
                                )?;
                                self.ip = *inspect;
                                return Ok(VmOutcome::Suspended(Suspension::Await(inner)));
                            }
                            iterator_close(agent, &iterator)?;
                            self.yield_star_stack.pop();
                            // spec 15.5.5 throw case, no-throw branch: after
                            // closing, the protocol violation is a TypeError.
                            let violation = JsError::new(
                                ErrorKind::TypeError,
                                "yield* protocol violation: iterator has no throw method".into(),
                            );
                            match self.throw_js_error(agent, body, violation)? {
                                CtlResult::Continue => continue,
                                CtlResult::Done(outcome) => return Ok(outcome),
                            }
                        }
                        Some(ResumeAbrupt::Return(value)) => {
                            let iterator = {
                                let Some(state) = self.yield_star_stack.last() else {
                                    return Err(JsError::new(
                                        ErrorKind::SyntaxError,
                                        "AsyncYieldStarResume without a delegation".into(),
                                    ));
                                };
                                state.iterator.clone()
                            };
                            if let Some(state) = self.yield_star_stack.last_mut() {
                                state.resumed_return = true;
                            }
                            let return_method = crate::context::get_property(
                                agent,
                                &iterator.iterator,
                                &JsString::from_utf8("return"),
                                iterator.iterator.clone(),
                            )?;
                            if is_callable(&return_method) {
                                let inner = crate::function::call(
                                    agent,
                                    &return_method,
                                    iterator.iterator.clone(),
                                    &[value],
                                )?;
                                self.ip = *inspect;
                                return Ok(VmOutcome::Suspended(Suspension::Await(inner)));
                            }
                            self.yield_star_stack.pop();
                            // spec 15.5.5 return case step b: no return
                            // method — await the received value, then return
                            // it (the driver resumes with `Resume::Return`).
                            return Ok(VmOutcome::Suspended(Suspension::AwaitReturn(value)));
                        }
                    }
                }
                Step::ImportCall { has_options, phase } => {
                    let options = if *has_options { Some(self.pop()) } else { None };
                    let specifier = self.pop();
                    let promise =
                        crate::module::dynamic_import(agent, &specifier, options.as_ref(), *phase)?;
                    self.stack.push(promise);
                }
                Step::ImportMeta => {
                    let meta = crate::module::import_meta(agent)?;
                    self.stack.push(meta);
                }
            }
        }
    }

    /// Convert an internal error into a thrown value routed through the
    /// handler table.
    fn throw_error(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        error: JsError,
    ) -> Result<VmOutcome, JsError> {
        // Engine errors throw as real Error objects (spec ch. 17), matching
        // the interpreter path (`to_throwable`); the message string is the
        // fallback until the built-ins are installed.
        let value = match crate::builtins::error::to_throwable(agent, &error) {
            Ok(value) => value,
            Err(_) => error_message_value(&error),
        };
        match self.throw_machinery(agent, body, value)? {
            CtlResult::Continue => self.run_inner(agent, body),
            CtlResult::Done(outcome) => Ok(outcome),
        }
    }

    /// Begin disposing a scope's `using` resources in reverse order, folding
    /// each result into the completion (spec 9.4.3). When an async-dispose
    /// awaits, the driver suspends with `Suspension::Await`; `run_abrupt`
    /// resumes it with the settled value. Returns `Some(outcome)` when the
    /// VM run finished (the folded completion ended the body), `None` when
    /// the run continues from the current ip.
    fn start_scope_disposal(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        resources: Vec<crate::env::DisposableResource>,
        completion: Completion,
        resume: DisposalResume,
    ) -> Result<Option<VmOutcome>, JsError> {
        self.pending_disposal = Some(PendingDisposal {
            resources,
            index: 0,
            completion,
            resume,
        });
        self.drive_pending_disposal(agent, body)
    }

    /// Drive the pending scope disposal: dispose the next resource, awaiting
    /// async-dispose results through the suspension machinery.
    fn drive_pending_disposal(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
    ) -> Result<Option<VmOutcome>, JsError> {
        loop {
            let Some(pending) = self.pending_disposal.as_ref() else {
                return Ok(None);
            };
            if pending.index >= pending.resources.len() {
                let pending = self.pending_disposal.take().unwrap();
                let completion = pending.completion;
                return Ok(match pending.resume {
                    DisposalResume::ApplyCompletion => match completion {
                        Completion::Normal(value) => {
                            self.completion = value;
                            None
                        }
                        Completion::Empty => None,
                        Completion::Throw(value) => {
                            match self.throw_machinery(agent, body, value)? {
                                CtlResult::Continue => None,
                                CtlResult::Done(outcome) => Some(outcome),
                            }
                        }
                        _ => {
                            return Err(JsError::new(
                                ErrorKind::SyntaxError,
                                "Illegal disposal completion".into(),
                            ));
                        }
                    },
                    DisposalResume::DeliverCatch {
                        saved_env,
                        env_depth,
                    } => {
                        // The disposal started from a throw, so the folded
                        // completion stays a throw; deliver it to the catch.
                        let value = match completion {
                            Completion::Throw(value) => value,
                            Completion::Normal(value) => value,
                            Completion::Empty => Value::Undefined,
                            _ => Value::Undefined,
                        };
                        self.thrown = Some(value);
                        self.restore_env(saved_env, env_depth);
                        None
                    }
                });
            }
            let resource = pending.resources[pending.index].clone();
            let method_result = if resource.method.is_undefined() {
                if resource.hint == crate::env::DisposalHint::Sync {
                    // Dispose with an undefined method and sync hint: no call
                    // and no await (spec 9.4.4 steps 1-4).
                    self.pending_disposal.as_mut().unwrap().index += 1;
                    continue;
                }
                // Async hint: Dispose returns undefined but still
                // Await(undefined) — a microtask boundary (spec 9.4.4 step
                // 3.a).
                Ok(Value::Undefined)
            } else {
                crate::function::call(agent, &resource.method, resource.value.clone(), &[])
            };
            match method_result {
                Err(error) => {
                    let value = crate::promise::error_value(agent, &error);
                    self.fold_disposal_error(agent, value)?;
                    self.pending_disposal.as_mut().unwrap().index += 1;
                }
                Ok(value) => {
                    if resource.hint == crate::env::DisposalHint::Sync {
                        self.pending_disposal.as_mut().unwrap().index += 1;
                        continue;
                    }
                    let promise_ctor = agent
                        .current_realm()?
                        .intrinsics
                        .get("%Promise%")
                        .unwrap_or(Value::Undefined);
                    let promise = crate::promise::promise_resolve(agent, &promise_ctor, value)?;
                    return Ok(Some(VmOutcome::Suspended(Suspension::Await(promise))));
                }
            }
        }
    }

    /// The await of an async dispose settled: fold the result into the
    /// pending disposal and keep driving. A rejected dispose is a throwing
    /// disposal — it never routes through the body's handler table.
    fn resume_pending_disposal(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        result: Result<Value, Value>,
    ) -> Result<VmOutcome, JsError> {
        if let Err(value) = result {
            self.fold_disposal_error(agent, value)?;
        }
        let Some(pending) = self.pending_disposal.as_mut() else {
            return self.run_inner(agent, body);
        };
        pending.index += 1;
        match self.drive_pending_disposal(agent, body)? {
            Some(outcome) => Ok(outcome),
            None => self.run_inner(agent, body),
        }
    }

    /// Fold a throwing disposal into the pending completion: a normal
    /// completion becomes the throw; a second throw nests a SuppressedError
    /// (spec 9.4.3 step 1.b).
    fn fold_disposal_error(&mut self, agent: &mut Agent, new_error: Value) -> Result<(), JsError> {
        let pending = self
            .pending_disposal
            .as_mut()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal in flight".into()))?;
        pending.completion = match std::mem::replace(&mut pending.completion, Completion::Empty) {
            Completion::Throw(original) => Completion::Throw(
                crate::builtins::disposable::make_suppressed_error(agent, new_error, original)?,
            ),
            _ => Completion::Throw(new_error),
        };
        Ok(())
    }

    /// The Await algorithm (spec 27.6.3.1) is driven by the async-function
    /// machinery: the VM suspends with the operand, and the driver attaches
    /// promise reactions that resume it.
    fn assign_ident(
        &mut self,
        agent: &mut Agent,
        name: crux::AtomId,
        op: AssignOp,
        set_name: bool,
    ) -> Result<(), JsError> {
        let reference = crate::context::resolve_binding(agent, &crux::lookup(name), self.strict)?;
        match op {
            AssignOp::Assign => {
                let value = self.pop();
                if set_name {
                    let display =
                        crate::function::default_binding_display_name(Some(crux::lookup(name)))
                            .unwrap_or_else(|| crux::lookup(name));
                    crate::function::set_function_name(&value, &display, None)?;
                }
                crate::context::put_value(agent, &reference, value.clone())?;
                self.stack.push(value);
            }
            AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign => {
                let value = self.pop();
                self.pop(); // old
                crate::context::put_value(agent, &reference, value.clone())?;
                self.stack.push(value);
            }
            _ => {
                let value = self.pop();
                let old = self.pop();
                let new = crate::expr::apply_compound(agent, op, &old, &value)?;
                crate::context::put_value(agent, &reference, new.clone())?;
                self.stack.push(new);
            }
        }
        Ok(())
    }

    fn assign_member(
        &mut self,
        agent: &mut Agent,
        object: Value,
        key: PropertyKeyName,
        old: Option<Value>,
        value: Value,
        op: AssignOp,
    ) -> Result<(), JsError> {
        match op {
            AssignOp::Assign
            | AssignOp::AndAssign
            | AssignOp::OrAssign
            | AssignOp::NullishAssign => {
                let reference = member_reference(&object, &key, self.strict);
                crate::context::put_value(agent, &reference, value.clone())?;
                self.stack.push(value);
            }
            _ => {
                let Some(old) = old else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "compound assignment without a cached old value".into(),
                    ));
                };
                let new = crate::expr::apply_compound(agent, op, &old, &value)?;
                let reference = member_reference(&object, &key, self.strict);
                crate::context::put_value(agent, &reference, new.clone())?;
                self.stack.push(new);
            }
        }
        Ok(())
    }

    fn assign_super(
        &mut self,
        agent: &mut Agent,
        base: Value,
        key: PropertyKeyName,
        old: Option<Value>,
        value: Value,
        op: AssignOp,
    ) -> Result<(), JsError> {
        let this = resolve_this_binding(agent)?;
        match op {
            AssignOp::Assign
            | AssignOp::AndAssign
            | AssignOp::OrAssign
            | AssignOp::NullishAssign => {
                let reference = super_reference(&base, &key, self.strict, &this);
                crate::context::put_value(agent, &reference, value.clone())?;
                self.stack.push(value);
            }
            _ => {
                let Some(old) = old else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "compound assignment without a cached old value".into(),
                    ));
                };
                let new = crate::expr::apply_compound(agent, op, &old, &value)?;
                let reference = super_reference(&base, &key, self.strict, &this);
                crate::context::put_value(agent, &reference, new.clone())?;
                self.stack.push(new);
            }
        }
        Ok(())
    }

    fn put_super(
        &mut self,
        agent: &mut Agent,
        base: Value,
        key: PropertyKeyName,
        value: Value,
    ) -> Result<(), JsError> {
        let this = resolve_this_binding(agent)?;
        let reference = super_reference(&base, &key, self.strict, &this);
        crate::context::put_value(agent, &reference, value)
    }

    fn do_call(&mut self, agent: &mut Agent, direct_eval: bool) -> Result<(), JsError> {
        let callee = self.pop();
        let this = self.pop();
        let base = self.args_base_stack.pop().ok_or_else(|| {
            JsError::new(
                ErrorKind::SyntaxError,
                "Call without an argument boundary".into(),
            )
        })?;
        let args = self.args.split_off(base);
        if is_eval_function(agent, &callee)? {
            let source = args.first().cloned().unwrap_or(Value::Undefined);
            let source = crux::convert::to_string(&source)?;
            let result = crate::script::perform_eval(agent, &source, self.strict, direct_eval)?;
            self.stack.push(result);
            return Ok(());
        }
        if !is_callable(&callee) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("{} is not a function", crux::value::type_of(&callee)),
            ));
        }
        let result = crate::function::call(agent, &callee, this, &args)?;
        self.stack.push(result);
        Ok(())
    }

    fn for_binding_put(
        &mut self,
        agent: &mut Agent,
        left: &ForBinding,
        value: Value,
    ) -> Result<(), JsError> {
        match left {
            ForBinding::Expr(expr) => {
                let reference = crate::expr::eval_reference(agent, expr, self.strict)?;
                crate::context::put_value(agent, &reference, value)?;
            }
            ForBinding::VarDecl { kind, pattern, .. } => {
                if *kind == VarDeclKind::Var {
                    crate::binding::binding_initialization(
                        agent,
                        pattern,
                        value,
                        None,
                        self.strict,
                    )?;
                } else {
                    let outer = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(outer.clone()));
                    let mut names = Vec::new();
                    crate::script::bound_names(pattern, &mut names);
                    for name in &names {
                        if *kind == VarDeclKind::Const
                            || matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                        {
                            // `using` bindings are immutable like `const`
                            // (spec 15.14.2).
                            env.create_immutable_binding(name, true)?;
                        } else {
                            env.create_mutable_binding(name, false)?;
                        }
                    }
                    // spec 14.7.5.6 step 5.e: AddDisposableResource runs
                    // before InitializeReferencedBinding.
                    if matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing) {
                        let kind = if *kind == VarDeclKind::AwaitUsing {
                            crate::eval::DisposalKind::Async
                        } else {
                            crate::eval::DisposalKind::Sync
                        };
                        let resource =
                            crate::eval::create_disposable_resource(agent, &value, kind)?;
                        env.add_disposable_resource(resource);
                    }
                    crate::binding::binding_initialization(
                        agent,
                        pattern,
                        value,
                        Some(&env),
                        self.strict,
                    )?;
                    // The per-iteration environment replaces the lexical
                    // environment without joining the stack; the loop's
                    // restore step disposes its resources and the next
                    // iteration's bind replaces it.
                    self.lexical_env = env;
                }
            }
        }
        Ok(())
    }

    /// Pop a per-iteration environment created by a for-in/for-of bind,
    /// disposing its `using` resources (spec 14.7.5.6 / 14.7.6.2: the
    /// iteration environment is disposed at iteration end).
    fn restore_per_iteration(&mut self, agent: &mut Agent) -> Result<(), JsError> {
        // The per-iteration environment lives in the lexical environment, not
        // the stack; the next iteration's bind step replaces it.
        let env = self.lexical_env.clone();
        let resources = env.drain_disposable_resources();
        for resource in resources.iter().rev() {
            if !resource.method.is_undefined() {
                crate::function::call(agent, &resource.method, resource.value.clone(), &[])?;
            }
        }
        Ok(())
    }

    /// The control-transfer machinery: route through pending finallys, then
    /// apply the control.
    fn control_transfer(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        ctl: Ctl,
    ) -> Result<CtlResult, JsError> {
        let ctl = ctl;
        loop {
            let decision = self.find_finally_frame(body, &ctl);
            match decision {
                Some((index, Some(finally))) => {
                    let frame = self.try_stack.remove(index);
                    let env = self.lexical_env.clone();
                    let depth = self.env_stack.len();
                    self.pending = Some(match &ctl {
                        Ctl::Normal { after } => PendingControl::Normal {
                            after: *after,
                            env,
                            depth,
                        },
                        Ctl::Break { target } => PendingControl::Break {
                            target: *target,
                            env,
                            depth,
                        },
                        Ctl::Continue { target } => PendingControl::Continue {
                            target: *target,
                            env,
                            depth,
                        },
                        Ctl::Return { value } => PendingControl::Return {
                            value: value.clone(),
                            env,
                            depth,
                        },
                        Ctl::Throw { value } => PendingControl::Throw {
                            value: value.clone(),
                            env,
                            depth,
                        },
                    });
                    self.restore_env(frame.saved_env, frame.env_depth);
                    self.ip = finally;
                    return Ok(CtlResult::Continue);
                }
                Some((index, None)) => {
                    self.try_stack.remove(index);
                }
                None => {
                    // A return (or throw re-applied after a finally) leaving
                    // the body closes the active for-of iterators, inner
                    // first; a throwing `return` replaces the completion
                    // (spec 7.4.6).
                    if let Ctl::Return { .. } = ctl {
                        self.close_for_of_return(agent)?;
                    } else if let Ctl::Throw { .. } = ctl {
                        self.close_for_of_throw(agent);
                    }
                    return Ok(match ctl {
                        Ctl::Normal { after } => {
                            self.ip = after;
                            CtlResult::Continue
                        }
                        Ctl::Break { target } => {
                            self.ip = target;
                            CtlResult::Continue
                        }
                        Ctl::Continue { target } => {
                            self.ip = target;
                            CtlResult::Continue
                        }
                        Ctl::Return { value } => {
                            CtlResult::Done(VmOutcome::Completed(Completion::Return(value)))
                        }
                        Ctl::Throw { value } => {
                            CtlResult::Done(VmOutcome::Completed(Completion::Throw(value)))
                        }
                    });
                }
            }
        }
    }

    /// The innermost try frame the control leaves, with its finally target
    /// (or `None` when the frame has no finally).
    fn find_finally_frame(&self, body: &CompiledBody, ctl: &Ctl) -> Option<(usize, Option<usize>)> {
        for (i, frame) in self.try_stack.iter().enumerate().rev() {
            let handler = body.handlers.get(frame.handler)?;
            let covered_end = handler.catch.map(|c| c.end).unwrap_or(handler.try_end);
            // The ip is the *next* step to run (advanced at loop top), so a
            // try region's own `Exit` step runs with ip == try_end + 1; that
            // frame must still be found so its finally runs on the normal
            // path (GeneratorPrototype/throw/try-finally*).
            if self.ip < handler.start || self.ip > covered_end + 1 {
                continue;
            }
            let leaves = match ctl {
                Ctl::Normal { after } => !(handler.start <= *after && *after < covered_end),
                Ctl::Break { target } | Ctl::Continue { target } => {
                    !(handler.start <= *target && *target < covered_end)
                }
                Ctl::Return { .. } | Ctl::Throw { .. } => true,
            };
            if !leaves {
                continue;
            }
            return Some((i, handler.finally));
        }
        None
    }

    /// The throw machinery: dispatch to a catch or route through finallys.
    fn throw_machinery(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        value: Value,
    ) -> Result<CtlResult, JsError> {
        let value = value;
        loop {
            let decision = {
                let mut found: Option<(usize, ThrowAction)> = None;
                for (i, frame) in self.try_stack.iter().enumerate().rev() {
                    let Some(handler) = body.handlers.get(frame.handler) else {
                        continue;
                    };
                    // The frame covers the try region and its catch body (a
                    // throw from the catch must still run the finally); only a
                    // throw inside the try region itself dispatches to the
                    // catch. The `+1` mirrors find_finally_frame: the Exit
                    // step's ip is covered_end + 1.
                    let covered_end = handler.catch.map(|c| c.end).unwrap_or(handler.try_end);
                    if self.ip < handler.start || self.ip > covered_end + 1 {
                        continue;
                    }
                    let in_try = self.ip < handler.try_end;
                    found = Some((
                        i,
                        if in_try && handler.catch.is_some() {
                            ThrowAction::Catch
                        } else if handler.finally.is_some() {
                            ThrowAction::Finally
                        } else {
                            ThrowAction::Pop
                        },
                    ));
                    break;
                }
                found
            };
            match decision {
                Some((index, ThrowAction::Catch)) => {
                    let handler_index = self.try_stack[index].handler;
                    let has_finally = body
                        .handlers
                        .get(handler_index)
                        .is_some_and(|h| h.finally.is_some());
                    let saved_env = self.try_stack[index].saved_env.clone();
                    let env_depth = self.try_stack[index].env_depth;
                    if !has_finally {
                        // Keep the frame when a finally is pending: the catch's
                        // `Exit` routes through it (spec 14.15.4 step 5: the
                        // finally runs even after a caught error).
                        self.try_stack.remove(index);
                    }
                    // Leave the try block's envs on the stack: `CatchBind`
                    // disposes their `using` resources (folding into the
                    // thrown value) before restoring the saved environment.
                    self.pending_catch_disposal = Some((saved_env, env_depth));
                    self.thrown = Some(value);
                    let catch_start = body
                        .handlers
                        .get(handler_index)
                        .and_then(|h| h.catch)
                        .map(|c| c.start)
                        .unwrap_or(self.ip);
                    self.ip = catch_start;
                    return Ok(CtlResult::Continue);
                }
                Some((index, ThrowAction::Finally)) => {
                    let frame = self.try_stack.remove(index);
                    let env = self.lexical_env.clone();
                    let depth = self.env_stack.len();
                    self.pending = Some(PendingControl::Throw { value, env, depth });
                    self.restore_env(frame.saved_env, frame.env_depth);
                    let finally = body
                        .handlers
                        .get(frame.handler)
                        .and_then(|h| h.finally)
                        .unwrap_or(self.ip);
                    self.ip = finally;
                    return Ok(CtlResult::Continue);
                }
                Some((index, ThrowAction::Pop)) => {
                    self.try_stack.remove(index);
                }
                None => {
                    self.close_for_of_throw(agent);
                    return Ok(CtlResult::Done(VmOutcome::Completed(Completion::Throw(
                        value,
                    ))));
                }
            }
        }
    }

    /// Convert an engine error into a thrown value and route it through the
    /// body's handler table. `yield*` protocol errors (GetIterator, inner
    /// `next`/`throw`/`return` errors) are catchable by the generator body
    /// (spec 15.5.5 uses `?` on every step).
    fn throw_js_error(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        error: JsError,
    ) -> Result<CtlResult, JsError> {
        let thrown = crate::promise::error_value(agent, &error);
        self.throw_machinery(agent, body, thrown)
    }
}

#[derive(Debug, Clone, Copy)]
enum ThrowAction {
    Catch,
    Finally,
    Pop,
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn is_nullish(value: &Value) -> bool {
    matches!(value.kind(), ValueKind::Undefined | ValueKind::Null)
}

/// Split an array/object element into its target and optional default
/// initializer (`[x = init]` / `{x = init}` become an `Assign` node).
fn unwrap_default(expr: &Expr) -> (&Expr, Option<&Expr>) {
    match &expr.kind {
        ExprKind::Assign {
            op: AssignOp::Assign,
            target,
            value: initializer,
        } => (target.as_ref(), Some(initializer.as_ref())),
        _ => (expr, None),
    }
}

/// Whether a binding pattern or a declaration initializer contains a
/// suspension point: a default initializer, computed key, or nested pattern
/// with `yield`/`await`. Such declarations must be compiled to steps (the
/// runtime `binding_initialization` evaluates defaults through the
/// synchronous tree-walker, which cannot suspend).
fn binding_pattern_contains_suspension_any(pattern: &BindingPattern, init: Option<&Expr>) -> bool {
    init.is_some_and(expr_contains_suspension)
        || match pattern {
            BindingPattern::Ident(_) => false,
            BindingPattern::Object(props) => props.iter().any(|prop| match prop {
                syntax::ast::ObjectBindingProperty::Property { key, element, .. } => {
                    pattern_element_contains_suspension(element)
                        || matches!(key, PropertyName::Computed(e) if expr_contains_suspension(e))
                }
                syntax::ast::ObjectBindingProperty::Rest(element) => {
                    pattern_element_contains_suspension(element)
                }
            }),
            BindingPattern::Array(elements) => elements.iter().any(|element| match element {
                ArrayBindingElement::Hole => false,
                ArrayBindingElement::Element(e) | ArrayBindingElement::Rest(e) => {
                    pattern_element_contains_suspension(e)
                }
            }),
        }
}

/// Whether a binding element (pattern + optional default) contains a
/// suspension point, either in the default initializer or in a nested
/// pattern's defaults/computed keys.
fn pattern_element_contains_suspension(element: &BindingElement) -> bool {
    binding_pattern_contains_suspension_any(&element.pattern, element.init.as_ref())
}

fn nullish_error(what: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, what.into())
}

/// The UTF-16 units of a value converted to a string; used by the string
/// concatenation steps, which must preserve lone surrogates (a lossy UTF-8
/// round-trip would replace them with U+FFFD).
fn string_units_of(value: &Value) -> Vec<u16> {
    match value.kind() {
        ValueKind::String(s) => s.as_slice().to_vec(),
        _ => crux::convert::to_string(value)
            .map(|s| s.as_slice().to_vec())
            .unwrap_or_else(|_| vec![]),
    }
}

fn error_message_value(error: &JsError) -> Value {
    Value::String(Handle::new(JsString::from_utf8(&error.message)))
}

fn update_value(_agent: &mut Agent, op: &UpdateOp, old: &Value) -> Result<Value, JsError> {
    let old_numeric = crux::convert::to_numeric(old)?;
    match old_numeric.kind() {
        ValueKind::Number(n) => {
            let delta = if matches!(op, UpdateOp::Increment) {
                1.0
            } else {
                -1.0
            };
            Ok(Value::Number(n + delta))
        }
        ValueKind::BigInt(b) => {
            let one = crux::BigInt::from(1i64);
            let delta = if matches!(op, UpdateOp::Increment) {
                one
            } else {
                crux::bigint::unary_minus(&one)
            };
            Ok(Value::BigInt(Handle::new(crux::bigint::add(&b, &delta))))
        }
        _ => unreachable!(),
    }
}

fn member_reference(
    object: &Value,
    key: &PropertyKeyName,
    strict: bool,
) -> crate::context::Reference {
    let name = match key {
        PropertyKeyName::Name(id) => PropertyKey::from_js_string(&crux::lookup(*id)),
        PropertyKeyName::Key(key) => key.clone(),
    };
    crate::context::Reference {
        base: crate::context::ReferenceBase::Value(object.clone()),
        name,
        strict,
        this_value: None,
        private_name: None,
    }
}

fn super_reference(
    base: &Value,
    key: &PropertyKeyName,
    strict: bool,
    this: &Value,
) -> crate::context::Reference {
    let name = match key {
        PropertyKeyName::Name(id) => PropertyKey::from_js_string(&crux::lookup(*id)),
        PropertyKeyName::Key(key) => key.clone(),
    };
    crate::context::Reference {
        base: crate::context::ReferenceBase::Value(base.clone()),
        name,
        strict,
        this_value: Some(this.clone()),
        private_name: None,
    }
}

fn is_eval_function(agent: &Agent, value: &Value) -> Result<bool, JsError> {
    let realm = agent.current_realm()?;
    Ok(realm.intrinsics.get("%eval%").as_ref() == Some(value))
}

fn array_set(array: &Value, key: &str, value: Value) -> Result<(), JsError> {
    let ValueKind::Object(obj) = array.kind() else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    obj.create_data_property(&JsString::from_utf8(key), value)?;
    Ok(())
}

/// Object literal `Init` property definition (spec 13.2.5.5 step 4): the
/// `__proto__` prototype-setter special case plus name inference.
fn object_init(
    object: &Value,
    key: &PropertyName,
    value: Value,
    set_name: bool,
    shorthand: bool,
) -> Result<(), JsError> {
    let ValueKind::Object(obj) = object.kind() else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let name = match key {
        PropertyName::Ident(id) => crux::lookup(*id),
        PropertyName::Str(text) => text.clone(),
        PropertyName::Number(n) => crux::convert::to_string(&Value::Number(*n))?,
        PropertyName::Computed(_) => {
            unreachable!("computed keys use the ObjectInitComputed step")
        }
    };
    if set_name {
        crate::function::set_function_name(&value, &name, None)?;
    }
    let name_text = name.to_string_lossy();
    if !shorthand && name_text == "__proto__" {
        match value.kind() {
            ValueKind::Object(proto) => {
                if !obj.set_prototype_of(Some(proto))? {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set prototype of non-extensible object".into(),
                    ));
                }
            }
            ValueKind::Null if !obj.set_prototype_of(None)? => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot set prototype of non-extensible object".into(),
                ));
            }
            // B.3.1 step 6: a non-object, non-null value sets neither the
            // prototype nor an own property — the definition is a no-op.
            _ => {}
        }
    } else {
        obj.create_data_property(&name, value)?;
    }
    Ok(())
}

/// A computed-key `Init` property definition (spec 13.2.5.5 step 4): a
/// computed key is never the `__proto__` prototype setter, and SetFunctionName
/// uses the key's display form (a symbol's bracketed description).
fn object_init_key(
    obj: &crux::object::JsObject,
    key: PropertyKey,
    value: Value,
    set_name: bool,
) -> Result<(), JsError> {
    if set_name {
        crate::function::set_function_name(&value, &crate::expr::property_key_display(&key), None)?;
    }
    obj.create_data_property_key(&key, value)?;
    Ok(())
}

/// MethodDefinition evaluation (spec 15.4.3) for the IR object literal steps.
fn object_method(
    agent: &mut Agent,
    object: &Value,
    key: PropertyKey,
    function: &Function,
) -> Result<(), JsError> {
    let ValueKind::Object(obj) = object.kind() else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let env = agent.running_context()?.lexical_environment.clone();
    let closure = crate::function::instantiate_method(agent, function, env, false)?;
    crate::function::make_method(agent, &closure, Value::Object(obj.clone()))?;
    crate::function::set_function_name(&closure, &crate::expr::property_key_display(&key), None)?;
    obj.create_data_property_key(&key, closure)?;
    Ok(())
}

/// Accessor definition (get/set PropertyDefinition) for the IR steps.
#[allow(clippy::too_many_arguments)]
fn object_accessor(
    agent: &mut Agent,
    object: &Value,
    key: PropertyKey,
    get: bool,
    param: Option<&BindingElement>,
    body: &syntax::ast::Block,
) -> Result<(), JsError> {
    let ValueKind::Object(obj) = object.kind() else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let env = agent.running_context()?.lexical_environment.clone();
    let params = if let Some(param) = param {
        vec![param.clone()]
    } else {
        Vec::new()
    };
    let closure = crate::function::instantiate_accessor(agent, params, body.clone(), env, false)?;
    crate::function::make_method(agent, &closure, Value::Object(obj.clone()))?;
    let prefix = if get { Some("get") } else { Some("set") };
    crate::function::set_function_name(&closure, &crate::expr::property_key_display(&key), prefix)?;
    let descriptor = crux::property::PropertyDescriptor {
        value: None,
        writable: None,
        get: if get { Some(closure.clone()) } else { None },
        set: if get { None } else { Some(closure) },
        enumerable: Some(true),
        configurable: Some(true),
    };
    obj.define_property_key(&key, &descriptor)?;
    Ok(())
}

/// TaggedTemplate evaluation (spec 13.3.6.2) for the IR step.
fn tagged_template(
    agent: &mut Agent,
    tag: Value,
    template: &syntax::ast::TemplateLiteral,
    substitutions: Vec<Value>,
) -> Result<Value, JsError> {
    if !is_callable(&tag) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", crux::value::type_of(&tag)),
        ));
    }
    let template_object =
        crate::builtins::array::array_create(agent, template.quasis.len() as f64)?;
    let raw = crate::builtins::array::array_create(agent, template.quasis.len() as f64)?;
    for (index, quasi) in template.quasis.iter().enumerate() {
        // A quasi whose TV is undefined (an invalid escape sequence) yields
        // the value *undefined*, not a string (spec 12.2.9.3).
        let cooked = match quasi.cooked.clone() {
            Some(cooked) => Value::String(Handle::new(cooked)),
            None => Value::Undefined,
        };
        template_object.create_data_property(&JsString::from_utf8(&index.to_string()), cooked)?;
        raw.create_data_property(
            &JsString::from_utf8(&index.to_string()),
            Value::String(Handle::new(quasi.raw.clone())),
        )?;
    }
    template_object.create_data_property(&JsString::from_utf8("raw"), Value::Object(raw))?;
    let mut args = vec![Value::Object(template_object)];
    args.extend(substitutions);
    crate::function::call(agent, &tag, Value::Undefined, &args)
}

fn iterator_result_done(agent: &mut Agent, result: &Value) -> Result<bool, JsError> {
    let done =
        crate::context::get_property(agent, result, &JsString::from_utf8("done"), result.clone())?;
    Ok(crux::convert::to_boolean(&done))
}

fn iterator_result_value(agent: &mut Agent, result: &Value) -> Result<Value, JsError> {
    crate::context::get_property(agent, result, &JsString::from_utf8("value"), result.clone())
}

/// GetIterator with the ~async~ hint for an async-generator `yield*` (spec
/// 14.5.5 step 1): `@@asyncIterator` when present, else the sync iterator
/// wrapped as an async-from-sync iterator.
fn get_async_iterator_record(
    agent: &mut Agent,
    value: &Value,
) -> Result<crate::expr::IteratorRecord, JsError> {
    let method = crate::expr::get_method(agent, value, "@@asyncIterator")?;
    if let Some(method) = method {
        let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
        if !matches!(iterator.kind(), ValueKind::Object(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "async iterator must be an object".into(),
            ));
        }
        let next = crate::context::get_property(
            agent,
            &iterator,
            &JsString::from_utf8("next"),
            iterator.clone(),
        )?;
        if !crux::value::is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "async iterator has no callable next".into(),
            ));
        }
        return Ok(crate::expr::IteratorRecord { iterator, next });
    }
    let sync = crate::expr::get_iterator(agent, value)?;
    let object = crate::async_await::async_from_sync_iterator(agent, &sync)?;
    let iterator = Value::Object(object);
    let next = crate::context::get_property(
        agent,
        &iterator,
        &JsString::from_utf8("next"),
        iterator.clone(),
    )?;
    Ok(crate::expr::IteratorRecord { iterator, next })
}

/// GetAsyncIterator (spec 27.1.1.2): the async iterator, wrapping sync
/// iterators in an AsyncFromSyncIterator.
fn async_from_sync_or_async(
    agent: &mut Agent,
    value: &Value,
) -> Result<crate::expr::IteratorRecord, JsError> {
    let async_method = crate::expr::get_method(agent, value, "@@asyncIterator")?;
    if let Some(method) = async_method {
        let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
        let next = crate::context::get_property(
            agent,
            &iterator,
            &JsString::from_utf8("next"),
            iterator.clone(),
        )?;
        if !is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Async iterator's next method is not callable".into(),
            ));
        }
        return Ok(crate::expr::IteratorRecord { iterator, next });
    }
    // Wrap the sync iterator.
    let sync = get_iterator(agent, value)?;
    let wrapped = crate::async_await::async_from_sync_iterator(agent, &sync)?;
    let next = crate::context::get_property(
        agent,
        &Value::Object(wrapped.clone()),
        &JsString::from_utf8("next"),
        Value::Object(wrapped.clone()),
    )?;
    Ok(crate::expr::IteratorRecord {
        iterator: Value::Object(wrapped),
        next,
    })
}

/// Rebuild a binding pattern from a destructuring assignment target.
fn pattern_of_target(target: &Expr) -> Result<BindingPattern, JsError> {
    match &target.kind {
        ExprKind::Object(literal) => {
            let mut properties = Vec::new();
            for prop in &literal.props {
                match prop {
                    ObjectProperty::Init { key, value, .. } => {
                        let element = binding_element_of_target(value)?;
                        properties.push(syntax::ast::ObjectBindingProperty::Property {
                            key: key.clone(),
                            element,
                            span: target.span,
                        });
                    }
                    ObjectProperty::Spread(expr) => {
                        let element = pattern_of_target(expr)?;
                        properties.push(syntax::ast::ObjectBindingProperty::Rest(BindingElement {
                            pattern: element,
                            init: None,
                            rest: true,
                            span: target.span,
                        }));
                    }
                    _ => {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "Invalid destructuring assignment target".into(),
                        ));
                    }
                }
            }
            Ok(BindingPattern::Object(properties))
        }
        ExprKind::Array(literal) => {
            let mut elements = Vec::new();
            for element in &literal.elements {
                match element {
                    ArrayElement::Hole => elements.push(syntax::ast::ArrayBindingElement::Hole),
                    ArrayElement::Expr(expr) => {
                        elements.push(syntax::ast::ArrayBindingElement::Element(
                            binding_element_of_target(expr)?,
                        ));
                    }
                    ArrayElement::Spread(expr) => {
                        let pattern = pattern_of_target(expr)?;
                        elements.push(syntax::ast::ArrayBindingElement::Rest(BindingElement {
                            pattern,
                            init: None,
                            rest: true,
                            span: target.span,
                        }));
                    }
                }
            }
            Ok(BindingPattern::Array(elements))
        }
        ExprKind::Ident(id) => {
            let name = crux::lookup(*id);
            let atom = crux::intern(name.as_slice());
            Ok(BindingPattern::Ident(atom))
        }
        ExprKind::Paren(inner) => pattern_of_target(inner),
        ExprKind::Member(member) => {
            // Member expressions are valid assignment targets too, but
            // destructuring a member target needs the reference machinery;
            // the tree-walker's binding_initialization with `None` handles
            // them via PutValue. Represent as an identifier-less pattern is
            // not possible; fall back to evaluating the pattern directly.
            let _ = member;
            Err(JsError::new(
                ErrorKind::SyntaxError,
                "Member destructuring assignment targets are not supported in resumable bodies"
                    .into(),
            ))
        }
        _ => Err(JsError::new(
            ErrorKind::SyntaxError,
            "Invalid destructuring assignment target".into(),
        )),
    }
}

/// A destructuring assignment element: an assignment expression `x = 1` is a
/// default initializer (spec 13.15.5: BindingInitialization of `x` with
/// default `1`), anything else is the plain target pattern.
fn binding_element_of_target(expr: &Expr) -> Result<BindingElement, JsError> {
    if let ExprKind::Assign {
        target,
        value,
        op: AssignOp::Assign,
    } = &expr.kind
    {
        Ok(BindingElement {
            pattern: pattern_of_target(target)?,
            init: Some((**value).clone()),
            rest: false,
            span: expr.span,
        })
    } else {
        Ok(BindingElement {
            pattern: pattern_of_target(expr)?,
            init: None,
            rest: false,
            span: expr.span,
        })
    }
}

// ---------------------------------------------------------------------------
// Suspension detection
// ---------------------------------------------------------------------------

/// Whether an expression contains a suspension point (`yield`/`await`) or a
/// construct the VM must linearize. Nested function/class bodies are separate
/// resumable units and never count.
pub fn expr_contains_suspension(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Yield { .. } | ExprKind::Await(_) => true,
        ExprKind::Function(_) | ExprKind::Arrow { .. } => false,
        ExprKind::Class(class) => class_contains_suspension(class),
        ExprKind::Unary { operand, .. } => expr_contains_suspension(operand),
        ExprKind::Update { target, .. } => expr_contains_suspension(target),
        ExprKind::Binary { left, right, .. } => {
            expr_contains_suspension(left) || expr_contains_suspension(right)
        }
        ExprKind::Logical { left, right, .. } => {
            expr_contains_suspension(left) || expr_contains_suspension(right)
        }
        ExprKind::Assign { target, value, .. } => {
            expr_contains_suspension(target) || expr_contains_suspension(value)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            expr_contains_suspension(test)
                || expr_contains_suspension(consequent)
                || expr_contains_suspension(alternate)
        }
        ExprKind::PrivateIn { object, .. } => expr_contains_suspension(object),
        ExprKind::Call(call) => {
            expr_contains_suspension(&call.callee)
                || call.args.iter().any(|a| match a {
                    Argument::Expr(e) => expr_contains_suspension(e),
                    Argument::Spread(e) => expr_contains_suspension(e),
                })
        }
        ExprKind::New(new) => {
            expr_contains_suspension(&new.callee)
                || new.args.iter().any(|a| match a {
                    Argument::Expr(e) => expr_contains_suspension(e),
                    Argument::Spread(e) => expr_contains_suspension(e),
                })
        }
        ExprKind::Member(member) => {
            expr_contains_suspension(&member.object)
                || matches!(&member.property, MemberProperty::Computed(e) if expr_contains_suspension(e))
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            expr_contains_suspension(tag) || quasi.exprs.iter().any(expr_contains_suspension)
        }
        ExprKind::Template(template) => template.exprs.iter().any(expr_contains_suspension),
        ExprKind::Paren(inner) => expr_contains_suspension(inner),
        ExprKind::Sequence(exprs) => exprs.iter().any(expr_contains_suspension),
        ExprKind::Array(literal) => literal.elements.iter().any(|e| match e {
            ArrayElement::Expr(expr) => expr_contains_suspension(expr),
            ArrayElement::Spread(expr) => expr_contains_suspension(expr),
            ArrayElement::Hole => false,
        }),
        ExprKind::Object(literal) => literal.props.iter().any(object_prop_contains_suspension),
        ExprKind::ImportCall { .. } => true,
        ExprKind::MetaProperty { .. } => true,
        _ => false,
    }
}

/// Whether a class definition's heritage or a computed element name contains a
/// suspension point. Field initializer values run at construction (not class
/// definition) and do not count; element bodies are separate resumable units.
fn class_contains_suspension(class: &Class) -> bool {
    class
        .heritage
        .as_ref()
        .is_some_and(expr_contains_suspension)
        || class
            .elements
            .iter()
            .any(class_element_name_contains_suspension)
}

fn class_element_name_contains_suspension(element: &ClassElement) -> bool {
    let name = match element {
        ClassElement::Method { name, .. }
        | ClassElement::Get { name, .. }
        | ClassElement::Set { name, .. }
        | ClassElement::Field { name, .. } => name,
        ClassElement::StaticBlock(_) => return false,
    };
    matches!(
        name,
        ClassElementName::Property(PropertyName::Computed(expr)) if expr_contains_suspension(expr)
    )
}

/// The computed public name expression of a class element, if any.
fn computed_public_name(element: &ClassElement) -> Option<&Expr> {
    let name = match element {
        ClassElement::Method { name, .. }
        | ClassElement::Get { name, .. }
        | ClassElement::Set { name, .. }
        | ClassElement::Field { name, .. } => name,
        ClassElement::StaticBlock(_) => return None,
    };
    match name {
        ClassElementName::Property(PropertyName::Computed(expr)) => Some(expr),
        _ => None,
    }
}

fn has_computed_public_name(element: &ClassElement) -> bool {
    computed_public_name(element).is_some()
}

fn object_prop_contains_suspension(prop: &ObjectProperty) -> bool {
    match prop {
        ObjectProperty::Init { key, value, .. } => {
            property_name_contains_suspension(key) || expr_contains_suspension(value)
        }
        ObjectProperty::Method { key, .. } => property_name_contains_suspension(key),
        ObjectProperty::Get { key, body } | ObjectProperty::Set { key, body, .. } => {
            property_name_contains_suspension(key)
                || body.stmts.iter().any(stmt_contains_suspension)
        }
        ObjectProperty::Spread(expr) => expr_contains_suspension(expr),
    }
}

fn property_name_contains_suspension(key: &PropertyName) -> bool {
    match key {
        PropertyName::Computed(expr) => expr_contains_suspension(expr),
        _ => false,
    }
}

/// Whether a statement contains a suspension point anywhere in its subtree.
pub fn stmt_contains_suspension(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Block(block) => block.stmts.iter().any(stmt_contains_suspension),
        StmtKind::Expr(expr) | StmtKind::Throw(expr) => expr_contains_suspension(expr),
        StmtKind::Return(Some(expr)) => expr_contains_suspension(expr),
        StmtKind::If {
            test,
            consequent,
            alternate,
        } => {
            expr_contains_suspension(test)
                || stmt_contains_suspension(consequent)
                || alternate.as_deref().is_some_and(stmt_contains_suspension)
        }
        StmtKind::While { test, body } | StmtKind::DoWhile { body, test } => {
            expr_contains_suspension(test) || stmt_contains_suspension(body)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_ref().is_some_and(for_init_contains_suspension)
                || test.as_ref().is_some_and(expr_contains_suspension)
                || update.as_ref().is_some_and(expr_contains_suspension)
                || stmt_contains_suspension(body)
        }
        StmtKind::ForIn { left, right, body } => {
            for_binding_contains_suspension(left)
                || expr_contains_suspension(right)
                || stmt_contains_suspension(body)
        }
        StmtKind::ForOf {
            left,
            right,
            body,
            is_await,
        } => {
            *is_await
                || for_binding_contains_suspension(left)
                || expr_contains_suspension(right)
                || stmt_contains_suspension(body)
        }
        StmtKind::Labeled { body, .. } => stmt_contains_suspension(body),
        StmtKind::Switch {
            discriminant,
            cases,
        } => {
            expr_contains_suspension(discriminant)
                || cases.iter().any(|c| {
                    c.test.as_ref().is_some_and(expr_contains_suspension)
                        || c.consequent.iter().any(stmt_contains_suspension)
                })
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            block.stmts.iter().any(stmt_contains_suspension)
                || handler
                    .as_ref()
                    .is_some_and(|h| h.body.stmts.iter().any(stmt_contains_suspension))
                || finalizer
                    .as_ref()
                    .is_some_and(|f| f.stmts.iter().any(stmt_contains_suspension))
        }
        StmtKind::ClassDecl(class) => class_contains_suspension(class),
        StmtKind::VarDecl { decls, .. } => decls
            .iter()
            .any(|d| binding_pattern_contains_suspension_any(&d.pattern, d.init.as_ref())),
        // An `await using` statement always implies an await (even with a
        // null initializer, spec 9.4.4), and its scope's async disposal must
        // suspend: compile it (and any enclosing scope) instead of batching
        // it to the tree walker.
        StmtKind::UsingDecl { is_await: true, .. } => true,
        StmtKind::UsingDecl { decls, .. } => decls
            .iter()
            .any(|d| binding_pattern_contains_suspension_any(&d.pattern, d.init.as_ref())),
        StmtKind::With { object, body } => {
            expr_contains_suspension(object) || stmt_contains_suspension(body)
        }
        _ => false,
    }
}

fn for_init_contains_suspension(init: &ForInit) -> bool {
    match init {
        ForInit::Expr(expr) => expr_contains_suspension(expr),
        ForInit::VarDecl { decls, .. } => decls
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_contains_suspension)),
    }
}

/// Whether an expression contains a `?.` link anywhere: a short-circuit then
/// propagates through the whole chain, so compiled member/call steps after
/// the link must be guarded (spec 13.4.3).
fn expr_may_short_circuit(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Member(member) => member.optional || expr_may_short_circuit(&member.object),
        ExprKind::Call(call) => call.optional || expr_may_short_circuit(&call.callee),
        ExprKind::Paren(inner) => expr_may_short_circuit(inner),
        ExprKind::New(new) => expr_may_short_circuit(&new.callee),
        ExprKind::TaggedTemplate { tag, .. } => expr_may_short_circuit(tag),
        _ => false,
    }
}

fn for_binding_contains_suspension(binding: &ForBinding) -> bool {
    match binding {
        ForBinding::Expr(expr) => expr_contains_suspension(expr),
        ForBinding::VarDecl { init, .. } => init.as_ref().is_some_and(expr_contains_suspension),
    }
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Fixup {
    Jump(usize, usize),
    JumpIfFalse(usize, usize),
    JumpIfTrue(usize, usize),
    JumpIfFalseKeep(usize, usize),
    JumpIfTrueKeep(usize, usize),
    JumpIfNullishKeep(usize, usize),
    JumpIfNotNullishKeep(usize, usize),
    Break(usize, usize),
    Continue(usize, usize),
    JumpIfChainShort(usize, usize),
    Exit(usize, usize),
    ForInNext(usize, usize),
    ForOfNext(usize, usize),
    DestructureUndef(usize, usize),
    AsyncForOfNext(usize, usize),
    SwitchTest(usize, usize),
    YieldStarNext(usize, usize),
    YieldStarResume(usize, usize, usize, usize),
    AsyncYieldStarNext(usize, usize),
    AsyncYieldStarInspect(usize, usize),
    AsyncYieldStarResume(usize, usize, usize, usize),
}

#[derive(Debug)]
enum Scope {
    Loop {
        break_target: usize,
        break_count: usize,
        continue_target: usize,
        continue_count: usize,
    },
    Switch {
        break_target: usize,
        break_count: usize,
    },
    Label {
        name: crux::AtomId,
        break_target: usize,
        break_count: usize,
        continue_target: Option<(usize, usize)>,
    },
    Iteration {
        open: usize,
        closed: usize,
        continue_target: usize,
        break_target: usize,
    },
}

#[derive(Default)]
struct Compiler {
    steps: Vec<Step>,
    handlers: Vec<Handler>,
    labels: HashMap<usize, usize>,
    fixups: Vec<Fixup>,
    scope_stack: Vec<Scope>,
    scope_count: usize,
    next_label: usize,
    /// Whether the enclosing function is an async generator: `yield*` then
    /// delegates through the async-iterator protocol with awaited results.
    is_async_generator: bool,
    /// The nesting depth of optional chains being compiled: the outermost
    /// chain node clears the runtime short-circuit flag when it finishes.
    chain_depth: usize,
}

impl Compiler {
    fn new_label(&mut self) -> usize {
        let label = self.next_label;
        self.next_label += 1;
        label
    }

    fn place(&mut self, label: usize) {
        self.labels.insert(label, self.steps.len());
    }

    fn emit(&mut self, step: Step) {
        self.steps.push(step);
    }

    fn jump(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::Jump(0));
        self.fixups.push(Fixup::Jump(index, target));
    }

    fn jump_if_false(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfFalse(0));
        self.fixups.push(Fixup::JumpIfFalse(index, target));
    }

    fn jump_if_true(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfTrue(0));
        self.fixups.push(Fixup::JumpIfTrue(index, target));
    }

    fn jump_if_false_keep(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfFalseKeep(0));
        self.fixups.push(Fixup::JumpIfFalseKeep(index, target));
    }

    fn jump_if_true_keep(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfTrueKeep(0));
        self.fixups.push(Fixup::JumpIfTrueKeep(index, target));
    }

    fn jump_if_nullish_keep(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfNullishKeep(0));
        self.fixups.push(Fixup::JumpIfNullishKeep(index, target));
    }

    fn jump_if_chain_short(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfChainShort(0));
        self.fixups.push(Fixup::JumpIfChainShort(index, target));
    }

    /// Enter an optional-chain node: the outermost node of a chain that may
    /// short-circuit emits the runtime clear when it finishes.
    fn enter_chain(&mut self, expr: &Expr) {
        if expr_may_short_circuit(expr) {
            self.chain_depth += 1;
        }
    }

    fn leave_chain(&mut self) {
        if self.chain_depth > 0 {
            self.chain_depth -= 1;
            if self.chain_depth == 0 {
                self.emit(Step::ClearChainShort);
            }
        }
    }

    /// Compile a member property access, skipping it when an upstream `?.`
    /// short-circuited (the chain value is already `undefined` on the stack,
    /// spec 13.4.3).
    fn compile_member_property_guarded(
        &mut self,
        member: &syntax::ast::MemberExpr,
    ) -> Result<(), JsError> {
        if expr_may_short_circuit(&member.object) {
            let end = self.new_label();
            self.jump_if_chain_short(end);
            self.compile_member_property(member)?;
            self.place(end);
        } else {
            self.compile_member_property(member)?;
        }
        Ok(())
    }

    /// Compile a call's arguments and the call step, skipping both when an
    /// upstream `?.` short-circuited: the whole chain is `undefined` (spec
    /// 13.4.3).
    fn compile_call_args_guarded(
        &mut self,
        args: &[Argument],
        direct_eval: bool,
    ) -> Result<(), JsError> {
        let short = self.new_label();
        let end = self.new_label();
        self.jump_if_chain_short(short);
        self.compile_arguments(args)?;
        self.emit(Step::Call { direct_eval });
        self.jump(end);
        self.place(short);
        self.emit(Step::Pop);
        self.emit(Step::Pop);
        self.emit(Step::Push(Value::Undefined));
        self.place(end);
        Ok(())
    }

    fn jump_if_not_nullish_keep(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfNotNullishKeep(0));
        self.fixups.push(Fixup::JumpIfNotNullishKeep(index, target));
    }

    fn emit_destructure_next(&mut self) {
        self.steps.push(Step::DestructureNext);
    }

    fn emit_destructure_undef(&mut self, use_default: usize) {
        let index = self.steps.len();
        self.steps.push(Step::DestructureUndef { use_default: 0 });
        self.fixups
            .push(Fixup::DestructureUndef(index, use_default));
    }

    fn leave_scopes(&mut self, count: usize) {
        for _ in 0..count {
            self.emit(Step::LeaveBlock);
        }
    }

    /// A label scoping a loop defers its continue target to the loop with a
    /// sentinel (see `labeled_continue_target`): when the loop's scope is
    /// pushed, resolve any sentinel targets in the label scopes directly
    /// beneath it (an intermediate non-loop label or other scope stops the
    /// walk).
    fn resolve_labeled_continue(&mut self, continue_target: usize, continue_count: usize) {
        // The loop's own scope is on top; the labels it resolves are beneath.
        for scope in self.scope_stack.iter_mut().rev().skip(1) {
            match scope {
                Scope::Label {
                    continue_target: target,
                    ..
                } => {
                    if let Some((usize::MAX, _)) = target {
                        *target = Some((continue_target, continue_count));
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
    }

    /// Whether a for-in/for-of head needs its lexical names in TDZ while the
    /// RHS evaluates, and if so emit the TDZ environment (spec 14.7.5.4 /
    /// 14.7.6.1).
    fn enter_iter_tdz_env(&mut self, left: &ForBinding) -> bool {
        if let ForBinding::VarDecl { kind, pattern, .. } = left
            && *kind != VarDeclKind::Var
        {
            let mut names = Vec::new();
            crate::script::bound_names(pattern, &mut names);
            if !names.is_empty() {
                self.emit(Step::EnterIterTdzEnv { names });
                return true;
            }
        }
        false
    }

    fn leave_iter_tdz_env(&mut self, entered: bool) {
        if entered {
            self.emit(Step::LeaveIterTdzEnv);
        }
    }

    fn resolve(&mut self) {
        for fixup in std::mem::take(&mut self.fixups) {
            match fixup {
                Fixup::Jump(index, label) => {
                    self.steps[index] = Step::Jump(self.labels[&label]);
                }
                Fixup::JumpIfFalse(index, label) => {
                    self.steps[index] = Step::JumpIfFalse(self.labels[&label]);
                }
                Fixup::JumpIfTrue(index, label) => {
                    self.steps[index] = Step::JumpIfTrue(self.labels[&label]);
                }
                Fixup::JumpIfFalseKeep(index, label) => {
                    self.steps[index] = Step::JumpIfFalseKeep(self.labels[&label]);
                }
                Fixup::JumpIfTrueKeep(index, label) => {
                    self.steps[index] = Step::JumpIfTrueKeep(self.labels[&label]);
                }
                Fixup::JumpIfNullishKeep(index, label) => {
                    self.steps[index] = Step::JumpIfNullishKeep(self.labels[&label]);
                }
                Fixup::JumpIfNotNullishKeep(index, label) => {
                    self.steps[index] = Step::JumpIfNotNullishKeep(self.labels[&label]);
                }
                Fixup::Break(index, label) => {
                    self.steps[index] = Step::Break {
                        target: self.labels[&label],
                    };
                }
                Fixup::Continue(index, label) => {
                    self.steps[index] = Step::Continue {
                        target: self.labels[&label],
                    };
                }
                Fixup::JumpIfChainShort(index, label) => {
                    self.steps[index] = Step::JumpIfChainShort(self.labels[&label]);
                }
                Fixup::Exit(index, label) => {
                    self.steps[index] = Step::Exit {
                        after: self.labels[&label],
                    };
                }
                Fixup::ForInNext(index, label) => {
                    self.steps[index] = Step::ForInNext {
                        done: self.labels[&label],
                    };
                }
                Fixup::ForOfNext(index, label) => {
                    self.steps[index] = Step::ForOfNext {
                        done: self.labels[&label],
                    };
                }
                Fixup::DestructureUndef(index, label) => {
                    self.steps[index] = Step::DestructureUndef {
                        use_default: self.labels[&label],
                    };
                }
                Fixup::AsyncForOfNext(index, label) => {
                    self.steps[index] = Step::AsyncForOfTest {
                        done: self.labels[&label],
                    };
                }
                Fixup::SwitchTest(index, label) => {
                    self.steps[index] = Step::SwitchTest {
                        case: self.labels[&label],
                    };
                }
                Fixup::YieldStarNext(index, label) => {
                    self.steps[index] = Step::YieldStarNext {
                        done: self.labels[&label],
                    };
                }
                Fixup::YieldStarResume(index, loop_label, done_label, yield_label) => {
                    self.steps[index] = Step::YieldStarResume {
                        loop_top: self.labels[&loop_label],
                        done: self.labels[&done_label],
                        yield_at: self.labels[&yield_label],
                    };
                }
                Fixup::AsyncYieldStarNext(index, label) => {
                    self.steps[index] = Step::AsyncYieldStarNext {
                        done: self.labels[&label],
                    };
                }
                Fixup::AsyncYieldStarInspect(index, label) => {
                    self.steps[index] = Step::AsyncYieldStarInspect {
                        done: self.labels[&label],
                    };
                }
                Fixup::AsyncYieldStarResume(index, loop_label, done_label, inspect_index) => {
                    self.steps[index] = Step::AsyncYieldStarResume {
                        loop_top: self.labels[&loop_label],
                        done: self.labels[&done_label],
                        inspect: inspect_index,
                    };
                }
            }
        }
    }

    fn break_target(&self, label: Option<&crux::AtomId>) -> Result<(usize, usize), JsError> {
        match label {
            None => {
                for scope in self.scope_stack.iter().rev() {
                    match scope {
                        Scope::Loop {
                            break_target,
                            break_count,
                            ..
                        } => return Ok((*break_target, *break_count)),
                        Scope::Switch {
                            break_target,
                            break_count,
                        } => return Ok((*break_target, *break_count)),
                        Scope::Iteration {
                            break_target,
                            closed,
                            ..
                        } => return Ok((*break_target, *closed)),
                        _ => {}
                    }
                }
                Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Illegal break statement".into(),
                ))
            }
            Some(label) => {
                for scope in self.scope_stack.iter().rev() {
                    if let Scope::Label {
                        name,
                        break_target,
                        break_count,
                        ..
                    } = scope
                        && name == label
                    {
                        return Ok((*break_target, *break_count));
                    }
                }
                Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Undefined label in break statement".into(),
                ))
            }
        }
    }

    fn continue_target(&self, label: Option<&crux::AtomId>) -> Result<(usize, usize), JsError> {
        match label {
            None => {
                for scope in self.scope_stack.iter().rev() {
                    match scope {
                        Scope::Loop {
                            continue_target,
                            continue_count,
                            ..
                        } => return Ok((*continue_target, *continue_count)),
                        Scope::Iteration {
                            continue_target,
                            open,
                            ..
                        } => return Ok((*continue_target, *open)),
                        _ => {}
                    }
                }
                Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Illegal continue statement".into(),
                ))
            }
            Some(label) => {
                // A labelled loop's continue target is the innermost loop
                // scope beneath the label.
                let mut found_label = false;
                for scope in self.scope_stack.iter().rev() {
                    match scope {
                        Scope::Label {
                            name,
                            continue_target,
                            ..
                        } => {
                            if name == label
                                && let Some((target, count)) = continue_target
                                && *target != usize::MAX
                            {
                                return Ok((*target, *count));
                            }
                            if name == label {
                                found_label = true;
                            }
                        }
                        Scope::Loop {
                            continue_target,
                            continue_count,
                            ..
                        } if found_label => {
                            return Ok((*continue_target, *continue_count));
                        }
                        Scope::Iteration {
                            continue_target,
                            open,
                            ..
                        } if found_label => {
                            return Ok((*continue_target, *open));
                        }
                        _ => {}
                    }
                }
                Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Undefined label in continue statement".into(),
                ))
            }
        }
    }
}

impl Compiler {
    fn compile_statements(&mut self, stmts: &[Stmt]) -> Result<(), JsError> {
        for stmt in stmts {
            self.compile_statement(stmt)?;
        }
        Ok(())
    }

    /// The declarations a block environment instantiates at entry:
    /// `block_declaration_instantiation` ignores every other statement kind.
    fn block_decls(stmts: &[Stmt]) -> Vec<Stmt> {
        stmts
            .iter()
            .filter(|stmt| match &stmt.kind {
                StmtKind::VarDecl { kind, .. } => *kind != VarDeclKind::Var,
                StmtKind::UsingDecl { .. } | StmtKind::ClassDecl(_) | StmtKind::FunctionDecl(_) => {
                    true
                }
                _ => false,
            })
            .cloned()
            .collect()
    }

    fn compile_statement(&mut self, stmt: &Stmt) -> Result<(), JsError> {
        match &stmt.kind {
            StmtKind::Block(block) => {
                self.emit(Step::ListBegin);
                self.emit(Step::EnterBlock {
                    decls: Self::block_decls(&block.stmts),
                });
                self.scope_count += 1;
                self.compile_statements(&block.stmts)?;
                self.scope_count -= 1;
                self.emit(Step::LeaveBlock);
                self.emit(Step::ListEnd);
            }
            StmtKind::Expr(expr) => {
                self.compile_expr(expr)?;
                self.emit(Step::SetCompletion);
            }
            StmtKind::VarDecl { kind, decls, .. } => {
                let declaration = *kind != VarDeclKind::Var;
                for decl in decls {
                    if let Some(init) = &decl.init {
                        // spec 14.3.2 step 2: a `var` identifier's binding is
                        // resolved before its initializer runs — a `with`
                        // object's property is the assignment target even
                        // when the initializer mutates the object.
                        if !declaration && let BindingPattern::Ident(name) = &decl.pattern {
                            self.emit(Step::ResolveVarIdent { name: *name });
                        }
                        self.compile_expr(init)?;
                        if let BindingPattern::Ident(name) = &decl.pattern
                            && crate::function::is_anonymous_function_definition(init)
                        {
                            // NamedEvaluation (spec 14.2.2 step 2.d): an
                            // anonymous function/class initializer is named
                            // after its binding.
                            self.emit(Step::SetFunctionName { name: *name });
                        }
                        if self.binding_pattern_contains_suspension(&decl.pattern) {
                            // Defaults/computed keys with `yield`/`await`
                            // compile inline so they can suspend.
                            self.compile_destructure_binding(&decl.pattern, declaration)?;
                        } else if !declaration && matches!(decl.pattern, BindingPattern::Ident(_)) {
                            self.emit(Step::PutVarReference);
                        } else {
                            let step = if declaration {
                                Step::DeclInit {
                                    pattern: decl.pattern.clone(),
                                }
                            } else {
                                Step::Destructure {
                                    pattern: decl.pattern.clone(),
                                }
                            };
                            self.emit(step);
                        }
                    } else if declaration {
                        // `let x;` initializes the binding to *undefined*
                        // (spec 14.2.1 step 4); the binding was created
                        // uninitialized by declaration instantiation.
                        self.emit(Step::Push(Value::Undefined));
                        self.emit(Step::DeclInit {
                            pattern: decl.pattern.clone(),
                        });
                    }
                }
            }
            StmtKind::UsingDecl { is_await, decls } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        self.compile_expr(init)?;
                        if let BindingPattern::Ident(name) = &decl.pattern
                            && crate::function::is_anonymous_function_definition(init)
                        {
                            // NamedEvaluation (spec 14.2.2 step 2.d): an
                            // anonymous function/class initializer is named
                            // after its binding.
                            self.emit(Step::SetFunctionName { name: *name });
                        }
                        self.emit(Step::UsingInit {
                            pattern: decl.pattern.clone(),
                            is_await: *is_await,
                        });
                    }
                }
            }
            StmtKind::Return(expr) => {
                match expr {
                    Some(expr) => self.compile_expr(expr)?,
                    None => self.emit(Step::Push(Value::Undefined)),
                }
                self.leave_scopes(self.scope_count);
                if self.is_async_generator {
                    // spec ReturnStatement evaluation: `return expr` in an
                    // async generator awaits the value before completing
                    // (AsyncGeneratorCompleteStep does not unwrap).
                    self.emit(Step::Await);
                }
                self.emit(Step::Return);
            }
            StmtKind::Throw(expr) => {
                self.compile_expr(expr)?;
                self.emit(Step::Throw);
            }
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                self.emit(Step::ResetCompletion);
                self.compile_expr(test)?;
                let else_label = self.new_label();
                let end_label = self.new_label();
                self.jump_if_false(else_label);
                self.compile_statement(consequent)?;
                self.jump(end_label);
                self.place(else_label);
                if let Some(alternate) = alternate {
                    self.compile_statement(alternate)?;
                }
                self.place(end_label);
                self.emit(Step::NormalizeCompletion);
            }
            StmtKind::While { test, body } => {
                self.emit(Step::ResetCompletion);
                let test_label = self.new_label();
                let end_label = self.new_label();
                let break_count = self.scope_count;
                let continue_count = self.scope_count;
                self.scope_stack.push(Scope::Loop {
                    break_target: end_label,
                    break_count,
                    continue_target: test_label,
                    continue_count,
                });
                self.resolve_labeled_continue(test_label, continue_count);
                self.place(test_label);
                self.compile_expr(test)?;
                self.jump_if_false(end_label);
                self.compile_statement(body)?;
                self.jump(test_label);
                self.place(end_label);
                self.emit(Step::NormalizeCompletion);
                self.scope_stack.pop();
            }
            StmtKind::DoWhile { body, test } => {
                self.emit(Step::ResetCompletion);
                let body_label = self.new_label();
                let test_label = self.new_label();
                let end_label = self.new_label();
                let break_count = self.scope_count;
                let continue_count = self.scope_count;
                self.scope_stack.push(Scope::Loop {
                    break_target: end_label,
                    break_count,
                    continue_target: test_label,
                    continue_count,
                });
                self.resolve_labeled_continue(test_label, continue_count);
                self.place(body_label);
                self.compile_statement(body)?;
                self.place(test_label);
                self.compile_expr(test)?;
                self.jump_if_true(body_label);
                self.place(end_label);
                self.emit(Step::NormalizeCompletion);
                self.scope_stack.pop();
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(init.as_ref(), test.as_ref(), update.as_ref(), body)?,
            StmtKind::ForIn { left, right, body } => {
                self.compile_for_in(left, right, body)?;
            }
            StmtKind::ForOf {
                left,
                right,
                body,
                is_await,
            } => {
                if *is_await {
                    self.compile_async_for_of(left, right, body)?;
                } else {
                    self.compile_for_of(left, right, body)?;
                }
            }
            StmtKind::Labeled { label, body } => {
                self.emit(Step::ResetCompletion);
                let break_target = self.new_label();
                let break_count = self.scope_count;
                let continue_target = labeled_continue_target(body);
                self.scope_stack.push(Scope::Label {
                    name: *label,
                    break_target,
                    break_count,
                    continue_target,
                });
                self.compile_statement(body)?;
                self.place(break_target);
                self.emit(Step::NormalizeCompletion);
                self.scope_stack.pop();
            }
            StmtKind::Break(label) => {
                let (target, count) = self.break_target(label.as_ref())?;
                self.leave_scopes(self.scope_count.saturating_sub(count));
                let index = self.steps.len();
                self.emit(Step::Break { target: 0 });
                self.fixups.push(Fixup::Break(index, target));
            }
            StmtKind::Continue(label) => {
                let (target, count) = self.continue_target(label.as_ref())?;
                self.leave_scopes(self.scope_count.saturating_sub(count));
                let index = self.steps.len();
                self.emit(Step::Continue { target: 0 });
                self.fixups.push(Fixup::Continue(index, target));
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => self.compile_switch(discriminant, cases)?,
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_ref())?,
            StmtKind::With { object, body } => {
                self.emit(Step::ResetCompletion);
                self.compile_expr(object)?;
                self.emit(Step::EnterWith);
                self.scope_count += 1;
                self.compile_statement(body)?;
                self.scope_count -= 1;
                self.emit(Step::LeaveBlock);
                self.emit(Step::NormalizeCompletion);
            }
            StmtKind::ClassDecl(class) => {
                // A class declaration whose heritage or computed names
                // suspend: evaluate the definition through the VM, then
                // initialize the declaration's binding (created uninitialized
                // by declaration instantiation).
                self.compile_class(class)?;
                if let Some(name) = class.name {
                    self.emit(Step::DeclInit {
                        pattern: BindingPattern::Ident(name),
                    });
                }
            }
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::FunctionDecl(_) => {
                self.emit(Step::FunctionDecl {
                    stmt: Box::new(stmt.clone()),
                });
            }
        }
        Ok(())
    }

    fn compile_for(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.emit(Step::ResetCompletion);
        let mut has_loop_env = false;
        let mut per_iteration: Vec<JsString> = Vec::new();
        match init {
            None => {}
            Some(ForInit::Expr(expr)) => {
                self.compile_expr(expr)?;
                self.emit(Step::Pop);
            }
            Some(ForInit::VarDecl { kind, decls }) => {
                if *kind == VarDeclKind::Var {
                    for decl in decls {
                        if let Some(init) = &decl.init {
                            self.compile_expr(init)?;
                            self.emit(Step::Destructure {
                                pattern: decl.pattern.clone(),
                            });
                        }
                    }
                } else {
                    self.emit(Step::EnterLoopEnv {
                        kind: *kind,
                        decls: decls.clone(),
                    });
                    self.scope_count += 1;
                    has_loop_env = true;
                    for decl in decls {
                        let mut names = Vec::new();
                        crate::script::bound_names(&decl.pattern, &mut names);
                        per_iteration.extend(names);
                    }
                }
            }
        }
        if !per_iteration.is_empty() {
            self.emit(Step::PerIteration {
                names: per_iteration.clone(),
            });
        }
        let test_label = self.new_label();
        let continue_label = self.new_label();
        let end_label = self.new_label();
        let break_count = if has_loop_env {
            self.scope_count - 1
        } else {
            self.scope_count
        };
        let continue_count = self.scope_count;
        self.scope_stack.push(Scope::Loop {
            break_target: end_label,
            break_count,
            continue_target: continue_label,
            continue_count,
        });
        self.resolve_labeled_continue(continue_label, continue_count);
        self.place(test_label);
        match test {
            Some(test) => self.compile_expr(test)?,
            None => self.emit(Step::Push(Value::Boolean(true))),
        }
        self.jump_if_false(end_label);
        self.compile_statement(body)?;
        self.place(continue_label);
        if !per_iteration.is_empty() {
            self.emit(Step::PerIteration {
                names: per_iteration.clone(),
            });
        }
        if let Some(update) = update {
            self.compile_expr(update)?;
            self.emit(Step::Pop);
        }
        self.jump(test_label);
        self.place(end_label);
        self.emit(Step::NormalizeCompletion);
        self.scope_stack.pop();
        if has_loop_env {
            self.scope_count -= 1;
            self.emit(Step::LeaveBlock);
        }
        Ok(())
    }

    fn compile_for_in(
        &mut self,
        left: &ForBinding,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.emit(Step::ResetCompletion);
        // Annex B.2.6: `for (var a = init in expr)` — the initializer runs
        // once and binds `a` before the RHS is evaluated (sloppy mode only).
        if let ForBinding::VarDecl {
            kind: VarDeclKind::Var,
            pattern,
            init: Some(init_expr),
            ..
        } = left
        {
            self.compile_expr(init_expr)?;
            self.emit(Step::ForInBind {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Var,
                    pattern: pattern.clone(),
                    init: None,
                },
            });
        }
        let tdz = self.enter_iter_tdz_env(left);
        self.compile_expr(right)?;
        self.leave_iter_tdz_env(tdz);
        self.emit(Step::ForInBegin);
        let top_label = self.new_label();
        let end_label = self.new_label();
        let continue_label = self.new_label();
        let scope = self.scope_count;
        self.scope_stack.push(Scope::Iteration {
            open: scope,
            closed: scope,
            continue_target: continue_label,
            break_target: end_label,
        });
        self.resolve_labeled_continue(continue_label, scope);
        self.place(top_label);
        let step_index = self.steps.len();
        self.emit(Step::ForInNext { done: 0 });
        self.fixups.push(Fixup::ForInNext(step_index, end_label));
        self.emit(Step::ForInBind { left: left.clone() });
        self.compile_statement(body)?;
        self.emit(Step::ForInRestore);
        self.place(continue_label);
        self.jump(top_label);
        self.place(end_label);
        self.emit(Step::NormalizeCompletion);
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_for_of(
        &mut self,
        left: &ForBinding,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.emit(Step::ResetCompletion);
        let tdz = self.enter_iter_tdz_env(left);
        self.compile_expr(right)?;
        self.leave_iter_tdz_env(tdz);
        self.emit(Step::ForOfBegin);
        let top_label = self.new_label();
        let end_label = self.new_label();
        let done_label = self.new_label();
        let continue_label = self.new_label();
        let scope = self.scope_count;
        self.scope_stack.push(Scope::Iteration {
            open: scope,
            closed: scope,
            continue_target: continue_label,
            break_target: end_label,
        });
        self.resolve_labeled_continue(continue_label, scope);
        self.place(top_label);
        let step_index = self.steps.len();
        self.emit(Step::ForOfNext { done: 0 });
        self.fixups.push(Fixup::ForOfNext(step_index, done_label));
        // A destructuring head is compiled as steps (member targets and
        // defaults with `yield`/`await` need the resumable machinery).
        match left {
            ForBinding::Expr(expr)
                if matches!(expr.kind, ExprKind::Array(_) | ExprKind::Object(_)) =>
            {
                self.compile_destructure_assign(expr)?;
            }
            _ => self.emit(Step::ForOfBind { left: left.clone() }),
        }
        self.compile_statement(body)?;
        self.emit(Step::ForOfRestore);
        self.place(continue_label);
        self.jump(top_label);
        self.place(end_label);
        self.emit(Step::ForOfClose);
        // The exhausted path jumped past the close (the iterator was already
        // popped by ForOfNext), so a `break` is the only way to reach it.
        self.place(done_label);
        self.emit(Step::NormalizeCompletion);
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_async_for_of(
        &mut self,
        left: &ForBinding,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.emit(Step::ResetCompletion);
        let tdz = self.enter_iter_tdz_env(left);
        self.compile_expr(right)?;
        self.leave_iter_tdz_env(tdz);
        self.emit(Step::AsyncForOfBegin);
        let top_label = self.new_label();
        let end_label = self.new_label();
        let done_label = self.new_label();
        let continue_label = self.new_label();
        let scope = self.scope_count;
        self.scope_stack.push(Scope::Iteration {
            open: scope,
            closed: scope,
            continue_target: continue_label,
            break_target: end_label,
        });
        self.resolve_labeled_continue(continue_label, scope);
        self.place(top_label);
        self.emit(Step::AsyncForOfNext);
        let step_index = self.steps.len();
        self.emit(Step::AsyncForOfTest { done: 0 });
        self.fixups
            .push(Fixup::AsyncForOfNext(step_index, done_label));
        // A destructuring head is compiled as steps (member targets and
        // defaults with `await` need the resumable machinery), mirroring the
        // sync for-of compiler.
        match left {
            ForBinding::Expr(expr)
                if matches!(expr.kind, ExprKind::Array(_) | ExprKind::Object(_)) =>
            {
                self.compile_destructure_assign(expr)?;
            }
            _ => self.emit(Step::AsyncForOfBind { left: left.clone() }),
        }
        self.compile_statement(body)?;
        self.emit(Step::AsyncForOfRestore);
        self.place(continue_label);
        self.jump(top_label);
        self.place(end_label);
        self.emit(Step::AsyncForOfClose);
        // The exhausted path jumped past the close (the iterator was already
        // popped by AsyncForOfTest), so a `break` is the only way to reach it.
        self.place(done_label);
        self.emit(Step::NormalizeCompletion);
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_switch(&mut self, discriminant: &Expr, cases: &[SwitchCase]) -> Result<(), JsError> {
        self.emit(Step::ResetCompletion);
        self.compile_expr(discriminant)?;
        self.emit(Step::SwitchDisc);
        let all_stmts: Vec<Stmt> = cases.iter().flat_map(|c| c.consequent.clone()).collect();
        self.emit(Step::EnterBlock {
            decls: Self::block_decls(&all_stmts),
        });
        self.emit(Step::ListBegin);
        self.scope_count += 1;
        let end_label = self.new_label();
        let default_label = self.new_label();
        self.scope_stack.push(Scope::Switch {
            break_target: end_label,
            break_count: self.scope_count - 1,
        });
        let mut case_labels = Vec::new();
        for _case in cases {
            case_labels.push(self.new_label());
        }
        for (index, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                self.compile_expr(test)?;
                let step_index = self.steps.len();
                self.emit(Step::SwitchTest { case: 0 });
                self.fixups
                    .push(Fixup::SwitchTest(step_index, case_labels[index]));
            }
        }
        self.jump(default_label);
        let mut has_default = false;
        for (index, case) in cases.iter().enumerate() {
            self.place(case_labels[index]);
            if case.test.is_none() {
                // The no-match path lands on the default case's statements.
                self.place(default_label);
                has_default = true;
            }
            self.compile_statements(&case.consequent)?;
        }
        if !has_default {
            self.place(default_label);
        }
        self.scope_stack.pop();
        self.scope_count -= 1;
        self.emit(Step::LeaveBlock);
        self.emit(Step::ListEnd);
        self.place(end_label);
        self.emit(Step::NormalizeCompletion);
        Ok(())
    }

    fn compile_try(
        &mut self,
        block: &syntax::ast::Block,
        handler: Option<&syntax::ast::CatchClause>,
        finalizer: Option<&syntax::ast::Block>,
    ) -> Result<(), JsError> {
        let handler_index = self.handlers.len();
        self.handlers.push(Handler {
            start: 0,
            try_end: 0,
            catch: None,
            finally: None,
        });
        let start = self.steps.len();
        self.handlers[handler_index].start = start;
        self.emit(Step::EnterTry {
            handler: handler_index,
        });
        self.emit(Step::ResetCompletion);
        self.emit(Step::ListBegin);
        self.emit(Step::EnterBlock {
            decls: Self::block_decls(&block.stmts),
        });
        self.scope_count += 1;
        self.compile_statements(&block.stmts)?;
        self.scope_count -= 1;
        self.emit(Step::LeaveBlock);
        self.emit(Step::ListEnd);
        let exit_index = self.steps.len();
        self.emit(Step::Exit { after: 0 });
        let after_label = self.new_label();
        self.fixups.push(Fixup::Exit(exit_index, after_label));
        self.handlers[handler_index].try_end = exit_index;

        if let Some(handler) = handler {
            let catch_start = self.steps.len();
            self.emit(Step::CatchBind {
                param: handler.param.clone(),
                decls: Self::block_decls(&handler.body.stmts),
            });
            self.emit(Step::ResetCompletion);
            self.emit(Step::ListBegin);
            self.scope_count += 1;
            self.compile_statements(&handler.body.stmts)?;
            self.scope_count -= 1;
            if handler.param.is_some() {
                // CatchBind pushed the block env and the parameter env in
                // addition to the body env: unwind the parameter chain before
                // the body's own LeaveBlock restores the pre-catch
                // environment.
                self.emit(Step::LeaveBlock);
                self.emit(Step::LeaveBlock);
            }
            self.emit(Step::LeaveBlock);
            self.emit(Step::ListEnd);
            let exit_index = self.steps.len();
            self.emit(Step::Exit { after: 0 });
            self.fixups.push(Fixup::Exit(exit_index, after_label));
            self.handlers[handler_index].catch = Some(CatchHandler {
                start: catch_start,
                end: exit_index,
            });
        }

        if let Some(finalizer) = finalizer {
            let finally_start = self.steps.len();
            self.handlers[handler_index].finally = Some(finally_start);
            self.emit(Step::SaveCompletion);
            self.emit(Step::ListBegin);
            self.emit(Step::EnterBlock {
                decls: Self::block_decls(&finalizer.stmts),
            });
            self.scope_count += 1;
            self.compile_statements(&finalizer.stmts)?;
            self.scope_count -= 1;
            self.emit(Step::LeaveBlock);
            self.emit(Step::ListEnd);
            self.emit(Step::RestoreCompletion);
            self.emit(Step::FinallyEnd);
        }
        self.place(after_label);
        self.emit(Step::NormalizeCompletion);
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), JsError> {
        match &expr.kind {
            ExprKind::Paren(inner) => self.compile_expr(inner),
            ExprKind::Sequence(exprs) => {
                for (index, expr) in exprs.iter().enumerate() {
                    self.compile_expr(expr)?;
                    if index + 1 < exprs.len() {
                        self.emit(Step::Pop);
                    }
                }
                Ok(())
            }
            ExprKind::Literal(literal) => self.compile_literal(literal),
            ExprKind::Ident(name) => {
                self.emit(Step::LoadIdent { name: *name });
                Ok(())
            }
            ExprKind::This => {
                self.emit(Step::ThisValue);
                Ok(())
            }
            ExprKind::Super => Err(JsError::new(
                ErrorKind::SyntaxError,
                "super is not valid here".into(),
            )),
            ExprKind::Function(function) => {
                self.emit(Step::CreateFunction {
                    function: Box::new(function.clone()),
                });
                Ok(())
            }
            ExprKind::Arrow {
                is_async,
                params,
                body,
            } => {
                self.emit(Step::CreateArrow {
                    is_async: *is_async,
                    params: params.clone(),
                    body: body.clone(),
                });
                Ok(())
            }
            ExprKind::Unary { op, operand } => match op {
                UnaryOp::Delete => self.compile_delete(operand),
                UnaryOp::Void => {
                    self.compile_expr(operand)?;
                    self.emit(Step::Pop);
                    self.emit(Step::Push(Value::Undefined));
                    Ok(())
                }
                UnaryOp::Typeof => {
                    // spec 13.5.3.2 step 1: `typeof` of an unresolvable
                    // identifier reference is "undefined", not an error
                    // (parentheses are transparent).
                    let mut operand = operand;
                    while let ExprKind::Paren(inner) = &operand.kind {
                        operand = inner;
                    }
                    if let ExprKind::Ident(name) = &operand.kind {
                        self.emit(Step::TypeofIdent { name: *name });
                    } else {
                        self.compile_expr(operand)?;
                        self.emit(Step::TypeofTop);
                    }
                    Ok(())
                }
                _ => {
                    self.compile_expr(operand)?;
                    self.emit(Step::Unary(*op));
                    Ok(())
                }
            },
            ExprKind::Update { op, prefix, target } => self.compile_update(op, *prefix, target),
            ExprKind::Binary { op, left, right } => {
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                self.emit(Step::Binary(*op));
                Ok(())
            }
            ExprKind::Logical { op, left, right } => {
                self.compile_expr(left)?;
                let end_label = self.new_label();
                match op {
                    LogicalOp::And => self.jump_if_false_keep(end_label),
                    LogicalOp::Or => self.jump_if_true_keep(end_label),
                    LogicalOp::Nullish => self.jump_if_not_nullish_keep(end_label),
                }
                self.emit(Step::Pop);
                self.compile_expr(right)?;
                self.place(end_label);
                Ok(())
            }
            ExprKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.compile_expr(test)?;
                let else_label = self.new_label();
                let end_label = self.new_label();
                self.jump_if_false(else_label);
                self.compile_expr(consequent)?;
                self.jump(end_label);
                self.place(else_label);
                self.compile_expr(alternate)?;
                self.place(end_label);
                Ok(())
            }
            ExprKind::Assign { op, target, value } => self.compile_assign(op, target, value),
            ExprKind::PrivateIn { name, object } => {
                self.compile_expr(object)?;
                self.emit(Step::PrivateIn { atom: *name });
                Ok(())
            }
            ExprKind::Call(call) => {
                self.enter_chain(expr);
                self.compile_call(call)?;
                self.leave_chain();
                Ok(())
            }
            ExprKind::New(new) => {
                if matches!(new.callee.kind, ExprKind::Super) {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "new super is a syntax error".into(),
                    ));
                }
                self.compile_expr(&new.callee)?;
                self.compile_arguments(&new.args)?;
                self.emit(Step::Construct);
                Ok(())
            }
            ExprKind::Member(member) => {
                self.enter_chain(expr);
                self.compile_member(member)?;
                self.leave_chain();
                Ok(())
            }
            ExprKind::TaggedTemplate { tag, quasi } => {
                self.compile_expr(tag)?;
                self.emit(Step::ArgsBase);
                for expr in &quasi.exprs {
                    self.compile_expr(expr)?;
                    self.emit(Step::ArgsPush);
                }
                self.emit(Step::TaggedTemplate(quasi.clone()));
                Ok(())
            }
            ExprKind::Template(template) => self.compile_template(template),
            ExprKind::Array(literal) => {
                self.emit(Step::ArrayBegin);
                for element in &literal.elements {
                    match element {
                        ArrayElement::Hole => self.emit(Step::ArrayHole),
                        ArrayElement::Expr(expr) => {
                            self.compile_expr(expr)?;
                            self.emit(Step::ArrayElement);
                        }
                        ArrayElement::Spread(expr) => {
                            self.compile_expr(expr)?;
                            self.emit(Step::ArraySpread);
                        }
                    }
                }
                self.emit(Step::ArrayEnd);
                Ok(())
            }
            ExprKind::Class(class) => self.compile_class(class),
            ExprKind::Object(literal) => self.compile_object(literal),
            ExprKind::Yield { delegate, argument } => {
                match argument {
                    Some(argument) => self.compile_expr(argument)?,
                    None => self.emit(Step::Push(Value::Undefined)),
                }
                if *delegate {
                    // yield* (spec 14.5.5): the delegation loop. Async
                    // generators await each inner result through the driver.
                    if self.is_async_generator {
                        self.emit(Step::AsyncYieldStarBegin);
                        let loop_label = self.new_label();
                        let done_label = self.new_label();
                        self.place(loop_label);
                        let step_index = self.steps.len();
                        self.emit(Step::AsyncYieldStarNext { done: 0 });
                        self.fixups
                            .push(Fixup::AsyncYieldStarNext(step_index, done_label));
                        let inspect_index = self.steps.len();
                        self.emit(Step::AsyncYieldStarInspect { done: 0 });
                        self.fixups
                            .push(Fixup::AsyncYieldStarInspect(inspect_index, done_label));
                        self.emit(Step::Yield { delegate: true });
                        let resume_index = self.steps.len();
                        self.emit(Step::AsyncYieldStarResume {
                            loop_top: 0,
                            done: 0,
                            inspect: 0,
                        });
                        self.fixups.push(Fixup::AsyncYieldStarResume(
                            resume_index,
                            loop_label,
                            done_label,
                            inspect_index,
                        ));
                        self.place(done_label);
                    } else {
                        self.emit(Step::YieldStarBegin);
                        let loop_label = self.new_label();
                        let done_label = self.new_label();
                        let yield_label = self.new_label();
                        self.place(loop_label);
                        let step_index = self.steps.len();
                        self.emit(Step::YieldStarNext { done: 0 });
                        self.fixups
                            .push(Fixup::YieldStarNext(step_index, done_label));
                        self.place(yield_label);
                        self.emit(Step::Yield { delegate: true });
                        let resume_index = self.steps.len();
                        self.emit(Step::YieldStarResume {
                            loop_top: 0,
                            done: 0,
                            yield_at: 0,
                        });
                        self.fixups.push(Fixup::YieldStarResume(
                            resume_index,
                            loop_label,
                            done_label,
                            yield_label,
                        ));
                        self.place(done_label);
                    }
                } else {
                    self.emit(Step::Yield { delegate: false });
                }
                Ok(())
            }
            ExprKind::Await(argument) => {
                self.compile_expr(argument)?;
                self.emit(Step::Await);
                Ok(())
            }
            ExprKind::ImportCall {
                specifier,
                options,
                phase,
            } => {
                self.compile_expr(specifier)?;
                let has_options = options.is_some();
                if let Some(options) = options {
                    self.compile_expr(options)?;
                }
                self.emit(Step::ImportCall {
                    has_options,
                    phase: *phase,
                });
                Ok(())
            }
            ExprKind::MetaProperty { meta, property } => {
                let meta_name = crux::lookup(*meta).to_string_lossy();
                let property_name = crux::lookup(*property).to_string_lossy();
                if meta_name == "import" && property_name == "meta" {
                    self.emit(Step::ImportMeta);
                } else {
                    // new.target (spec 13.3.5.3).
                    self.emit(Step::NewTarget);
                }
                Ok(())
            }
        }
    }

    /// A literal's value is known at compile time; a `RegExp` literal
    /// constructs its object at runtime (spec 13.2.4.4).
    fn compile_literal(&mut self, literal: &syntax::ast::Literal) -> Result<(), JsError> {
        match literal {
            syntax::ast::Literal::Null => self.emit(Step::Push(Value::Null)),
            syntax::ast::Literal::Boolean(value) => {
                self.emit(Step::Push(Value::Boolean(*value)));
            }
            syntax::ast::Literal::Number(value) => self.emit(Step::Push(Value::Number(*value))),
            syntax::ast::Literal::BigInt(value) => {
                self.emit(Step::Push(Value::BigInt(Handle::new(value.clone()))));
            }
            syntax::ast::Literal::Str(value) => {
                self.emit(Step::Push(Value::String(Handle::new(value.clone()))));
            }
            syntax::ast::Literal::RegExp { pattern, flags } => {
                self.emit(Step::RegExpLiteral {
                    pattern: pattern.clone(),
                    flags: flags.clone(),
                });
            }
        }
        Ok(())
    }

    fn compile_delete(&mut self, operand: &Expr) -> Result<(), JsError> {
        match &operand.kind {
            ExprKind::Ident(name) => {
                self.emit(Step::DeleteIdent { name: *name });
                Ok(())
            }
            ExprKind::Member(member) => {
                if matches!(member.object.kind, ExprKind::Super) {
                    // `delete super.x` throws a ReferenceError before the key
                    // evaluates (spec 13.5.1.2 step 4.b).
                    self.emit(Step::DeleteSuper);
                    return Ok(());
                }
                match &member.property {
                    MemberProperty::Name(name) => {
                        self.compile_expr(&member.object)?;
                        self.emit(Step::DeleteMemberName { name: *name });
                    }
                    MemberProperty::Computed(key) => {
                        self.compile_expr(&member.object)?;
                        self.compile_expr(key)?;
                        self.emit(Step::DeleteMemberComputed);
                    }
                    MemberProperty::Private(_) => {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "delete of a private name is a syntax error".into(),
                        ));
                    }
                }
                Ok(())
            }
            _ => {
                self.compile_expr(operand)?;
                self.emit(Step::Pop);
                self.emit(Step::Push(Value::Boolean(true)));
                Ok(())
            }
        }
    }

    fn compile_update(
        &mut self,
        op: &UpdateOp,
        prefix: bool,
        target: &Expr,
    ) -> Result<(), JsError> {
        match &target.kind {
            ExprKind::Ident(name) => {
                // The assignment target's reference resolves before the RHS:
                // PutValue uses the initially created reference even when the
                // binding disappears while the RHS evaluates (spec 13.15.3
                // steps 1, 5 — a `with` scope whose binding the RHS deletes).
                self.emit(Step::ResolveVarIdent { name: *name });
                self.emit(Step::GetVarReference);
                self.emit(Step::UpdateVarReference { op: *op, prefix });
            }
            ExprKind::Member(member) => {
                if matches!(member.object.kind, ExprKind::Super) {
                    // super.x++ / super[x]++ (spec 13.3.7.1): GetValue
                    // through the super reference, then PutValue the
                    // incremented value.
                    match &member.property {
                        MemberProperty::Name(name) => {
                            self.emit(Step::ResolveSuperRefName { name: *name });
                        }
                        MemberProperty::Computed(key) => {
                            // GetSuperBase runs the this-binding check before
                            // the key evaluates.
                            self.emit(Step::GetSuperBase);
                            self.compile_expr(key)?;
                            self.emit(Step::ResolveSuperRefComputed);
                        }
                        MemberProperty::Private(_) => {
                            return Err(JsError::new(
                                ErrorKind::SyntaxError,
                                "update of super.#x is a syntax error".into(),
                            ));
                        }
                    }
                    self.emit(Step::GetVarReference);
                    self.emit(Step::UpdateVarReference { op: *op, prefix });
                    return Ok(());
                }
                self.compile_expr(&member.object)?;
                match &member.property {
                    MemberProperty::Name(name) => {
                        self.emit(Step::Dup);
                        self.emit(Step::GetMemberName { name: *name });
                        self.emit(Step::UpdateMemberName {
                            name: *name,
                            op: *op,
                            prefix,
                        });
                    }
                    MemberProperty::Computed(key) => {
                        self.compile_expr(key)?;
                        self.emit(Step::ToPropertyKey);
                        self.emit(Step::Dup2);
                        self.emit(Step::GetMemberComputed);
                        self.emit(Step::UpdateMemberComputed { op: *op, prefix });
                    }
                    MemberProperty::Private(atom) => {
                        self.emit(Step::Dup);
                        self.emit(Step::GetPrivate { atom: *atom });
                        self.emit(Step::UpdatePrivate {
                            atom: *atom,
                            op: *op,
                            prefix,
                        });
                    }
                }
            }
            ExprKind::Super => {
                return Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "update of super is a syntax error".into(),
                ));
            }
            _ => {
                return Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Invalid left-hand side in update expression".into(),
                ));
            }
        }
        Ok(())
    }

    fn compile_assign(
        &mut self,
        op: &AssignOp,
        target: &Expr,
        value: &Expr,
    ) -> Result<(), JsError> {
        if matches!(target.kind, ExprKind::Object(_) | ExprKind::Array(_)) {
            self.compile_expr(value)?;
            if Self::destructure_needs_steps(target) {
                // Keep a copy: the assignment expression's value is the RHS.
                self.emit(Step::Dup);
                self.compile_destructure_assign(target)?;
            } else {
                let pattern = pattern_of_target(target)?;
                self.emit(Step::Destructure { pattern });
            }
            return Ok(());
        }
        let set_name = matches!(target.kind, ExprKind::Ident(_))
            && crate::function::is_anonymous_function_definition(value);
        match &target.kind {
            ExprKind::Ident(name) => {
                match op {
                    AssignOp::Assign => {
                        // Resolve the reference before the RHS: PutValue uses
                        // the initially created reference even if the binding
                        // disappears while the RHS evaluates (spec 13.15.3
                        // steps 1, 5 — e.g. a `with` scope whose binding the
                        // RHS deletes).
                        self.emit(Step::ResolveVarIdent { name: *name });
                        self.compile_expr(value)?;
                        if set_name {
                            self.emit(Step::SetFunctionName { name: *name });
                        }
                        self.emit(Step::PutVarReference);
                    }
                    AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign => {
                        self.emit(Step::LoadIdent { name: *name });
                        let end_label = self.new_label();
                        match op {
                            AssignOp::AndAssign => self.jump_if_false_keep(end_label),
                            AssignOp::OrAssign => self.jump_if_true_keep(end_label),
                            _ => self.jump_if_not_nullish_keep(end_label),
                        }
                        self.emit(Step::Pop);
                        self.compile_expr(value)?;
                        self.emit(Step::AssignIdent {
                            name: *name,
                            op: *op,
                            set_name,
                        });
                        self.place(end_label);
                    }
                    _ => {
                        self.emit(Step::ResolveVarIdent { name: *name });
                        self.emit(Step::GetVarReference);
                        self.compile_expr(value)?;
                        self.emit(Step::PutVarReferenceOp { op: *op });
                    }
                }
                Ok(())
            }
            ExprKind::Member(member) => self.compile_member_assign(member, op, value),
            ExprKind::Super => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Invalid left-hand side in assignment".into(),
            )),
            _ => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Invalid left-hand side in assignment".into(),
            )),
        }
    }

    /// Whether a destructuring assignment target needs step-based compilation:
    /// a member target (the tree-walker has no reference machinery for it) or
    /// a suspension point in a default or computed key.
    fn destructure_needs_steps(target: &Expr) -> bool {
        fn member_in(expr: &Expr) -> bool {
            match &expr.kind {
                ExprKind::Member(_) => true,
                ExprKind::Assign { target, value, .. } => member_in(target) || member_in(value),
                ExprKind::Array(lit) => lit.elements.iter().any(|el| match el {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => member_in(e),
                    ArrayElement::Hole => false,
                }),
                ExprKind::Object(lit) => lit.props.iter().any(|prop| match prop {
                    ObjectProperty::Init { value, .. } | ObjectProperty::Spread(value) => {
                        member_in(value)
                    }
                    _ => false,
                }),
                ExprKind::Paren(inner) => member_in(inner),
                _ => false,
            }
        }
        member_in(target) || expr_contains_suspension(target)
    }

    /// Whether a binding pattern contains a suspension point in any default
    /// initializer or computed key. Such patterns must be compiled to steps
    /// (the runtime `binding_initialization` evaluates defaults through the
    /// synchronous tree-walker, which cannot suspend).
    fn binding_pattern_contains_suspension(&self, pattern: &BindingPattern) -> bool {
        match pattern {
            BindingPattern::Ident(_) => false,
            BindingPattern::Object(props) => props.iter().any(|prop| match prop {
                syntax::ast::ObjectBindingProperty::Property { key, element, .. } => {
                    pattern_element_contains_suspension(element)
                        || matches!(key, PropertyName::Computed(e) if expr_contains_suspension(e))
                }
                syntax::ast::ObjectBindingProperty::Rest(element) => {
                    pattern_element_contains_suspension(element)
                }
            }),
            BindingPattern::Array(elements) => elements.iter().any(|element| match element {
                ArrayBindingElement::Hole => false,
                ArrayBindingElement::Element(e) | ArrayBindingElement::Rest(e) => {
                    pattern_element_contains_suspension(e)
                }
            }),
        }
    }

    /// Compile a destructuring binding pattern into steps (spec 13.13.11):
    /// the value is on top of the stack; defaults and computed keys compile
    /// inline so `yield`/`await` in them suspends the resumable body, and
    /// each element binds into the lexical (`DeclInit`) or var (`Destructure`)
    /// environment. Only used when `binding_pattern_contains_suspension` is
    /// true — otherwise the single-step wholesale binding is faster.
    fn compile_destructure_binding(
        &mut self,
        pattern: &BindingPattern,
        lexical: bool,
    ) -> Result<(), JsError> {
        let bind = |compiler: &mut Self, element: &BindingElement| -> Result<(), JsError> {
            // A default initializer compiles inline so it can suspend; a
            // nested pattern with suspension recurses step-wise; otherwise
            // the whole element binds in one step. The value is on top of
            // the stack either way (spec 13.13.11 step 5.d).
            let bind_element = |compiler: &mut Self| -> Result<(), JsError> {
                if compiler.binding_pattern_contains_suspension(&element.pattern) {
                    compiler.compile_destructure_binding(&element.pattern, lexical)
                } else {
                    compiler.emit(if lexical {
                        Step::DeclInit {
                            pattern: element.pattern.clone(),
                        }
                    } else {
                        Step::Destructure {
                            pattern: element.pattern.clone(),
                        }
                    });
                    compiler.emit(Step::Pop);
                    Ok(())
                }
            };
            if let Some(init) = &element.init {
                let use_default = compiler.new_label();
                let after = compiler.new_label();
                compiler.emit_destructure_undef(use_default);
                bind_element(compiler)?;
                compiler.jump(after);
                compiler.place(use_default);
                compiler.compile_expr(init)?;
                bind_element(compiler)?;
                compiler.place(after);
            } else {
                bind_element(compiler)?;
            }
            Ok(())
        };
        match pattern {
            BindingPattern::Ident(_) => {
                // A plain identifier with a suspension default cannot occur
                // (the default belongs to the declaration's init, not the
                // pattern); treat as a plain binding.
                self.emit(if lexical {
                    Step::DeclInit {
                        pattern: pattern.clone(),
                    }
                } else {
                    Step::Destructure {
                        pattern: pattern.clone(),
                    }
                });
                self.emit(Step::Pop);
            }
            BindingPattern::Array(elements) => {
                self.emit(Step::DestructureBegin);
                let end_label = self.new_label();
                for element in elements {
                    match element {
                        ArrayBindingElement::Hole => {
                            self.emit_destructure_next();
                            self.emit(Step::Pop);
                        }
                        ArrayBindingElement::Element(element) => {
                            self.emit_destructure_next();
                            bind(self, element)?;
                        }
                        ArrayBindingElement::Rest(element) => {
                            self.emit(Step::DestructureRest);
                            bind(self, element)?;
                            self.place(end_label);
                            return Ok(());
                        }
                    }
                }
                self.emit(Step::DestructureClose);
                self.place(end_label);
            }
            BindingPattern::Object(props) => {
                let mut excluded: Vec<crux::property::PropertyKey> = Vec::new();
                self.emit(Step::DestructureObjCoercible);
                for prop in props {
                    match prop {
                        syntax::ast::ObjectBindingProperty::Property { key, element, .. } => {
                            match key {
                                PropertyName::Ident(id) => {
                                    let key = crux::property::PropertyKey::String(*id);
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Str(text) => {
                                    let key = crux::property::PropertyKey::from_js_string(text);
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Number(n) => {
                                    let key = crux::property::PropertyKey::from_js_string(
                                        &crux::convert::to_string(&Value::Number(*n))?,
                                    );
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Computed(expr) => {
                                    self.compile_expr(expr)?;
                                    self.emit(Step::DestructureObjKeyComputed);
                                }
                            }
                            bind(self, element)?;
                        }
                        syntax::ast::ObjectBindingProperty::Rest(element) => {
                            self.emit(Step::DestructureObjRest {
                                excluded: excluded.clone(),
                            });
                            bind(self, element)?;
                        }
                    }
                }
                self.emit(Step::DestructureObjEnd);
            }
        }
        Ok(())
    }

    /// Compile a destructuring assignment pattern into steps (spec 13.15.5):
    /// the value is on top of the stack. Defaults and computed keys are
    /// compiled inline, so `yield`/`await` in them suspends the resumable
    /// body; member targets assign through references.
    fn compile_destructure_assign(&mut self, target: &Expr) -> Result<(), JsError> {
        match &target.kind {
            ExprKind::Array(lit) => {
                self.emit(Step::DestructureBegin);
                let end_label = self.new_label();
                for element in &lit.elements {
                    match element {
                        ArrayElement::Hole => {
                            self.emit_destructure_next();
                            self.emit(Step::Pop);
                        }
                        ArrayElement::Expr(expr) => {
                            let (inner, init) = unwrap_default(expr);
                            // A member target evaluates its reference before
                            // the iterator steps (spec 13.15.5.5 note).
                            if let ExprKind::Member(member) = &inner.kind {
                                self.compile_member_reference(member)?;
                            }
                            self.emit_destructure_next();
                            self.compile_assign_value(inner, init)?;
                        }
                        ArrayElement::Spread(expr) => {
                            // The rest target's reference is evaluated before
                            // the remaining values are collected (spec
                            // 13.15.5.5); a `yield`/`await` in a computed key
                            // suspends here.
                            if let ExprKind::Member(member) = &expr.kind {
                                self.compile_member_reference(member)?;
                            }
                            self.emit(Step::DestructureRest);
                            self.compile_assign_value(expr, None)?;
                            self.place(end_label);
                            return Ok(());
                        }
                    }
                }
                self.emit(Step::DestructureClose);
                self.place(end_label);
                Ok(())
            }
            ExprKind::Object(lit) => {
                let mut excluded: Vec<crux::property::PropertyKey> = Vec::new();
                self.emit(Step::DestructureObjCoercible);
                for prop in &lit.props {
                    match prop {
                        ObjectProperty::Init { key, value, .. } => {
                            let (inner, init) = unwrap_default(value);
                            // KeyedDestructuringAssignmentEvaluation: the
                            // target reference is evaluated before the
                            // property read (spec 13.15.5.6).
                            if let ExprKind::Member(member) = &inner.kind {
                                self.compile_member_reference(member)?;
                            }
                            match key {
                                PropertyName::Ident(id) => {
                                    let key = crux::property::PropertyKey::String(*id);
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Str(text) => {
                                    let key = crux::property::PropertyKey::from_js_string(text);
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Number(n) => {
                                    let key = crux::property::PropertyKey::from_js_string(
                                        &crux::convert::to_string(&Value::Number(*n))?,
                                    );
                                    self.emit(Step::DestructureObjKey { key: key.clone() });
                                    excluded.push(key);
                                }
                                PropertyName::Computed(expr) => {
                                    self.compile_expr(expr)?;
                                    self.emit(Step::DestructureObjKeyComputed);
                                }
                            }
                            self.compile_assign_value(inner, init)?;
                        }
                        ObjectProperty::Spread(expr) => {
                            // The rest target's reference is evaluated before
                            // the rest object is collected (mirroring the
                            // array-pattern case above; the tree-walker
                            // evaluates it after CopyDataProperties, but the
                            // stack protocol needs the base below the value).
                            if let ExprKind::Member(member) = &expr.kind {
                                self.compile_member_reference(member)?;
                            }
                            self.emit(Step::DestructureObjRest {
                                excluded: excluded.clone(),
                            });
                            self.compile_assign_value(expr, None)?;
                        }
                        _ => {
                            return Err(JsError::new(
                                ErrorKind::SyntaxError,
                                "Invalid destructuring assignment target".into(),
                            ));
                        }
                    }
                }
                self.emit(Step::DestructureObjEnd);
                Ok(())
            }
            ExprKind::Paren(inner) => self.compile_destructure_assign(inner),
            _ => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Invalid destructuring assignment target".into(),
            )),
        }
    }

    /// Assign the value on top of the stack to an element target, applying
    /// the default when it is undefined (spec 13.15.5.5). Nested patterns
    /// recurse; member targets consume the pre-compiled reference below the
    /// value.
    fn compile_assign_value(&mut self, inner: &Expr, init: Option<&Expr>) -> Result<(), JsError> {
        match &inner.kind {
            ExprKind::Array(_) | ExprKind::Object(_) => {
                if let Some(init) = init {
                    let use_default = self.new_label();
                    let after = self.new_label();
                    self.emit_destructure_undef(use_default);
                    self.compile_destructure_assign(inner)?;
                    self.jump(after);
                    self.place(use_default);
                    self.compile_expr(init)?;
                    self.compile_destructure_assign(inner)?;
                    self.place(after);
                } else {
                    self.compile_destructure_assign(inner)?;
                }
                Ok(())
            }
            ExprKind::Ident(id) => {
                if let Some(init) = init {
                    let use_default = self.new_label();
                    let after = self.new_label();
                    let set_name = crate::function::is_anonymous_function_definition(init);
                    self.emit_destructure_undef(use_default);
                    self.emit(Step::AssignIdent {
                        name: *id,
                        op: AssignOp::Assign,
                        set_name,
                    });
                    self.emit(Step::Pop);
                    self.jump(after);
                    self.place(use_default);
                    self.compile_expr(init)?;
                    self.emit(Step::AssignIdent {
                        name: *id,
                        op: AssignOp::Assign,
                        set_name,
                    });
                    self.emit(Step::Pop);
                    self.place(after);
                } else {
                    self.emit(Step::AssignIdent {
                        name: *id,
                        op: AssignOp::Assign,
                        set_name: false,
                    });
                    self.emit(Step::Pop);
                }
                Ok(())
            }
            ExprKind::Member(member) => {
                if let Some(init) = init {
                    let use_default = self.new_label();
                    let after = self.new_label();
                    self.emit_destructure_undef(use_default);
                    self.emit_member_assign(member)?;
                    self.emit(Step::Pop);
                    self.jump(after);
                    self.place(use_default);
                    self.compile_expr(init)?;
                    self.emit_member_assign(member)?;
                    self.emit(Step::Pop);
                    self.place(after);
                } else {
                    self.emit_member_assign(member)?;
                    self.emit(Step::Pop);
                }
                Ok(())
            }
            ExprKind::Paren(inner) => self.compile_assign_value(inner, init),
            _ => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Invalid destructuring assignment target".into(),
            )),
        }
    }

    /// The reference parts of a member target: the base (and computed key)
    /// are pushed for the later `AssignMember*` step (spec 13.15.5.5: the
    /// reference is evaluated before the iterator steps).
    fn compile_member_reference(
        &mut self,
        member: &syntax::ast::MemberExpr,
    ) -> Result<(), JsError> {
        if matches!(member.object.kind, ExprKind::Super) {
            self.emit(Step::GetSuperBase);
            if let MemberProperty::Computed(key) = &member.property {
                self.compile_expr(key)?;
            }
            return Ok(());
        }
        self.compile_expr(&member.object)?;
        if let MemberProperty::Computed(key) = &member.property {
            self.compile_expr(key)?;
        }
        Ok(())
    }

    /// The `AssignMember*`/`AssignPrivate` step consuming the pre-compiled
    /// reference and the value on top of the stack.
    fn emit_member_assign(&mut self, member: &syntax::ast::MemberExpr) -> Result<(), JsError> {
        match &member.property {
            MemberProperty::Name(name) => {
                self.emit(Step::AssignMemberName {
                    name: *name,
                    op: AssignOp::Assign,
                });
            }
            MemberProperty::Computed(_) => {
                self.emit(Step::AssignMemberComputed {
                    op: AssignOp::Assign,
                });
            }
            MemberProperty::Private(atom) => {
                self.emit(Step::AssignPrivate {
                    atom: *atom,
                    op: AssignOp::Assign,
                });
            }
        }
        Ok(())
    }

    /// `&&=`, `||=`, `??=` on a member/super/private target (spec 13.15.4-6):
    /// resolve the reference (base consumed), read the old value, test it,
    /// short-circuit with the old value as the expression result, or evaluate
    /// the RHS and assign. The reference lives on the var-reference stack, so
    /// both paths leave exactly one value.
    fn compile_logical_assign(
        &mut self,
        member: &syntax::ast::MemberExpr,
        op: &AssignOp,
        value: &Expr,
    ) -> Result<(), JsError> {
        let end_label = self.new_label();
        let short_label = self.new_label();
        if matches!(member.object.kind, ExprKind::Super) {
            match &member.property {
                MemberProperty::Name(name) => {
                    self.emit(Step::ResolveSuperRefName { name: *name });
                }
                MemberProperty::Computed(key) => {
                    // GetSuperBase runs the this-binding check before the key
                    // evaluates (spec 13.3.7.1); the base copy it leaves is
                    // consumed by ResolveSuperRefComputed.
                    self.emit(Step::GetSuperBase);
                    self.compile_expr(key)?;
                    self.emit(Step::ResolveSuperRefComputed);
                }
                MemberProperty::Private(_) => {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "super.#x is a syntax error".into(),
                    ));
                }
            }
        } else {
            self.compile_expr(&member.object)?;
            match &member.property {
                MemberProperty::Name(name) => {
                    self.emit(Step::ResolveMemberRefName { name: *name });
                }
                MemberProperty::Computed(key) => {
                    self.compile_expr(key)?;
                    self.emit(Step::ResolveMemberRefComputed);
                }
                MemberProperty::Private(atom) => {
                    self.emit(Step::ResolvePrivateRef { atom: *atom });
                }
            }
        }
        self.emit(Step::GetVarReference);
        self.emit(Step::Dup);
        match op {
            AssignOp::AndAssign => self.jump_if_false_keep(short_label),
            AssignOp::OrAssign => self.jump_if_true_keep(short_label),
            _ => self.jump_if_not_nullish_keep(short_label),
        }
        // The write path: discard the old value, evaluate the RHS, assign
        // through the pre-resolved reference.
        self.emit(Step::Pop);
        self.compile_expr(value)?;
        self.emit(Step::PutVarReference);
        self.jump(end_label);
        // The short-circuit path: keep the old value, drop the reference.
        self.place(short_label);
        self.emit(Step::Pop);
        self.emit(Step::PopVarReference);
        self.place(end_label);
        Ok(())
    }

    fn compile_member_assign(
        &mut self,
        member: &syntax::ast::MemberExpr,
        op: &AssignOp,
        value: &Expr,
    ) -> Result<(), JsError> {
        if matches!(
            op,
            AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign
        ) {
            return self.compile_logical_assign(member, op, value);
        }
        if matches!(member.object.kind, ExprKind::Super) {
            // super.x = v / super[x] = v
            self.emit(Step::GetSuperBase);
            match &member.property {
                MemberProperty::Name(name) => {
                    let needs_old = matches!(
                        op,
                        AssignOp::AddAssign
                            | AssignOp::SubAssign
                            | AssignOp::MulAssign
                            | AssignOp::DivAssign
                            | AssignOp::RemAssign
                            | AssignOp::ExpAssign
                            | AssignOp::LeftShiftAssign
                            | AssignOp::RightShiftAssign
                            | AssignOp::UnsignedRightShiftAssign
                            | AssignOp::BitAndAssign
                            | AssignOp::BitXorAssign
                            | AssignOp::BitOrAssign
                    );
                    if needs_old {
                        self.emit(Step::Dup);
                        self.emit(Step::GetSuperName { name: *name });
                    }
                    self.compile_expr(value)?;
                    self.emit(Step::AssignSuperName {
                        name: *name,
                        op: *op,
                    });
                }
                MemberProperty::Computed(key) => {
                    self.compile_expr(key)?;
                    let needs_old = matches!(
                        op,
                        AssignOp::AddAssign
                            | AssignOp::SubAssign
                            | AssignOp::MulAssign
                            | AssignOp::DivAssign
                            | AssignOp::RemAssign
                            | AssignOp::ExpAssign
                            | AssignOp::LeftShiftAssign
                            | AssignOp::RightShiftAssign
                            | AssignOp::UnsignedRightShiftAssign
                            | AssignOp::BitAndAssign
                            | AssignOp::BitXorAssign
                            | AssignOp::BitOrAssign
                    );
                    if needs_old {
                        // ToPropertyKey runs once, before the read and the
                        // write each consume a copy (spec 13.15.4 step 2.c).
                        self.emit(Step::ToPropertyKey);
                        self.emit(Step::Dup2);
                        self.emit(Step::GetSuperComputed);
                    }
                    self.compile_expr(value)?;
                    self.emit(Step::AssignSuperComputed { op: *op });
                }
                MemberProperty::Private(_) => {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "super.#x is a syntax error".into(),
                    ));
                }
            }
            return Ok(());
        }
        match &member.property {
            MemberProperty::Name(name) => {
                self.compile_expr(&member.object)?;
                let needs_old = is_compound_assign(op);
                if needs_old {
                    self.emit(Step::Dup);
                    self.emit(Step::GetMemberName { name: *name });
                }
                self.compile_expr(value)?;
                self.emit(Step::AssignMemberName {
                    name: *name,
                    op: *op,
                });
            }
            MemberProperty::Computed(key) => {
                self.compile_expr(&member.object)?;
                self.compile_expr(key)?;
                if is_compound_assign(op) {
                    // ToPropertyKey runs once, before the read and the write
                    // each consume a copy (spec 13.15.4 step 2.c).
                    self.emit(Step::ToPropertyKey);
                    self.emit(Step::Dup2);
                    self.emit(Step::GetMemberComputed);
                }
                self.compile_expr(value)?;
                self.emit(Step::AssignMemberComputed { op: *op });
            }
            MemberProperty::Private(atom) => {
                self.compile_expr(&member.object)?;
                self.compile_expr(value)?;
                self.emit(Step::AssignPrivate {
                    atom: *atom,
                    op: *op,
                });
            }
        }
        Ok(())
    }

    fn compile_member(&mut self, member: &syntax::ast::MemberExpr) -> Result<(), JsError> {
        if matches!(member.object.kind, ExprKind::Super) {
            self.emit(Step::GetSuperBase);
            match &member.property {
                MemberProperty::Name(name) => {
                    self.emit(Step::GetSuperName { name: *name });
                }
                MemberProperty::Computed(key) => {
                    self.compile_expr(key)?;
                    self.emit(Step::GetSuperComputed);
                }
                MemberProperty::Private(_) => {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "super.#x is a syntax error".into(),
                    ));
                }
            }
            return Ok(());
        }
        if member.optional {
            let short = self.new_label();
            let end = self.new_label();
            self.compile_expr(&member.object)?;
            self.emit(Step::Dup);
            self.jump_if_nullish_keep(short);
            self.compile_member_property_guarded(member)?;
            self.jump(end);
            self.place(short);
            self.emit(Step::Pop);
            self.emit(Step::Pop);
            self.emit(Step::Push(Value::Undefined));
            // The rest of the chain (keys, args, links) is skipped.
            self.emit(Step::SetChainShort);
            self.place(end);
        } else {
            self.compile_expr(&member.object)?;
            self.compile_member_property_guarded(member)?;
        }
        Ok(())
    }

    fn compile_member_property(&mut self, member: &syntax::ast::MemberExpr) -> Result<(), JsError> {
        match &member.property {
            MemberProperty::Name(name) => {
                self.emit(Step::GetMemberName { name: *name });
            }
            MemberProperty::Computed(key) => {
                self.compile_expr(key)?;
                self.emit(Step::GetMemberComputed);
            }
            MemberProperty::Private(atom) => {
                self.emit(Step::GetPrivate { atom: *atom });
            }
        }
        Ok(())
    }

    fn compile_arguments(&mut self, args: &[Argument]) -> Result<(), JsError> {
        self.emit(Step::ArgsBase);
        for argument in args {
            match argument {
                Argument::Expr(expr) => {
                    self.compile_expr(expr)?;
                    self.emit(Step::ArgsPush);
                }
                Argument::Spread(expr) => {
                    self.compile_expr(expr)?;
                    self.emit(Step::ArgsSpread);
                }
            }
        }
        Ok(())
    }

    fn compile_call(&mut self, call: &syntax::ast::CallExpr) -> Result<(), JsError> {
        if matches!(call.callee.kind, ExprKind::Super) {
            self.compile_arguments(&call.args)?;
            self.emit(Step::SuperCall);
            return Ok(());
        }
        if let ExprKind::Member(member) = &call.callee.kind {
            if matches!(member.object.kind, ExprKind::Super) {
                // super.m(args): this value first, then base, then the method.
                self.emit(Step::ThisValue);
                self.emit(Step::GetSuperBase);
                match &member.property {
                    MemberProperty::Name(name) => {
                        self.emit(Step::GetSuperName { name: *name });
                    }
                    MemberProperty::Computed(key) => {
                        self.compile_expr(key)?;
                        self.emit(Step::GetSuperComputed);
                    }
                    MemberProperty::Private(_) => {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "super.#m() is a syntax error".into(),
                        ));
                    }
                }
                if call.optional {
                    self.compile_optional_call_tail(&call.args)?;
                } else {
                    self.compile_arguments(&call.args)?;
                    self.emit(Step::Call { direct_eval: false });
                }
                return Ok(());
            }
            // obj.m(args)
            self.compile_expr(&member.object)?;
            self.emit(Step::Dup);
            if member.optional {
                // `?.` on the member: a nullish object short-circuits the
                // whole chain — the property access and the call are skipped.
                let short = self.new_label();
                let end = self.new_label();
                self.jump_if_nullish_keep(short);
                self.compile_member_property_guarded(member)?;
                if call.optional {
                    self.compile_optional_call_tail(&call.args)?;
                } else {
                    self.compile_call_args_guarded(&call.args, false)?;
                }
                self.jump(end);
                self.place(short);
                self.emit(Step::Pop);
                self.emit(Step::Pop);
                self.emit(Step::Push(Value::Undefined));
                self.emit(Step::SetChainShort);
                self.place(end);
            } else {
                self.compile_member_property_guarded(member)?;
                if call.optional {
                    // The callee is on top: nullish → undefined.
                    self.compile_optional_call_tail(&call.args)?;
                } else {
                    // The member chain may still have short-circuited
                    // upstream (`a?.b.m(args)`): skip the argument
                    // evaluation and call.
                    self.compile_call_args_guarded(&call.args, false)?;
                }
            }
            return Ok(());
        }
        // A plain callee.
        let direct_eval = matches!(
            call.callee.kind,
            ExprKind::Ident(id) if crux::lookup(id) == JsString::from_utf8("eval")
        );
        self.emit(Step::Push(Value::Undefined));
        self.compile_expr(&call.callee)?;
        if call.optional {
            self.compile_optional_call_tail(&call.args)?;
        } else {
            // An upstream `?.` in the callee (`(a?.b)()`) skips the arguments.
            self.compile_call_args_guarded(&call.args, direct_eval)?;
        }
        Ok(())
    }

    /// The optional-call tail: nullish callee → *undefined* (no argument
    /// evaluation); otherwise call.
    fn compile_optional_call_tail(&mut self, args: &[Argument]) -> Result<(), JsError> {
        let short = self.new_label();
        let end = self.new_label();
        self.emit(Step::Dup);
        self.jump_if_nullish_keep(short);
        self.compile_call_args_guarded(args, false)?;
        self.jump(end);
        self.place(short);
        self.emit(Step::Pop);
        self.emit(Step::Pop);
        self.emit(Step::Push(Value::Undefined));
        self.emit(Step::SetChainShort);
        self.place(end);
        Ok(())
    }

    fn compile_template(&mut self, template: &syntax::ast::TemplateLiteral) -> Result<(), JsError> {
        let first = template
            .quasis
            .first()
            .map(|q| q.cooked.clone().unwrap_or_else(|| JsString::from_utf8("")))
            .unwrap_or_else(|| JsString::from_utf8(""));
        self.emit(Step::PushStr(first));
        for (index, expr) in template.exprs.iter().enumerate() {
            self.compile_expr(expr)?;
            self.emit(Step::ConcatStr);
            let quasi = template
                .quasis
                .get(index + 1)
                .map(|q| q.cooked.clone().unwrap_or_else(|| JsString::from_utf8("")))
                .unwrap_or_else(|| JsString::from_utf8(""));
            self.emit(Step::ConcatStrConst(quasi));
        }
        Ok(())
    }

    /// A class definition whose heritage or a computed element name contains a
    /// suspension point: the VM sets up the class scope, evaluates the
    /// heritage and each computed name in order (suspending as needed), and
    /// builds the class from the precomputed keys (spec 15.7.14).
    fn compile_class(&mut self, class: &Class) -> Result<(), JsError> {
        let binding = class.name;
        let key_count = class
            .elements
            .iter()
            .filter(|e| has_computed_public_name(e))
            .count();
        self.emit(Step::ClassBegin {
            class: Box::new(class.clone()),
            binding,
            key_count,
        });
        if let Some(heritage) = &class.heritage {
            self.compile_expr(heritage)?;
            self.emit(Step::ClassHeritage);
        }
        for element in &class.elements {
            if let Some(expr) = computed_public_name(element) {
                self.compile_expr(expr)?;
                self.emit(Step::ClassKeyToPropertyKey);
            }
        }
        self.emit(Step::ClassFinish {
            class: Box::new(class.clone()),
            binding,
            key_count,
        });
        Ok(())
    }

    fn compile_object(&mut self, literal: &ObjectLiteral) -> Result<(), JsError> {
        self.emit(Step::ObjectBegin);
        for property in &literal.props {
            match property {
                ObjectProperty::Init {
                    key,
                    value,
                    shorthand,
                } => {
                    let set_name = crate::function::is_anonymous_function_definition(value);
                    match key {
                        PropertyName::Ident(id) => {
                            self.compile_expr(value)?;
                            self.emit(Step::ObjectInitName {
                                name: *id,
                                set_name,
                                shorthand: *shorthand,
                            });
                        }
                        PropertyName::Computed(key_expr) => {
                            self.compile_expr(key_expr)?;
                            self.compile_expr(value)?;
                            self.emit(Step::ObjectInitComputed { set_name });
                        }
                        PropertyName::Str(text) => {
                            self.compile_expr(value)?;
                            let id = crux::intern(text.as_slice());
                            self.emit(Step::ObjectInitName {
                                name: id,
                                set_name,
                                shorthand: false,
                            });
                        }
                        PropertyName::Number(n) => {
                            self.compile_expr(value)?;
                            let text = crux::convert::to_string(&Value::Number(*n))?;
                            let id = crux::intern(text.as_slice());
                            self.emit(Step::ObjectInitName {
                                name: id,
                                set_name,
                                shorthand: false,
                            });
                        }
                    }
                }
                ObjectProperty::Method { key, function } => match key {
                    PropertyName::Ident(id) => {
                        self.emit(Step::ObjectMethodName {
                            name: *id,
                            function: function.clone(),
                        });
                    }
                    PropertyName::Computed(key_expr) => {
                        self.compile_expr(key_expr)?;
                        self.emit(Step::ObjectMethodComputed {
                            function: function.clone(),
                        });
                    }
                    PropertyName::Str(text) => {
                        let id = crux::intern(text.as_slice());
                        self.emit(Step::ObjectMethodName {
                            name: id,
                            function: function.clone(),
                        });
                    }
                    PropertyName::Number(n) => {
                        let text = crux::convert::to_string(&Value::Number(*n))?;
                        let id = crux::intern(text.as_slice());
                        self.emit(Step::ObjectMethodName {
                            name: id,
                            function: function.clone(),
                        });
                    }
                },
                ObjectProperty::Get { key, body } => {
                    self.compile_accessor(key, true, None, body)?;
                }
                ObjectProperty::Set {
                    key,
                    param,
                    init,
                    body,
                } => {
                    let element = BindingElement {
                        pattern: param.clone(),
                        init: init.clone(),
                        rest: false,
                        span: body.span,
                    };
                    self.compile_accessor(key, false, Some(element), body)?;
                }
                ObjectProperty::Spread(expr) => {
                    self.compile_expr(expr)?;
                    self.emit(Step::ObjectSpread);
                }
            }
        }
        Ok(())
    }

    fn compile_accessor(
        &mut self,
        key: &PropertyName,
        get: bool,
        param: Option<BindingElement>,
        body: &syntax::ast::Block,
    ) -> Result<(), JsError> {
        match key {
            PropertyName::Ident(id) => {
                self.emit(Step::ObjectAccessorName {
                    name: *id,
                    get,
                    param,
                    body: body.clone(),
                });
            }
            PropertyName::Computed(key_expr) => {
                self.compile_expr(key_expr)?;
                self.emit(Step::ObjectAccessorComputed {
                    get,
                    param,
                    body: body.clone(),
                });
            }
            PropertyName::Str(text) => {
                let id = crux::intern(text.as_slice());
                self.emit(Step::ObjectAccessorName {
                    name: id,
                    get,
                    param,
                    body: body.clone(),
                });
            }
            PropertyName::Number(n) => {
                let text = crux::convert::to_string(&Value::Number(*n))?;
                let id = crux::intern(text.as_slice());
                self.emit(Step::ObjectAccessorName {
                    name: id,
                    get,
                    param,
                    body: body.clone(),
                });
            }
        }
        Ok(())
    }
}

fn is_compound_assign(op: &AssignOp) -> bool {
    matches!(
        op,
        AssignOp::AddAssign
            | AssignOp::SubAssign
            | AssignOp::MulAssign
            | AssignOp::DivAssign
            | AssignOp::RemAssign
            | AssignOp::ExpAssign
            | AssignOp::LeftShiftAssign
            | AssignOp::RightShiftAssign
            | AssignOp::UnsignedRightShiftAssign
            | AssignOp::BitAndAssign
            | AssignOp::BitXorAssign
            | AssignOp::BitOrAssign
    )
}

/// The continue target of a labelled statement, when it is a loop.
fn labeled_continue_target(body: &Stmt) -> Option<(usize, usize)> {
    // The label scope is resolved against the loop scope when the loop is
    // compiled beneath it; `continue label` uses the innermost loop scope
    // whose break target the label's break target shares. We represent the
    // target by deferring to the loop scope: the label's continue target is
    // marked with a sentinel that continue_target resolves through the loop
    // scopes.
    let is_loop = matches!(
        body.kind,
        StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::ForIn { .. }
            | StmtKind::ForOf { .. }
    );
    if is_loop { Some((usize::MAX, 0)) } else { None }
}

/// Compile a function body for resumable execution.
pub fn compile_body(function: &EcmaFunction) -> Result<CompiledBody, JsError> {
    let mut compiler = Compiler {
        is_async_generator: function.is_generator && function.is_async,
        ..Compiler::default()
    };
    compiler.compile_statements(&function.body.stmts)?;
    compiler.resolve();
    Ok(CompiledBody {
        steps: compiler.steps,
        handlers: compiler.handlers,
        strict: function.strict,
    })
}

/// Compile a statement list standalone (modules and top-level await).
pub fn compile_statements(stmts: &[Stmt], strict: bool) -> Result<CompiledBody, JsError> {
    let mut compiler = Compiler::default();
    compiler.compile_statements(stmts)?;
    compiler.resolve();
    Ok(CompiledBody {
        steps: compiler.steps,
        handlers: compiler.handlers,
        strict,
    })
}
