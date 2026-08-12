//! ECMAScript RegExp pattern parser and backtracking matcher (spec ch. 22.2,
//! Annex B). `compile` parses a pattern per its `u`/`v` flags and produces a
//! `Regex`; `Regex::exec` runs the backtracking engine over UTF-16 input,
//! returning capture spans in code units. The runtime RegExp built-in owns
//! the `lastIndex` protocol, `RegExpExec`, and the `@@match`/`@@replace`/...
//! machinery.
//!
//! The engine is a recursive backtracker over a compiled AST with an
//! undo-log (trail) for captures, so choices are tried in spec order
//! (leftmost-first, greedy-quantifiers, first-success lookarounds).

pub mod engine;
pub mod escape;
pub mod parse;

use std::collections::HashMap;

/// The `d g i m s u v y` flag set (spec 22.2.5 RegExpInitialize).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    pub d: bool,
    pub g: bool,
    pub i: bool,
    pub m: bool,
    pub s: bool,
    pub u: bool,
    pub v: bool,
    pub y: bool,
}

impl Flags {
    pub fn none() -> Self {
        Flags {
            d: false,
            g: false,
            i: false,
            m: false,
            s: false,
            u: false,
            v: false,
            y: false,
        }
    }

    /// Parse the flag string: unknown or duplicated flags are SyntaxErrors
    /// (spec 22.2.5 steps 5-6).
    pub fn parse(units: &[u16]) -> Result<Flags, Error> {
        let mut flags = Flags::none();
        for &u in units {
            let bit = match u {
                0x64 => &mut flags.d,
                0x67 => &mut flags.g,
                0x69 => &mut flags.i,
                0x6D => &mut flags.m,
                0x73 => &mut flags.s,
                0x75 => &mut flags.u,
                0x76 => &mut flags.v,
                0x79 => &mut flags.y,
                _ => {
                    return Err(Error::syntax("Invalid regular expression flags"));
                }
            };
            if *bit {
                return Err(Error::syntax("Duplicate regular expression flag"));
            }
            *bit = true;
        }
        if flags.u && flags.v {
            return Err(Error::syntax(
                "The 'u' and 'v' regular expression flags cannot be used together",
            ));
        }
        Ok(flags)
    }

    pub fn has_unicode(&self) -> bool {
        self.u || self.v
    }
}

impl std::fmt::Display for Flags {
    /// The canonical `"dgimsuvy"` order for `RegExp.prototype.flags`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.d {
            f.write_str("d")?;
        }
        if self.g {
            f.write_str("g")?;
        }
        if self.i {
            f.write_str("i")?;
        }
        if self.m {
            f.write_str("m")?;
        }
        if self.s {
            f.write_str("s")?;
        }
        if self.u {
            f.write_str("u")?;
        }
        if self.v {
            f.write_str("v")?;
        }
        if self.y {
            f.write_str("y")?;
        }
        Ok(())
    }
}

/// A regular expression parse error (mapped to SyntaxError by the runtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
}

impl Error {
    pub fn syntax(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A character-set predicate for `\d \w \s` and `\p{…}` classes that are not
/// case-folded and not in `/v` set arithmetic (matched per character at match
/// time instead of enumerated into ranges).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    Digits,
    Word,
    Space,
    GeneralCategory(&'static str),
    Script(&'static str),
    ScriptExtensions(&'static str),
    Binary(&'static str),
}

/// A compiled character class: explicit inclusive ranges (code points) plus
/// optional string atoms (`/v` `\q{…}`), or a predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharClass {
    pub ranges: Vec<(u32, u32)>,
    pub strings: Vec<Vec<u32>>,
    pub negated: bool,
    pub predicate: Option<Predicate>,
    /// The input character is canonicalized before the membership test
    /// (ignore-case was in effect when the class was parsed).
    pub fold: bool,
}

impl CharClass {
    pub fn new(negated: bool) -> Self {
        CharClass {
            ranges: Vec::new(),
            strings: Vec::new(),
            negated,
            predicate: None,
            fold: false,
        }
    }

    pub fn from_predicate(predicate: Predicate, negated: bool) -> Self {
        CharClass {
            ranges: Vec::new(),
            strings: Vec::new(),
            negated,
            predicate: Some(predicate),
            fold: false,
        }
    }

    pub fn singleton(cp: u32) -> Self {
        CharClass {
            ranges: vec![(cp, cp)],
            strings: Vec::new(),
            negated: false,
            predicate: None,
            fold: false,
        }
    }

    /// Add a range (inclusive), coalescing with neighbours.
    pub fn add_range(&mut self, start: u32, end: u32) {
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
        for (s, e) in self.ranges.drain(..) {
            match merged.last_mut() {
                Some((_, last_end)) if s <= *last_end + 1 => {
                    *last_end = (*last_end).max(e);
                }
                _ => merged.push((s, e)),
            }
        }
        self.ranges = merged;
    }

    /// Negate in place against the full code point space.
    pub fn negate(&mut self) {
        let mut out = Vec::new();
        let mut next = 0u32;
        for (s, e) in &self.ranges {
            if next < *s {
                out.push((next, s - 1));
            }
            next = e.saturating_add(1);
        }
        if next <= 0x10FFFF {
            out.push((next, 0x10FFFF));
        }
        self.ranges = out;
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty() && self.strings.is_empty() && self.predicate.is_none()
    }
}

/// A compiled pattern node. Character positions are code-unit indices.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Empty,
    /// A literal character. In ignore-case scope the pattern character is
    /// pre-canonicalized and `fold` canonicalizes the input at match time.
    Char {
        cp: u32,
        fold: bool,
    },
    /// `.` — `dot_all` is the `s` flag in effect at this dot.
    Any {
        dot_all: bool,
    },
    Start {
        multiline: bool,
    },
    End {
        multiline: bool,
    },
    /// `\b` / `\B` — `extra_folded` adds the unicode+i word extras.
    WordBoundary {
        extra_folded: bool,
    },
    NotWordBoundary {
        extra_folded: bool,
    },
    Class(CharClass),
    Sequence(Vec<Node>),
    Alternate(Vec<Vec<Node>>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        /// Capture indices inside the repeated subexpression; each iteration
        /// starts with them cleared (spec RepeatMatcher copies and clears the
        /// atom's captures before matching).
        owned_captures: Vec<usize>,
    },
    Capture {
        index: usize,
        node: Box<Node>,
    },
    /// `\1` / `\k<name>` — every 1-based capture index the backref can bind
    /// to (a duplicate name contributes several; the last that participated
    /// wins). `fold` canonicalizes the comparison units.
    Backref {
        indices: Vec<usize>,
        fold: bool,
    },
    Lookahead {
        negate: bool,
        node: Box<Node>,
    },
    Lookbehind {
        negate: bool,
        node: Box<Node>,
    },
}

/// A parsed and compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    pub program: Node,
    pub flags: Flags,
    /// Number of capturing groups (excluding group 0).
    pub capturing_groups: usize,
    /// Named groups: name (code points) → every 1-based capture index using
    /// it, in source order (duplicate names are valid per ES2025).
    pub named_groups: HashMap<Vec<u32>, Vec<usize>>,
    /// The first-occurrence order of group names (drives the `groups` object
    /// key order).
    pub named_group_order: Vec<Vec<u32>>,
    /// Whether any GroupName appears (drives the `groups` object).
    pub has_group_names: bool,
}

/// The result of a successful `exec`: capture spans in code units.
/// `captures[0]` is the whole match; `None` groups did not participate.
pub type Match = Vec<Option<(usize, usize)>>;

/// Compile a pattern (UTF-16 code units) with the given flags.
pub fn compile(pattern: &[u16], flags: Flags) -> Result<Regex, Error> {
    parse::compile_pattern(pattern, flags)
}

/// Compile with a flag string (used by the runtime constructor).
pub fn compile_with_flags(pattern: &[u16], flags: &[u16]) -> Result<Regex, Error> {
    let flags = Flags::parse(flags)?;
    compile(pattern, flags)
}

impl Regex {
    /// Run the matcher at a specific code-unit index, returning the capture
    /// spans or `None` (spec RegExpBuiltinExec's matcher invocation). The
    /// runtime drives the lastIndex search loop with this.
    pub fn exec_at(&self, input: &[u16], start: usize) -> Option<Match> {
        engine::exec(self, input, start)
    }

    /// Leftmost search: try `start`, advancing by one character on failure
    /// until a match is found or the input is exhausted.
    pub fn exec(&self, input: &[u16], start: usize) -> Option<Match> {
        let mut index = start;
        while index <= input.len() {
            if let Some(m) = self.exec_at(input, index) {
                return Some(m);
            }
            if index >= input.len() {
                break;
            }
            index = self.advance_string_index(input, index);
        }
        None
    }

    /// spec 22.2.2.5 AdvanceStringIndex for empty-match handling.
    pub fn advance_string_index(&self, input: &[u16], index: usize) -> usize {
        if index >= input.len() {
            return index + 1;
        }
        if self.flags.has_unicode() {
            let (_, _, count) = crux_code_point_at(input, index);
            index + count
        } else {
            index + 1
        }
    }
}

/// CodePointAt for the regexp crate (avoids a crux dependency).
pub(crate) fn crux_code_point_at(input: &[u16], index: usize) -> (u32, bool, usize) {
    let Some(&hi) = input.get(index) else {
        return (0, false, 0);
    };
    if (0xD800..=0xDBFF).contains(&hi) {
        if let Some(&lo) = input.get(index + 1)
            && (0xDC00..=0xDFFF).contains(&lo)
        {
            let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
            return (cp, false, 2);
        }
        (hi as u32, true, 1)
    } else if (0xDC00..=0xDFFF).contains(&hi) {
        (hi as u32, true, 1)
    } else {
        (hi as u32, false, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(flags: &str) -> Flags {
        Flags::parse(&flags.encode_utf16().collect::<Vec<u16>>()).unwrap()
    }

    #[test]
    fn flags_parse_and_reject() {
        let flags = f("gim");
        assert!(flags.g && flags.i && flags.m && !flags.u);
        assert_eq!(flags.to_string(), "gim");
        assert!(Flags::parse(&"gg".encode_utf16().collect::<Vec<u16>>()).is_err());
        assert!(Flags::parse(&"x".encode_utf16().collect::<Vec<u16>>()).is_err());
        assert!(Flags::parse(&"uv".encode_utf16().collect::<Vec<u16>>()).is_err());
        assert_eq!(f("dgy").to_string(), "dgy");
    }

    #[test]
    fn basic_matching() {
        let re = compile("a+b".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).unwrap();
        let m = re
            .exec("xxaab".encode_utf16().collect::<Vec<u16>>().as_slice(), 0)
            .unwrap();
        assert_eq!(m[0], Some((2, 5)));
    }

    #[test]
    fn capture_spans() {
        let re = compile(
            "(a)(b)?c".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input: Vec<u16> = "xabc".encode_utf16().collect();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1, 4)));
        assert_eq!(m[1], Some((1, 2)));
        assert_eq!(m[2], Some((2, 3)));
        assert_eq!(
            re.exec(&"xac".encode_utf16().collect::<Vec<u16>>(), 0)
                .unwrap()[2],
            None
        );
    }

    #[test]
    fn class_predicates() {
        let re = compile(
            "\\d+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"ab123".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((2, 5)));
        assert!(compile("\\d".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_ok());
    }

    #[test]
    fn quantifiers_greedy_and_lazy() {
        let re = compile(
            "a.*b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aXbYb".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 5)));
        let re = compile(
            "a.*?b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aXbYb".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 3)));
    }

    #[test]
    fn anchors_and_boundaries() {
        let re = compile(
            "^b\\b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert_eq!(
            re.exec(&"b c".encode_utf16().collect::<Vec<u16>>(), 0)
                .unwrap()[0],
            Some((0, 1))
        );
        assert!(
            re.exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        let re = compile("c$".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).unwrap();
        assert!(
            re.exec(&"abc".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"abc ".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
    }

    #[test]
    fn alternation_prefers_first() {
        let re = compile(
            "a|ab".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 1)));
    }

    #[test]
    fn backreferences() {
        let re = compile(
            "(a|b)\\1".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"aa".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        // A backreference to a group that did not participate matches the
        // empty string, so `(a)?b\1` matches "b" with group 1 unset.
        let re = compile(
            "(a)?b\\1".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"b".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 1)));
        assert_eq!(m[1], None);
    }

    #[test]
    fn lookahead_and_lookbehind() {
        let re = compile(
            "a(?=b)".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"ac".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        // Lookahead captures persist (spec example: ["aba", "a"]).
        let re = compile(
            "(?=(a+))a*b\\1"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"baaabac".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((3, 6)));
        assert_eq!(m[1], Some((3, 4)));
        let re = compile(
            "(?<=a)b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"cb".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
    }

    #[test]
    fn case_insensitive() {
        let re = compile(
            "abc".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("i"),
        )
        .unwrap();
        assert!(
            re.exec(&"aBc".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        let re = compile(
            "[a-z]".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("i"),
        )
        .unwrap();
        assert!(
            re.exec(&"Z".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
    }

    #[test]
    fn unicode_mode() {
        let re = compile(
            "\\u{1F600}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("u"),
        )
        .unwrap();
        assert!(re.exec(&[0xD83D, 0xDE00], 0).is_some());
        // `.` in unicode mode consumes the whole code point.
        let re = compile(".".encode_utf16().collect::<Vec<u16>>().as_slice(), f("u")).unwrap();
        let m = re.exec_at(&[b'a' as u16, 0xD83D, 0xDE00], 1).unwrap();
        assert_eq!(m[0], Some((1, 3)));
    }

    #[test]
    fn property_escapes() {
        let re = compile(
            "\\p{Letter}+"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f("u"),
        )
        .unwrap();
        assert!(
            re.exec(&"abc".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"123".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        let re = compile(
            "\\P{Number}"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f("u"),
        )
        .unwrap();
        assert!(
            re.exec(&"a".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
        assert!(
            re.exec(&"5".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
    }

    #[test]
    fn named_groups_and_escapes() {
        let re = compile(
            "(?<word>\\w+)"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"hey".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[1], Some((0, 3)));
        // Duplicate names are valid only across alternatives (ES2025); `\k<a>`
        // refers to the group that participated (the last one in source order).
        let re = compile(
            "(?:(?<a>x)|(?<a>y))\\k<a>"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"xx".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[1], Some((0, 1)));
        assert_eq!(m[2], None);
        let m = re
            .exec(&"yy".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[1], None);
        assert_eq!(m[2], Some((0, 1)));
        let re = compile(
            "\\x41".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"A".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
    }

    #[test]
    fn zero_width_repeats_terminate() {
        // Atoms that can match the empty string must not recurse forever when
        // quantified (e.g. `(?:)*` on any input used to overflow the stack).
        let input: Vec<u16> = "aaa".encode_utf16().collect();
        for pattern in [
            "(?:)*", "(?:)+", "(?:)*?", "(?:)+?", "(?=a)*", "(?=a)*b", "(?:)*?b",
        ] {
            let re = compile(
                pattern.encode_utf16().collect::<Vec<u16>>().as_slice(),
                f(""),
            )
            .unwrap();
            let _ = re.exec(&input, 0);
        }
        // `(?:)*` matches empty; an optional empty iteration is discarded, so
        // the quantified group's captures are unset (spec RepeatMatcher
        // step 2.b, matching V8's ["", null, null]).
        let re = compile(
            "(())*".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((0, 0)));
        assert_eq!(m[1], None);
        // The empty iteration still counts toward `min`.
        let re = compile(
            "(?:){2}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((0, 0)));
        // A quantified lookahead backtracks to fewer iterations instead of
        // looping: at 0 the lookahead holds but `b` cannot match `a`, so the
        // search moves on and matches `b` at index 1.
        let re = compile(
            "(?=a)*b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((1, 2)));
    }

    #[test]
    fn parse_errors() {
        for bad in ["(", ")", "[", "\\", "(?<n>", "a{2,1}", "(?=a"] {
            assert!(
                compile(bad.encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_err(),
                "expected error for {bad:?}"
            );
        }
    }
}
