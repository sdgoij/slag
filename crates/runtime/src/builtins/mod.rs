//! Standard built-in objects (spec ch. 18-26), installed during
//! SetDefaultGlobalBindings. Each submodule defines its intrinsics and the
//! global bindings that point at them; agent-dependent built-ins dispatch by
//! intrinsic identity from `runtime::function::call`/`construct` (the %eval%
//! pattern), because the crux-level native closures cannot reach the agent.

pub mod function;
pub mod object;
pub mod promise;
