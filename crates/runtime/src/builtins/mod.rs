//! Standard built-in objects (spec ch. 18-26), installed during
//! SetDefaultGlobalBindings. Each submodule defines its intrinsics and the
//! global bindings that point at them; agent-dependent built-ins dispatch by
//! intrinsic identity from `runtime::function::call`/`construct` (the %eval%
//! pattern), because the crux-level native closures cannot reach the agent.

pub mod array;
pub mod bigint;
pub mod boolean;
pub mod date;
pub mod error;
pub mod function;
pub mod global;
pub mod math;
pub mod number;
pub mod object;
pub mod promise;
pub mod regexp;
pub mod string;
pub mod symbol;
pub mod typed_array;
pub mod weakref;
