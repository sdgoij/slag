//! The buffer side of the embedding API: handing bytes to JavaScript, taking
//! them back, and building views over them.
//!
//! These are the engine's own entry points rather than wrappers, because the
//! shapes are the engine's — an `Agent` in hand, object ids, language `Value`s.
//! They are here so that a host has one place to depend on, and so that the
//! V8-shaped bridge (`crates/v8`) reaches the engine through this crate rather
//! than through `runtime`'s built-ins.
//!
//! A host that embeds Slag directly should stay with [`Context`](crate::Context)
//! and reach for these only for buffer work the facade does not cover yet.

pub use runtime::builtins::array_buffer::{
    array_buffer_from_block, detach_array_buffer, is_detached, is_shared,
    shared_array_buffer_from_block,
};
pub use runtime::builtins::typed_array::{
    typed_array_buffer_path as typed_array_from_buffer, view_out_of_bounds,
};
