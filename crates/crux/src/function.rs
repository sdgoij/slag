//! Function objects (spec 10.2).
//!
//! Phase 4 carries identity and name only so that function declarations can
//! be instantiated and bound; [[Call]]/[[Construct]], [[Environment]],
//! [[ECMAScriptCode]], and the remaining slots join with Phase 7.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::string::JsString;

static NEXT_FUNCTION_ID: AtomicU64 = AtomicU64::new(1);

/// A function object. Equality is identity (like `Symbol` and `JsObject`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    id: u64,
    pub name: Option<JsString>,
}

impl Function {
    pub fn new(name: Option<JsString>) -> Self {
        Self {
            id: NEXT_FUNCTION_ID.fetch_add(1, Ordering::Relaxed),
            name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_are_identity_equal() {
        let a = Function::new(Some(JsString::from_utf8("f")));
        let b = Function::new(Some(JsString::from_utf8("f")));
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn carries_the_binding_name() {
        let f = Function::new(Some(JsString::from_utf8("fib")));
        assert_eq!(f.name.unwrap().to_string_lossy(), "fib");
        assert!(Function::new(None).name.is_none());
    }
}
