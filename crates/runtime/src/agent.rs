//! Agents (spec 9.7) and the surrounding-agent operations.
//!
//! The agent owns the execution context stack and the job queues; its
//! record fields ([[]] names below) are the Agent Record fields of the
//! spec 9.7 table. Single-threaded: [[CanBlock]] is false, so
//! AgentCanSuspend() is false.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::JsString;
use crux::symbol::Symbol;
use crux::value::Value;

use crate::context::ExecutionContext;
use crate::job::Job;
use crate::realm::{Realm, initialize_host_defined_realm};

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

/// The surrounding agent: the execution context stack, the job queues, and
/// the Agent Record fields of spec 9.7.
#[derive(Debug)]
pub struct Agent {
    pub execution_context_stack: Vec<ExecutionContext>,
    pub(crate) promise_jobs: VecDeque<Job>,
    pub(crate) generic_jobs: VecDeque<Job>,
    pub(crate) timeout_jobs: VecDeque<(Instant, Job)>,
    /// [[LittleEndian]]: the host byte order used by GetValueFromBuffer.
    pub little_endian: bool,
    /// [[CanBlock]]: false for the main thread; Atomics.wait joins in
    /// Phase 17.
    pub can_block: bool,
    /// [[Signifier]]: globally unique per agent.
    pub signifier: u64,
    /// [[IsLockFree1/2/8]]: whether atomic ops of those sizes are lock-free.
    pub is_lock_free: [bool; 3],
    /// [[KeptAlive]]: objects/symbols kept alive until the end of the
    /// current Job (WeakRef, Phase 13).
    pub kept_alive: Vec<Value>,
    /// [[GlobalSymbolRegistry]]: `Symbol.for` entries (Phase 8).
    pub global_symbol_registry: RefCell<Vec<(JsString, Symbol)>>,
    /// [[ModuleAsyncEvaluationCount]]: module linking (Phase 7).
    pub module_async_evaluation_count: u32,
}

impl Agent {
    pub fn new() -> Self {
        Self {
            execution_context_stack: Vec::new(),
            promise_jobs: VecDeque::new(),
            generic_jobs: VecDeque::new(),
            timeout_jobs: VecDeque::new(),
            little_endian: cfg!(target_endian = "little"),
            can_block: false,
            signifier: NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed),
            is_lock_free: [
                is_lock_free_for_size(1),
                is_lock_free_for_size(2),
                is_lock_free_for_size(8),
            ],
            kept_alive: Vec::new(),
            global_symbol_registry: RefCell::new(Vec::new()),
            module_async_evaluation_count: 0,
        }
    }

    /// The running execution context: the top of the stack. Invariant: the
    /// stack is never empty after `initialize_host_defined_realm` has run.
    pub fn running_context(&self) -> Result<&ExecutionContext, JsError> {
        self.execution_context_stack.last().ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                "No running execution context".into(),
            )
        })
    }

    pub fn running_context_mut(&mut self) -> Result<&mut ExecutionContext, JsError> {
        self.execution_context_stack.last_mut().ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                "No running execution context".into(),
            )
        })
    }

    /// spec 9.7.1 AgentSignifier.
    pub fn agent_signifier(&self) -> u64 {
        self.signifier
    }

    /// spec 9.7.2 AgentCanSuspend: false because [[CanBlock]] is false.
    pub fn agent_can_suspend(&self) -> bool {
        self.can_block
    }

    /// InitializeHostDefinedRealm (spec 9.3.4) and push the bootstrap
    /// execution context.
    pub fn initialize_host_defined_realm(&mut self) -> Result<Handle<Realm>, JsError> {
        let realm = initialize_host_defined_realm(self)?;
        self.push_bootstrap_context(realm.clone());
        Ok(realm)
    }

    /// Push the initial execution context created in
    /// InitializeHostDefinedRealm: Function and ScriptOrModule are null.
    pub fn push_bootstrap_context(&mut self, realm: Handle<Realm>) {
        let global_env = realm.global_env.clone();
        self.execution_context_stack.push(ExecutionContext {
            function: None,
            realm,
            script_or_module: None,
            lexical_environment: global_env.clone(),
            variable_environment: global_env,
            private_environment: None,
        });
    }

    /// The current Realm Record (the Realm component of the running context).
    pub fn current_realm(&self) -> Result<Handle<Realm>, JsError> {
        Ok(self.running_context()?.realm.clone())
    }

    /// HostEnqueueGenericJob (spec 9.5.3): schedule a job without additional
    /// constraints such as priority.
    pub fn enqueue_generic_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        self.generic_jobs.push_back(Job::new(realm, closure));
    }

    /// HostEnqueuePromiseJob (spec 9.5.4): schedule a job at promise
    /// priority. Jobs run in the order their enqueues happened.
    pub fn enqueue_promise_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        self.promise_jobs.push_back(Job::new(realm, closure));
    }

    /// HostEnqueueTimeoutJob (spec 9.5.5): schedule a job to run after at
    /// least `milliseconds` milliseconds.
    pub fn enqueue_timeout_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        milliseconds: u64,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_millis(milliseconds);
        self.timeout_jobs
            .push_back((deadline, Job::new(realm, closure)));
    }

    /// RunJobs: drain the job queues — promise jobs first (FIFO), then due
    /// timeouts, then generic jobs — until nothing runnable remains.
    pub fn run_jobs(&mut self) -> Result<(), JsError> {
        loop {
            if let Some(job) = self.promise_jobs.pop_front() {
                self.run_job(job)?;
                continue;
            }
            let now = Instant::now();
            if let Some(index) = self
                .timeout_jobs
                .iter()
                .position(|(deadline, _)| *deadline <= now)
            {
                let Some((_, job)) = self.timeout_jobs.remove(index) else {
                    continue;
                };
                self.run_job(job)?;
                continue;
            }
            if let Some(job) = self.generic_jobs.pop_front() {
                self.run_job(job)?;
                continue;
            }
            break;
        }
        Ok(())
    }

    fn run_job(&mut self, job: Job) -> Result<Value, JsError> {
        (job.closure)(self)
    }

    pub fn job_queues_empty(&self) -> bool {
        self.promise_jobs.is_empty() && self.generic_jobs.is_empty() && self.timeout_jobs.is_empty()
    }

    /// Parse and evaluate a Script (spec 16.1.4-16.1.6) in the current
    /// realm, returning the script's completion value.
    pub fn run_script(&mut self, source: &str) -> Result<Value, JsError> {
        let realm = self.current_realm()?;
        let script = crate::script::parse_script(source, realm)?;
        crate::script::script_evaluation(self, &script)
    }
}

impl Default for Agent {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether atomic operations on values of `bytes` bytes are lock-free
/// ([[IsLockFree1/2/8]]). Implementation-defined per spec 9.7; 4-byte
/// operations are always lock-free (there is no [[IsLockFree4]]).
fn is_lock_free_for_size(bytes: usize) -> bool {
    matches!(bytes, 1 | 2) || (bytes == 8 && cfg!(target_pointer_width = "64"))
}

/// A helper used by tests and the CLI: create an agent, bootstrap its
/// realm, and evaluate `source`.
pub fn evaluate(source: &str) -> Result<Value, JsError> {
    let mut agent = Agent::new();
    agent.initialize_host_defined_realm()?;
    agent.run_script(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_record_fields_are_set() {
        let agent = Agent::new();
        assert!(!agent.can_block);
        assert!(!agent.agent_can_suspend());
        assert!(agent.little_endian || !agent.little_endian); // host-dependent
        assert_eq!(agent.is_lock_free.len(), 3);
        assert!(agent.job_queues_empty());
        assert!(agent.execution_context_stack.is_empty());
    }

    #[test]
    fn signifiers_are_unique() {
        let a = Agent::new();
        let b = Agent::new();
        assert_ne!(a.agent_signifier(), b.agent_signifier());
    }

    #[test]
    fn bootstrap_context_sets_global_environments() {
        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        let context = agent.running_context().unwrap();
        assert!(context.function.is_none());
        assert!(context.script_or_module.is_none());
        assert!(context.private_environment.is_none());
        assert_eq!(context.realm.global_object, realm.global_object);
    }

    #[test]
    fn running_context_requires_a_bootstrap_context() {
        let agent = Agent::new();
        assert!(agent.running_context().is_err());
        assert!(agent.current_realm().is_err());
    }

    #[test]
    fn timeout_jobs_run_when_due() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        agent.enqueue_timeout_job(Some(realm), 0, {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("timed");
                Ok(Value::Undefined)
            }
        });
        agent.run_jobs().unwrap();
        assert_eq!(*order.borrow(), vec!["timed"]);
    }
}
