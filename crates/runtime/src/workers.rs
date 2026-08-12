//! Experimental multi-agent mode (PLAN Phase 17, the `workers` cargo
//! feature): each worker runs an [`Agent`] on its own OS thread with its own
//! realm and job queue. Workers share `SharedArrayBuffer` byte blocks with
//! the spawning agent, and `Atomics.wait`/`notify` coordinate through the
//! global waiter registry in `builtins::atomics`. Worker creation is a host
//! seam (`HostHooks::create_worker`); `spawn_worker` is the runtime's
//! implementation for hosts and tests.

use crux::error::JsError;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::typed_array::SharedBuffer;

use crate::agent::Agent;

/// Run `source` as a worker agent on a new OS thread, with the shared byte
/// block `shared` exposed to the script as the global `$shared`
/// SharedArrayBuffer. The worker's `[[CanBlock]]` is true, so `Atomics.wait`
/// suspends the thread until a `notify` or timeout. The join handle yields
/// the completion value's display form, or the first error message. (A
/// `Value` itself cannot cross the thread boundary: it is `Rc`-based.)
pub fn spawn_worker(
    shared: SharedBuffer,
    byte_length: usize,
    source: String,
) -> std::thread::JoinHandle<Result<String, String>> {
    std::thread::spawn(move || {
        let result = (|| -> Result<crux::value::Value, JsError> {
            let mut agent = Agent::new();
            agent.can_block = true;
            agent.initialize_host_defined_realm()?;
            let sab = crate::builtins::array_buffer::shared_array_buffer_from_block(
                &mut agent,
                shared,
                byte_length,
            )?;
            agent
                .current_realm()?
                .global_object
                .define_property_or_throw(
                    &JsString::from_utf8("$shared"),
                    &PropertyDescriptor {
                        value: Some(sab),
                        writable: Some(true),
                        get: None,
                        set: None,
                        enumerable: Some(false),
                        configurable: Some(true),
                    },
                )?;
            let value = agent.run_script(&source)?;
            agent.run_jobs()?;
            Ok(value)
        })();
        result
            .map(|value| format!("{value}"))
            .map_err(|error| format!("{error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::value::Value;

    /// Extract the shared byte block of the SharedArrayBuffer `source`
    /// evaluates to.
    fn shared_block(source: &str) -> (SharedBuffer, usize) {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let sab = agent.run_script(source).unwrap();
        let Value::Object(obj) = &sab else {
            panic!("expected a SharedArrayBuffer object");
        };
        let state = agent.buffer_data.get(&obj.id()).unwrap();
        let state = state.borrow();
        (state.shared.clone(), state.byte_length)
    }

    /// A fresh agent with `shared` bound as the global `__sab`.
    fn agent_with_shared(shared: SharedBuffer, byte_length: usize) -> Agent {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let sab = crate::builtins::array_buffer::shared_array_buffer_from_block(
            &mut agent,
            shared,
            byte_length,
        )
        .unwrap();
        agent
            .current_realm()
            .unwrap()
            .global_object
            .define_property_or_throw(
                &JsString::from_utf8("__sab"),
                &PropertyDescriptor {
                    value: Some(sab),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )
            .unwrap();
        agent
    }

    /// Message passing: the worker waits on slot 0, then returns slot 1.
    #[test]
    fn worker_message_passing_via_notify() {
        let (shared, byte_length) = shared_block("new SharedArrayBuffer(16)");
        let worker = spawn_worker(
            shared.clone(),
            byte_length,
            "const ta = new Int32Array($shared);\
             const s = Atomics.wait(ta, 0, 0);\
             JSON.stringify([s, ta[1]]);"
                .to_string(),
        );
        let mut agent = agent_with_shared(shared, byte_length);
        let result = agent
            .run_script(
                "const ta = new Int32Array(__sab);\
                 Atomics.store(ta, 1, 42);\
                 Atomics.store(ta, 0, 1);\
                 Atomics.notify(ta, 0, 1);",
            )
            .unwrap();
        assert!(
            matches!(result, Value::Number(n) if n == 0.0 || n == 1.0),
            "notify must wake at most one waiter, got {result:?}"
        );
        let worker_result = worker.join().unwrap().unwrap();
        assert!(
            worker_result == "[\"not-equal\",42]" || worker_result == "[\"ok\",42]",
            "unexpected worker result: {worker_result}"
        );
    }

    /// A SeqCst counter incremented by several workers: after they all
    /// finish, the count is exactly the sum of all increments.
    #[test]
    fn worker_incr_counter_stress() {
        let (shared, byte_length) = shared_block("new SharedArrayBuffer(64)");
        let mut handles = Vec::new();
        for _ in 0..4 {
            let worker = spawn_worker(
                shared.clone(),
                byte_length,
                "const ta = new Int32Array($shared);\
                 for (let i = 0; i < 5000; i++) Atomics.add(ta, 0, 1);"
                    .to_string(),
            );
            handles.push(worker);
        }
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let count = shared.atomic_load(0, 4).unwrap();
        assert_eq!(count, 4 * 5000);
    }
}
