//! The platform and its tasks (`v8::Platform`, `v8::PlatformImpl`, `v8::Task`,
//! `v8::IdleTask`).
//!
//! Slag does no work off the calling thread: nothing compiles in the
//! background, nothing marks concurrently, and there is no worker pool. So the
//! platform a host installs is held and never handed a task — there is none to
//! hand it, which is also why [`Task`] and [`IdleTask`] have no constructor
//! here. What a host gets from this shape is that its initialization sequence
//! runs and its own `PlatformImpl` type-checks; what it does not get is V8's
//! background work, because Slag has no background work.
//!
//! The one part that is the engine's own: `pump_message_loop` drains the
//! isolate's job queue (promise jobs, due timers, generic jobs), because that
//! queue *is* Slag's message loop.

use crate::isolate::Isolate;
use crate::support::{SharedRef, UniqueRef};

/// A V8 platform (v8::Platform).
///
/// The crate we stand in for wraps a C++ object with a thread pool; this holds
/// the host's [`PlatformImpl`] when the platform is a custom one, and nothing
/// otherwise, because the engine has nothing to post.
pub struct Platform {
    /// The host's implementation, held only for its `Drop`: nothing in Slag
    /// posts a task, so nothing here is ever called.
    #[allow(dead_code)]
    host: Option<Box<dyn PlatformImpl>>,
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Platform(..)")
    }
}

/// A unit of foreground work (v8::Task).
///
/// A host receives one from its [`PlatformImpl`], schedules it, and runs it.
/// Nothing in Slag posts one, so the only way to have a `Task` is to be handed
/// it by a platform the engine drives — which is the state this bridge is in
/// until the engine grows background work.
pub struct Task {
    job: Box<dyn FnOnce() + Send>,
}

impl Task {
    /// Run the task, consuming it (v8::Task::Run).
    pub fn run(self) {
        (self.job)();
    }
}

/// A unit of idle work (v8::IdleTask): the same, with a deadline to finish by.
pub struct IdleTask {
    job: Box<dyn FnOnce(f64) + Send>,
}

impl IdleTask {
    /// Run the idle task with `deadline_in_seconds`, consuming it
    /// (v8::IdleTask::Run).
    pub fn run(self, deadline_in_seconds: f64) {
        (self.job)(deadline_in_seconds);
    }
}

/// How a host schedules the work a platform is handed (v8::PlatformImpl).
///
/// Every method has the crate we stand in for's default: run the task right
/// away. A host overrides them to hand work to its own event loop.
pub trait PlatformImpl: Send + Sync {
    /// Called when a task is posted for the given isolate.
    fn post_task(&self, isolate_ptr: *mut std::ffi::c_void, task: Task) {
        let _ = isolate_ptr;
        task.run();
    }

    /// The same, for a task that must not run inside a nested message loop.
    fn post_non_nestable_task(&self, isolate_ptr: *mut std::ffi::c_void, task: Task) {
        let _ = isolate_ptr;
        task.run();
    }

    /// Called when a task is posted to run after `delay_in_seconds`.
    fn post_delayed_task(
        &self,
        isolate_ptr: *mut std::ffi::c_void,
        task: Task,
        delay_in_seconds: f64,
    ) {
        let _ = (isolate_ptr, delay_in_seconds);
        task.run();
    }

    /// The same, for a delayed task that must not run inside a nested loop.
    fn post_non_nestable_delayed_task(
        &self,
        isolate_ptr: *mut std::ffi::c_void,
        task: Task,
        delay_in_seconds: f64,
    ) {
        let _ = (isolate_ptr, delay_in_seconds);
        task.run();
    }

    /// Called when an idle task is posted for the given isolate.
    fn post_idle_task(&self, isolate_ptr: *mut std::ffi::c_void, task: IdleTask) {
        let _ = isolate_ptr;
        task.run(0.0);
    }
}

/// A platform with the default behavior (`v8::platform::NewDefaultPlatform`).
///
/// `thread_pool_size` and `idle_task_support` describe a thread pool the engine
/// does not have; they are taken for the shape's sake.
pub fn new_default_platform(thread_pool_size: u32, idle_task_support: bool) -> UniqueRef<Platform> {
    Platform::new(thread_pool_size, idle_task_support)
}

/// A default platform that does not enforce thread-isolated allocations
/// (`v8::platform::NewUnprotectedDefaultPlatform`).
pub fn new_unprotected_default_platform(
    thread_pool_size: u32,
    idle_task_support: bool,
) -> UniqueRef<Platform> {
    Platform::new_unprotected(thread_pool_size, idle_task_support)
}

/// A default platform with no worker pool
/// (`v8::platform::NewSingleThreadedDefaultPlatform`).
pub fn new_single_threaded_default_platform(idle_task_support: bool) -> UniqueRef<Platform> {
    Platform::new_single_threaded(idle_task_support)
}

/// A platform that hands its work to `platform_impl`
/// (`v8::platform::NewCustomPlatform`).
///
/// The host's implementation is owned by the platform, and dropped with it.
pub fn new_custom_platform(
    thread_pool_size: u32,
    idle_task_support: bool,
    unprotected: bool,
    platform_impl: impl PlatformImpl + 'static,
) -> UniqueRef<Platform> {
    Platform::new_custom(
        thread_pool_size,
        idle_task_support,
        unprotected,
        platform_impl,
    )
}

impl Platform {
    /// A platform with the default behavior. See [`new_default_platform`].
    pub fn new(thread_pool_size: u32, idle_task_support: bool) -> UniqueRef<Self> {
        let _ = (thread_pool_size, idle_task_support);
        UniqueRef::new(Self { host: None })
    }

    /// See [`new_unprotected_default_platform`].
    pub fn new_unprotected(thread_pool_size: u32, idle_task_support: bool) -> UniqueRef<Self> {
        Self::new(thread_pool_size, idle_task_support)
    }

    /// See [`new_single_threaded_default_platform`].
    pub fn new_single_threaded(idle_task_support: bool) -> UniqueRef<Self> {
        let _ = idle_task_support;
        UniqueRef::new(Self { host: None })
    }

    /// See [`new_custom_platform`].
    pub fn new_custom(
        thread_pool_size: u32,
        idle_task_support: bool,
        unprotected: bool,
        platform_impl: impl PlatformImpl + 'static,
    ) -> UniqueRef<Self> {
        let _ = (thread_pool_size, idle_task_support, unprotected);
        UniqueRef::new(Self {
            host: Some(Box::new(platform_impl)),
        })
    }

    /// Pump the isolate's message loop (`v8::Platform::PumpMessageLoop`).
    ///
    /// Answers whether there was work to run. The tasks a V8 platform queues do
    /// not exist here, so what is pumped is the engine's own job queue — which
    /// is the message loop a Slag host has. `wait_for_work` asks whether to
    /// block for work that could arrive from another thread; work only ever
    /// arrives on this one, so it does not change what happens.
    pub fn pump_message_loop(
        platform: &SharedRef<Self>,
        isolate: &Isolate,
        wait_for_work: bool,
    ) -> bool {
        let _ = (platform, wait_for_work);
        let mut isolate = *isolate;
        let had_work = !isolate.engine_mut().agent().job_queues_empty();
        if let Err(error) = isolate.run_microtasks() {
            // A job that threw is reported the way a host's loop reports it: as
            // the isolate's pending exception, not as this return value.
            crate::throw(&isolate, &error);
        }
        had_work
    }

    /// Run pending idle tasks (`v8::Platform::RunIdleTasks`).
    ///
    /// Nothing to run: Slag's collector works at allocation safe points and
    /// nothing compiles in the background, so the engine has no work it would
    /// rather do later.
    pub fn run_idle_tasks(
        platform: &SharedRef<Self>,
        isolate: &Isolate,
        idle_time_in_seconds: f64,
    ) {
        let _ = (platform, isolate, idle_time_in_seconds);
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::eval_number;

    /// Pumping the message loop is draining the engine's job queue — the queue a
    /// queueing engine has — and the answer says whether there was work in it.
    #[test]
    fn pumping_the_message_loop_runs_the_engine_job_queue() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        let platform = crate::new_default_platform(0, false).make_shared();

        let code = crate::String::new(
            scope,
            "Promise.resolve().then(function () { globalThis.ran = 1; })",
        )
        .expect("string");
        let script = crate::Script::compile(scope, code, None).expect("compile");
        script.run(scope).expect("run");

        assert!(super::Platform::pump_message_loop(&platform, scope, false));
        assert_eq!(eval_number(scope, "globalThis.ran"), 1.0);
        assert!(!super::Platform::pump_message_loop(&platform, scope, false));

        // Idle time has no work in it: Slag's collector runs at allocation safe
        // points and nothing compiles in the background.
        super::Platform::run_idle_tasks(&platform, scope, 0.001);
    }
}
