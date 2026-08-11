//! Realms, environment records, execution contexts, job queue, evaluator,
//! module linking, and all standard built-ins (spec ch. 9-10, 13-26).
//!
//! Phase 4 adds the execution model: the agent (ch. 9.7), realms and
//! intrinsics (ch. 9.3), environment records with every ch. 9 abstract
//! method (ch. 9.2), execution contexts and reference resolution (ch. 9.4),
//! and the job queue machinery (ch. 9.5), plus a minimal evaluator that
//! satisfies Phase 4's exit criteria. Phases 5-17 add the object model,
//! full evaluation, modules, and built-ins.

pub mod agent;
pub mod context;
pub mod env;
pub mod eval;
pub mod expr;
pub mod flow;
pub mod function;
pub mod host;
pub mod job;
pub mod realm;
pub mod script;

pub use agent::{Agent, evaluate};
pub use context::{ExecutionContext, Reference, ReferenceBase};
pub use env::{EnvRecord, EnvRef};
pub use host::HostHooks;
pub use realm::{Intrinsics, Realm};
pub use script::ScriptRecord;
