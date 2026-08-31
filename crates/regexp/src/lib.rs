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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Whether the program contains no backreferences (computed once at
    /// compile time): gates the repeat failure memo, see `engine`.
    pub(crate) backref_free: bool,
    /// The peeled atoms of repeats eligible for the exhausted memo (computed
    /// once at compile time): no enclosing repeat has min >= 2.
    pub(crate) memo_safe: std::collections::HashSet<usize>,
    /// Leading-unit prefilter for the search loop, computed at compile time:
    /// the UTF-16 units any match must start with (None = no skip, e.g. the
    /// pattern can match empty). See `engine::search_prefilter`.
    pub(crate) prefilter: Option<Vec<(u32, u32)>>,
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

    /// Leftmost search that also reports the index the match started at (the
    /// runtime's RegExpBuiltinExec loop needs it for lastIndex). Applies a
    /// leading-char prefilter so positions where no match can start are
    /// skipped without a full matcher attempt.
    pub fn search_at(&self, input: &[u16], start: usize) -> Option<(usize, Match)> {
        engine::search(self, input, start)
    }

    /// Leftmost search: try `start`, advancing by one character on failure
    /// until a match is found or the input is exhausted.
    pub fn exec(&self, input: &[u16], start: usize) -> Option<Match> {
        self.search_at(input, start).map(|(_, m)| m)
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
    fn greedy_single_char_repeat_backtracks_iteratively() {
        // The iterative fast path must keep greedy semantics: consume as
        // much as possible, then backtrack one character at a time.
        let re = compile(
            "[ab]+b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 2)));
        let re = compile(
            "a{2,3}a".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaa".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 3)));
    }

    #[test]
    fn greedy_single_char_repeat_large_input_does_not_overflow() {
        // A multi-megabyte `X+` match used to recurse once per character and
        // overflow the stack (the generated property-escape fixtures build
        // ~2M-unit strings); the fast path consumes iteratively.
        let re = compile(
            "[a]+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input = vec![b'a' as u16; 1_000_000];
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((0, 1_000_000)));
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
    fn unicode_surrogate_pairs_in_classes() {
        // `[\uD834\uDF06]` is one class member (a code point) in unicode
        // mode: it matches the pair, not either lone surrogate (spec 22.2.1).
        let re = compile(
            "[\\uD834\\uDF06]"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f("u"),
        )
        .unwrap();
        assert!(re.exec(&[0xD834, 0xDF06], 0).is_some());
        assert!(re.exec(&[0xD834], 0).is_none());
        assert!(re.exec(&[0xDF06], 0).is_none());
        let re = compile(
            "[\\uD800\\uDC00]"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f("u"),
        )
        .unwrap();
        assert!(re.exec(&[0xD800, 0xDC00], 0).is_some());
        assert!(re.exec(&[0xD800], 0).is_none());
        assert!(re.exec(&[0xDC00], 0).is_none());
        // Outside unicode mode each escape stays a code unit.
        let re = compile(
            "[\\uD834\\uDF06]"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        assert!(re.exec(&[0xD834], 0).is_some());
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
        // Lookbehind assertions can never be quantified (spec 22.2.1); in
        // non-unicode mode lookaheads may be (Annex B).
        for bad in ["(?<=.)?", "(?<!.)?", "(?<=.){2,3}", "(?<=.)*", "(?<=.)+"] {
            assert!(
                compile(bad.encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_err(),
                "expected error for {bad:?}"
            );
        }
        for good in ["(?=.)?", "(?=.){2,3}"] {
            assert!(
                compile(good.encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_ok(),
                "expected {good:?} to parse"
            );
        }
        // A `\k` not followed by a name is an error when the pattern has
        // named groups, and always in unicode mode; a lone `\k` stays the
        // identity escape otherwise (Annex B).
        for bad in ["(?<a>.)\\k", "\\k(?<a>.)"] {
            assert!(
                compile(bad.encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_err(),
                "expected error for {bad:?}"
            );
        }
        assert!(
            compile(
                "\\k".encode_utf16().collect::<Vec<u16>>().as_slice(),
                f("u")
            )
            .is_err()
        );
        assert!(compile("\\k".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).is_ok());
        assert!(
            compile(
                "(?<a>.)\\k<a>"
                    .encode_utf16()
                    .collect::<Vec<u16>>()
                    .as_slice(),
                f("")
            )
            .is_ok()
        );
    }

    /// /v mode: class-set ranges (`[0-9]`), `\q{…}` string disjunctions, and
    /// their interactions (spec 22.2.2 ClassSetExpression).
    #[test]
    fn v_flag_class_sets() {
        let v = |_pattern: &str| f("v");
        let m = |pattern: &str, input: &str| {
            let re = compile(
                pattern.encode_utf16().collect::<Vec<u16>>().as_slice(),
                v(pattern),
            )
            .unwrap();
            re.exec(&input.encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        };
        // ClassSetRange.
        assert!(m("^[0-9]+$", "123"));
        assert!(!m("^[0-9]+$", "1a3"));
        assert!(m("^[a-z]+$", "abc"));
        // Nested class operands and difference: the empty set matches nothing.
        assert!(!m("^[[0-9]--[0-9]]+$", "5"));
        // `\\q{…}` string disjunction: a multi-char string matches whole.
        assert!(m("^[\\q{9\\uFE0F\\u20E3}]+$", "9\u{FE0F}\u{20E3}"));
        assert!(!m("^[\\q{9\\uFE0F\\u20E3}]+$", "9"));
        // Union of a string disjunction and a char.
        assert!(m("^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$", "9\u{FE0F}\u{20E3}"));
        assert!(m("^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$", "0"));
        assert!(!m("^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$", "6\u{FE0F}\u{20E3}"));
        // The greedy repeat backtracks across the string alternatives: "ab"
        // is one member even when "a" is tried first.
        assert!(m("^[\\q{a|ab}]+$", "ab"));
        assert!(m("^[\\q{ab|a}]+$", "ab"));
        // Single-char strings survive an intersection with a char class; a
        // multi-char string does not (spec ClassSetExpression semantics).
        assert!(m("^[\\d&&\\q{0|2|4|9\\uFE0F\\u20E3}]+$", "2"));
        assert!(!m(
            "^[\\d&&\\q{0|2|4|9\\uFE0F\\u20E3}]+$",
            "9\u{FE0F}\u{20E3}"
        ));
        assert!(!m("^[\\d&&\\q{0|2|4|9\\uFE0F\\u20E3}]+$", "x"));
        assert!(!m(
            "^[\\d&&\\q{0|2|4|9\\uFE0F\\u20E3}]+$",
            "9\\uFE0F\\u20E3"
        ));
        assert!(!m("^[\\d&&\\q{0|2|4|9\\uFE0F\\u20E3}]+$", "x"));
        // A range over a class escape is an early error (V8 behavior).
        assert!(
            compile(
                "[\\d-a]".encode_utf16().collect::<Vec<u16>>().as_slice(),
                v("v")
            )
            .is_err()
        );
    }

    #[test]
    fn capture_repeat_fast_path_backtracks() {
        // A capture-wrapped single atom takes the iterative fast path; the
        // reported span must be the last iteration's, and backtracking must
        // shrink it correctly (spec RepeatMatcher).
        let re = compile(
            "(a)+b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 4)));
        assert_eq!(m[1], Some((2, 3)));
        // Zero iterations leave the capture undefined.
        let re = compile(
            "(a)*b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"b".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[1], None);
        // A nested pair of captures both track the last iteration.
        let re = compile(
            "((a))+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaa".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[1], Some((2, 3)));
        assert_eq!(m[2], Some((2, 3)));
    }

    #[test]
    fn capture_repeat_fast_path_does_not_overflow() {
        // The capture-wrapped single atom must consume iteratively like the
        // capture-free case (a multi-megabyte `(a)+` used to recurse once per
        // character and overflow the stack).
        let re = compile(
            "(a)+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input = vec![b'a' as u16; 1_000_000];
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[1], Some((999_999, 1_000_000)));
    }

    #[test]
    fn search_prefilter_skips_and_hits() {
        // The search loop's leading-char prefilter must never skip a real
        // match, and must find matches after a run of skipped positions.
        let re = compile("a*b".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).unwrap();
        let m = re
            .exec(&"xxb".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((2, 3)));
        // `a?b`: first char may be `a` or `b` (the union prefilter).
        let re = compile("a?b".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).unwrap();
        let m = re
            .exec(&"xb".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((1, 2)));
        // Alternation union prefilter.
        let re = compile(
            "(foo|bar|baz)"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"xxbar".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((2, 5)));
        // Anchored: the prefilter must not conflict with `^`.
        let re = compile(
            "^abc".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"xxabc".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        // A pattern that can match empty disables the prefilter entirely;
        // the greedy `x*` still consumes the `x` before `$` succeeds.
        let re = compile("x*$".encode_utf16().collect::<Vec<u16>>().as_slice(), f("")).unwrap();
        let m = re
            .exec(&"yx".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((1, 2)));
    }

    #[test]
    fn search_prefilter_unicode_non_bmp() {
        // A non-BMP first character must not be skipped by the unit-set
        // mapping (high surrogate).
        let re = compile(
            "[\\u{10000}]b"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f("u"),
        )
        .unwrap();
        let input: Vec<u16> = "x\u{10000}b".encode_utf16().collect();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1, 4)));
    }

    #[test]
    fn predicate_prefilter_search() {
        // `\d` and `\p{…}` classes now contribute their leading-char set to
        // the prefilter, so a search skips positions that cannot start a
        // match (behavioral check; the speedup is in the perf harness).
        let re = compile(
            "\\d{4}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 100_000];
        input.extend("1234".encode_utf16());
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((100_000, 100_004)));
        // A full-space predicate (`\p{Any}`) must not produce a prefilter
        // that skips everything.
        let re = compile(
            "\\p{Any}+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("u"),
        )
        .unwrap();
        let m = re
            .exec(&"ab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 2)));
    }

    #[test]
    fn ignore_case_prefilter_never_skips_a_fold_class_member() {
        // A folded literal's leading-char set is its full fold class, so the
        // search must not skip a member the forward closure would miss:
        // `/k/iu` matches U+212A KELVIN SIGN (which folds to `k`).
        let re = compile("k".encode_utf16().collect::<Vec<u16>>().as_slice(), f("iu")).unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 100_000];
        input.push(0x212A);
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((100_000, 100_001)));
        // The classic fold pairs: `/ß/iu` matches ẞ and vice versa.
        let re = compile(
            "\u{DF}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("iu"),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 1000];
        input.push(0x1E9E);
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1000, 1001)));
        // Legacy mode folds through uppercase: `/é/i` matches É.
        let re = compile(
            "\u{E9}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("i"),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 1000];
        input.push(0xC9);
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1000, 1001)));
        // A non-BMP fold maps through the high surrogate.
        let re = compile(
            "\u{10428}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("iu"),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 1000];
        input.extend("\u{10400}".encode_utf16());
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1000, 1002)));
    }

    #[test]
    fn ignore_case_prefilter_skips_to_folded_match() {
        // The folded literal now contributes its leading-char set, so a
        // search skips positions that cannot start the match (behavioral
        // check; the speedup is in the perf harness).
        let re = compile(
            "abcdefghij".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("i"),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'x' as u16; 100_000];
        input.extend("ABCDEFGHIJ".encode_utf16());
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((100_000, 100_010)));
        // A folded class contributes too: `/[a-z]+/i` after a run of
        // non-letters.
        let re = compile(
            "[a-z]+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("i"),
        )
        .unwrap();
        let mut input: Vec<u16> = vec![b'0' as u16; 100_000];
        input.extend("Abc".encode_utf16());
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((100_000, 100_003)));
    }

    #[test]
    fn linear_sequence_repeat_fast_path() {
        // `(ab)+` is a linear sequence of single atoms: it takes the
        // iterative fast path, and the capture reports the last iteration's
        // span.
        let re = compile(
            "(ab)+c".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"abababc".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 7)));
        assert_eq!(m[1], Some((4, 6)));
        // Backtracking through the linear sequence still finds the match.
        let re = compile(
            "(ab)+b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"ababb".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 5)));
        assert_eq!(m[1], Some((2, 4)));
        // Non-capturing linear sequence.
        let re = compile(
            "(?:ab)+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"abab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 4)));
    }

    #[test]
    fn linear_sequence_repeat_does_not_overflow() {
        // A multi-megabyte `(ab)+` must consume iteratively like `(a)+`.
        let re = compile(
            "(ab)+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input: Vec<u16> = "ab".repeat(500_000).encode_utf16().collect();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[1], Some((input.len() - 2, input.len())));
    }

    #[test]
    fn branching_atom_repeat_does_not_overflow() {
        // The explicit-stack engine iterates even branching repeats
        // (`(a|b)+`, `(a?b)+` are not simple atoms and take the general
        // path); a multi-megabyte input must not recurse per iteration.
        let re = compile(
            "(a|b)+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input: Vec<u16> = "ab".repeat(500_000).encode_utf16().collect();
        let m = re.exec(&input, 0).unwrap();
        // The last iteration matches the final `b`.
        assert_eq!(m[1], Some((input.len() - 1, input.len())));
        // A nested optional repeat is not simple either.
        let re = compile(
            "(a?b)+".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(re.exec(&input, 0).is_some());
    }

    #[test]
    fn nested_repeat_failure_is_memoized() {
        // `(a+)+b` and `(?:a*)*b` are the classic exponential-backtracking
        // patterns: every partition of the a-run into groups is explored.
        // The failure memo turns them polynomial (the probe took ~12s at
        // n=25; with the memo it is microseconds).
        for pattern in ["(a+)+b", "(?:a*)*b"] {
            let re = compile(
                pattern.encode_utf16().collect::<Vec<u16>>().as_slice(),
                f(""),
            )
            .unwrap();
            let input: Vec<u16> = "a".repeat(25).encode_utf16().collect();
            let start = std::time::Instant::now();
            assert!(re.exec(&input, 0).is_none());
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
        }
    }

    #[test]
    fn nested_repeat_memo_respects_soundness_gates() {
        // The exhausted memo must stay off when an enclosing repeat has
        // min >= 2: there the continuation depends on the enclosing repeat's
        // iteration count, so a per-(atom, position) entry would hide the
        // match here (g1=g2=g3="a", then b).
        let re = compile(
            "(a+){3,}b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 4)));
        // A backreference anywhere also disables the exhausted memo (the
        // continuation reads captures); the match must still be found.
        let re = compile(
            "((a+)+b)\\1"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaabaaab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 8)));
    }

    #[test]
    fn nested_repeat_stop_choice_still_found() {
        // `(a+)+a` on "aaa": the inner `a+` exhausts its shrink levels at
        // some positions, but the match stops the enclosing repeat early;
        // the recorded memo entries must not hide it.
        let re = compile(
            "(a+)+a".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaa".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 3)));
    }

    #[test]
    fn failure_memo_survives_search_loop_position_attempts() {
        // The memo persists across `match_at` calls; positions that exhausted
        // the repeat must not poison a later position where it matches.
        let re = compile(
            "(a+)+b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let input: Vec<u16> = "aaaaaaaaaXaaab".encode_utf16().collect();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((10, 14)));
    }

    #[test]
    fn search_prefilter_never_visits_surrogate_halves() {
        // The prefilter skip must advance code-point-aware under `/u` (the
        // spec's AdvanceStringIndex): position 1 of a surrogate pair is
        // never visited, so the lone low-surrogate literal must not match.
        let re = compile(
            "\\udf06".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f("u"),
        )
        .unwrap();
        let input: Vec<u16> = vec![0xD834, 0xDF06];
        assert!(re.exec(&input, 0).is_none());
        // Without `/u`, every unit is a position: the low surrogate matches
        // at index 1.
        let re = compile(
            "\\udf06".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re.exec(&input, 0).unwrap();
        assert_eq!(m[0], Some((1, 2)));
    }

    #[test]
    fn lookbehind_capture_repeat_spans() {
        // A capture repeat inside a lookbehind runs right-to-left; the span
        // must still be recorded (start, end) with start < end.
        let re = compile(
            "(?<=(\\w){3})def"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"abcdef".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((3, 6)));
        assert_eq!(m[1], Some((0, 1)));
    }

    #[test]
    fn coalesced_repeats_match_like_originals() {
        // Adjacent greedy capture-free repeats of the same atom collapse:
        // `a*a*a*a*b` ≡ `a*b` (the pathological case becomes linear).
        let re = compile(
            "a*a*a*a*b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaaaab".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 6)));
        assert!(
            re.exec(&"aaaaac".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        // `a{2}a{3}` ≡ `a{5}`.
        let re = compile(
            "a{2}a{3}".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        let m = re
            .exec(&"aaaaa".encode_utf16().collect::<Vec<u16>>(), 0)
            .unwrap();
        assert_eq!(m[0], Some((0, 5)));
        assert!(
            re.exec(&"aaaa".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_none()
        );
        // Lazy repeats keep their own semantics.
        let re = compile(
            "a*?a*?b".encode_utf16().collect::<Vec<u16>>().as_slice(),
            f(""),
        )
        .unwrap();
        assert!(
            re.exec(&"aab".encode_utf16().collect::<Vec<u16>>(), 0)
                .is_some()
        );
    }
}
