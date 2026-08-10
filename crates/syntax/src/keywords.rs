//! Keywords and reserved words (spec 12.6.2, 12.6.11).

use std::collections::HashMap;
use std::sync::OnceLock;

use crux::{AtomId, intern_utf8};

/// The keywords of the ECMAScript grammar (spec 12.6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Break,
    Case,
    Catch,
    Class,
    Const,
    Continue,
    Debugger,
    Default,
    Delete,
    Do,
    Else,
    Enum,
    Export,
    Extends,
    False,
    Finally,
    For,
    Function,
    If,
    Import,
    In,
    Instanceof,
    New,
    Null,
    Return,
    Super,
    Switch,
    This,
    Throw,
    True,
    Try,
    Typeof,
    Var,
    Void,
    While,
    With,
}

const KEYWORD_TEXTS: &[(&str, Keyword)] = &[
    ("break", Keyword::Break),
    ("case", Keyword::Case),
    ("catch", Keyword::Catch),
    ("class", Keyword::Class),
    ("const", Keyword::Const),
    ("continue", Keyword::Continue),
    ("debugger", Keyword::Debugger),
    ("default", Keyword::Default),
    ("delete", Keyword::Delete),
    ("do", Keyword::Do),
    ("else", Keyword::Else),
    ("enum", Keyword::Enum),
    ("export", Keyword::Export),
    ("extends", Keyword::Extends),
    ("false", Keyword::False),
    ("finally", Keyword::Finally),
    ("for", Keyword::For),
    ("function", Keyword::Function),
    ("if", Keyword::If),
    ("import", Keyword::Import),
    ("in", Keyword::In),
    ("instanceof", Keyword::Instanceof),
    ("new", Keyword::New),
    ("null", Keyword::Null),
    ("return", Keyword::Return),
    ("super", Keyword::Super),
    ("switch", Keyword::Switch),
    ("this", Keyword::This),
    ("throw", Keyword::Throw),
    ("true", Keyword::True),
    ("try", Keyword::Try),
    ("typeof", Keyword::Typeof),
    ("var", Keyword::Var),
    ("void", Keyword::Void),
    ("while", Keyword::While),
    ("with", Keyword::With),
];

fn keyword_table() -> &'static HashMap<AtomId, Keyword> {
    static TABLE: OnceLock<HashMap<AtomId, Keyword>> = OnceLock::new();
    TABLE.get_or_init(|| {
        KEYWORD_TEXTS
            .iter()
            .map(|(text, kw)| (intern_utf8(text), *kw))
            .collect()
    })
}

/// The keyword corresponding to an identifier, if any.
pub fn from_identifier(atom: AtomId) -> Option<Keyword> {
    keyword_table().get(&atom).copied()
}

/// FutureReservedWord (spec 12.6.11): reserved in strict mode.
pub const FUTURE_RESERVED_WORDS: &[&str] = &[
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
];

fn future_reserved_table() -> &'static std::collections::HashSet<AtomId> {
    static TABLE: OnceLock<std::collections::HashSet<AtomId>> = OnceLock::new();
    TABLE.get_or_init(|| {
        FUTURE_RESERVED_WORDS
            .iter()
            .map(|text| intern_utf8(text))
            .collect()
    })
}

/// Whether `atom` is a FutureReservedWord (strict-mode reserved).
pub fn is_future_reserved_word(atom: AtomId) -> bool {
    future_reserved_table().contains(&atom)
}

/// Whether `atom` is a keyword or a FutureReservedWord.
pub fn is_reserved_word(atom: AtomId) -> bool {
    from_identifier(atom).is_some() || is_future_reserved_word(atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(text: &str) -> AtomId {
        intern_utf8(text)
    }

    #[test]
    fn classifies_keywords() {
        assert_eq!(from_identifier(kw("function")), Some(Keyword::Function));
        assert_eq!(from_identifier(kw("null")), Some(Keyword::Null));
        assert_eq!(from_identifier(kw("if")), Some(Keyword::If));
        assert_eq!(from_identifier(kw("foo")), None);
        assert_eq!(from_identifier(kw("await")), None);
    }

    #[test]
    fn reserved_word_classification() {
        assert!(is_reserved_word(kw("function")));
        assert!(is_reserved_word(kw("enum")));
        assert!(is_future_reserved_word(kw("yield")));
        assert!(is_future_reserved_word(kw("let")));
        assert!(is_reserved_word(kw("let")));
        assert!(!is_reserved_word(kw("async")));
        assert!(!is_reserved_word(kw("of")));
        assert!(!is_reserved_word(kw("get")));
    }

    #[test]
    fn tables_are_stable_across_calls() {
        assert_eq!(from_identifier(kw("typeof")), Some(Keyword::Typeof));
        assert_eq!(from_identifier(kw("typeof")), Some(Keyword::Typeof));
        assert!(is_future_reserved_word(kw("interface")));
    }
}
