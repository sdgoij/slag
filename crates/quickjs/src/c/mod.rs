//! The C/FFI drop-in (`quickjs.h` symbols).
//!
//! The shared `JSValue` representation and boxes are the first piece (Phase 2);
//! the `#[no_mangle]` exports sit on top. C hosts reach these symbols, so the
//! `dead_code` lint is turned off for this module: it is not dead, it is
//! consumed from the other side of the ABI.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]
#![allow(dead_code)]

mod abi;
mod alloc;
mod atom;
#[cfg(feature = "bellard")]
mod bellard;
mod buffer;
mod class;
mod context;
mod cstring;
mod error;
mod exception;
mod function;
mod introspect;
mod job;
mod loader;
mod module;
mod object;
mod promise;
mod runtime;
mod typed_array;
mod value;
