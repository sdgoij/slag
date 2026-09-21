//! The object side of the embedding API: the two barriers an own-key
//! enumeration has to cross before it reads the keys.
//!
//! Both are the engine's own helpers, not wrappers. They are here for the same
//! reason the buffer entries are: the V8-shaped bridge (`crates/v8`) enumerates
//! keys as `v8::Object::GetOwnPropertyNames` promises, and doing that by
//! reaching into `runtime` would be the bridge keeping its own copy of rules
//! the engine already applies in its own built-ins.

/// Materialize a function's pending `prototype`, which every own-key
/// enumeration has to see.
pub use runtime::function::materialize_pending_prototype_value;

/// Evaluate a deferred module namespace, whose `[[OwnPropertyKeys]]` triggers
/// its module.
pub use runtime::module::ensure_deferred_namespace_evaluation;
