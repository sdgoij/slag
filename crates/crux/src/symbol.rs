//! Symbol values (spec 6.1.6.3).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::string::JsString;

static NEXT_SYMBOL_ID: AtomicU64 = AtomicU64::new(1);

/// A unique Symbol value. Two Symbols are the same only when they carry the
/// same `id`, even with identical descriptions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub id: u64,
    pub description: Option<JsString>,
}

impl Symbol {
    pub fn new(description: Option<JsString>) -> Self {
        Self {
            id: NEXT_SYMBOL_ID.fetch_add(1, Ordering::Relaxed),
            description,
        }
    }
}

/// spec 6.1.6.3.7 SymbolDescriptiveString.
pub fn descriptive_string(symbol: &Symbol) -> String {
    match &symbol.description {
        Some(d) => format!("Symbol({d})"),
        None => "Symbol()".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_with_same_description_are_distinct() {
        let a = Symbol::new(Some(JsString::from_utf8("x")));
        let b = Symbol::new(Some(JsString::from_utf8("x")));
        assert_ne!(a, b);
    }

    #[test]
    fn cloned_symbol_is_identical() {
        let a = Symbol::new(None);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn descriptive_string_formats_description() {
        let with_desc = Symbol::new(Some(JsString::from_utf8("iterator")));
        assert_eq!(descriptive_string(&with_desc), "Symbol(iterator)");
        let no_desc = Symbol::new(None);
        assert_eq!(descriptive_string(&no_desc), "Symbol()");
    }
}
