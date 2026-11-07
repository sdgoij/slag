//! A QuickJS facade over Slag.
//!
//! The public Rust API is shaped after `rquickjs`, the way `crates/v8` is
//! shaped after `rusty_v8`. The C/FFI drop-in (`quickjs.h` symbols) lives under
//! `src/c/` and is enabled by the `ng` or `bellard` feature.
//!
//! # rquickjs API fidelity
//!
//! The Rust surface and its known divergences:
//!
//! | rquickjs | here | notes |
//! |---|---|---|
//! | `Runtime` | [`Runtime`] | heap limits, terminate, `run_microtasks`, contexts, functions, classes |
//! | `Context`/`Ctx` | [`Context`] | `eval`, `global`, `set_global`, `run_microtasks` |
//! | `Value` | [`Value`] | primitives + `as_*` accessors; copyable, no `'js` lifetime |
//! | `Object` | [`Object`] | `new`, get/set/has/delete |
//! | `Array` | [`Array`] | `new`, `length`, get/set |
//! | `Function` | [`Function`] | `call`, `as_value` |
//! | `String` | [`String`] | `from_str`, `to_string`, `len` |
//! | `Symbol` | [`Symbol`] | `new`, `description`, `to_string` |
//! | `BigInt` | [`BigInt`] | `from_i64/u64/f64/str`, `to_i64/u64/f64/string` |
//! | `Error` | [`Error`] | `kind`, `message`, `thrown` |
//! | `Class` | [`Class`] | constructor/prototype/method; finalizers via `crux::host::HostOps`, not wired |
//! | `Module` | [`Module`] | `new`/`evaluate`/`namespace`/`status`/`name` |
//! | `Promise` | [`Promise`] | resolve/reject/then/state/result |
//! | `ArrayBuffer`/`TypedArray`/`DataView` | same names | byte-level access |
//!
//! Known divergences:
//! - No `'js` lifetime: values are copyable handles and the context is passed
//!   explicitly, the way `crates/v8` carries no isolate lifetime.
//! - `String`/`Symbol`/`BigInt` constructors take `&Context` for rquickjs parity
//!   though the engine does not need it yet.
//! - Multiple contexts per runtime: realms are distinct, but `eval` runs in the
//!   agent's current realm rather than the context's (an engine gap, not a
//!   facade one).
//! - Static module imports resolve through the engine's own registry; dynamic
//!   `import()` and `import.meta` are embedding gaps.

#[cfg(all(feature = "ng", feature = "bellard"))]
compile_error!("qjs: enable at most one of `ng` or `bellard`");

mod array;
mod class;
mod context;
mod exception;
mod function;
mod jsruntime;
mod module;
mod object;
mod promise;
mod string;
mod typed_array;
mod value;

#[cfg(any(feature = "ng", feature = "bellard"))]
mod c;

pub use array::Array;
pub use class::Class;
pub use context::Context;
pub use exception::Error;
pub use function::{CallArgs, Function};
pub use jsruntime::Runtime;
pub use module::{Module, ModuleStatus};
pub use object::Object;
pub use promise::Promise;
pub use string::{BigInt, String, Symbol};
pub use typed_array::{ArrayBuffer, DataView, ElementType, TypedArray};
pub use value::Value;

/// Panic containment for the C boundary. A Rust panic in a host callback (or
/// anywhere inside an FFI entry point) is caught by [`guard`], which poisons the
/// thread's isolate so later calls reject instead of running over torn state.
/// Re-exported so hosts can clear the poison after rebuilding their state.
pub use ffi::{guard, is_poisoned, unpoison};

/// The facade's result type (rquickjs's `Result`).
pub type Result<T> = std::result::Result<T, Error>;
