//! Core language values, errors, and source spans shared across the workspace.
//!
//! Mirrors spec ch. 6 (ECMAScript Data Types and Values) and hosts the shared
//! abstract operations (ch. 7) that every other crate depends on.

pub mod bigint;
pub mod convert;
pub mod error;
pub mod function;
pub mod handle;
pub mod heap;
pub mod host;
pub mod map;
pub mod number;
pub mod object;
pub mod ops;
pub mod property;
pub mod proxy;
pub mod span;
pub mod string;
pub mod symbol;
pub mod typed_array;
pub mod value;

pub use bigint::BigInt;
pub use error::{ErrorKind, JsError};
pub use function::Function;
pub use handle::Handle;
pub use map::{Map, MapAttrs, canonical_empty_map};
pub use object::{JsObject, Property};
pub use property::{PropertyDescriptor, PropertyKey};
pub use span::{SourceLocation, Span};
pub use string::{AtomId, JsString, intern, intern_utf8, lookup, proto_atom};
pub use symbol::{Symbol, descriptive_string};
pub use value::{PAYLOAD_MASK, TAG_MASK, TAG_OBJECT, TAG_PREFIX, TAG_STRING, Value, type_of};
