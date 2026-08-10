//! Jobs (spec 9.5): Job Abstract Closures and the host hooks that enqueue
//! them, plus the host's job-running loop.
//!
//! The promise-specific job constructors (NewPromiseReactionJob,
//! NewPromiseResolveThenableJob) and JobCallback records arrive with the
//! Promise built-in in Phase 15; the queue machinery lives here.

use crux::error::JsError;
use crux::handle::Handle;
use crux::value::Value;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::realm::initialize_host_defined_realm;

    #[test]
    fn jobs_drain_in_fifo_order_with_promise_priority() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm.clone());

        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        agent.enqueue_generic_job(Some(realm.clone()), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("generic-1");
                Ok(Value::Undefined)
            }
        });
        agent.enqueue_promise_job(Some(realm.clone()), {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("promise-1");
                Ok(Value::Undefined)
            }
        });
        agent.enqueue_promise_job(Some(realm.clone()), {
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
        agent.push_bootstrap_context(realm.clone());

        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        agent.enqueue_generic_job(Some(realm.clone()), {
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
}
