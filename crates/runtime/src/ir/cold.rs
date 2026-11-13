//! The interpreter's cold step handlers.
//!
//! `run_inner_inner`'s dispatch is one `match` over 233 `Step` variants. The
//! census (`.notes/perf.md`, 2026-11-11) says 40 of them run 99% of all
//! dispatched steps and the other 193 run 1% — yet all 233 shared one function,
//! so an edit anywhere in the interpreter re-laid every arm after it (a no-op
//! arm swap measured 6.8% on `promise_churn`).
//!
//! The 193 arms therefore live here, one method per family. Each method is its
//! own layout unit and is `#[cold] #[inline(never)]`, so an edit to the
//! generator machinery cannot move the arithmetic arms, and the cold code is
//! out of the hot loop's I-cache footprint. The dispatch pays one call per cold
//! step, on paths that already allocate and run the iterator protocol.
//!
//! The arms are moved verbatim. The only rewrites are the ones the move forces:
//! `continue` (which resumed the dispatch loop) becomes `Ok(StepControl::Next)`,
//! and `return Ok(x)` becomes `Ok(StepControl::Outcome(x))`.

use super::*;

/// What a cold step handler did.
///
/// `Next` is the common answer: the step finished and the dispatch loop
/// advances. It is deliberately not a `VmOutcome` variant — no driver outside
/// `run_inner_inner` may see it, and a handler that could leak it would be a
/// correctness bug in whichever driver received it.
pub(crate) enum StepControl {
    /// The step completed; the loop moves to the next one.
    Next,
    /// The step produced a run outcome.
    Outcome(VmOutcome),
}

impl Vm {
    /// `for await`-of steps, which await an iterator result before advancing.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_async_(
        &mut self,
        agent: &mut Agent,
        _body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::AsyncForOfClose => {
                // AsyncIteratorClose with a normal completion (spec
                // 14.7.5.7 step 8.b): the `return` method is invoked when
                // present; its errors propagate. The exhausted path pops
                // the stack at AsyncForOfTest, so only a break (or other
                // early exit reaching the end label) closes here.
                self.async_for_of_boundaries.pop();
                if let Some(iterator) = self.async_for_of_stack.pop() {
                    iterator_close(agent, &iterator)?;
                }
            }
            Step::AsyncForOfBegin { top, end } => {
                let rhs = self.pop();
                let iterator = async_from_sync_or_async(agent, &rhs)?;
                self.async_for_of_stack.push(iterator);
                self.async_for_of_boundaries.push((*top, *end));
            }
            Step::AsyncForOfNext => {
                let Some(iterator) = self.async_for_of_stack.last() else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "AsyncForOfNext without a for-await-of".into(),
                    ));
                };
                let iterator = iterator.clone();
                // The next() promise is in flight: a rejection must not
                // close the iterator (spec 14.7.5.5 — the close is only
                // for body abrupt completions). `AsyncForOfTest` clears
                // the flag on a fulfilled resume; the rejection path
                // discards the record instead.
                self.async_for_of_stepping = true;
                let next_result =
                    crate::function::call(agent, &iterator.next, iterator.iterator, &[])?;
                return Ok(StepControl::Outcome(VmOutcome::Suspended(
                    Suspension::Await(next_result),
                )));
            }
            Step::AsyncForOfTest { done } => {
                self.async_for_of_stepping = false;
                // Resumed with the awaited iterator result object.
                let result = self.pop();
                let is_done = iterator_result_done(agent, &result)?;
                if is_done {
                    self.async_for_of_stack.pop();
                    self.async_for_of_boundaries.pop();
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
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// The fused call and construct steps that are not on the hot path: the store forms, `Construct`, and tail calls.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_call(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::Call { direct_eval, span } => {
                self.set_site(agent, *span);
                if let Some(&callee) = self.stack.last() {
                    self.record_call_site(agent, body, &callee);
                }
                self.do_call(agent, *direct_eval)?;
                if let Some(outcome) = self.withdrawn_call_outcome() {
                    return Ok(StepControl::Outcome(outcome));
                }
            }
            Step::TailCallFast { argc, direct_eval } => {
                return self
                    .tail_call_fast(agent, *argc as usize, *direct_eval, body)
                    .map(StepControl::Outcome);
            }
            Step::TailCall { direct_eval } => {
                return self
                    .tail_call_vector(agent, *direct_eval, body)
                    .map(StepControl::Outcome);
            }
            Step::TailCallFastGlobal { name, argc } => {
                return self
                    .do_tail_call_global(agent, *name, *argc as usize, body)
                    .map(StepControl::Outcome);
            }
            Step::TailCallFastSlot { slot, argc } => {
                return self
                    .do_tail_call_slot(agent, *slot, *argc as usize, body)
                    .map(StepControl::Outcome);
            }
            Step::TailCallSelf { argc } => {
                // Cut 46: the callee is statically the running body
                // itself — rebind the frame with the new arguments and
                // re-enter the body's entry on this Vm (no callee
                // lookup, no context swap, no frame realloc).
                self.tail_call_self(agent, *argc as usize, body)?;
                return Ok(StepControl::Next);
            }
            Step::TailCallSelfCheck { argc } => {
                // Cut 47: a global-name self-tail-call — the shared
                // machinery handles the callee either way (an identity
                // match frame-replaces, a reassigned name calls the
                // resolved callee); the JIT adds the identity check
                // before committing to the jump.
                return self
                    .tail_call_fast(agent, *argc as usize, false, body)
                    .map(StepControl::Outcome);
            }
            Step::TailCallSelfVector => {
                // Cut 51: the vector-form self-tail-call — the arguments
                // sit in the Vm's vector; rebind the frame in place and
                // re-enter the body's entry on this Vm (no callee
                // lookup, no context swap, no runtime round-trip).
                self.tail_call_self_vector(agent, body)?;
                return Ok(StepControl::Next);
            }
            Step::TailCallSelfCheckVector => {
                // Cut 51: the vector-form checked self-tail-call — the
                // shared vector machinery handles the callee either way
                // (an identity match frame-replaces, a reassigned name
                // calls the resolved callee); the JIT adds the identity
                // check before committing to the jump.
                return self
                    .tail_call_vector(agent, false, body)
                    .map(StepControl::Outcome);
            }
            Step::CallFastGlobal {
                name,
                argc,
                direct_eval,
                span,
            } => {
                self.set_site(agent, *span);
                if crate::feedback::enabled() {
                    let callee = self.load_global_value(agent, *name)?;
                    self.record_call_site(agent, body, &callee);
                }
                self.do_call_fast_global(agent, *name, None, *argc as usize, *direct_eval)?;
                if let Some(outcome) = self.withdrawn_call_outcome() {
                    return Ok(StepControl::Outcome(outcome));
                }
            }
            Step::CallFastSlot { slot, argc, span } => {
                self.set_site(agent, *span);
                let callee = *self.frame_get(*slot);
                self.record_call_site(agent, body, &callee);
                self.do_call_fast_slot(agent, *slot, None, *argc as usize)?;
                if let Some(outcome) = self.withdrawn_call_outcome() {
                    return Ok(StepControl::Outcome(outcome));
                }
            }
            Step::CallFastIdent { name, argc, span } => {
                self.set_site(agent, *span);
                self.do_call_fast_ident(agent, *name, *argc as usize, body.env_constant)?;
                if let Some(outcome) = self.withdrawn_call_outcome() {
                    return Ok(StepControl::Outcome(outcome));
                }
            }
            Step::CallFastSlotStore {
                callee_slot,
                arg_slots,
                store_slot,
                span,
            } => {
                self.set_site(agent, *span);
                if crate::feedback::enabled() {
                    let callee = *self.frame_get(*callee_slot);
                    self.record_call_site(agent, body, &callee);
                }
                // Cut 35 slice 17: the fused `x = f(args)` — TDZ-check
                // the arg slots in order (their `LoadLocal` checks),
                // run the slot-callee call, and store the result to
                // the target with the `FusedStoreLocal` TDZ check. The
                // evaluation order matches the unfused steps exactly.
                // Cut 35 slice 23: the args stay in the caller's frame
                // — the callee may read them straight from there (no
                // stack push); the call core materializes them on a
                // miss or fallback.
                let arg_base = arg_slots.first().copied();
                for &slot in arg_slots {
                    if self.frame_get(slot).is_uninitialized() {
                        return Err(JsError::new(
                            ErrorKind::ReferenceError,
                            "Cannot access a binding before initialization".into(),
                        ));
                    }
                }
                self.do_call_fast_slot(agent, *callee_slot, arg_base, arg_slots.len())?;
                // This fused step is not emitted by the current compiler
                // (a global or local callee compiles to `CallFast`/
                // `CallFastSlot` here), so no test reaches its general
                // path; a withdrawn activation, should one ever appear, is
                // completed in place rather than deferred, keeping the
                // arm's result placement exactly as it was. Recorded in
                // `.notes/embedding.md` §9.
                self.complete_pending_call(agent)?;
                let result = self.pop();
                if self.frame_get(*store_slot).is_uninitialized() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                *self.frame_get_mut(*store_slot) = result;
            }
            Step::CallFastGlobalStore {
                name,
                arg_slots,
                store_slot,
                span,
            } => {
                self.set_site(agent, *span);
                if crate::feedback::enabled() {
                    let callee = self.load_global_value(agent, *name)?;
                    self.record_call_site(agent, body, &callee);
                }
                // Cut 35 slice 20: the fused `x = f(args)` for a global
                // callee — same shape as `CallFastSlotStore` through
                // `do_call_fast_global` (the global leaf cache). The
                // arg slots are TDZ-checked here and passed by caller-
                // frame base (Cut 35 slice 23) so the callee can read
                // them without a stack push.
                let arg_base = arg_slots.first().copied();
                for &slot in arg_slots {
                    if self.frame_get(slot).is_uninitialized() {
                        return Err(JsError::new(
                            ErrorKind::ReferenceError,
                            "Cannot access a binding before initialization".into(),
                        ));
                    }
                }
                self.do_call_fast_global(agent, *name, arg_base, arg_slots.len(), false)?;
                // See `CallFastSlotStore` above: the fused step is not
                // emitted by the current compiler, so complete a withdrawn
                // activation in place rather than deferring it.
                self.complete_pending_call(agent)?;
                let result = self.pop();
                if self.frame_get(*store_slot).is_uninitialized() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                *self.frame_get_mut(*store_slot) = result;
            }
            Step::Construct { span } => {
                self.set_site(agent, *span);
                let callee = self.pop();
                // A `new` whose body will run on a `Vm` of its own can be
                // parked and resumed, so this step declares it parkable, the
                // same way `complete_call` declares a call.
                let resumable = self.pause_ok && agent.pause_resumable_callee(&callee);
                if resumable {
                    agent.set_pause_ticket(true);
                }
                let result = self.step_construct(agent, callee);
                if resumable {
                    agent.clear_pause_ticket();
                }
                match result {
                    Ok(result) => self.stack.push(result),
                    Err(error) => {
                        // A stop below this step: the constructed object lands
                        // where the step's result would, so the resume pushes
                        // it here (`Vm::land_call_result`).
                        if agent.pause_in_flight() {
                            let site = self.stack.len();
                            self.set_pause_site(site, 0);
                        }
                        return Err(error);
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
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Class definition, `super` access, and private names.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_class(
        &mut self,
        agent: &mut Agent,
        _body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::GetSuperComputedKeep => {
                // Mirror of GetMemberComputedKeep for super: the key is
                // converted once and the converted value left for the
                // write; the base captured by GetSuperBase is used, not
                // recomputed (a key whose toString mutates the prototype
                // must still see the original base, spec 13.3.7.1).
                let key = self.pop();
                let base = self.pop();
                let key = crate::context::to_property_key(agent, &key)?;
                let this = self.vm_this_binding(agent)?;
                let value = crate::context::get_property_key(agent, &base, &key, this)?;
                self.pop(); // the write copy of the raw key
                let key = match key {
                    PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
                };
                self.stack.push(key);
                self.stack.push(value);
            }
            Step::GetSuperName { name } => {
                let base = self.pop();
                let this = self.vm_this_binding(agent)?;
                let value = crate::context::get_property(agent, &base, &crux::lookup(*name), this)?;
                self.stack.push(value);
            }
            Step::GetSuperComputed => {
                let key = self.pop();
                let base = self.pop();
                let key = crate::context::to_property_key(agent, &key)?;
                let this = self.vm_this_binding(agent)?;
                let value = crate::context::get_property_key(agent, &base, &key, this)?;
                self.stack.push(value);
            }
            Step::GetPrivate { atom } => {
                let object = self.pop();
                let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                let value = crate::context::private_get(agent, &object, name_id)?;
                self.stack.push(value);
            }
            Step::GetSuperBase => {
                // SuperProperty evaluation checks GetThisBinding first
                // (spec 13.3.7.1): an uninitialized `this` in a derived
                // constructor throws before the base is read or the key
                // evaluates.
                self.vm_this_binding(agent)?;
                let base = self.vm_super_base(agent)?;
                self.stack.push(base);
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
                    | AssignOp::NullishAssign => value,
                    _ => {
                        let old = crate::context::private_get(agent, &object, name_id)?;
                        crate::expr::apply_compound(agent, *op, &old, &value)?
                    }
                };
                crate::context::private_set(agent, &object, name_id, new)?;
                self.stack.push(new);
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
            Step::ResolveSuperRefName { name } => {
                let this = self.vm_this_binding(agent)?;
                let base = self.vm_super_base(agent)?;
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
                let this = self.vm_this_binding(agent)?;
                self.var_ref_stack.push(crate::context::Reference {
                    base: crate::context::ReferenceBase::Value(base),
                    name: key,
                    strict: self.strict,
                    this_value: Some(this),
                    private_name: None,
                });
            }
            Step::UpdateSuperName { name, op, prefix } => {
                let old = self.pop();
                let base = self.pop();
                let (old_numeric, new) = update_value(agent, op, &old)?;
                self.put_super(agent, base, PropertyKeyName::Name(*name), new)?;
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::UpdateSuperComputed { op, prefix } => {
                let old = self.pop();
                let key = self.pop();
                let base = self.pop();
                let (old_numeric, new) = update_value(agent, op, &old)?;
                let key = crate::context::to_property_key(agent, &key)?;
                self.put_super(agent, base, PropertyKeyName::Key(key), new)?;
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::UpdatePrivate { atom, op, prefix } => {
                let old = self.pop();
                let object = self.pop();
                let (old_numeric, new) = update_value(agent, op, &old)?;
                let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                crate::context::private_set(agent, &object, name_id, new)?;
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::DeleteSuper => {
                // `delete super.x` is a ReferenceError before the key is
                // even evaluated (spec 13.5.1.2 step 4.b).
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    "Unsupported reference to 'super'".into(),
                ));
            }
            Step::PrivateIn { atom } => {
                let object = self.pop();
                // `#x in rhs` requires an object RHS (spec 13.11.1 step
                // 4), like the `in` operator's non-object check.
                if !matches!(object.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot use 'in' operator to search for a property in a non-object".into(),
                    ));
                }
                let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                self.stack.push(Value::Boolean(crate::context::private_in(
                    &object, name_id,
                )?));
            }
            Step::SuperCall => {
                let base = self.args_base_stack.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "SuperCall without an argument boundary".into(),
                    )
                })?;
                // GC-2: the argument copy below is a local `Vec<Value>`
                // the stack scan cannot see, and the super constructor's
                // setup allocates — suppress `--gc-stress` for the window
                // (see the `Call` step).
                let _stress = StressSuppress::new();
                let args = self.args.split_off(base);
                let new_target = get_new_target(agent)?;
                let super_ctor = get_super_constructor(agent)?;
                let result = crate::function::construct(agent, &super_ctor, &args, &new_target)?;
                let this_env = get_this_environment(agent)?;
                this_env.bind_this_value(result)?;
                let function_value = agent.running_context()?.function;
                if let Some(function_value) = function_value {
                    crate::function::initialize_instance_elements(agent, &result, &function_value)?;
                }
                self.stack.push(result);
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
                let outer_env = self.lexical_env;
                let class_env = new_declarative_environment(Some(outer_env));
                if let Some(binding) = binding {
                    let name = crux::lookup(*binding);
                    class_env.create_immutable_binding(&name, true)?;
                }
                let outer_private_env = agent.running_context()?.private_environment;
                let class_private_env = crate::context::new_private_environment(outer_private_env);
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
                if let Ok(mut context) = agent.running_context_mut() {
                    context.lexical_environment = class_env;
                    context.private_environment = Some(class_private_env);
                }
                self.lexical_env = class_env;
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
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
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
                self.lexical_env = state.outer_env;
                if let Ok(mut context) = agent.running_context_mut() {
                    context.lexical_environment = state.outer_env;
                    context.private_environment = state.outer_private_env;
                }
                let class_value = crate::class::class_definition_evaluation_with_scope(
                    agent,
                    class,
                    *binding,
                    state.class_env,
                    state.class_private_env,
                    state.outer_private_env,
                    state.outer_env,
                    state.heritage,
                    &keys,
                )?;
                self.stack.push(class_value);
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Destructuring patterns, including rest elements and object rest.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_destructure(
        &mut self,
        agent: &mut Agent,
        _body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::Destructure { pattern } => {
                let value = self.pop();
                crate::binding::binding_initialization(agent, pattern, value, None, self.strict)?;
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
                // An abrupt iterator step does not close the iterator
                // (the error path in `run_inner` skips the close while the
                // flag is set; it stays set on the error path).
                self.destructure_stepping = true;
                match iterator_step(agent, &iterator) {
                    Ok(Some(value)) => {
                        self.destructure_stepping = false;
                        self.stack.push(value);
                    }
                    Ok(None) => {
                        self.destructure_stepping = false;
                        // The iterator is exhausted but the element still
                        // receives *undefined*: a default initializer must
                        // run (and may suspend) even after exhaustion.
                        self.destructure_done[index] = true;
                        self.stack.push(Value::Undefined);
                    }
                    Err(error) => return Err(error),
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
                // An abrupt step during the rest collection leaves the
                // iterator open (mirroring DestructureNext); the flag stays
                // set on the error path.
                self.destructure_stepping = true;
                let mut collected = Vec::new();
                let stepped = loop {
                    match iterator_step(agent, &iterator) {
                        Ok(Some(value)) => collected.push(value),
                        Ok(None) => break Ok(()),
                        Err(error) => break Err(error),
                    }
                };
                match stepped {
                    Ok(()) => {
                        self.destructure_stepping = false;
                    }
                    Err(error) => return Err(error),
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
                self.destructure_excluded.push(Vec::new());
            }
            Step::DestructureObjKey { key } => {
                let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "DestructureObjKey without an object".into(),
                    )
                })?;
                let value = crate::context::get_property_key(agent, &object, key, object)?;
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
                if let Some(frame) = self.destructure_excluded.last_mut() {
                    frame.push(key.clone());
                }
                let value = crate::context::get_property_key(agent, &object, &key, object)?;
                self.stack.push(value);
            }
            Step::DestructureObjKeyStore => {
                let key = self.pop();
                self.destructure_assign_keys.push(key);
            }
            Step::DestructureObjKeyGet => {
                let key = self.destructure_assign_keys.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "DestructureObjKeyGet without a stored key".into(),
                    )
                })?;
                let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "DestructureObjKeyGet without an object".into(),
                    )
                })?;
                let key = crate::context::to_property_key(agent, &key)?;
                if let Some(frame) = self.destructure_excluded.last_mut() {
                    frame.push(key.clone());
                }
                let value = crate::context::get_property_key(agent, &object, &key, object)?;
                self.stack.push(value);
            }
            Step::DestructureObjRest { excluded } => {
                let object = self.destructure_obj_stack.last().cloned().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "DestructureObjRest without an object".into(),
                    )
                })?;
                let mut all = excluded.clone();
                if let Some(frame) = self.destructure_excluded.last() {
                    all.extend(frame.iter().cloned());
                }
                let rest = crate::binding::rest_object(agent)?;
                crate::binding::copy_data_properties_excluding(agent, &rest, &object, &all)?;
                self.stack.push(Value::Object(rest));
            }
            Step::DestructureClose => {
                if let Some(index) = self.destructure_stack.len().checked_sub(1) {
                    let done = self.destructure_done.get(index).copied().unwrap_or(false);
                    // Pop before closing: a throwing `return` then reaches
                    // `run_inner` with an empty destructure stack, so the
                    // error-path close does not close it a second time.
                    let iterator = self.destructure_stack.pop().unwrap();
                    self.destructure_done.pop();
                    if !done {
                        iterator_close(agent, &iterator)?;
                    }
                }
            }
            Step::DestructureObjEnd => {
                self.destructure_obj_stack.pop();
                self.destructure_excluded.pop();
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Generator and yield* steps: `yield`, `yield*`, their async forms, and the resumption protocol they drive.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_generator(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::Yield { delegate } => {
                let value = self.pop();
                return Ok(StepControl::Outcome(VmOutcome::Suspended(
                    Suspension::Yield {
                        value,
                        delegate: *delegate,
                    },
                )));
            }
            Step::YieldStarBegin => {
                let value = self.pop();
                // GetIterator runs inside the generator body, so its
                // TypeError is catchable by the body's try/catch (spec
                // 15.5.5 step 4 uses `?`).
                let iterator = match get_iterator(agent, &value) {
                    Ok(iterator) => iterator,
                    Err(error) => match self.throw_js_error(agent, body, error)? {
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                let received = state.received;
                let iterator = state.iterator.clone();
                let next = match crate::expr::iterator_next_method(agent, &iterator) {
                    Ok(next) => next,
                    Err(error) => match self.throw_js_error(agent, body, error)? {
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                    },
                };
                let result =
                    match crate::function::call(agent, &next, iterator.iterator, &[received]) {
                        Ok(result) => result,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                        },
                    };
                if !matches!(result.kind(), ValueKind::Object(_)) {
                    // Spec 15.5.5 normal case step a.iii.
                    let error = JsError::new(
                        ErrorKind::TypeError,
                        "yield*: iterator next() result is not an object".into(),
                    );
                    match self.throw_js_error(agent, body, error)? {
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                    }
                }
                let done_flag = match iterator_result_done(agent, &result) {
                    Ok(done_flag) => done_flag,
                    Err(error) => match self.throw_js_error(agent, body, error)? {
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                    },
                };
                if done_flag {
                    let value = match iterator_result_value(agent, &result) {
                        Ok(value) => value,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                            iterator.iterator,
                        ) {
                            Ok(method) => method,
                            Err(error) => match self.throw_js_error(agent, body, error)? {
                                CtlResult::Continue => return Ok(StepControl::Next),
                                CtlResult::Done(outcome) => {
                                    return Ok(StepControl::Outcome(outcome));
                                }
                            },
                        };
                        if is_callable(&throw_method) {
                            let inner = match crate::function::call(
                                agent,
                                &throw_method,
                                iterator.iterator,
                                &[value],
                            ) {
                                Ok(inner) => inner,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                },
                            };
                            if !matches!(inner.kind(), ValueKind::Object(_)) {
                                // Spec 15.5.5 throw case step b.iii.
                                let error = JsError::new(
                                    ErrorKind::TypeError,
                                    "yield*: iterator throw() result is not an object".into(),
                                );
                                match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                }
                            }
                            let done_flag = match iterator_result_done(agent, &inner) {
                                Ok(done_flag) => done_flag,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                },
                            };
                            if done_flag {
                                let value = match iterator_result_value(agent, &inner) {
                                    Ok(value) => value,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => return Ok(StepControl::Next),
                                        CtlResult::Done(outcome) => {
                                            return Ok(StepControl::Outcome(outcome));
                                        }
                                    },
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
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                }
                            }
                            self.yield_star_stack.pop();
                            let violation = JsError::new(
                                ErrorKind::TypeError,
                                "yield* protocol violation: iterator has no throw method".into(),
                            );
                            match self.throw_js_error(agent, body, violation)? {
                                CtlResult::Continue => return Ok(StepControl::Next),
                                CtlResult::Done(outcome) => {
                                    return Ok(StepControl::Outcome(outcome));
                                }
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
                            iterator.iterator,
                        ) {
                            Ok(method) => method,
                            Err(error) => match self.throw_js_error(agent, body, error)? {
                                CtlResult::Continue => return Ok(StepControl::Next),
                                CtlResult::Done(outcome) => {
                                    return Ok(StepControl::Outcome(outcome));
                                }
                            },
                        };
                        if is_callable(&return_method) {
                            let inner = match crate::function::call(
                                agent,
                                &return_method,
                                iterator.iterator,
                                &[value],
                            ) {
                                Ok(inner) => inner,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                },
                            };
                            if !matches!(inner.kind(), ValueKind::Object(_)) {
                                // Spec 15.5.5 return case step c.vi.
                                let error = JsError::new(
                                    ErrorKind::TypeError,
                                    "yield*: iterator return() result is not an object".into(),
                                );
                                match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                }
                            }
                            let done_flag = match iterator_result_done(agent, &inner) {
                                Ok(done_flag) => done_flag,
                                Err(error) => match self.throw_js_error(agent, body, error)? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
                                },
                            };
                            if done_flag {
                                // A done return result completes the
                                // generator with ReturnCompletion of its
                                // value (spec 15.5.5 return case).
                                let value = match iterator_result_value(agent, &inner) {
                                    Ok(value) => value,
                                    Err(error) => match self.throw_js_error(agent, body, error)? {
                                        CtlResult::Continue => return Ok(StepControl::Next),
                                        CtlResult::Done(outcome) => {
                                            return Ok(StepControl::Outcome(outcome));
                                        }
                                    },
                                };
                                self.yield_star_stack.pop();
                                match self.control_transfer(agent, body, Ctl::Return { value })? {
                                    CtlResult::Continue => return Ok(StepControl::Next),
                                    CtlResult::Done(outcome) => {
                                        return Ok(StepControl::Outcome(outcome));
                                    }
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
                                CtlResult::Continue => return Ok(StepControl::Next),
                                CtlResult::Done(outcome) => {
                                    return Ok(StepControl::Outcome(outcome));
                                }
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
                let received = state.received;
                let iterator = state.iterator.clone();
                let next = crate::expr::iterator_next_method(agent, &iterator)?;
                let result = crate::function::call(agent, &next, iterator.iterator, &[received])?;
                // The await resume pushes the fulfilled iterator result.
                return Ok(StepControl::Outcome(VmOutcome::Suspended(
                    Suspension::Await(result),
                )));
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
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                    }
                }
                let done_flag = match iterator_result_done(agent, &result) {
                    Ok(done_flag) => done_flag,
                    Err(error) => match self.throw_js_error(agent, body, error)? {
                        CtlResult::Continue => return Ok(StepControl::Next),
                        CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                    },
                };
                if done_flag {
                    // Getter errors are catchable by the body (spec
                    // 15.5.5 uses `?` on IteratorValue).
                    let value = match iterator_result_value(agent, &result) {
                        Ok(value) => value,
                        Err(error) => match self.throw_js_error(agent, body, error)? {
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                            iterator.iterator,
                        )?;
                        if is_callable(&throw_method) {
                            let inner = crate::function::call(
                                agent,
                                &throw_method,
                                iterator.iterator,
                                &[value],
                            )?;
                            self.ip = *inspect;
                            return Ok(StepControl::Outcome(VmOutcome::Suspended(
                                Suspension::Await(inner),
                            )));
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
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
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
                            iterator.iterator,
                        )?;
                        if is_callable(&return_method) {
                            let inner = crate::function::call(
                                agent,
                                &return_method,
                                iterator.iterator,
                                &[value],
                            )?;
                            self.ip = *inspect;
                            return Ok(StepControl::Outcome(VmOutcome::Suspended(
                                Suspension::Await(inner),
                            )));
                        }
                        self.yield_star_stack.pop();
                        // spec 15.5.5 return case step b: no return
                        // method — await the received value, then return
                        // it (the driver resumes with `Resume::Return`).
                        return Ok(StepControl::Outcome(VmOutcome::Suspended(
                            Suspension::AwaitReturn(value),
                        )));
                    }
                }
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// `for-in` and synchronous `for-of` heads.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_iteration(
        &mut self,
        agent: &mut Agent,
        _body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::ForInBegin { top, end } => {
                let rhs = self.pop();
                if is_nullish(&rhs) {
                    // spec ForInOfHeadEvaluation step 7.a: a nullish
                    // exprValue is a break completion — the loop is
                    // skipped, not an error (the S12.6.4 and cptn-skip
                    // fixtures). The empty-key state makes ForInNext pop
                    // straight to the done label.
                    let dummy = crux::object::JsObject::ordinary_object_create(None);
                    self.for_in_stack.push(ForInState {
                        base: dummy,
                        keys: std::rc::Rc::new(Vec::new()),
                        index: 0,
                        base_generation: 0,
                        fast: false,
                        top: *top,
                        end: *end,
                    });
                } else {
                    let obj = crate::context::to_object(agent, &rhs)?;
                    let obj = crate::context::as_object(&obj).ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "for-in over a non-object".into())
                    })?;
                    // A generation-tracked base re-enters the same
                    // unchanged chain cheaply: the cached enumeration is
                    // reused when the live chain still matches its
                    // (id, generation) snapshot.
                    let keys = if for_in_generation_tracked(&obj) {
                        match for_in_cache_keys(agent, &obj)? {
                            Some(keys) => keys,
                            None => {
                                let keys = crate::eval::for_in_key_levels(agent, &rhs)?;
                                for_in_cache_put(agent, &obj, &keys)?;
                                std::rc::Rc::new(keys)
                            }
                        }
                    } else {
                        std::rc::Rc::new(crate::eval::for_in_key_levels(agent, &rhs)?)
                    };
                    let fast = for_in_generation_tracked(&obj)
                        && keys.iter().all(|(level, _)| *level == 0);
                    self.for_in_stack.push(ForInState {
                        base: obj,
                        keys,
                        index: 0,
                        base_generation: obj.generation.get(),
                        fast,
                        top: *top,
                        end: *end,
                    });
                }
            }
            Step::ForInBind { left } => {
                let value = self.pop();
                self.for_binding_put(agent, left, value)?;
            }
            Step::ForInRestore => {
                self.restore_per_iteration(agent)?;
            }
            Step::ForOfBegin { top, end, .. } => {
                let rhs = self.pop();
                let entry = match crate::expr::for_of_begin(agent, &rhs)? {
                    crate::expr::ForOfState::Generic(record) => ForOfEntry::Generic(record),
                    crate::expr::ForOfState::FastArray(array) => {
                        ForOfEntry::Fast { array, index: 0 }
                    }
                };
                self.for_of_stack.push(entry);
                self.for_of_boundaries.push((*top, *end));
            }
            Step::ForOfNext { done, back } => {
                self.for_of_next(agent, *done, *back, ForOfNextTarget::Stack)?;
            }
            Step::ForOfBind { left } => {
                let value = self.pop();
                self.for_binding_put(agent, left, value)?;
            }
            Step::ForOfBindGlobal { name } => {
                let value = self.pop();
                self.store_global_value(agent, *name, value)?;
            }
            Step::ForOfRestore => {
                self.restore_per_iteration(agent)?;
            }
            Step::ForOfClose => {
                self.for_of_boundaries.pop();
                if let Some(ForOfEntry::Generic(iterator)) = self.for_of_stack.pop() {
                    iterator_close(agent, &iterator)?;
                }
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Literal and allocation steps: array and object construction, tagged templates, typed-array hoists, accessor definitions.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_literal(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::RegExpLiteral { pattern, flags } => {
                let value = crate::expr::eval_regexp_literal(agent, pattern, flags)?;
                self.stack.push(value);
            }
            Step::TaggedTemplate(template) => {
                let tag = self.pop();
                let this = self.pop();
                let base = self.args_base_stack.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "TaggedTemplate without an argument boundary".into(),
                    )
                })?;
                // GC-2: the substitution Vec is unrooted while the tag
                // invocation allocates — suppress `--gc-stress` for the
                // window (see the tail-call dispatch).
                let _stress = StressSuppress::new();
                let substitutions = self.args.split_off(base);
                let value = tagged_template(agent, self, this, tag, template, substitutions)?;
                self.stack.push(value);
            }
            Step::TailTaggedTemplate(template) => {
                let tag = self.pop();
                let this = self.pop();
                let base = self.args_base_stack.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "TailTaggedTemplate without an argument boundary".into(),
                    )
                })?;
                // GC-2: borrow the substitutions from the traced
                // `self.args` (see the `Construct` step) — the template
                // object creation below allocates.
                let substitutions = &self.args[base..];
                // The tag invocation is the tail call: build the
                // template object + substitution arguments, lay them out
                // as a fast-form call, and let the shared dispatcher
                // frame-replace (or run a leaf in place) and complete the
                // return.
                let template_object = crate::expr::get_template_object(agent, template)?;
                let mut args = vec![template_object];
                args.extend_from_slice(substitutions);
                let argc = args.len();
                let keep = self.stack.len();
                self.stack.push(this);
                self.stack.push(tag);
                self.stack.extend(args);
                let n = self.stack.len();
                return match self.tail_call_shared(
                    agent,
                    this,
                    tag,
                    argc,
                    n - argc,
                    keep,
                    false,
                    body,
                )? {
                    TailOutcome::Returned(outcome) => Ok(StepControl::Outcome(outcome)),
                    TailOutcome::Replaced(ir) => Ok(StepControl::Outcome(VmOutcome::TailCall(ir))),
                };
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
                // The literal's own array is the fresh dense Array
                // `ArrayBegin` created (unreachable before it completes),
                // so the index-native CreateDataProperty stores directly
                // — no per-element string key + descriptor + dispatch
                // round trip.
                match array.kind() {
                    ValueKind::Object(obj) => {
                        obj.create_data_property_index(index as u64, value)?;
                    }
                    _ => array_set(&array, &index.to_string(), value)?,
                }
                *self.array_index()? = index + 1;
                self.stack.push(array);
            }
            Step::ArraySpread => {
                let iterable = self.pop();
                let array = self.pop();
                let iterator = get_iterator(agent, &iterable)?;
                while let Some(value) = iterator_step(agent, &iterator)? {
                    let index = *self.array_index()?;
                    match array.kind() {
                        ValueKind::Object(obj) => {
                            obj.create_data_property_index(index as u64, value)?;
                        }
                        _ => array_set(&array, &index.to_string(), value)?,
                    }
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
                // A dense literal whose element defines already reached
                // the element count (no trailing holes) needs no [[Set]]
                // of length — assigning the own length its current value
                // is unobservable, and the full ArraySetLength path costs
                // ~1us per literal.
                let needs_set = match obj.array_length_dense() {
                    Some(current) => current as usize != length,
                    None => true,
                };
                if needs_set {
                    obj.set(
                        &JsString::from_utf8("length"),
                        Value::Number(length as f64),
                        true,
                    )?;
                }
                self.stack.push(array);
            }
            Step::ArrayFast { count } => {
                // The whole-literal fused create (the array analogue of
                // `ObjectFast`): the N element expressions already
                // pushed (in source order) and this step pops them,
                // creates a fresh dense Array, and appends the values
                // (the literal is unobservable until it escapes). The
                // fused create is shared with the JIT's `array_fast`
                // helper (see `array_fast_create`).
                let n = *count as usize;
                let base = self.stack.len() - n;
                let array = array_fast_create(agent, &self.stack[base..base + n])?;
                self.stack.truncate(base);
                self.stack.push(array);
            }
            Step::ObjectBegin => {
                let proto = agent
                    .current_realm()?
                    .intrinsics
                    .object_prototype()
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
            Step::ObjectKeyToPropertyKey => {
                let key = self.pop();
                let key = crate::context::to_property_key(agent, &key)?;
                let key = match key {
                    PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
                };
                self.stack.push(key);
            }
            Step::ObjectMethodName { name, function } => {
                let object = self.pop();
                object_method(
                    agent,
                    &object,
                    PropertyKey::String(*name),
                    function,
                    self.strict,
                )?;
                self.stack.push(object);
            }
            Step::ObjectMethodComputed { function } => {
                let key = self.pop();
                let object = self.pop();
                let key = crate::context::to_property_key(agent, &key)?;
                object_method(agent, &object, key, function, self.strict)?;
                self.stack.push(object);
            }
            Step::ObjectAccessorName {
                name,
                get,
                param,
                body,
                span,
            } => {
                let object = self.pop();
                object_accessor(
                    agent,
                    &object,
                    PropertyKey::String(*name),
                    *get,
                    param.as_ref(),
                    body,
                    self.strict,
                    *span,
                )?;
                self.stack.push(object);
            }
            Step::ObjectAccessorComputed {
                get,
                param,
                body,
                span,
            } => {
                let key = self.pop();
                let object = self.pop();
                let key = crate::context::to_property_key(agent, &key)?;
                object_accessor(
                    agent,
                    &object,
                    key,
                    *get,
                    param.as_ref(),
                    body,
                    self.strict,
                    *span,
                )?;
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
            Step::TypedArrayLengthHoist { target } => {
                // Gap-close M6 slice 2: pop the receiver; a probe-hit
                // typed array (IntegerIndexed, no own `length`) pushes
                // its effective length — the value the compiler stores
                // in the hidden hoist slot; anything else jumps to the
                // general per-iteration loop. Mirror of the runtime
                // `typed_array_length` FFI probe; no user code runs.
                let Some(receiver) = self.stack.pop() else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "Value stack underflow".into(),
                    ));
                };
                if let ValueKind::Object(object_ref) = receiver.kind()
                    && let crux::object::ObjectKind::IntegerIndexed(slots) = &object_ref.kind
                    && !object_ref.has_own_property_atom(Self::length_atom())
                {
                    self.stack.push(Value::Number(
                        crux::object::typed_array_effective_length(slots) as f64,
                    ));
                } else {
                    self.ip = *target;
                }
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Everything else: debug checks, hoist guards, computed member compounds, context-slot updates, imports.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_misc(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::DebugCheck { span } => {
                // A debug point is where a session is consulted (§14.4). It
                // returns an outcome rather than an error so the enclosing
                // `catch`/`finally` do not run, and the request is cleared
                // so the resumed run does not stop again at the same point.
                // Only an activation a starter declared parkable stops; a
                // nested lane, a generator's resumption, an eval body or a
                // module body defers, because resuming those would need
                // state nobody kept. A run parked for a frame evaluation is
                // not paused again.
                if self.pause_ok && !agent.has_paused_run() {
                    // A breakpoint outranks a step: a session that is
                    // stepping still stops where it asked to. The point is
                    // matched against the body's source identity, so a
                    // breakpoint set in one script cannot fire in another
                    // whose offsets happen to coincide.
                    let stop = if agent.breakpoint_at(body.script.get(), span.start) {
                        Some(PauseReason::Breakpoint)
                    } else if agent.pause_requested() {
                        agent.clear_pause_request();
                        Some(PauseReason::Requested)
                    } else if agent.step_allows_stop() {
                        Some(PauseReason::Step)
                    } else {
                        None
                    };
                    if let Some(reason) = stop {
                        let info = PauseInfo {
                            reason,
                            position: Some(*span),
                        };
                        agent.set_pause_info(info);
                        return Ok(StepControl::Outcome(VmOutcome::Paused(info)));
                    }
                }
            }
            Step::StoreLocal { slot } => {
                let value = self.pop();
                if self.frame_get(*slot).is_uninitialized() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                *self.frame_get_mut(*slot) = value;
            }
            Step::FusedStoreGlobal { name } => {
                let value = self.pop();
                self.store_global_value(agent, *name, value)?;
                self.completion = value;
                self.completion_is_empty = false;
            }
            Step::LoadGlobal { name } => {
                let value = self.load_global_value(agent, *name)?;
                self.stack.push(value);
            }
            Step::StoreGlobal { name } => {
                let value = self.pop();
                self.store_global_value(agent, *name, value)?;
            }
            Step::ResolveIdentStore { name } => {
                // The certified store's target: resolve it before the RHS
                // only when the chain is not clean (a `with`/outer
                // binding), preserving the reference-before-RHS rule.
                if !(body.env_constant && self.globals_unshadowed) {
                    let reference =
                        crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                    self.var_ref_stack.push(reference);
                }
            }
            Step::StoreIdent { name } => {
                // The certified-body store: a clean chain lets the global
                // store cell apply; a shadowing/declarative binding takes
                // the pre-resolved reference (or the environment resolve
                // for a declarative cell). The value stays as the
                // expression result.
                let value = self.pop();
                if body.env_constant && self.globals_unshadowed {
                    self.store_ident_value(agent, *name, value, true)?;
                } else if let Some(reference) = self.var_ref_stack.pop() {
                    crate::context::put_value(agent, &reference, value)?;
                } else {
                    // The resolve skipped the push because the chain was
                    // clean when the statement started, so the binding it
                    // targeted is the global one even if the flag has
                    // changed since.
                    self.store_ident_value(agent, *name, value, true)?;
                }
                self.stack.push(value);
            }
            Step::Inc { slot } => {
                let old = *self.frame_get(*slot);
                if old.is_uninitialized() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                *self.frame_get_mut(*slot) = update_value(agent, &UpdateOp::Increment, &old)?.1;
            }
            Step::Dec { slot } => {
                let old = *self.frame_get(*slot);
                if old.is_uninitialized() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                *self.frame_get_mut(*slot) = update_value(agent, &UpdateOp::Decrement, &old)?.1;
            }
            Step::UpdateGlobal { name, op, prefix } => {
                let value = self.update_global(agent, *name, *op, *prefix)?;
                self.stack.push(value);
            }
            Step::IncGlobal { name } => {
                self.update_global(agent, *name, UpdateOp::Increment, true)?;
            }
            Step::DecGlobal { name } => {
                self.update_global(agent, *name, UpdateOp::Decrement, true)?;
            }
            Step::CreateFunction {
                function,
                strict,
                named,
                outer_chain,
                per_iteration_chain,
            } => {
                let env = agent.running_context()?.lexical_environment;
                let value = crate::function::instantiate_function_expression(
                    agent,
                    function,
                    env,
                    *strict,
                    outer_chain.clone(),
                    per_iteration_chain.clone(),
                    named.clone(),
                )?;
                self.stack.push(value);
            }
            Step::CreateArrow {
                is_async,
                params,
                body,
                strict,
                named,
                outer_chain,
                per_iteration_chain,
                span,
            } => {
                let env = agent.running_context()?.lexical_environment;
                let value = crate::function::instantiate_arrow(
                    agent,
                    *is_async,
                    params.as_slice(),
                    body,
                    env,
                    *strict,
                    outer_chain.clone(),
                    per_iteration_chain.clone(),
                    *span,
                    named.clone(),
                )?;
                self.stack.push(value);
            }
            Step::NewTarget => {
                // Cut 62: the certified path's `new.target` is the
                // per-run `current_new_target` (the frame-slot model
                // creates no FunctionEnv to carry it); the env path
                // reads the running this-environment's binding.
                let value = if self.body_context.is_some() {
                    self.current_new_target.unwrap_or(Value::Undefined)
                } else {
                    get_new_target(agent)?
                };
                self.stack.push(value);
            }
            Step::Dup2 => {
                let b = self.pop();
                let a = self.pop();
                self.stack.push(a);
                self.stack.push(b);
                self.stack.push(a);
                self.stack.push(b);
            }
            Step::GetMemberComputed => {
                let key = self.pop();
                let object = self.pop();
                let value = self.get_member_computed(agent, object, key)?;
                self.stack.push(value);
            }
            Step::GetMemberComputedKeep => {
                // Compound/update paths: the (object, key) pair was
                // duplicated; convert the key once (after the nullish
                // check, spec 13.3.3 step 3 before step 6) and leave the
                // converted key on the stack for the write, so the key's
                // ToPropertyKey runs exactly once (spec 13.15.4).
                let key = self.pop();
                let object = self.pop();
                if is_nullish(&object) {
                    return Err(nullish_error("Cannot read properties of null"));
                }
                let key = crate::context::to_property_key(agent, &key)?;
                let value = crate::context::get_property_key(agent, &object, &key, object)?;
                self.pop(); // the write copy of the raw key
                let key = match key {
                    PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
                };
                self.stack.push(key);
                self.stack.push(value);
            }
            Step::ThisValue => {
                let this = self.vm_this_binding(agent)?;
                self.stack.push(this);
            }
            Step::AssignIdent { name, op, set_name } => {
                self.assign_ident(agent, *name, *op, *set_name)?;
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
                if let Some(old) = old {
                    if is_nullish(&object) {
                        return Err(nullish_error("Cannot set properties of null"));
                    }
                    let key = crate::context::to_property_key(agent, &key)?;
                    self.assign_member(
                        agent,
                        object,
                        PropertyKeyName::Key(key),
                        Some(old),
                        value,
                        *op,
                    )?;
                } else {
                    self.assign_computed_plain(agent, object, key, value)?;
                }
            }
            Step::ResolveVarIdent { name } => {
                let reference =
                    crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                self.var_ref_stack.push(reference);
            }
            Step::ResolveMemberRefComputed => {
                let key = self.pop();
                let object = self.pop();
                // RequireObjectCoercible before ToPropertyKey (spec 13.3.3
                // steps 4-5): the nullish-base TypeError fires before the
                // key's coercion — the logical-assignment lhs-before-rhs
                // fixtures pin the key's toString must not run.
                if is_nullish(&object) {
                    return Err(nullish_error("Cannot convert undefined or null to object"));
                }
                let key = crate::context::to_property_key(agent, &key)?;
                let reference = member_reference(&object, &PropertyKeyName::Key(key), self.strict);
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
                crate::context::put_value(agent, &reference, value)?;
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
            Step::GetVarReferenceThis => {
                let reference = self.var_ref_stack.last().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "GetVarReferenceThis without a resolution".into(),
                    )
                })?;
                self.stack.push(crate::context::get_this_value(reference));
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
                crate::context::put_value(agent, &reference, new)?;
                self.stack.push(new);
            }
            Step::UpdateVarReference { op, prefix } => {
                let old = self.pop();
                let (old_numeric, new) = update_value(agent, op, &old)?;
                let reference = self.var_ref_stack.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "UpdateVarReference without a resolution".into(),
                    )
                })?;
                crate::context::put_value(agent, &reference, new)?;
                self.stack.push(if *prefix { new } else { old_numeric });
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
                    value,
                    Some(&self.lexical_env),
                    self.strict,
                )?;
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
                let (old_numeric, new) = update_value(agent, op, &old)?;
                let reference =
                    crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
                crate::context::put_value(agent, &reference, new)?;
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::UpdateMemberComputed { op, prefix } => {
                let old = self.pop();
                let key = self.pop();
                let object = self.pop();
                if is_nullish(&object) {
                    return Err(nullish_error("Cannot set properties of null"));
                }
                let (old_numeric, new) = update_value(agent, op, &old)?;
                let key = crate::context::to_property_key(agent, &key)?;
                crate::context::put_value(
                    agent,
                    &member_reference(&object, &PropertyKeyName::Key(key), self.strict),
                    new,
                )?;
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::DeleteIdent { name } => {
                let reference =
                    crate::context::resolve_binding(agent, &crux::lookup(*name), self.strict)?;
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
            Step::TypeofTop => {
                let value = self.pop();
                self.stack.push(agent.type_of_value(&value));
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
                self.stack.push(agent.type_of_value(&value));
            }
            Step::PushStr(text) => {
                self.stack.push(Value::String(Handle::new(text.clone())));
            }
            Step::TemplateSub => {
                let value = self.pop();
                let out = crate::ir::template_sub(agent, &value)?;
                self.stack.push(out);
            }
            Step::TemplateJoin { quasis } => {
                let subs = quasis.len().saturating_sub(1);
                let base = self.stack.len().saturating_sub(subs);
                let out = crate::ir::template_join(agent, quasis, &self.stack[base..])?;
                self.stack.truncate(base);
                self.stack.push(out);
            }
            Step::ArgsSpread => {
                let iterable = self.pop();
                let iterator = get_iterator(agent, &iterable)?;
                while let Some(value) = iterator_step(agent, &iterator)? {
                    self.args.push(value);
                }
            }
            Step::SaveCompletion => {
                self.completion_stack
                    .push((self.completion, self.completion_is_empty));
            }
            Step::RestoreCompletion => {
                if let Some((value, empty)) = self.completion_stack.pop() {
                    self.completion = value;
                    self.completion_is_empty = empty;
                }
            }
            Step::FusedCompoundLocal { slot, op, rhs } => {
                // `frame[slot] = frame[slot] <op> rhs`, the shape the general
                // loop path emitted as six steps. The left operand is read
                // before the RHS, matching `LoadLocal` -> `LoadLocal` order
                // (both are pure, so only a global RHS could observe it).
                let left = *self.frame_get(*slot);
                let right = match rhs {
                    CompoundRhs::Imm(imm) => Value::Number(*imm),
                    CompoundRhs::Slot(rhs_slot) => *self.frame_get(*rhs_slot),
                    CompoundRhs::Global(name) => self.load_global_value(agent, *name)?,
                    CompoundRhs::Counter => Value::Number(self.loop_counter),
                };
                let value = Self::binary_inline(agent, *op, &left, &right)?;
                *self.frame_get_mut(*slot) = value;
            }
            Step::JumpIfLtImm { slot, imm, target } => {
                self.jump_if_rel_imm(agent, BinaryOp::LessThan, *slot, *imm, *target)?
            }
            Step::JumpIfEqImm { slot, imm, target } => {
                self.jump_if_strict_eq_imm(*slot, *imm, false, *target)?
            }
            Step::JumpIfNeqImm { slot, imm, target } => {
                self.jump_if_strict_eq_imm(*slot, *imm, true, *target)?
            }
            Step::JumpIfLeImm { slot, imm, target } => {
                self.jump_if_rel_imm(agent, BinaryOp::LessEqual, *slot, *imm, *target)?
            }
            Step::JumpIfGtImm { slot, imm, target } => {
                self.jump_if_rel_imm(agent, BinaryOp::GreaterThan, *slot, *imm, *target)?
            }
            Step::JumpIfGeImm { slot, imm, target } => {
                self.jump_if_rel_imm(agent, BinaryOp::GreaterEqual, *slot, *imm, *target)?
            }
            Step::JumpIfLtGlobalImm { name, imm, target } => {
                self.jump_if_rel_global(agent, BinaryOp::LessThan, *name, *imm, *target)?
            }
            Step::JumpIfLeGlobalImm { name, imm, target } => {
                self.jump_if_rel_global(agent, BinaryOp::LessEqual, *name, *imm, *target)?
            }
            Step::JumpIfGtGlobalImm { name, imm, target } => {
                self.jump_if_rel_global(agent, BinaryOp::GreaterThan, *name, *imm, *target)?
            }
            Step::JumpIfGeGlobalImm { name, imm, target } => {
                self.jump_if_rel_global(agent, BinaryOp::GreaterEqual, *name, *imm, *target)?
            }
            Step::JumpIfRelLimit {
                op,
                slot,
                limit,
                target,
            } => self.jump_if_rel_limit(agent, *op, *slot, *limit, *target)?,
            Step::HoistMemberGuard {
                reads,
                prim_slots,
                numbers,
                int_slots,
                target,
            } => {
                // LICM: a once-per-loop guard. Every VALUE-operand slot
                // must hold a non-object primitive (an object/function
                // operand's coercion can run user code, which could mutate
                // a hoisted receiver), and every read must be a data-cell
                // hit with a non-object value. On success the values land
                // in the hidden hoist slots for the fast copy; any miss
                // jumps to the general per-iteration loop.
                let mut ok = prim_slots.iter().all(|slot| {
                    let value = *self.frame_get(*slot);
                    if *numbers {
                        value.as_number().is_some()
                    } else {
                        !value.is_object() && !value.is_function()
                    }
                }) && int_slots.iter().all(|slot| {
                    // An exact int32: `ToInt32` of such a value is the value
                    // itself, which is what the fused RMW's wrapping i32 op
                    // assumes. `trunc()` rejects NaN/fractional, the bounds
                    // reject the saturating conversion's range.
                    self.frame_get(*slot).as_number().is_some_and(|n| {
                        n.trunc() == n && (-2147483648.0..=2147483647.0).contains(&n)
                    })
                });
                if ok {
                    for (recv_slot, name, hoist_slot, strict) in reads {
                        let object = *self.frame_get(*recv_slot);
                        // The TDZ marker is a reserved tag that must never
                        // reach `cell_object`/`Value::kind`; a TDZ receiver
                        // means the body would throw, so take the general
                        // loop (which raises it on the body's own read).
                        let hit = if object.is_uninitialized() {
                            None
                        } else {
                            Self::cell_object(&object)
                                .and_then(|cell| Self::member_cell_warm_probe(agent, &cell, *name))
                                .filter(|value| {
                                    !value.is_object()
                                        && !value.is_function()
                                        && !(*strict && (value.is_bigint() || value.is_symbol()))
                                })
                        };
                        match hit {
                            Some(value) => *self.frame_get_mut(*hoist_slot) = value,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
                if !ok {
                    self.ip = *target;
                }
            }
            Step::HoistGlobalGuard { reads, env, target } => {
                // LICM: a once-per-loop global read guard — a data-cell hit
                // with a non-object value is loop-invariant. An `Env` read
                // also needs the body's globals to be unshadowed, exactly
                // the `LoadIdent` fast-path gate.
                let mut ok = !(*env) || self.globals_unshadowed;
                if ok {
                    for (name, hoist_slot) in reads {
                        match self.try_global_cell_read(agent, *name) {
                            Some(value) if !value.is_object() && !value.is_function() => {
                                *self.frame_get_mut(*hoist_slot) = value;
                            }
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
                if !ok {
                    self.ip = *target;
                }
            }
            Step::BuilderBind { slot } => {
                if let Some(slot) = slot {
                    self.builder_bind(*slot);
                }
            }
            Step::BuilderStore { slot } => {
                if let Some(slot) = slot {
                    self.builder_store(*slot);
                }
            }
            Step::PopAcc => {
                // Cut 35 slice 22: the acc-path `counter = expr` gate
                // admits only provably-Number RHSes, so the popped value
                // is a Number.
                let value = self.pop();
                debug_assert!(
                    value.is_number(),
                    "the acc-path counter store is gated to a Number"
                );
                self.loop_counter = value.as_number().unwrap_or(0.0);
            }
            Step::IncAcc => {
                // The field is a raw f64 holding a Number (the acc-path
                // gates admit only Number inits/stores) — plain float
                // arithmetic, no Value round-trip.
                self.loop_counter += 1.0;
            }
            Step::DecAcc => {
                self.loop_counter -= 1.0;
            }
            Step::StoreContextSlot { depth, index } => {
                let value = self.pop();
                let env = self.context_chain_env(*depth)?;
                let declarative = context_env(&env);
                // An assignment to a binding still in the TDZ throws.
                if declarative.slot_value(*index).is_none() {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    ));
                }
                // The certified body's own const writes are rejected by
                // the scan, but a nested closure's write to an enclosing
                // `const` is only checked here: throw like the env path.
                if !declarative.slot_mutable(*index) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Assignment to constant variable".into(),
                    ));
                }
                env.set_slot(*index, value);
            }
            Step::InitContextSlot { index } => {
                let value = self.pop();
                let env = self.context_chain_env(0)?;
                env.set_slot(*index, value);
            }
            Step::UpdateContextSlot {
                depth,
                index,
                op,
                prefix,
            } => {
                let env = self.context_chain_env(*depth)?;
                let declarative = context_env(&env);
                let old = declarative.slot_value(*index).ok_or_else(|| {
                    JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    )
                })?;
                // An enclosing `const` (only reachable from a nested
                // closure) must reject the update like the env path.
                if !declarative.slot_mutable(*index) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Assignment to constant variable".into(),
                    ));
                }
                let (old_numeric, new) = update_value(agent, op, &old)?;
                env.set_slot(*index, new);
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::JumpIfTrue(target) => {
                let value = self.pop();
                if truthy(&value) {
                    self.ip = *target;
                }
            }
            Step::JumpIfFalseKeep(target) => {
                let value = self.pop();
                if !truthy(&value) {
                    self.stack.push(value);
                    self.ip = *target;
                } else {
                    // The left operand is not the result: leave it for the
                    // following Pop to discard (it was already popped).
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
            Step::InvalidAssignmentTarget => {
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    "Invalid left-hand side in assignment".into(),
                ));
            }
            Step::Await { span } => {
                // The suspension is a site like a call's: an error raised
                // while nothing is on the stack is reported against the
                // await the code is at, which is what the message a stalled
                // module answers its position from (`module.rs`).
                self.set_site(agent, *span);
                let value = self.pop();
                return Ok(StepControl::Outcome(VmOutcome::Suspended(
                    Suspension::Await(value),
                )));
            }
            Step::ImportMeta => {
                let meta = crate::module::import_meta(agent)?;
                self.stack.push(meta);
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// Block, loop-environment, argument-object, declaration and script-global steps.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_scope(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::ScriptGlobalFlush { pairs } => {
                // The fused `LoadLocal` + `StoreGlobal` pair per name: the
                // slot is read with `LoadLocal`'s TDZ check, then written to
                // the global in slot order (an accessor on one of the names
                // runs in the same sequence the unfused pairs gave it).
                for &(slot, name) in pairs.iter() {
                    let value = *self.frame_get(slot);
                    if value.is_uninitialized() {
                        return Err(JsError::new(
                            ErrorKind::ReferenceError,
                            "Cannot access a binding before initialization".into(),
                        ));
                    }
                    // The sync runs around every call in a top-level loop,
                    // and the common iteration writes back what the global
                    // still holds. The global-value cell only ever caches a
                    // writable data property, so an identical value means
                    // the store, its write barrier and both cell mirrors
                    // are all unobservable — skip them. A strict body keeps
                    // the store: a strict-mode assignment to a non-writable
                    // global throws whatever the value is.
                    if !self.strict
                        && let Some(current) = self.try_global_var_cell_read(agent, name)
                        && current.bits() == value.bits()
                    {
                        continue;
                    }
                    self.store_global_value(agent, name, value)?;
                }
            }
            Step::ScriptGlobalReload { pairs } => {
                // The fused `LoadGlobal` + `InitLocal` pair per name, in the
                // same slot order. The names are this script's own declared
                // `var`s, which live on the global object for the run, so
                // the generation-validated value cell answers them — every
                // write to a global data property either bumps the global's
                // generation or mirrors the cell, which is the invariant
                // `LoadIdent` already reads by.
                for &(slot, name) in pairs.iter() {
                    let value = match self.try_global_var_cell_read(agent, name) {
                        Some(value) => value,
                        None => self.load_global_value(agent, name)?,
                    };
                    *self.frame_get_mut(slot) = value;
                }
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
            Step::FunctionDeclInit {
                function,
                frame_slot,
                context_slot,
                outer_chain,
                per_iteration_chain,
                ..
            } => {
                // A certified body's hoisted top-level function
                // declaration: instantiate the closure against the
                // capture context (the binding was created by the
                // certified layout) and store it. The declaration runs
                // at body entry, so it sees only the INHERITED
                // per-iteration chain (the loops inside this body have
                // not started).
                let env = agent.running_context()?.lexical_environment;
                let value = crate::function::instantiate_function(
                    agent,
                    function,
                    env,
                    self.strict,
                    outer_chain.clone(),
                    per_iteration_chain.clone(),
                    false,
                    None,
                )?;
                if let Some(slot) = frame_slot {
                    *self.frame_get_mut(*slot) = value;
                } else if let Some(index) = context_slot {
                    let env = self.context_chain_env(0)?;
                    env.set_slot(*index, value);
                } else {
                    unreachable!(
                        "FunctionDeclInit without a binding slot (the scan allocated one)"
                    );
                }
            }
            Step::DeclInit { pattern } => {
                let value = self.pop();
                crate::binding::binding_initialization(
                    agent,
                    pattern,
                    value,
                    Some(&self.lexical_env),
                    self.strict,
                )?;
                self.stack.push(value);
            }
            Step::ResolveMemberRefName { name } => {
                let object = self.pop();
                let reference =
                    member_reference(&object, &PropertyKeyName::Name(*name), self.strict);
                self.var_ref_stack.push(reference);
            }
            Step::SetFunctionName { name } => {
                let value = self.pop();
                let raw = crux::lookup(*name);
                let display = crate::function::default_binding_display_name(Some(raw.clone()))
                    .unwrap_or_else(|| raw.clone());
                if display == raw {
                    // The name is this site's atom: its box is shared, where
                    // boxing it per evaluation was one allocation per named
                    // closure.
                    crate::function::set_function_name_value(
                        &value,
                        agent.atom_string_value(*name),
                    )?;
                } else {
                    crate::function::set_function_name(&value, &display, None)?;
                }
                self.stack.push(value);
            }
            Step::UpdateMemberName { name, op, prefix } => {
                let old = self.pop();
                let object = self.pop();
                if is_nullish(&object) {
                    return Err(nullish_error("Cannot set properties of null"));
                }
                let (old_numeric, new) = update_value(agent, op, &old)?;
                crate::context::put_value(
                    agent,
                    &member_reference(&object, &PropertyKeyName::Name(*name), self.strict),
                    new,
                )?;
                self.stack.push(if *prefix { new } else { old_numeric });
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
            Step::EnterBlock { decls } => {
                let old_env = self.lexical_env;
                let env = new_declarative_environment(Some(old_env));
                block_declaration_instantiation(agent, decls, &env, self.strict)?;
                // A certified body's closures resolve captured bindings
                // through the static context chain, not these envs (the
                // flat slot/context layout is authoritative), so the
                // block env is scaffolding to them — mark it
                // context-transparent like a per-iteration env so a
                // closure created inside the block (a `try` block, whose
                // `EnterBlock` is always emitted) still reaches its
                // capture context. The env path keeps it visible.
                if body.scope.is_some()
                    && let EnvRecord::Declarative(declarative) = &*env
                {
                    declarative.mark_context_transparent();
                }
                self.lexical_env = env;
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
                    let completion = Completion::Normal(self.completion);
                    if let Some(outcome) = self.start_scope_disposal(
                        agent,
                        body,
                        resources,
                        completion,
                        DisposalResume::ApplyCompletion,
                    )? {
                        return Ok(StepControl::Outcome(outcome));
                    }
                }
            }
            Step::EnterIterTdzEnv { names } => {
                let old_env = self.lexical_env;
                let env = new_declarative_environment(Some(old_env));
                for name in names {
                    env.create_mutable_binding(name, false)?;
                }
                self.lexical_env = env;
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
                let with_env =
                    crate::env::new_object_environment(obj, true, Some(self.lexical_env));
                self.lexical_env = with_env;
                self.env_stack.push(with_env);
            }
            Step::PerIteration { names } => {
                let last = self.lexical_env;
                let outer = last.outer().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::ReferenceError,
                        "No outer environment for per-iteration bindings".into(),
                    )
                })?;
                let env = crate::env::new_per_iteration_environment(Some(outer), &last, names)?;
                // The per-iteration environment replaces the lexical
                // environment without joining the stack; the loop's exit
                // restores the loop environment directly.
                if let EnvRecord::Declarative(declarative) = &*env {
                    declarative.mark_context_transparent();
                }
                self.lexical_env = env;
            }
            Step::EnterPerIteration { names } => {
                // Cut 3 continuation (per-iteration loop heads): the
                // first per-iteration env of a certified loop — fresh
                // bindings copied from the capture context's head slots,
                // pushed on the env stack so the loop's exit/break
                // `LeaveBlock` pops it and restores the capture context.
                // Later iterations re-use `PerIteration` above (the
                // copies then come from the previous per-iteration env).
                let source = self.body_context.unwrap_or(self.lexical_env);
                let env = crate::env::new_per_iteration_environment(
                    Some(self.lexical_env),
                    &source,
                    names,
                )?;
                if let EnvRecord::Declarative(declarative) = &*env {
                    declarative.mark_context_transparent();
                }
                self.lexical_env = env;
                self.env_stack.push(env);
            }
            Step::LoadPerIteration { depth, index } => {
                let env = self.per_iteration_env(*depth)?;
                let value = context_env(&env).slot_value(*index).ok_or_else(|| {
                    JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    )
                })?;
                self.stack.push(value);
            }
            Step::StorePerIteration { depth, index } => {
                let value = self.pop();
                let env = self.per_iteration_env(*depth)?;
                env.set_slot(*index, value);
            }
            Step::UpdatePerIteration {
                depth,
                index,
                op,
                prefix,
            } => {
                let env = self.per_iteration_env(*depth)?;
                let old = context_env(&env).slot_value(*index).ok_or_else(|| {
                    JsError::new(
                        ErrorKind::ReferenceError,
                        "Cannot access a binding before initialization".into(),
                    )
                })?;
                let (old_numeric, new) = update_value(agent, op, &old)?;
                env.set_slot(*index, new);
                self.stack.push(if *prefix { new } else { old_numeric });
            }
            Step::EnterLoopEnv { kind, decls } => {
                let old_env = self.lexical_env;
                let env = new_declarative_environment(Some(old_env));
                // The head initializers (evaluated through the tree-walker)
                // run with the loop env active: a closure they create
                // captures the loop's own bindings, not the outer scope
                // (spec 14.7.4.2 — the loop env is the running
                // execution context's environment while the declarations
                // initialize).
                self.lexical_env = env;
                self.env_stack.push(env);
                if let Ok(mut context) = agent.running_context_mut() {
                    context.lexical_environment = env;
                }
                let head = (|| -> Result<(), JsError> {
                    for decl in decls {
                        let mut names = Vec::new();
                        crate::script::bound_names(&decl.pattern, &mut names);
                        for name in &names {
                            if *kind == VarDeclKind::Const
                                || matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                            {
                                // `using` heads bind immutably like
                                // `const` (spec 15.14.2).
                                env.create_immutable_binding(name, true)?;
                            } else {
                                env.create_mutable_binding(name, false)?;
                            }
                        }
                        let value = match &decl.init {
                            Some(init) => crate::expr::eval_expr(agent, init, self.strict)?,
                            None => Value::Undefined,
                        };
                        // A `using` head registers the disposable resource
                        // before the binding initializes (spec 14.7.4.2
                        // step 5.e, like the UsingInit step).
                        if matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing) {
                            let disposal_kind = if *kind == VarDeclKind::AwaitUsing {
                                crate::eval::DisposalKind::Async
                            } else {
                                crate::eval::DisposalKind::Sync
                            };
                            let resource = crate::eval::create_disposable_resource(
                                agent,
                                &value,
                                disposal_kind,
                            )?;
                            env.add_disposable_resource(resource);
                        }
                        crate::binding::binding_initialization(
                            agent,
                            &decl.pattern,
                            value,
                            Some(&env),
                            self.strict,
                        )?;
                    }
                    Ok(())
                })();
                if let Err(error) = head {
                    // spec 14.7.4.2 step 9: an abrupt head disposes the
                    // loop environment's resources before propagating,
                    // folding a throwing disposal into the error.
                    return Err(
                        match crate::eval::dispose_env_resources(agent, &env, Err(error)) {
                            Ok(Completion::Throw(value)) => crate::flow::uncaught_error(value),
                            Ok(_) => {
                                JsError::new(ErrorKind::TypeError, "Uncaught disposal error".into())
                            }
                            Err(error) => error,
                        },
                    );
                }
            }
            Step::FastLoopBind { var, num } => {
                self.fast_loop_bind(agent, *var, *num)?;
            }
            Step::FastLoopStore { var, num } => {
                self.fast_loop_store(agent, *var, *num)?;
            }
            Step::CreateArguments { slot, mapped } => {
                // Cut 3 continuation: the strict body's unmapped
                // arguments object, built from the call's arguments; a
                // sloppy simple-param body gets the mapped object
                // aliasing its formals through the capture context.
                let value = match mapped {
                    Some(formals) => {
                        let func = agent
                            .running_context()?
                            .function
                            .unwrap_or(Value::Undefined);
                        let formals: Vec<JsString> =
                            formals.iter().map(|id| crux::lookup(*id)).collect();
                        crate::function::create_mapped_arguments_object(
                            agent,
                            func,
                            &self.call_args,
                            &formals,
                            self.lexical_env,
                        )?
                    }
                    None => {
                        crate::function::create_unmapped_arguments_object(agent, &self.call_args)?
                    }
                };
                *self.frame_get_mut(*slot) = value;
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }

    /// `try`/`catch`/`finally` state, `throw`, and `continue` across a finally.
    #[cold]
    #[inline(never)]
    pub(crate) fn cold_tryctl(
        &mut self,
        agent: &mut Agent,
        body: &CompiledBody,
        step: &Step,
    ) -> Result<StepControl, JsError> {
        match step {
            Step::CatchBind { param, decls } => {
                // A caught throw discarded the try block's envs: dispose
                // their `using` resources first, folding into the thrown
                // value (spec 9.4.3 on the try block's abrupt
                // completion), so the catch observes the folded error.
                if let Some((saved_env, depth)) = self.pending_catch_disposal.take() {
                    let mut resources = Vec::new();
                    for index in (depth..self.env_stack.len()).rev() {
                        if let Some(env) = self.env_stack.get(index) {
                            resources.extend(env.drain_disposable_resources().into_iter().rev());
                        }
                    }
                    self.env_stack.truncate(depth);
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
                            return Ok(StepControl::Outcome(outcome));
                        }
                    } else {
                        self.restore_env(saved_env, depth);
                    }
                }
                let thrown = self.thrown.take().unwrap_or(Value::Undefined);
                let old_env = self.lexical_env;
                let env = new_declarative_environment(Some(old_env));
                // The catch parameter binds in its own environment, so a
                // default initializer's closure captures the parameter
                // (spec 15.1.7 step 7). All the created environments go on
                // the stack so the body's LeaveBlock(s) unwind back to
                // `old_env` — otherwise the parameter environment would
                // stay active after the catch, leaking its bindings.
                // A certified body's closures resolve captures through the
                // static context chain, so its catch envs are scaffolding
                // to them — mark them context-transparent (like a
                // per-iteration env) so a closure created inside the catch
                // still reaches its capture context. The env path keeps
                // them visible.
                if body.scope.is_some()
                    && let EnvRecord::Declarative(declarative) = &*env
                {
                    declarative.mark_context_transparent();
                }
                let body_env = match &param {
                    Some(param) => {
                        let param_env = new_declarative_environment(Some(env));
                        // Annex B.3.5: a direct eval's var-vs-lexical walk
                        // skips the catch parameter's environment.
                        param_env.mark_catch_param_env();
                        if body.scope.is_some()
                            && let EnvRecord::Declarative(declarative) = &*param_env
                        {
                            declarative.mark_context_transparent();
                        }
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
                        if let Ok(mut context) = agent.running_context_mut() {
                            context.lexical_environment = param_env;
                        }
                        crate::binding::binding_initialization(
                            agent,
                            param,
                            thrown,
                            Some(&param_env),
                            self.strict,
                        )?;
                        self.env_stack.push(param_env);
                        new_declarative_environment(Some(param_env))
                    }
                    None => env,
                };
                if body.scope.is_some()
                    && let EnvRecord::Declarative(declarative) = &*body_env
                {
                    declarative.mark_context_transparent();
                }
                block_declaration_instantiation(agent, decls, &body_env, self.strict)?;
                self.lexical_env = body_env;
                self.env_stack.push(body_env);
                // A certified body's catch parameter lives in a flat frame
                // slot (the scope scan allocates it; a captured or
                // destructuring parameter keeps the body on the env path):
                // write the thrown value so the slot reads in the catch
                // body see it. A parameter the scan left UNBOUND (it
                // shadows a live same-name binding) is skipped: the slot
                // belongs to the shadowed binding.
                if let Some(scope) = &body.scope
                    && let Some(param) = param
                    && let BindingPattern::Ident(name) = param
                    && !scope.shadowed_catch_params.contains(name)
                    && let Some(slot) = scope.slots.get(name)
                {
                    *self.frame_get_mut(*slot) = thrown;
                }
            }
            Step::FinallyEnd => {
                let pending = self.pending.pop().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::SyntaxError,
                        "FinallyEnd without a pending control".into(),
                    )
                })?;
                let ctl = match pending {
                    PendingControl::Normal {
                        after, env, depth, ..
                    } => {
                        self.restore_env(env, depth);
                        Ctl::Normal { after }
                    }
                    PendingControl::Break {
                        target, env, depth, ..
                    } => {
                        self.restore_env(env, depth);
                        Ctl::Break { target }
                    }
                    PendingControl::Continue {
                        target, env, depth, ..
                    } => {
                        self.restore_env(env, depth);
                        Ctl::Continue { target }
                    }
                    PendingControl::Return {
                        value, env, depth, ..
                    } => {
                        self.restore_env(env, depth);
                        Ctl::Return { value }
                    }
                    PendingControl::Throw {
                        value, env, depth, ..
                    } => {
                        self.restore_env(env, depth);
                        // A pending throw applies through the throw
                        // machinery (a catch in the same body may cover
                        // it); control_transfer only routes finallys.
                        match self.throw_machinery(agent, body, value)? {
                            CtlResult::Continue => return Ok(StepControl::Next),
                            CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                        }
                    }
                };
                match self.control_transfer(agent, body, ctl)? {
                    CtlResult::Continue => {}
                    CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                }
            }
            Step::Continue { target } => {
                match self.control_transfer(agent, body, Ctl::Continue { target: *target })? {
                    CtlResult::Continue => return Ok(StepControl::Next),
                    CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                }
            }
            Step::Throw { span } => {
                self.set_site(agent, *span);
                let value = self.pop();
                match self.throw_machinery(agent, body, value)? {
                    CtlResult::Continue => return Ok(StepControl::Next),
                    CtlResult::Done(outcome) => return Ok(StepControl::Outcome(outcome)),
                }
            }
            // The dispatcher's or-pattern admits only this family's
            // variants; anything else is a wiring mistake, not a case.
            _ => unreachable!("step dispatched to the wrong cold family"),
        }
        Ok(StepControl::Next)
    }
}
