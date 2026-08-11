//! The resumable-function IR (PLAN §4.5): generator and async function bodies
//! compile to a linear `Vec<Step>` with an explicit instruction pointer, so
//! `yield`/`await` suspend and resume exactly like the spec's generator
//! resume algorithm. This is the seed of the Phase 18 bytecode VM.
//!
//! The compiler batches suspension-free statements (`Step::Stmt`) and
//! expression subtrees (`Step::Expr`) to the existing tree-walking evaluator,
//! and linearizes only the paths that contain suspension points. Control flow
//! is explicit jumps; lexical environments mirror the compiler's scope stack
//! on the VM side; `try`/`catch`/`finally` use a handler table plus a runtime
//! try stack and a pending-control slot.

use std::collections::HashMap;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, is_callable};
use syntax::ast::{
    Argument, ArrayElement, AssignOp, BinaryOp, BindingElement, BindingPattern, Expr, ExprKind,
    ForBinding, ForInit, Function, LogicalOp, MemberProperty, ObjectLiteral, ObjectProperty,
    PropertyName, Stmt, StmtKind, SwitchCase, UnaryOp, UpdateOp, VarDeclKind, VarDeclarator,
};

use crate::agent::Agent;
use crate::context::{
    get_new_target, get_super_base, get_super_constructor, get_this_environment,
    resolve_this_binding,
};
use crate::env::{EnvRef, new_declarative_environment};
use crate::eval::{block_declaration_instantiation, eval_statement};
use crate::expr::{get_iterator, iterator_close, iterator_step};
use crate::flow::Completion;
use crate::function::EcmaFunction;

/// One resumable-function instruction.
#[derive(Debug, Clone)]
pub enum Step {
    // ----- eager evaluation (batched to the tree walker) -----
    /// Evaluate a suspension-free expression subtree and push its value.
    Expr(Expr),
    /// Evaluate a suspension-free statement (updates the completion register).
    Stmt(Stmt),
    // ----- stack -----
    Push(Value),
    Pop,
    Dup,
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
    TypeofTop,
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
    // ----- literals -----
    ArrayBegin,
    ArrayElement,
    ArraySpread,
    ArrayHole,
    ObjectBegin,
    ObjectInitName {
        name: crux::AtomId,
        set_name: bool,
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
        param: Option<BindingPattern>,
        body: syntax::ast::Block,
    },
    ObjectAccessorComputed {
        get: bool,
        param: Option<BindingPattern>,
        body: syntax::ast::Block,
    },
    ObjectSpread,
    PushStr(JsString),
    ConcatStr,
    ConcatStrConst(JsString),
    // ----- arguments -----
    ArgsPush,
    ArgsSpread,
    // ----- statements -----
    EnterBlock {
        stmts: Vec<Stmt>,
    },
    LeaveBlock,
    EnterTry {
        handler: usize,
    },
    Exit {
        after: usize,
    },
    CatchBind {
        param: Option<BindingPattern>,
        stmts: Vec<Stmt>,
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
    // ----- modules -----
    ImportCall {
        has_options: bool,
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
    Yield { value: Value, delegate: bool },
    Await(Value),
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
    pub for_in_stack: Vec<(Vec<Value>, usize)>,
    pub for_of_stack: Vec<crate::expr::IteratorRecord>,
    pub async_for_of_stack: Vec<crate::expr::IteratorRecord>,
    pub yield_star_stack: Vec<YieldStarState>,
    pub switch_disc: Option<Value>,
    pub strict: bool,
}

/// The per-`yield*` delegation state.
#[derive(Debug)]
pub struct YieldStarState {
    pub iterator: crate::expr::IteratorRecord,
    pub received: Value,
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
            for_in_stack: Vec::new(),
            for_of_stack: Vec::new(),
            async_for_of_stack: Vec::new(),
            yield_star_stack: Vec::new(),
            switch_disc: None,
            strict,
        }
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Undefined)
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
                self.stack.push(value);
                self.run_inner(agent, body)
            }
            Resume::Throw(value) => match self.throw_machinery(body, value)? {
                CtlResult::Continue => self.run_inner(agent, body),
                CtlResult::Done(outcome) => Ok(outcome),
            },
            Resume::Return(value) => match self.control_transfer(body, Ctl::Return { value })? {
                CtlResult::Continue => self.run_inner(agent, body),
                CtlResult::Done(outcome) => Ok(outcome),
            },
        }
    }

    fn run_inner(&mut self, agent: &mut Agent, body: &CompiledBody) -> Result<VmOutcome, JsError> {
        loop {
            let steps = &body.steps;
            if self.ip >= steps.len() {
                // Fell off the end: the body completes normally with the
                // statement-list completion value.
                return Ok(VmOutcome::Completed(Completion::Normal(
                    self.completion.clone(),
                )));
            }
            let step = steps.get(self.ip).cloned().ok_or_else(|| {
                JsError::new(
                    ErrorKind::SyntaxError,
                    "Instruction pointer out of bounds".into(),
                )
            })?;
            self.ip += 1;
            if let Ok(context) = agent.running_context_mut() {
                context.lexical_environment = self.lexical_env.clone();
            }
            match step {
                Step::Expr(expr) => {
                    let value = match crate::expr::eval_expr(agent, &expr, self.strict) {
                        Ok(value) => value,
                        Err(error) => {
                            return self.throw_error(agent, body, error);
                        }
                    };
                    self.stack.push(value);
                }
                Step::Stmt(stmt) => {
                    let completion = match eval_statement(agent, &stmt, self.strict) {
                        Ok(completion) => completion,
                        Err(error) => {
                            return self.throw_error(agent, body, error);
                        }
                    };
                    if let Some(outcome) = self.apply_statement_completion(body, completion)? {
                        return Ok(outcome);
                    }
                    if let Ok(context) = agent.running_context_mut() {
                        self.lexical_env = context.lexical_environment.clone();
                    }
                }
                Step::Push(value) => self.stack.push(value),
                Step::Pop => {
                    self.pop();
                }
                Step::Dup => {
                    let top = self.pop();
                    self.stack.push(top.clone());
                    self.stack.push(top);
                }
                Step::Unary(op) => {
                    let operand = self.pop();
                    let value = crate::expr::eval_unary_value(agent, &op, operand)?;
                    self.stack.push(value);
                }
                Step::Binary(op) => {
                    let right = self.pop();
                    let left = self.pop();
                    let value = crate::expr::apply_binary(agent, op, &left, &right)?;
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
                        &crux::lookup(name),
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
                    let key = crux::convert::to_property_key(&key)?;
                    let value =
                        crate::context::get_property_key(agent, &object, &key, object.clone())?;
                    self.stack.push(value);
                }
                Step::GetSuperName { name } => {
                    self.pop(); // base
                    let this = resolve_this_binding(agent)?;
                    let base = get_super_base(agent)?;
                    let value =
                        crate::context::get_property(agent, &base, &crux::lookup(name), this)?;
                    self.stack.push(value);
                }
                Step::GetSuperComputed => {
                    let key = self.pop();
                    self.pop(); // base
                    let key = crux::convert::to_property_key(&key)?;
                    let this = resolve_this_binding(agent)?;
                    let base = get_super_base(agent)?;
                    let value = crate::context::get_property_key(agent, &base, &key, this)?;
                    self.stack.push(value);
                }
                Step::GetPrivate { atom } => {
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, atom)?.id;
                    let value = crate::context::private_get(agent, &object, name_id)?;
                    self.stack.push(value);
                }
                Step::ThisValue => {
                    let this = resolve_this_binding(agent)?;
                    self.stack.push(this);
                }
                Step::GetSuperBase => {
                    let base = get_super_base(agent)?;
                    self.stack.push(base);
                }
                Step::AssignIdent { name, op, set_name } => {
                    self.assign_ident(agent, name, op, set_name)?;
                }
                Step::AssignMemberName { name, op } => {
                    let value = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    self.assign_member(agent, object, PropertyKeyName::Name(name), value, op)?;
                }
                Step::AssignMemberComputed { op } => {
                    let value = self.pop();
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let key = crux::convert::to_property_key(&key)?;
                    self.assign_member(agent, object, PropertyKeyName::Key(key), value, op)?;
                }
                Step::AssignSuperName { name, op } => {
                    let value = self.pop();
                    self.pop(); // base
                    self.assign_super(agent, PropertyKeyName::Name(name), value, op)?;
                }
                Step::AssignSuperComputed { op } => {
                    let value = self.pop();
                    let key = self.pop();
                    self.pop(); // base
                    let key = crux::convert::to_property_key(&key)?;
                    self.assign_super(agent, PropertyKeyName::Key(key), value, op)?;
                }
                Step::AssignPrivate { atom, op } => {
                    let value = self.pop();
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, atom)?.id;
                    let old = crate::context::private_get(agent, &object, name_id)?;
                    let new = match op {
                        AssignOp::Assign
                        | AssignOp::AndAssign
                        | AssignOp::OrAssign
                        | AssignOp::NullishAssign => value.clone(),
                        _ => crate::expr::apply_compound(agent, op, &old, &value)?,
                    };
                    crate::context::private_set(agent, &object, name_id, new.clone())?;
                    self.stack.push(new);
                }
                Step::Destructure { pattern } => {
                    let value = self.pop();
                    crate::binding::binding_initialization(
                        agent,
                        &pattern,
                        value.clone(),
                        None,
                        self.strict,
                    )?;
                    self.stack.push(value);
                }
                Step::UpdateIdent { name, op, prefix } => {
                    let old = self.pop();
                    let new = update_value(agent, &op, &old)?;
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(name), self.strict)?;
                    crate::context::put_value(agent, &reference, new.clone())?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::UpdateMemberName { name, op, prefix } => {
                    let old = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let new = update_value(agent, &op, &old)?;
                    crate::context::put_value(
                        agent,
                        &member_reference(&object, &PropertyKeyName::Name(name), self.strict),
                        new.clone(),
                    )?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::UpdateMemberComputed { op, prefix } => {
                    let old = self.pop();
                    let key = self.pop();
                    let object = self.pop();
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let new = update_value(agent, &op, &old)?;
                    let key = crux::convert::to_property_key(&key)?;
                    crate::context::put_value(
                        agent,
                        &member_reference(&object, &PropertyKeyName::Key(key), self.strict),
                        new.clone(),
                    )?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::UpdateSuperName { name, op, prefix } => {
                    let old = self.pop();
                    self.pop(); // base
                    let new = update_value(agent, &op, &old)?;
                    self.put_super(agent, PropertyKeyName::Name(name), new.clone())?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::UpdateSuperComputed { op, prefix } => {
                    let old = self.pop();
                    let key = self.pop();
                    self.pop(); // base
                    let new = update_value(agent, &op, &old)?;
                    self.put_super(
                        agent,
                        PropertyKeyName::Key(crux::convert::to_property_key(&key)?),
                        new.clone(),
                    )?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::UpdatePrivate { atom, op, prefix } => {
                    let old = self.pop();
                    let object = self.pop();
                    let new = update_value(agent, &op, &old)?;
                    let name_id = crate::context::resolve_private_name(agent, atom)?.id;
                    crate::context::private_set(agent, &object, name_id, new.clone())?;
                    self.stack.push(if prefix { new } else { old });
                }
                Step::DeleteIdent { name } => {
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(name), self.strict)?;
                    let deleted = crate::context::delete_property_or_throw(&reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::DeleteMemberName { name } => {
                    let object = self.pop();
                    let reference = crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(object),
                        name: PropertyKey::from_js_string(&crux::lookup(name)),
                        strict: self.strict,
                        this_value: None,
                        private_name: None,
                    };
                    let deleted = crate::context::delete_property_or_throw(&reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::DeleteMemberComputed => {
                    let key = self.pop();
                    let object = self.pop();
                    let key = crux::convert::to_property_key(&key)?;
                    let reference = crate::context::Reference {
                        base: crate::context::ReferenceBase::Value(object),
                        name: key,
                        strict: self.strict,
                        this_value: None,
                        private_name: None,
                    };
                    let deleted = crate::context::delete_property_or_throw(&reference)?;
                    self.stack.push(Value::Boolean(deleted));
                }
                Step::TypeofTop => {
                    let value = self.pop();
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf8(
                            crux::value::type_of(&value),
                        ))));
                }
                Step::PrivateIn { atom } => {
                    let object = self.pop();
                    let name_id = crate::context::resolve_private_name(agent, atom)?.id;
                    self.stack.push(Value::Boolean(crate::context::private_in(
                        &object, name_id,
                    )?));
                }
                Step::Call { direct_eval } => {
                    self.do_call(agent, direct_eval)?;
                }
                Step::SuperCall => {
                    let args = std::mem::take(&mut self.args);
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
                    let args = std::mem::take(&mut self.args);
                    let result = crate::function::construct(agent, &callee, &args, &callee)?;
                    self.stack.push(result);
                }
                Step::TaggedTemplate(template) => {
                    let tag = self.pop();
                    let substitutions = std::mem::take(&mut self.args);
                    let value = tagged_template(agent, tag, &template, substitutions)?;
                    self.stack.push(value);
                }
                Step::ArrayBegin => {
                    let array = crux::object::JsObject::array_create(None, 0.0)?;
                    self.stack.push(Value::Object(array));
                }
                Step::ArrayElement => {
                    let value = self.pop();
                    let array = self.pop();
                    let index = array_length(agent, &array)?;
                    array_set(&array, &index.to_string(), value)?;
                    self.stack.push(array);
                }
                Step::ArraySpread => {
                    let iterable = self.pop();
                    let array = self.pop();
                    let iterator = get_iterator(agent, &iterable)?;
                    while let Some(value) = iterator_step(agent, &iterator)? {
                        let index = array_length(agent, &array)?;
                        array_set(&array, &index.to_string(), value)?;
                    }
                    self.stack.push(array);
                }
                Step::ArrayHole => {
                    let array = self.pop();
                    let length = array_length(agent, &array)?;
                    array_set(&array, "length", Value::Number(length + 1.0))?;
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
                Step::ObjectInitName { name, set_name } => {
                    let value = self.pop();
                    let object = self.pop();
                    object_init(agent, &object, &PropertyName::Ident(name), value, set_name)?;
                    self.stack.push(object);
                }
                Step::ObjectInitComputed { set_name } => {
                    let value = self.pop();
                    let key = self.pop();
                    let object = self.pop();
                    let name = property_name_from_value(key)?;
                    object_init(agent, &object, &name, value, set_name)?;
                    self.stack.push(object);
                }
                Step::ObjectMethodName { name, function } => {
                    let object = self.pop();
                    object_method(agent, &object, &PropertyName::Ident(name), &function)?;
                    self.stack.push(object);
                }
                Step::ObjectMethodComputed { function } => {
                    let key = self.pop();
                    let object = self.pop();
                    let name = property_name_from_value(key)?;
                    object_method(agent, &object, &name, &function)?;
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
                        &PropertyName::Ident(name),
                        get,
                        param,
                        &body,
                    )?;
                    self.stack.push(object);
                }
                Step::ObjectAccessorComputed { get, param, body } => {
                    let key = self.pop();
                    let object = self.pop();
                    let name = property_name_from_value(key)?;
                    object_accessor(agent, &object, &name, get, param, &body)?;
                    self.stack.push(object);
                }
                Step::ObjectSpread => {
                    let from = self.pop();
                    let object = self.pop();
                    let Value::Object(obj) = &object else {
                        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
                    };
                    crate::expr::copy_data_properties(obj, &from)?;
                    self.stack.push(object);
                }
                Step::PushStr(text) => {
                    self.stack.push(Value::String(Handle::new(text)));
                }
                Step::ConcatStr => {
                    let value = self.pop();
                    let acc = self.pop();
                    let text = crux::convert::to_string(&value)?.to_string_lossy();
                    let acc_text = string_of(&acc);
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf8(&format!(
                            "{acc_text}{text}"
                        )))));
                }
                Step::ConcatStrConst(text) => {
                    let acc = self.pop();
                    let acc_text = string_of(&acc);
                    self.stack
                        .push(Value::String(Handle::new(JsString::from_utf8(&format!(
                            "{acc_text}{}",
                            text.to_string_lossy()
                        )))));
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
                Step::EnterBlock { stmts } => {
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    block_declaration_instantiation(agent, &stmts, &env, self.strict)?;
                    self.lexical_env = env.clone();
                    self.env_stack.push(env);
                }
                Step::LeaveBlock => {
                    let popped = self.env_stack.pop().ok_or_else(|| {
                        JsError::new(ErrorKind::SyntaxError, "Environment stack underflow".into())
                    })?;
                    // Restore to the popped environment's outer, which may
                    // differ from the stack's previous entry (per-iteration
                    // environments live outside the stack).
                    self.lexical_env = popped.outer().unwrap_or(popped);
                }
                Step::EnterTry { handler } => {
                    self.try_stack.push(TryFrame {
                        handler,
                        saved_env: self.lexical_env.clone(),
                        env_depth: self.env_stack.len(),
                    });
                }
                Step::Exit { after } => {
                    match self.control_transfer(body, Ctl::Normal { after })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::CatchBind { param, stmts } => {
                    let thrown = self.thrown.take().unwrap_or(Value::Undefined);
                    let old_env = self.lexical_env.clone();
                    let env = new_declarative_environment(Some(old_env));
                    if let Some(param) = &param {
                        let BindingPattern::Ident(name) = param else {
                            return Err(JsError::new(
                                ErrorKind::TypeError,
                                "catch destructuring is not supported in resumable bodies".into(),
                            ));
                        };
                        let name = crux::lookup(*name);
                        env.create_mutable_binding(&name, false)?;
                        env.initialize_binding(&name, thrown)?;
                    }
                    block_declaration_instantiation(agent, &stmts, &env, self.strict)?;
                    self.lexical_env = env.clone();
                    self.env_stack.push(env);
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
                    match self.control_transfer(body, ctl)? {
                        CtlResult::Continue => {}
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::EnterWith => {
                    let object = self.pop();
                    let Value::Object(obj) = object else {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "Cannot use 'with' on a non-object value".into(),
                        ));
                    };
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
                    for name in &names {
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
                    for decl in &decls {
                        let mut names = Vec::new();
                        crate::script::bound_names(&decl.pattern, &mut names);
                        for name in &names {
                            if kind == VarDeclKind::Const {
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
                    let keys = crate::eval::for_in_keys(agent, &rhs)?;
                    self.for_in_stack.push((keys, 0));
                }
                Step::ForInNext { done } => {
                    let Some((keys, index)) = self.for_in_stack.last_mut() else {
                        return Err(JsError::new(
                            ErrorKind::SyntaxError,
                            "ForInNext without a for-in".into(),
                        ));
                    };
                    if *index >= keys.len() {
                        self.for_in_stack.pop();
                        self.ip = done;
                    } else {
                        let key = keys[*index].clone();
                        *index += 1;
                        self.stack.push(key);
                    }
                }
                Step::ForInBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, &left, value)?;
                }
                Step::ForInRestore => {
                    self.restore_per_iteration();
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
                    match iterator_step(agent, &iterator)? {
                        Some(value) => self.stack.push(value),
                        None => {
                            self.for_of_stack.pop();
                            self.ip = done;
                        }
                    }
                }
                Step::ForOfBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, &left, value)?;
                }
                Step::ForOfRestore => {
                    self.restore_per_iteration();
                }
                Step::ForOfClose => {
                    if let Some(iterator) = self.for_of_stack.pop() {
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
                    if iterator_result_done(agent, &result)? {
                        self.async_for_of_stack.pop();
                        self.ip = done;
                    } else {
                        let value = iterator_result_value(agent, &result)?;
                        self.stack.push(value);
                    }
                }
                Step::AsyncForOfBind { left } => {
                    let value = self.pop();
                    self.for_binding_put(agent, &left, value)?;
                }
                Step::AsyncForOfRestore => {
                    self.restore_per_iteration();
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
                        self.ip = case;
                    }
                }
                Step::SetCompletion => {
                    let value = self.pop();
                    self.completion = value;
                }
                Step::Jump(target) => self.ip = target,
                Step::JumpIfFalse(target) => {
                    let value = self.pop();
                    if !crux::convert::to_boolean(&value) {
                        self.ip = target;
                    }
                }
                Step::JumpIfTrue(target) => {
                    let value = self.pop();
                    if crux::convert::to_boolean(&value) {
                        self.ip = target;
                    }
                }
                Step::JumpIfFalseKeep(target) => {
                    let value = self.pop();
                    if !crux::convert::to_boolean(&value) {
                        self.stack.push(value);
                        self.ip = target;
                    }
                }
                Step::JumpIfTrueKeep(target) => {
                    let value = self.pop();
                    if crux::convert::to_boolean(&value) {
                        self.stack.push(value);
                        self.ip = target;
                    }
                }
                Step::JumpIfNullishKeep(target) => {
                    let value = self.pop();
                    if is_nullish(&value) {
                        self.stack.push(value);
                        self.ip = target;
                    }
                }
                Step::JumpIfNotNullishKeep(target) => {
                    let value = self.pop();
                    if !is_nullish(&value) {
                        self.stack.push(value);
                        self.ip = target;
                    }
                }
                Step::Return => {
                    let value = self.pop();
                    match self.control_transfer(body, Ctl::Return { value })? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Throw => {
                    let value = self.pop();
                    match self.throw_machinery(body, value)? {
                        CtlResult::Continue => continue,
                        CtlResult::Done(outcome) => return Ok(outcome),
                    }
                }
                Step::Yield { delegate } => {
                    let value = self.pop();
                    return Ok(VmOutcome::Suspended(Suspension::Yield { value, delegate }));
                }
                Step::Await => {
                    let value = self.pop();
                    return Ok(VmOutcome::Suspended(Suspension::Await(value)));
                }
                Step::YieldStarBegin => {
                    let value = self.pop();
                    let iterator = get_iterator(agent, &value)?;
                    self.yield_star_stack.push(YieldStarState {
                        iterator,
                        received: Value::Undefined,
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
                    let result = crate::function::call(
                        agent,
                        &iterator.next,
                        iterator.iterator.clone(),
                        &[received],
                    )?;
                    if iterator_result_done(agent, &result)? {
                        let value = iterator_result_value(agent, &result)?;
                        self.yield_star_stack.pop();
                        self.stack.push(value);
                        self.ip = done;
                    } else {
                        let value = iterator_result_value(agent, &result)?;
                        self.stack.push(value);
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
                            self.ip = loop_top;
                        }
                        Some(ResumeAbrupt::Throw(value)) => {
                            let Some(state) = self.yield_star_stack.last() else {
                                return Err(JsError::new(
                                    ErrorKind::SyntaxError,
                                    "YieldStarResume without a delegation".into(),
                                ));
                            };
                            let iterator = state.iterator.clone();
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
                                if iterator_result_done(agent, &inner)? {
                                    let value = iterator_result_value(agent, &inner)?;
                                    self.yield_star_stack.pop();
                                    self.stack.push(value);
                                    self.ip = done;
                                } else {
                                    let value = iterator_result_value(agent, &inner)?;
                                    self.stack.push(value);
                                    self.ip = yield_at;
                                }
                            } else {
                                // IteratorClose then rethrow.
                                iterator_close(agent, &iterator)?;
                                self.yield_star_stack.pop();
                                match self.throw_machinery(body, value)? {
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
                                if iterator_result_done(agent, &inner)? {
                                    let value = iterator_result_value(agent, &inner)?;
                                    self.yield_star_stack.pop();
                                    self.stack.push(value);
                                    self.ip = done;
                                } else {
                                    let value = iterator_result_value(agent, &inner)?;
                                    self.stack.push(value);
                                    self.ip = yield_at;
                                }
                            } else {
                                // No return method: the delegation completes.
                                self.yield_star_stack.pop();
                                self.stack.push(value);
                                self.ip = done;
                            }
                        }
                    }
                }
                Step::ImportCall { has_options } => {
                    let options = if has_options { Some(self.pop()) } else { None };
                    let specifier = self.pop();
                    let promise =
                        crate::module::dynamic_import(agent, &specifier, options.as_ref())?;
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
        let value = error
            .value
            .clone()
            .unwrap_or_else(|| error_message_value(&error));
        match self.throw_machinery(body, value)? {
            CtlResult::Continue => self.run_inner(agent, body),
            CtlResult::Done(outcome) => Ok(outcome),
        }
    }

    /// Deliver a batched statement's completion; `Some(outcome)` means the VM
    /// run finished.
    fn apply_statement_completion(
        &mut self,
        body: &CompiledBody,
        completion: Completion,
    ) -> Result<Option<VmOutcome>, JsError> {
        match completion {
            Completion::Normal(value) => self.completion = value,
            Completion::Empty => {}
            Completion::Break { .. } | Completion::Continue { .. } => {
                return Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Illegal control flow in a batched statement".into(),
                ));
            }
            Completion::Return(value) => {
                return match self.control_transfer(body, Ctl::Return { value })? {
                    CtlResult::Continue => Ok(None),
                    CtlResult::Done(outcome) => Ok(Some(outcome)),
                };
            }
            Completion::Throw(value) => {
                return match self.throw_machinery(body, value)? {
                    CtlResult::Continue => Ok(None),
                    CtlResult::Done(outcome) => Ok(Some(outcome)),
                };
            }
        }
        Ok(None)
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
                    crate::function::set_function_name(&value, &crux::lookup(name), None)?;
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
                let old = self.pop();
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
        key: PropertyKeyName,
        value: Value,
        op: AssignOp,
    ) -> Result<(), JsError> {
        let base = get_super_base(agent)?;
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
                let old = self.pop();
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
        key: PropertyKeyName,
        value: Value,
    ) -> Result<(), JsError> {
        let base = get_super_base(agent)?;
        let this = resolve_this_binding(agent)?;
        let reference = super_reference(&base, &key, self.strict, &this);
        crate::context::put_value(agent, &reference, value)
    }

    fn do_call(&mut self, agent: &mut Agent, direct_eval: bool) -> Result<(), JsError> {
        let callee = self.pop();
        let this = self.pop();
        let args = std::mem::take(&mut self.args);
        if is_eval_function(agent, &callee)? {
            let source = args.first().cloned().unwrap_or(Value::Undefined);
            let source = crux::convert::to_string(&source)?;
            let result = crate::script::perform_eval(
                agent,
                &source.to_string_lossy(),
                self.strict,
                direct_eval,
            )?;
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
                        if *kind == VarDeclKind::Const {
                            env.create_immutable_binding(name, true)?;
                        } else {
                            env.create_mutable_binding(name, false)?;
                        }
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
                    // restore step is a no-op.
                    self.lexical_env = env;
                }
            }
        }
        Ok(())
    }

    /// Pop a per-iteration environment created by a for-in/for-of bind.
    fn restore_per_iteration(&mut self) {
        // The per-iteration environment lives in the lexical environment, not
        // the stack; the next iteration's bind step replaces it.
    }

    /// The control-transfer machinery: route through pending finallys, then
    /// apply the control.
    fn control_transfer(&mut self, body: &CompiledBody, ctl: Ctl) -> Result<CtlResult, JsError> {
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
            if self.ip < handler.start || self.ip >= covered_end {
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
    fn throw_machinery(&mut self, body: &CompiledBody, value: Value) -> Result<CtlResult, JsError> {
        let value = value;
        loop {
            let decision = {
                let mut found: Option<(usize, ThrowAction)> = None;
                for (i, frame) in self.try_stack.iter().enumerate().rev() {
                    let Some(handler) = body.handlers.get(frame.handler) else {
                        continue;
                    };
                    if self.ip < handler.start || self.ip >= handler.try_end {
                        continue;
                    }
                    found = Some((
                        i,
                        if handler.catch.is_some() {
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
                    let frame = self.try_stack.remove(index);
                    self.restore_env(frame.saved_env, frame.env_depth);
                    self.thrown = Some(value);
                    let catch_start = body
                        .handlers
                        .get(frame.handler)
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
                    return Ok(CtlResult::Done(VmOutcome::Completed(Completion::Throw(
                        value,
                    ))));
                }
            }
        }
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
    matches!(value, Value::Undefined | Value::Null)
}

fn nullish_error(what: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, what.into())
}

fn string_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string_lossy(),
        other => crux::convert::to_string(other)
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|_| "undefined".into()),
    }
}

fn error_message_value(error: &JsError) -> Value {
    Value::String(Handle::new(JsString::from_utf8(&error.message)))
}

fn update_value(_agent: &mut Agent, op: &UpdateOp, old: &Value) -> Result<Value, JsError> {
    let old_numeric = crux::convert::to_numeric(old)?;
    match old_numeric {
        Value::Number(n) => {
            let delta = if matches!(op, UpdateOp::Increment) {
                1.0
            } else {
                -1.0
            };
            Ok(Value::Number(n + delta))
        }
        Value::BigInt(b) => {
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

fn property_name_from_value(value: Value) -> Result<PropertyName, JsError> {
    let key = crux::convert::to_property_key(&value)?;
    match key {
        PropertyKey::String(id) => Ok(PropertyName::Ident(id)),
        PropertyKey::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "symbol property keys are not supported in object literals yet".into(),
        )),
    }
}

fn array_length(agent: &mut Agent, array: &Value) -> Result<f64, JsError> {
    let length =
        crate::context::get_property(agent, array, &JsString::from_utf8("length"), array.clone())?;
    crux::convert::to_number(&length)
}

fn array_set(array: &Value, key: &str, value: Value) -> Result<(), JsError> {
    let Value::Object(obj) = array else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    obj.create_data_property(&JsString::from_utf8(key), value)?;
    Ok(())
}

/// Object literal `Init` property definition (spec 13.2.5.5 step 4): the
/// `__proto__` prototype-setter special case plus name inference.
fn object_init(
    agent: &mut Agent,
    object: &Value,
    key: &PropertyName,
    value: Value,
    set_name: bool,
) -> Result<(), JsError> {
    let Value::Object(obj) = object else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let name = match key {
        PropertyName::Ident(id) => crux::lookup(*id),
        PropertyName::Str(text) => text.clone(),
        PropertyName::Number(n) => crux::convert::to_string(&Value::Number(*n))?,
        PropertyName::Computed(expr) => {
            let key = crate::expr::eval_expr(agent, expr, false)?;
            let key = crux::convert::to_property_key(&key)?;
            return object_init_key(agent, obj, key, value, set_name);
        }
    };
    if set_name {
        crate::function::set_function_name(&value, &name, None)?;
    }
    let name_text = name.to_string_lossy();
    if name_text == "__proto__" {
        match value {
            Value::Object(proto) => {
                if !obj.set_prototype_of(Some(proto))? {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set prototype of non-extensible object".into(),
                    ));
                }
            }
            Value::Null => {
                if !obj.set_prototype_of(None)? {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set prototype of non-extensible object".into(),
                    ));
                }
            }
            _ => {
                obj.create_data_property(&name, value)?;
            }
        }
    } else {
        obj.create_data_property(&name, value)?;
    }
    Ok(())
}

fn object_init_key(
    _agent: &mut Agent,
    obj: &crux::object::JsObject,
    key: PropertyKey,
    value: Value,
    set_name: bool,
) -> Result<(), JsError> {
    if set_name {
        let name = match &key {
            PropertyKey::String(id) => crux::lookup(*id),
            PropertyKey::Symbol(_) => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "symbol property keys are not supported in object literals yet".into(),
                ));
            }
        };
        crate::function::set_function_name(&value, &name, None)?;
    }
    let name_text = match &key {
        PropertyKey::String(id) => crux::lookup(*id).to_string_lossy(),
        PropertyKey::Symbol(_) => String::new(),
    };
    if name_text == "__proto__" {
        match value {
            Value::Object(proto) => {
                if !obj.set_prototype_of(Some(proto))? {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set prototype of non-extensible object".into(),
                    ));
                }
            }
            Value::Null => {
                if !obj.set_prototype_of(None)? {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set prototype of non-extensible object".into(),
                    ));
                }
            }
            _ => {
                obj.create_data_property_key(&key, value)?;
            }
        }
    } else {
        obj.create_data_property_key(&key, value)?;
    }
    Ok(())
}

/// MethodDefinition evaluation (spec 15.4.3) for the IR object literal steps.
fn object_method(
    agent: &mut Agent,
    object: &Value,
    key: &PropertyName,
    function: &Function,
) -> Result<(), JsError> {
    let Value::Object(obj) = object else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let name = match key {
        PropertyName::Ident(id) => crux::lookup(*id),
        PropertyName::Str(text) => text.clone(),
        PropertyName::Number(n) => crux::convert::to_string(&Value::Number(*n))?,
        PropertyName::Computed(expr) => {
            let key = crate::expr::eval_expr(agent, expr, false)?;
            let key = crux::convert::to_property_key(&key)?;
            let PropertyKey::String(id) = key else {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "symbol property keys are not supported yet".into(),
                ));
            };
            crux::lookup(id)
        }
    };
    let env = agent.running_context()?.lexical_environment.clone();
    let closure = crate::function::instantiate_method(agent, function, env, false)?;
    crate::function::make_method(agent, &closure, Value::Object(obj.clone()))?;
    crate::function::set_function_name(&closure, &name, None)?;
    obj.create_data_property(&name, closure)?;
    Ok(())
}

/// Accessor definition (get/set PropertyDefinition) for the IR steps.
#[allow(clippy::too_many_arguments)]
fn object_accessor(
    agent: &mut Agent,
    object: &Value,
    key: &PropertyName,
    get: bool,
    param: Option<BindingPattern>,
    body: &syntax::ast::Block,
) -> Result<(), JsError> {
    let Value::Object(obj) = object else {
        return Err(JsError::new(ErrorKind::TypeError, "not an object".into()));
    };
    let name = match key {
        PropertyName::Ident(id) => crux::lookup(*id),
        PropertyName::Str(text) => text.clone(),
        PropertyName::Number(n) => crux::convert::to_string(&Value::Number(*n))?,
        PropertyName::Computed(expr) => {
            let key = crate::expr::eval_expr(agent, expr, false)?;
            let key = crux::convert::to_property_key(&key)?;
            let PropertyKey::String(id) = key else {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "symbol property keys are not supported yet".into(),
                ));
            };
            crux::lookup(id)
        }
    };
    let env = agent.running_context()?.lexical_environment.clone();
    let params = if let Some(param) = param {
        vec![BindingElement {
            pattern: param,
            init: None,
            rest: false,
            span: body.span,
        }]
    } else {
        Vec::new()
    };
    let closure = crate::function::instantiate_accessor(agent, params, body.clone(), env, false)?;
    crate::function::make_method(agent, &closure, Value::Object(obj.clone()))?;
    let prefix = if get { Some("get") } else { Some("set") };
    crate::function::set_function_name(&closure, &name, prefix)?;
    let descriptor = crux::property::PropertyDescriptor {
        value: None,
        writable: None,
        get: if get { Some(closure.clone()) } else { None },
        set: if get { None } else { Some(closure) },
        enumerable: Some(true),
        configurable: Some(true),
    };
    obj.define_property(&name, &descriptor)?;
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
    let template_object = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    let raw = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    for (index, quasi) in template.quasis.iter().enumerate() {
        let cooked = quasi
            .cooked
            .clone()
            .unwrap_or_else(|| JsString::from_utf8(""));
        template_object.create_data_property(
            &JsString::from_utf8(&index.to_string()),
            Value::String(Handle::new(cooked)),
        )?;
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
                        let element = pattern_of_target(value)?;
                        properties.push(syntax::ast::ObjectBindingProperty::Property {
                            key: key.clone(),
                            element: BindingElement {
                                pattern: element,
                                init: None,
                                rest: false,
                                span: target.span,
                            },
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
                        let pattern = pattern_of_target(expr)?;
                        elements.push(syntax::ast::ArrayBindingElement::Element(BindingElement {
                            pattern,
                            init: None,
                            rest: false,
                            span: target.span,
                        }));
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

// ---------------------------------------------------------------------------
// Suspension detection
// ---------------------------------------------------------------------------

/// Whether an expression contains a suspension point (`yield`/`await`) or a
/// construct the VM must linearize. Nested function/class bodies are separate
/// resumable units and never count.
pub fn expr_contains_suspension(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Yield { .. } | ExprKind::Await(_) => true,
        ExprKind::Function(_) | ExprKind::Arrow { .. } | ExprKind::Class(_) => false,
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
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => decls
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_contains_suspension)),
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
    Exit(usize, usize),
    ForInNext(usize, usize),
    ForOfNext(usize, usize),
    AsyncForOfNext(usize, usize),
    SwitchTest(usize, usize),
    YieldStarNext(usize, usize),
    YieldStarResume(usize, usize, usize, usize),
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

    fn jump_if_not_nullish_keep(&mut self, target: usize) {
        let index = self.steps.len();
        self.steps.push(Step::JumpIfNotNullishKeep(0));
        self.fixups.push(Fixup::JumpIfNotNullishKeep(index, target));
    }

    fn leave_scopes(&mut self, count: usize) {
        for _ in 0..count {
            self.emit(Step::LeaveBlock);
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
            if !stmt_contains_suspension(stmt) {
                self.emit(Step::Stmt(stmt.clone()));
                continue;
            }
            self.compile_statement(stmt)?;
        }
        Ok(())
    }

    fn compile_statement(&mut self, stmt: &Stmt) -> Result<(), JsError> {
        match &stmt.kind {
            StmtKind::Block(block) => {
                self.emit(Step::EnterBlock {
                    stmts: block.stmts.clone(),
                });
                self.scope_count += 1;
                self.compile_statements(&block.stmts)?;
                self.scope_count -= 1;
                self.emit(Step::LeaveBlock);
            }
            StmtKind::Expr(expr) => {
                self.compile_expr(expr)?;
                self.emit(Step::SetCompletion);
            }
            StmtKind::VarDecl { decls, .. } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        self.compile_expr(init)?;
                        self.emit(Step::Destructure {
                            pattern: decl.pattern.clone(),
                        });
                    }
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        self.compile_expr(init)?;
                        self.emit(Step::Destructure {
                            pattern: decl.pattern.clone(),
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
            }
            StmtKind::While { test, body } => {
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
                self.place(test_label);
                self.compile_expr(test)?;
                self.jump_if_false(end_label);
                self.compile_statement(body)?;
                self.jump(test_label);
                self.place(end_label);
                self.scope_stack.pop();
            }
            StmtKind::DoWhile { body, test } => {
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
                self.place(body_label);
                self.compile_statement(body)?;
                self.place(test_label);
                self.compile_expr(test)?;
                self.jump_if_true(body_label);
                self.place(end_label);
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
                self.scope_stack.pop();
            }
            StmtKind::Break(label) => {
                let (target, count) = self.break_target(label.as_ref())?;
                self.leave_scopes(self.scope_count.saturating_sub(count));
                self.jump(target);
            }
            StmtKind::Continue(label) => {
                let (target, count) = self.continue_target(label.as_ref())?;
                self.leave_scopes(self.scope_count.saturating_sub(count));
                self.jump(target);
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
                self.compile_expr(object)?;
                self.emit(Step::EnterWith);
                self.scope_count += 1;
                self.compile_statement(body)?;
                self.scope_count -= 1;
                self.emit(Step::LeaveBlock);
            }
            _ => {
                // Empty, Debugger, FunctionDecl, ClassDecl.
                self.emit(Step::Stmt(stmt.clone()));
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
        self.compile_expr(right)?;
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
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_for_of(
        &mut self,
        left: &ForBinding,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.compile_expr(right)?;
        self.emit(Step::ForOfBegin);
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
        self.place(top_label);
        let step_index = self.steps.len();
        self.emit(Step::ForOfNext { done: 0 });
        self.fixups.push(Fixup::ForOfNext(step_index, end_label));
        self.emit(Step::ForOfBind { left: left.clone() });
        self.compile_statement(body)?;
        self.emit(Step::ForOfRestore);
        self.place(continue_label);
        self.jump(top_label);
        self.place(end_label);
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_async_for_of(
        &mut self,
        left: &ForBinding,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), JsError> {
        self.compile_expr(right)?;
        self.emit(Step::AsyncForOfBegin);
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
        self.place(top_label);
        self.emit(Step::AsyncForOfNext);
        let step_index = self.steps.len();
        self.emit(Step::AsyncForOfTest { done: 0 });
        self.fixups
            .push(Fixup::AsyncForOfNext(step_index, end_label));
        self.emit(Step::AsyncForOfBind { left: left.clone() });
        self.compile_statement(body)?;
        self.emit(Step::AsyncForOfRestore);
        self.place(continue_label);
        self.jump(top_label);
        self.place(end_label);
        self.scope_stack.pop();
        Ok(())
    }

    fn compile_switch(&mut self, discriminant: &Expr, cases: &[SwitchCase]) -> Result<(), JsError> {
        self.compile_expr(discriminant)?;
        self.emit(Step::SwitchDisc);
        let all_stmts: Vec<Stmt> = cases.iter().flat_map(|c| c.consequent.clone()).collect();
        self.emit(Step::EnterBlock { stmts: all_stmts });
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
        for (index, case) in cases.iter().enumerate() {
            self.place(case_labels[index]);
            self.compile_statements(&case.consequent)?;
        }
        self.place(default_label);
        self.scope_stack.pop();
        self.scope_count -= 1;
        self.emit(Step::LeaveBlock);
        self.place(end_label);
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
        self.emit(Step::EnterBlock {
            stmts: block.stmts.clone(),
        });
        self.scope_count += 1;
        self.compile_statements(&block.stmts)?;
        self.scope_count -= 1;
        self.emit(Step::LeaveBlock);
        let exit_index = self.steps.len();
        self.emit(Step::Exit { after: 0 });
        let after_label = self.new_label();
        self.fixups.push(Fixup::Exit(exit_index, after_label));
        self.handlers[handler_index].try_end = exit_index;

        if let Some(handler) = handler {
            let catch_start = self.steps.len();
            self.emit(Step::CatchBind {
                param: handler.param.clone(),
                stmts: handler.body.stmts.clone(),
            });
            self.scope_count += 1;
            self.compile_statements(&handler.body.stmts)?;
            self.scope_count -= 1;
            self.emit(Step::LeaveBlock);
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
            self.emit(Step::EnterBlock {
                stmts: finalizer.stmts.clone(),
            });
            self.scope_count += 1;
            self.compile_statements(&finalizer.stmts)?;
            self.scope_count -= 1;
            self.emit(Step::LeaveBlock);
            self.emit(Step::FinallyEnd);
        }
        self.place(after_label);
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), JsError> {
        if !expr_contains_suspension(expr) {
            self.emit(Step::Expr(expr.clone()));
            return Ok(());
        }
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
            ExprKind::Unary { op, operand } => match op {
                UnaryOp::Delete => self.compile_delete(operand),
                UnaryOp::Void => {
                    self.compile_expr(operand)?;
                    self.emit(Step::Pop);
                    self.emit(Step::Push(Value::Undefined));
                    Ok(())
                }
                UnaryOp::Typeof => {
                    self.compile_expr(operand)?;
                    self.emit(Step::TypeofTop);
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
            ExprKind::Call(call) => self.compile_call(call),
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
            ExprKind::Member(member) => self.compile_member(member),
            ExprKind::TaggedTemplate { tag, quasi } => {
                self.compile_expr(tag)?;
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
                Ok(())
            }
            ExprKind::Object(literal) => self.compile_object(literal),
            ExprKind::Yield { delegate, argument } => {
                match argument {
                    Some(argument) => self.compile_expr(argument)?,
                    None => self.emit(Step::Push(Value::Undefined)),
                }
                if *delegate {
                    // yield* (spec 14.5.5): the delegation loop.
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
            ExprKind::ImportCall { specifier, options } => {
                self.compile_expr(specifier)?;
                let has_options = options.is_some();
                if let Some(options) = options {
                    self.compile_expr(options)?;
                }
                self.emit(Step::ImportCall { has_options });
                Ok(())
            }
            ExprKind::MetaProperty { meta, property } => {
                let meta_name = crux::lookup(*meta).to_string_lossy();
                let property_name = crux::lookup(*property).to_string_lossy();
                if meta_name == "import" && property_name == "meta" {
                    self.emit(Step::ImportMeta);
                } else {
                    self.emit(Step::Expr(expr.clone()));
                }
                Ok(())
            }
            _ => {
                self.emit(Step::Expr(expr.clone()));
                Ok(())
            }
        }
    }

    fn compile_delete(&mut self, operand: &Expr) -> Result<(), JsError> {
        match &operand.kind {
            ExprKind::Ident(name) => {
                self.emit(Step::DeleteIdent { name: *name });
                Ok(())
            }
            ExprKind::Member(member) => {
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
                self.emit(Step::Expr(target.clone()));
                self.emit(Step::UpdateIdent {
                    name: *name,
                    op: *op,
                    prefix,
                });
            }
            ExprKind::Member(member) => {
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
                        self.emit(Step::Dup);
                        self.emit(Step::Dup);
                        self.emit(Step::GetMemberComputed);
                        self.emit(Step::UpdateMemberComputed { op: *op, prefix });
                    }
                    MemberProperty::Private(atom) => {
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
            let pattern = pattern_of_target(target)?;
            self.emit(Step::Destructure { pattern });
            return Ok(());
        }
        let set_name = matches!(target.kind, ExprKind::Ident(_))
            && crate::function::is_anonymous_function_definition(value);
        match &target.kind {
            ExprKind::Ident(name) => {
                match op {
                    AssignOp::Assign => {
                        self.compile_expr(value)?;
                        self.emit(Step::AssignIdent {
                            name: *name,
                            op: *op,
                            set_name,
                        });
                    }
                    AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign => {
                        self.emit(Step::Expr(target.clone()));
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
                        self.emit(Step::Expr(target.clone()));
                        self.compile_expr(value)?;
                        self.emit(Step::AssignIdent {
                            name: *name,
                            op: *op,
                            set_name,
                        });
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

    fn compile_member_assign(
        &mut self,
        member: &syntax::ast::MemberExpr,
        op: &AssignOp,
        value: &Expr,
    ) -> Result<(), JsError> {
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
                        self.emit(Step::Dup);
                        self.emit(Step::Dup);
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
                    self.emit(Step::Dup);
                    self.emit(Step::Dup);
                    self.emit(Step::GetMemberComputed);
                }
                self.compile_expr(value)?;
                self.emit(Step::AssignMemberComputed { op: *op });
            }
            MemberProperty::Private(atom) => {
                self.compile_expr(&member.object)?;
                self.emit(Step::GetPrivate { atom: *atom });
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
            self.compile_member_property(member)?;
            self.jump(end);
            self.place(short);
            self.emit(Step::Pop);
            self.emit(Step::Pop);
            self.emit(Step::Push(Value::Undefined));
            self.place(end);
        } else {
            self.compile_expr(&member.object)?;
            self.compile_member_property(member)?;
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
            self.compile_member_property(member)?;
            if call.optional {
                // The callee is on top: nullish → undefined.
                let short = self.new_label();
                let end = self.new_label();
                self.emit(Step::Dup);
                self.jump_if_nullish_keep(short);
                self.compile_arguments(&call.args)?;
                self.emit(Step::Call { direct_eval: false });
                self.jump(end);
                self.place(short);
                self.emit(Step::Pop);
                self.emit(Step::Pop);
                self.emit(Step::Push(Value::Undefined));
                self.place(end);
            } else {
                self.compile_arguments(&call.args)?;
                self.emit(Step::Call { direct_eval: false });
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
            self.compile_arguments(&call.args)?;
            self.emit(Step::Call { direct_eval });
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
        self.compile_arguments(args)?;
        self.emit(Step::Call { direct_eval: false });
        self.jump(end);
        self.place(short);
        self.emit(Step::Pop);
        self.emit(Step::Pop);
        self.emit(Step::Push(Value::Undefined));
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

    fn compile_object(&mut self, literal: &ObjectLiteral) -> Result<(), JsError> {
        self.emit(Step::ObjectBegin);
        for property in &literal.props {
            match property {
                ObjectProperty::Init { key, value, .. } => {
                    let set_name = crate::function::is_anonymous_function_definition(value);
                    match key {
                        PropertyName::Ident(id) => {
                            self.compile_expr(value)?;
                            self.emit(Step::ObjectInitName {
                                name: *id,
                                set_name,
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
                            self.emit(Step::ObjectInitName { name: id, set_name });
                        }
                        PropertyName::Number(n) => {
                            self.compile_expr(value)?;
                            let text = crux::convert::to_string(&Value::Number(*n))?;
                            let id = crux::intern(text.as_slice());
                            self.emit(Step::ObjectInitName { name: id, set_name });
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
                ObjectProperty::Set { key, param, body } => {
                    self.compile_accessor(key, false, Some(param.clone()), body)?;
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
        param: Option<BindingPattern>,
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
    compile_statements(&function.body.stmts, function.strict)
}

/// Compile a statement list (module bodies and other resumable contexts).
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
