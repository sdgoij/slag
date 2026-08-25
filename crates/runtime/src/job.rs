//! Jobs (spec 9.5): Job Abstract Closures and the host hooks that enqueue
//! them, plus the host's job-running loop, JobCallback Records, and the
//! JobCallback host hooks.
//!
//! The promise-specific job constructors (NewPromiseReactionJob,
//! NewPromiseResolveThenableJob) arrive with the Promise built-in in
//! Phase 15; the queue machinery and JobCallback hooks live here.

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::realm::Realm;

/// The body of a Job: an Abstract Closure that runs against the agent.
pub type JobClosure = Box<dyn FnOnce(&mut Agent) -> Result<Value, JsError>>;

/// A Job: an Abstract Closure that initiates an ECMAScript computation when
/// no other computation is in progress (spec 9.5). The closure captures
/// everything it needs and receives the agent so it can evaluate code and
/// enqueue further jobs.
pub struct Job {
    /// The realm the job runs in; `None` for jobs that evaluate no code.
    pub realm: Option<Handle<Realm>>,
    pub closure: JobClosure,
}

impl Job {
    pub fn new(
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) -> Self {
        Self {
            realm,
            closure: Box::new(closure),
        }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("realm", &self.realm)
            .finish_non_exhaustive()
    }
}

/// A JobCallback Record (spec 9.5.1): a function object to invoke when a
/// Job runs, plus a host-defined slot for propagating host context.
#[derive(Debug, Clone)]
pub struct JobCallback {
    pub callback: Value,
    pub host_defined: Option<Value>,
}

/// HostMakeJobCallback (spec 9.5.2): the default implementation wraps the
/// callback with an empty host-defined field.
pub fn host_make_job_callback(callback: Value) -> JobCallback {
    JobCallback {
        callback,
        host_defined: None,
    }
}

/// HostCallJobCallback (spec 9.5.3): the default implementation performs
/// Call on the callback with the given this value and argument list.
pub fn host_call_job_callback(
    job_callback: &JobCallback,
    this_value: Value,
    arg_list: &[Value],
) -> Result<Value, JsError> {
    if !is_callable(&job_callback.callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "callback is not callable".into(),
        ));
    }
    crux::function::call(&job_callback.callback, this_value, arg_list)
}

/// HostCallJobCallback through the runtime's agent-aware call dispatcher, so
/// ECMAScript-function callbacks (user functions) reach their bodies. Promise
/// reaction jobs use this.
pub fn host_call_job_callback_agent(
    agent: &mut crate::agent::Agent,
    job_callback: &JobCallback,
    this_value: Value,
    arg_list: &[Value],
) -> Result<Value, JsError> {
    if !is_callable(&job_callback.callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "callback is not callable".into(),
        ));
    }
    crate::function::call(agent, &job_callback.callback, this_value, arg_list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::realm::initialize_host_defined_realm;

    #[test]
    fn jobs_drain_in_fifo_order_with_promise_priority() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm);

        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        agent.enqueue_generic_job(Some(realm), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("generic-1");
                Ok(Value::Undefined)
            }
        });
        agent.enqueue_promise_job(Some(realm), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("promise-1");
                Ok(Value::Undefined)
            }
        });
        agent.enqueue_promise_job(Some(realm), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("promise-2");
                Ok(Value::Undefined)
            }
        });
        agent.enqueue_generic_job(Some(realm), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("generic-2");
                Ok(Value::Undefined)
            }
        });

        agent.run_jobs().unwrap();
        // Promise jobs drain before generic jobs, FIFO within each queue.
        assert_eq!(
            *order.borrow(),
            vec!["promise-1", "promise-2", "generic-1", "generic-2"]
        );
        assert!(agent.job_queues_empty());
    }

    #[test]
    fn jobs_enqueued_while_running_are_drained() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm);

        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        agent.enqueue_generic_job(Some(realm), {
            let order = order.clone();
            move |agent| {
                order.borrow_mut().push("first");
                agent.enqueue_generic_job(None, {
                    let order = order.clone();
                    move |_| {
                        order.borrow_mut().push("second");
                        Ok(Value::Undefined)
                    }
                });
                Ok(Value::Undefined)
            }
        });

        agent.run_jobs().unwrap();
        assert_eq!(*order.borrow(), vec!["first", "second"]);
        assert!(agent.job_queues_empty());
    }

    #[test]
    fn job_callback_records_wrap_callables() {
        let fun = Value::Function(crux::Function::new(None));
        let record = host_make_job_callback(fun.clone());
        assert_eq!(record.callback, fun);
        assert!(record.host_defined.is_none());
    }

    #[test]
    fn host_call_job_callback_requires_callables() {
        // A non-callable callback fails the IsCallable assertion.
        let record = host_make_job_callback(Value::Undefined);
        assert!(host_call_job_callback(&record, Value::Undefined, &[]).is_err());
        // A function callback is dispatched to Call, which arrives in
        // Phase 7; until then the call reports the pending capability.
        let record = host_make_job_callback(Value::Function(crux::Function::new(None)));
        let err = host_call_job_callback(&record, Value::Undefined, &[]).unwrap_err();
        assert_eq!(err.kind, crux::ErrorKind::TypeError);
    }
}
