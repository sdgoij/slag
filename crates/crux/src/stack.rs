//! The native-stack probe, re-exported from [`stackprobe`].
//!
//! It lives in its own crate because `regexp` needs the same answer and must not
//! depend on `crux` for it; see that crate's docs for why a nesting count cannot
//! bound the stack.

pub use stackprobe::has_headroom;
pub use stackprobe::install_probe;
pub use stackprobe::probe_installed;
pub use stackprobe::stack_bounds;
