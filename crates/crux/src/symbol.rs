//! Symbol values (spec 6.1.6.3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::handle::Handle;
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

/// The Well-Known Symbols (spec 6.1.6.3.5): built-in Symbol values that are
/// explicitly referenced by algorithms (e.g. %Symbol.unscopables% in the
/// Object Environment Record HasBinding method).
///
/// The spec stores these in each Realm's intrinsics table; Phase 4 uses
/// process-wide singletons instead, which is unobservable until the Symbol
/// built-in lands in Phase 8.
pub const WELL_KNOWN_SYMBOLS: &[&str] = &[
    "asyncDispose",
    "asyncIterator",
    "dispose",
    "hasInstance",
    "isConcatSpreadable",
    "iterator",
    "match",
    "matchAll",
    "replace",
    "search",
    "species",
    "split",
    "toPrimitive",
    "toStringTag",
    "unscopables",
];

/// Returns the canonical well-known symbol for `name` (the short name, e.g.
/// "unscopables"), creating it on first use. The table is thread-local: each
/// agent gets its own set (well-known symbols are per-realm), and a thread-
/// local table keeps `JsString` (whose rope children are `Rc`) free of a
/// `Send` requirement.
pub fn well_known(name: &str) -> Handle<Symbol> {
    thread_local! {
        static TABLE: std::cell::RefCell<HashMap<String, Handle<Symbol>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    TABLE.with(|table| {
        table
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| {
                Handle::new(Symbol::new(Some(JsString::from_utf8(&format!(
                    "Symbol.{name}"
                )))))
            })
            .clone()
    })
}

/// %Symbol.unscopables%: consulted by `with`-statement environment records
/// (spec 9.2.3.1).
pub fn unscopables() -> Handle<Symbol> {
    well_known("unscopables")
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

    #[test]
    fn well_known_symbols_are_stable_and_distinct() {
        let mut ids = std::collections::HashSet::new();
        for name in WELL_KNOWN_SYMBOLS {
            let sym = well_known(name);
            assert_eq!(
                sym.description.as_ref().unwrap().to_string_lossy(),
                format!("Symbol.{name}")
            );
            assert!(ids.insert(sym.id), "duplicate id for {name}");
        }
        // Repeated lookups return the same symbol value.
        assert_eq!(well_known("unscopables"), well_known("unscopables"));
        assert_ne!(well_known("unscopables"), well_known("iterator"));
    }
}
