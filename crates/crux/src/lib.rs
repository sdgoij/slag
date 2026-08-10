//! Core language values, errors, and source spans shared across the workspace.
//!
//! Mirrors spec ch. 6 (ECMAScript Data Types and Values) and hosts the shared
//! abstract operations (ch. 7) that every other crate depends on.

pub mod bigint;
pub mod convert;
pub mod error;
pub mod function;
pub mod handle;
pub mod number;
pub mod object;
pub mod ops;
pub mod property;
pub mod span;
pub mod string;
pub mod symbol;
pub mod value;

pub use bigint::BigInt;
pub use error::{ErrorKind, JsError};
pub use function::Function;
pub use handle::Handle;
pub use object::{JsObject, Property};
pub use property::{PropertyDescriptor, PropertyKey};
pub use span::{SourceLocation, Span};
pub use string::{AtomId, JsString, intern, intern_utf8, lookup};
pub use symbol::{Symbol, descriptive_string};
pub use value::{Value, type_of};
