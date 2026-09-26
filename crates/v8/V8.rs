//! The initialization state a process shares (`v8::V8`).
//!
//! The crate we stand in for keeps a state machine — uninitialized, platform
//! set, initialized, disposed, platform shut down — and panics on any move that
//! is not the next one. That machine *is* the behavior of these functions, so it
//! is reproduced here in full, with the same panic message.
//!
//! One divergence, and it is the engine's: Slag's heap, agent and isolate are
//! all thread-local, so this state is thread-local too, where V8's is
//! process-global. A host that initializes V8 on one thread and asks
//! `get_current_platform` from another finds it uninitialized, with a panic
//! saying so, rather than a platform it cannot safely share.

use std::cell::RefCell;

use crate::platform::Platform;
use crate::support::SharedRef;

enum GlobalState {
    Uninitialized,
    PlatformInitialized(SharedRef<Platform>),
    Initialized(SharedRef<Platform>),
    Disposed(SharedRef<Platform>),
    PlatformShutdown,
}

/// The handler a fatal error is reported through, once a host installs one.
type FatalErrorHandler = Box<dyn Fn(&str, i32, &str)>;

thread_local! {
    static GLOBAL_STATE: RefCell<GlobalState> = const { RefCell::new(GlobalState::Uninitialized) };
    static FLAGS: RefCell<Option<String>> = const { RefCell::new(None) };
    static FATAL_ERROR_HANDLER: RefCell<Option<FatalErrorHandler>> = const { RefCell::new(None) };
}

/// Install the handler a fatal error is reported through
/// (v8::V8::SetFatalErrorHandler).
///
/// Recorded, and never called: V8's fatal handler exists because V8 can die in the
/// middle of an operation and has a file, a line and a message to report it with,
/// where this engine's failures are a Rust panic or the allocator's own abort —
/// neither of which has a location to hand a host. A host that installs one gets
/// the crate's surface and the process's failure reporting rather than V8's; what
/// it must not get is a handler that looks installed and silently is not, which is
/// why the slot is kept and the tier is stated here.
pub fn set_fatal_error_handler(handler: impl Fn(&str, i32, &str) + 'static) {
    FATAL_ERROR_HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::new(handler)));
}

/// Whether a fatal-error handler has been installed (this bridge's own accessor,
/// so the tier above is testable rather than only documented).
#[cfg(test)]
pub(crate) fn has_fatal_error_handler() -> bool {
    FATAL_ERROR_HANDLER.with(|slot| slot.borrow().is_some())
}

/// Panic unless V8 is initialized (v8::V8::AssertInitialized).
pub fn assert_initialized() {
    GLOBAL_STATE.with(|state| match &*state.borrow() {
        GlobalState::Initialized(_) => {}
        _ => panic!("Invalid global state"),
    });
}

/// Hand the command line to V8 and get back what it did not understand
/// (v8::V8::SetFlagsFromCommandLine).
///
/// Slag's engine has no flag surface, so nothing is consumed and the arguments
/// come back as they went in: a host that parses its own flags off the returned
/// list sees all of them.
pub fn set_flags_from_command_line(args: Vec<String>) -> Vec<String> {
    set_flags_from_command_line_with_usage(args, None)
}

/// [`set_flags_from_command_line`] with a usage string to print
/// (v8::V8::SetFlagsFromCommandLine).
///
/// The usage string is for the flags a host's engine would have printed it for;
/// there are none here, so nothing is printed.
pub fn set_flags_from_command_line_with_usage(
    args: Vec<String>,
    usage: Option<&str>,
) -> Vec<String> {
    let _ = usage;
    args
}

/// Record the flags a host set (v8::V8::SetFlagsFromString).
///
/// Slag's engine is not flag-configurable, so the string is kept rather than
/// interpreted — see [`get_flags`]. A host whose behavior depends on a flag
/// taking effect gets that flag's *absence* rather than a wrong one.
pub fn set_flags_from_string(flags: &str) {
    FLAGS.with(|slot| *slot.borrow_mut() = Some(flags.to_string()));
}

/// The flags a host has set, as given (this bridge's own accessor — the crate we
/// stand in for reads flags back only through its own flag state).
pub fn get_flags() -> Option<String> {
    FLAGS.with(|slot| slot.borrow().clone())
}

/// The version of what is behind this API (`v8::VERSION_STRING`).
///
/// The same statement [`get_version`] makes, so a host that reports the version
/// in a header and one that asks the API agree — and neither claims to be V8.
pub const VERSION_STRING: &str = concat!("slag (v8 API ", env!("CARGO_PKG_VERSION"), ")");

/// The version string (v8::V8::GetVersion).
///
/// There is no V8 here, so this names what is: the API level this bridge
/// implements, behind the engine serving it. A host that compares it against a
/// V8 version string finds a different one, which is the truthful answer.
pub fn get_version() -> &'static str {
    VERSION_STRING
}

/// Set the platform to use (v8::V8::InitializePlatform). Must come before
/// [`initialize`].
pub fn initialize_platform(platform: SharedRef<Platform>) {
    GLOBAL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let next = match &*state {
            GlobalState::Uninitialized => GlobalState::PlatformInitialized(platform.clone()),
            _ => panic!("Invalid global state"),
        };
        *state = next;
    });
}

/// Initialize the engine (v8::V8::Initialize). Must come after
/// [`initialize_platform`] and before the first isolate.
pub fn initialize() {
    GLOBAL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let next = match &*state {
            GlobalState::PlatformInitialized(platform) => {
                GlobalState::Initialized(SharedRef::clone(platform))
            }
            _ => panic!("Invalid global state"),
        };
        *state = next;
    });
}

/// The platform this thread initialized V8 with (v8::V8::GetCurrentPlatform).
pub fn get_current_platform() -> SharedRef<Platform> {
    GLOBAL_STATE.with(|state| match &*state.borrow() {
        GlobalState::Initialized(platform) => SharedRef::clone(platform),
        _ => panic!("Invalid global state"),
    })
}

/// Release the engine's resources (v8::V8::Dispose).
///
/// Disposing is permanent, as it is there; the engine itself is thread-local, so
/// what this releases is the state this thread recorded.
///
/// # Safety
///
/// The crate we stand in for requires every isolate to be disposed first. The
/// engine owns its own heaps, so nothing here can be dangling, but a host must
/// still not use an isolate it created before this call.
pub unsafe fn dispose() -> bool {
    GLOBAL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let next = match &*state {
            GlobalState::Initialized(platform) => GlobalState::Disposed(SharedRef::clone(platform)),
            _ => panic!("Invalid global state"),
        };
        *state = next;
    });
    true
}

/// Drop the platform after [`dispose`] (v8::V8::DisposePlatform).
/// Disposes the platform the state has been keeping since
/// [`dispose`] (v8::V8::DisposePlatform).
pub fn dispose_platform() {
    GLOBAL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let next = match &*state {
            // Reading the platform is what drops it: the state stops keeping it.
            GlobalState::Disposed(platform) => {
                drop(SharedRef::clone(platform));
                GlobalState::PlatformShutdown
            }
            _ => panic!("Invalid global state"),
        };
        *state = next;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlatformImpl;
    use crate::platform::new_custom_platform;

    /// The handler installed is the one kept: this engine reports its failures
    /// through a panic or the allocator's abort, so the slot is what the install
    /// can honestly be — and a host that reads it back must see its own handler
    /// rather than a promise of V8's reporting.
    #[test]
    fn a_fatal_error_handler_is_kept() {
        assert!(!has_fatal_error_handler());
        set_fatal_error_handler(|file: &str, line: i32, message: &str| {
            let _ = (file, line, message);
        });
        assert!(has_fatal_error_handler());
        // The handler's own shape: a file, a line and a message, as V8's
        // `FatalErrorCallback` takes them.
        set_fatal_error_handler(|_file: &str, _line: i32, _message: &str| {});
        assert!(has_fatal_error_handler());
    }

    /// A host with no implementation of its own.
    struct Noop;

    impl PlatformImpl for Noop {}

    /// The state machine *is* the behavior of these functions, so it is tested
    /// as one sequence: each move is allowed only from the state before it, and
    /// a host that gets it wrong is told so rather than given a quiet success.
    ///
    /// One sequence, because the state is per-thread and a host initializes it
    /// once.
    #[test]
    fn the_initialization_state_machine_admits_only_the_next_move() {
        fn refused<T>(body: impl FnOnce() -> T + std::panic::UnwindSafe) -> bool {
            std::panic::catch_unwind(body).is_err()
        }

        // Nothing is initialized yet, so nothing can be asked of it.
        assert!(refused(assert_initialized));
        assert!(refused(get_current_platform));
        assert!(refused(dispose_platform));

        // Silence the panic messages the refusals print; cosmetic only, and
        // restored before the test ends.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        initialize_platform(new_custom_platform(0, false, true, Noop).make_shared());
        assert!(refused(assert_initialized));
        assert!(refused(|| initialize_platform(
            new_custom_platform(0, false, true, Noop).make_shared()
        )));

        initialize();
        assert!(refused(initialize));
        assert_initialized();
        let current = get_current_platform();
        drop(current);

        assert!(unsafe { dispose() });
        assert!(refused(get_current_platform));
        assert!(refused(initialize));
        dispose_platform();
        assert!(refused(dispose_platform));

        std::panic::set_hook(hook);
    }

    /// Flags are recorded rather than interpreted, which is what the accessor is
    /// for: a host can see what it set, and the engine honors none of it.
    #[test]
    fn flags_are_recorded_verbatim_and_left_for_the_host_to_parse() {
        assert_eq!(get_flags(), None);
        set_flags_from_string("--js-float16array --expose-gc");
        assert_eq!(
            get_flags().as_deref(),
            Some("--js-float16array --expose-gc")
        );

        let args = vec!["deno".to_string(), "--allow-net".to_string()];
        assert_eq!(set_flags_from_command_line(args.clone()), args);

        assert!(get_version().starts_with("slag"));
    }
}
