//! The WebAssembly core engine.
//!
//! Implemented cut-by-cut against the pinned `waspec` submodule (the
//! official WebAssembly specification repository) — binary format (ch. 5),
//! validation (ch. 3), and execution (ch. 4). V8's implementation is the
//! secondary reference. The cut plan and status live in
//! `.notes/wasm-plan.md`.
//!
//! Cut 1: the full module/type/instruction decoders. Cut 2 validates the
//! decoded module; Cut 3+ executes it.

pub mod binary;
pub mod exec;
pub mod instr;
pub mod module;
pub mod simd;
pub mod types;
pub mod valid;
pub mod values;

pub use binary::{Error as DecodeError, decode};
pub use exec::{ExecFail, ExternVal, FuncKey, Instance, InstantiateError, Memory, Store};
pub use module::Module;
pub use valid::validate;
pub use values::{Trap, Value};

/// The decoder's error type, re-exported under its phase's name for clarity
/// next to `valid::Error`.
pub use valid::Error as ValidError;
