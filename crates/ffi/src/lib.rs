//! Shared C-ABI plumbing for the drop-in surfaces (`crates/jsc`,
//! `crates/v8`): panic containment, handle tables, value/string marshaling,
//! and exception conversion. Not a public API by itself.
//!
//! Opaque C refs (`JSValueRef`, `JSStringRef`, ...) are ids into
//! thread-local strongly-owned tables, so host-held values stay alive for
//! as long as the host holds the ref — the Rc-based value model makes
//! "protected" handles unnecessary.

pub mod exception;
pub mod guard;
pub mod marshal;
pub mod tables;

pub use exception::throw;
pub use guard::guard;
pub use tables::{release_string, release_value, retain_string, retain_value, string, value};
