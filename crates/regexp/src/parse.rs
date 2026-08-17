//! The pattern parser (spec 22.2.1 grammar, Annex B amendments): a recursive
//! descent over the pattern's code points (unicode mode) or code units
//! (legacy mode), parameterized by the `u`/`v` flags and inline modifiers.

use std::collections::{HashMap, HashSet};

use crate::{CharClass, Error, Flags, Node, Predicate, Regex};

const MAX_DEPTH: usize = 200;

/// A "character" of the pattern: a code point in unicode mode, otherwise a
/// code unit.
type Atom = u32;

struct Parser<'a> {
    atoms: &'a [Atom],
    pos: usize,
    flags: Flags,
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    capturing_groups: usize,
    /// Total capturing groups in the whole pattern, pre-scanned so decimal
    /// escapes can resolve forward references (spec DecimalEscape uses the
    /// full-pattern group count).
    total_groups: usize,
    /// Every name → capture indices over the whole pattern, pre-scanned so
    /// `\k<name>` resolves forward references.
    total_named_groups: HashMap<Vec<Atom>, Vec<usize>>,
    /// Named groups: name (code points) → the 1-based capture indices that
    /// use it, in source order (duplicate names are valid per ES2025).
    named_groups: HashMap<Vec<Atom>, Vec<usize>>,
    /// Open alternatives' used group names, bottom (outermost) first. A name
    /// may only repeat across alternatives of the same Disjunction (ES2025
    /// early error); a disjunction's alternatives merge their union into the
    /// enclosing alternative when it completes.
    alt_names: Vec<HashSet<Vec<Atom>>>,
    /// The first-occurrence order of group names (drives the `groups` object
    /// key order).
    named_group_order: Vec<Vec<Atom>>,
    has_group_names: bool,
    depth: usize,
}

pub(crate) fn compile_pattern(pattern: &[u16], flags: Flags) -> Result<Regex, Error> {
    let atoms: Vec<Atom> = if flags.has_unicode() {
        let mut atoms = Vec::with_capacity(pattern.len());
        let mut i = 0;
        while i < pattern.len() {
            let (cp, _, count) = crate::crux_code_point_at(pattern, i);
            atoms.push(cp);
            i += count;
        }
        atoms
    } else {
        pattern.iter().map(|&u| u as Atom).collect()
    };
    let (total_groups, total_named_groups) = scan_pattern(&atoms);
    let has_group_names = !total_named_groups.is_empty();
    let mut parser = Parser {
        atoms: &atoms,
        pos: 0,
        flags,
        ignore_case: flags.i,
        multiline: flags.m,
        dot_all: flags.s,
        capturing_groups: 0,
        total_groups,
        total_named_groups,
        named_groups: HashMap::new(),
        named_group_order: Vec::new(),
        alt_names: Vec::new(),
        has_group_names,
        depth: 0,
    };
    let program = parser.parse_disjunction()?;
    if parser.pos != parser.atoms.len() {
        return Err(Error::syntax("Unmatched parentheses in regular expression"));
    }
    Ok(Regex {
        program,
        flags,
        capturing_groups: parser.capturing_groups,
        named_groups: parser.named_groups,
        named_group_order: parser.named_group_order,
        has_group_names: parser.has_group_names,
    })
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Atom> {
        self.atoms.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<Atom> {
        self.atoms.get(self.pos + offset).copied()
    }

    fn next(&mut self) -> Option<Atom> {
        let atom = self.atoms.get(self.pos).copied()?;
        self.pos += 1;
        Some(atom)
    }

    fn eat(&mut self, atom: Atom) -> bool {
        if self.peek() == Some(atom) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn error(&self, message: &str) -> Error {
        Error::syntax(message)
    }

    fn unicode(&self) -> bool {
        self.flags.has_unicode()
    }

    fn push_depth(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("Regular expression too deeply nested"));
        }
        Ok(())
    }

    fn pop_depth(&mut self) {
        self.depth -= 1;
    }

    /// Pattern :: Disjunction. Each alternative tracks its own group names;
    /// the union merges into the enclosing alternative when the disjunction
    /// completes (spec: duplicate names are only allowed across alternatives
    /// of the same Disjunction).
    fn parse_disjunction(&mut self) -> Result<Node, Error> {
        let mut alternatives = Vec::new();
        let mut all_names: HashSet<Vec<Atom>> = HashSet::new();
        loop {
            self.alt_names.push(HashSet::new());
            let alt = self.parse_alternative();
            let alt_names = self.alt_names.pop().unwrap_or_default();
            all_names.extend(alt_names);
            alternatives.push(alt?);
            if !self.eat(0x7C) {
                break;
            }
        }
        if let Some(parent) = self.alt_names.last_mut() {
            parent.extend(all_names);
        }
        if alternatives.len() == 1 {
            Ok(alternatives.pop().map(Node::Sequence).unwrap())
        } else {
            Ok(Node::Alternate(alternatives))
        }
    }

    /// A literal character node, pre-canonicalized under ignore-case.
    fn char_node(&self, cp: u32) -> Node {
        if self.ignore_case {
            Node::Char {
                cp: crate::engine::canonicalize(self.unicode(), cp),
                fold: true,
            }
        } else {
            Node::Char { cp, fold: false }
        }
    }

    /// Alternative :: [empty] Term*
    fn parse_alternative(&mut self) -> Result<Vec<Node>, Error> {
        let mut nodes = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some(0x29) => break,
                Some(0x7C) => break,
                _ => nodes.push(self.parse_term()?),
            }
        }
        Ok(nodes)
    }

    /// Term :: Assertion | Atom Quantifier?
    fn parse_term(&mut self) -> Result<Node, Error> {
        let node = match self.peek() {
            Some(0x5E) => {
                self.next();
                Node::Start {
                    multiline: self.multiline,
                }
            }
            Some(0x24) => {
                self.next();
                Node::End {
                    multiline: self.multiline,
                }
            }
            Some(0x5C) => self.parse_backslash()?,
            Some(0x5B) => self.parse_character_class(false)?,
            Some(0x2E) => {
                self.next();
                Node::Any {
                    dot_all: self.dot_all,
                }
            }
            Some(0x28) => self.parse_group()?,
            // A quantifier with no preceding atom is a SyntaxError (spec
            // Term :: Atom Quantifier?; `a**`, `*a`, `x{1}{1,}` all fail).
            // A `{` is only a quantifier start when it matches the
            // DecimalDigits form; otherwise it is a PatternCharacter.
            Some(atom)
                if matches!(atom, 0x2A | 0x2B | 0x3F)
                    || (atom == 0x7B
                        && self.peek_at(1).is_some_and(|a| (0x30..=0x39).contains(&a))) =>
            {
                return Err(self.error("Nothing to repeat"));
            }
            Some(atom) => {
                self.next();
                if self.unicode() && matches!(atom, 0x7B | 0x7D | 0x5D) {
                    // Annex B adds `{`/`}`/`]` as legacy PatternCharacters;
                    // in unicode mode they are not atoms.
                    return Err(self.error("Invalid pattern character"));
                }
                self.char_node(atom)
            }
            None => return Err(self.error("Unexpected end of pattern")),
        };
        let quantifier = self.parse_quantifier()?;
        if quantifier.is_some()
            && (matches!(node, Node::Lookbehind { .. })
                || (self.unicode()
                    && matches!(
                        node,
                        Node::Start { .. }
                            | Node::End { .. }
                            | Node::WordBoundary { .. }
                            | Node::NotWordBoundary { .. }
                            | Node::Lookahead { .. }
                            | Node::Lookbehind { .. }
                    )))
        {
            // Annex B's ExtendedTerm :: Assertion Quantifier does not apply
            // in unicode mode (spec 22.2.1), and never applies to a
            // lookbehind assertion in any mode.
            return Err(self.error("Assertion cannot be quantified"));
        }
        match quantifier {
            None => Ok(node),
            Some((min, max, greedy)) => {
                let owned_captures = collect_captures(&node);
                Ok(Node::Repeat {
                    node: Box::new(node),
                    min,
                    max,
                    greedy,
                    owned_captures,
                })
            }
        }
    }

    /// Quantifier :: QuantifierPrefix ? | ? | * | + | { DecimalDigits } | ...
    /// Returns `None` when there is no quantifier (a lone `{` is a literal in
    /// non-unicode mode, an error in unicode mode).
    fn parse_quantifier(&mut self) -> Result<Option<(u32, Option<u32>, bool)>, Error> {
        let (min, max) = match self.peek() {
            Some(0x2A) => {
                self.next();
                (0, None)
            }
            Some(0x2B) => {
                self.next();
                (1, None)
            }
            Some(0x3F) => {
                self.next();
                (0, Some(1))
            }
            Some(0x7B) => {
                // `{` is only a quantifier when it matches the DecimalDigits
                // grammar; otherwise it is a PatternCharacter (legacy mode)
                // or a SyntaxError (unicode mode).
                let Some(q) = self.try_parse_braced_quantifier()? else {
                    if self.unicode() {
                        return Err(self.error("Invalid quantifier"));
                    }
                    return Ok(None);
                };
                q
            }
            _ => return Ok(None),
        };
        let greedy = !self.eat(0x3F);
        Ok(Some((min, max, greedy)))
    }

    fn try_parse_braced_quantifier(&mut self) -> Result<Option<(u32, Option<u32>)>, Error> {
        // The cursor is at `{`. Valid forms: {n}, {n,}, {n,m}. A `{` not
        // followed by a decimal digit is not a quantifier (a literal in
        // legacy mode, an error in unicode mode).
        let start = self.pos;
        self.next(); // {
        if !self.peek().is_some_and(|a| (0x30..=0x39).contains(&a)) {
            self.pos = start;
            return Ok(None);
        }
        let min = self.parse_decimal_digits()?;
        match self.peek() {
            Some(0x7D) => {
                self.next();
                Ok(Some((min, Some(min))))
            }
            Some(0x2C) => {
                self.next();
                match self.peek() {
                    Some(0x7D) => {
                        self.next();
                        Ok(Some((min, None)))
                    }
                    Some(digit) if (0x30..=0x39).contains(&digit) => {
                        let max = self.parse_decimal_digits()?;
                        if max < min {
                            return Err(self.error("numbers out of order in {} quantifier"));
                        }
                        if !self.eat(0x7D) {
                            return Err(self.error("Unterminated quantifier"));
                        }
                        Ok(Some((min, Some(max))))
                    }
                    _ => {
                        self.pos = start;
                        Ok(None)
                    }
                }
            }
            _ => {
                self.pos = start;
                Ok(None)
            }
        }
    }

    /// Parse a run of decimal digits already started by `first` (a digit).
    /// The spec caps the MV at 2^53 - 1; values beyond are a SyntaxError, and
    /// anything above u32::MAX is clamped (no real match could ever need it).
    fn parse_decimal_digits(&mut self) -> Result<u32, Error> {
        let mut value = 0u64;
        let mut any = false;
        while let Some(d) = self.peek()
            && (0x30..=0x39).contains(&d)
        {
            self.next();
            any = true;
            value = value * 10 + (d - 0x30) as u64;
            if value > 0x1F_FFFF_FFFF_FFFF {
                return Err(self.error("Quantifier number too large"));
            }
        }
        if !any {
            return Err(self.error("Expected decimal digits"));
        }
        Ok(value.min(u32::MAX as u64) as u32)
    }

    /// Atom :: ... | `\` AtomEscape | CharacterClass | `(` GroupSpecifier? ...
    fn parse_backslash(&mut self) -> Result<Node, Error> {
        // Cursor at `\`.
        self.next();
        let Some(atom) = self.next() else {
            return Err(self.error("\\ at end of pattern"));
        };
        match atom {
            0x64 | 0x44 => {
                let negated = atom == 0x44;
                Ok(Node::Class(self.escape_class(Predicate::Digits, negated)))
            }
            0x73 | 0x53 => {
                let negated = atom == 0x53;
                Ok(Node::Class(self.escape_class(Predicate::Space, negated)))
            }
            0x77 | 0x57 => {
                let negated = atom == 0x57;
                Ok(Node::Class(self.escape_class(Predicate::Word, negated)))
            }
            0x70 | 0x50 => {
                if self.unicode() {
                    if !self.eat(0x7B) {
                        return Err(self.error("Invalid regular expression: missing { after \\p"));
                    }
                    let negated = atom == 0x50;
                    let class = self.parse_property_escape(negated)?;
                    Ok(Node::Class(class))
                } else {
                    // `\p`/`\P` are identity escapes in legacy mode.
                    Ok(self.char_node(atom))
                }
            }
            0x62 => Ok(Node::WordBoundary {
                extra_folded: self.unicode() && self.ignore_case,
            }),
            0x42 => Ok(Node::NotWordBoundary {
                extra_folded: self.unicode() && self.ignore_case,
            }),
            _ => self.parse_atom_escape(atom),
        }
    }

    /// `\d \w \s` and `\p{…}` class escapes. Predicates stay as predicates
    /// unless the class needs folding (ignore-case) or `/v` set arithmetic,
    /// which force enumeration into explicit ranges. Under ignore-case the
    /// fold applies to the final set, so a negated class folds its complement.
    fn predicate_class(&mut self, predicate: Predicate, negated: bool) -> CharClass {
        if self.ignore_case {
            let mut ranges = crate::engine::scan_predicate(&predicate, false, self.unicode());
            if negated {
                crate::engine::complement(&mut ranges);
            }
            CharClass {
                ranges: crate::engine::fold_ranges(&ranges, self.unicode()),
                strings: Vec::new(),
                negated: false,
                predicate: None,
                fold: true,
            }
        } else if self.flags.v {
            let ranges = crate::engine::scan_predicate(&predicate, false, self.unicode());
            CharClass {
                ranges,
                strings: Vec::new(),
                negated,
                predicate: None,
                fold: false,
            }
        } else {
            let mut class = CharClass::from_predicate(predicate, negated);
            class.fold = false;
            class
        }
    }

    fn escape_class(&mut self, predicate: Predicate, negated: bool) -> CharClass {
        if self.ignore_case {
            // GetWordCharacters & co. apply Canonicalize to the input char, so
            // the base set is folded first and a negated escape is the
            // complement of the folded set (unlike `\p{}`, whose fold happens
            // at the class level, after the complement).
            let mut ranges = crate::engine::scan_predicate(&predicate, true, self.unicode());
            if negated {
                crate::engine::complement(&mut ranges);
            }
            CharClass {
                ranges,
                strings: Vec::new(),
                negated: false,
                predicate: None,
                fold: true,
            }
        } else {
            self.predicate_class(predicate, negated)
        }
    }

    /// `\p{…}` / `\P{…}`: General_Category, Script, Script_Extensions, or a
    /// binary property (spec 22.2.3.13).
    fn parse_property_escape(&mut self, negated: bool) -> Result<CharClass, Error> {
        let mut name: Vec<u32> = Vec::new();
        while let Some(a) = self.peek()
            && a != 0x7D
        {
            if !is_letter_or_underscore_or_digit(a) {
                return Err(self.error("Invalid property escape"));
            }
            name.push(self.next().unwrap());
        }
        if !self.eat(0x7D) {
            return Err(self.error("Unterminated property escape"));
        }
        let (prop, value) = match name.iter().position(|&a| a == 0x3D) {
            Some(eq) => (name[..eq].to_vec(), Some(name[eq + 1..].to_vec())),
            None => (name, None),
        };
        let prop_text = code_points_to_string(&prop);
        let predicate = match prop_text.as_str() {
            "General_Category" | "gc" => {
                let Some(value) = value else {
                    return Err(self.error("Invalid property escape"));
                };
                let value_text = code_points_to_string(&value);
                let Some(abbr) = category_abbreviation(&value_text) else {
                    return Err(self.error("Invalid property escape"));
                };
                Predicate::GeneralCategory(abbr)
            }
            "Script" | "sc" => {
                let Some(value) = value else {
                    return Err(self.error("Invalid property escape"));
                };
                let value_text = code_points_to_string(&value);
                let Some(canonical) = canonical_script_name(&value_text) else {
                    return Err(self.error("Invalid property escape"));
                };
                Predicate::Script(canonical)
            }
            "Script_Extensions" | "scx" => {
                let Some(value) = value else {
                    return Err(self.error("Invalid property escape"));
                };
                let value_text = code_points_to_string(&value);
                let Some(canonical) = canonical_script_name(&value_text) else {
                    return Err(self.error("Invalid property escape"));
                };
                Predicate::ScriptExtensions(canonical)
            }
            _ => {
                if value.is_some() {
                    return Err(self.error("Invalid property escape"));
                }
                // Property-of-strings (`\\p{RGI_Emoji}` etc., spec
                // 22.2.3.13 + the /v UnicodeSets proposal): a set of strings,
                // only meaningful in /v mode. Match the string set directly
                // when the name is one.
                if let Some(strings) = unicode::property_of_strings(&prop_text) {
                    if !self.flags.v {
                        return Err(self.error("Invalid property escape"));
                    }
                    if negated {
                        return Err(self.error("Invalid property escape"));
                    }
                    let mut class = CharClass::new(false);
                    class.strings = strings.iter().map(|s| s.to_vec()).collect();
                    return Ok(class);
                }
                // Binary properties use canonical names; also accept the
                // general-category long names as aliases.
                match binary_property_name(&prop_text) {
                    Some(name) => Predicate::Binary(name),
                    None => match category_abbreviation(&prop_text) {
                        Some(abbr) => Predicate::GeneralCategory(abbr),
                        None => return Err(self.error("Invalid property escape")),
                    },
                }
            }
        };
        Ok(self.property_class(predicate, negated))
    }

    fn property_class(&mut self, predicate: Predicate, negated: bool) -> CharClass {
        self.predicate_class(predicate, negated)
    }

    /// AtomEscape after the backslash has been consumed.
    fn parse_atom_escape(&mut self, atom: Atom) -> Result<Node, Error> {
        match atom {
            0x30 => {
                if self.peek().is_some_and(|a| (0x30..=0x39).contains(&a)) {
                    if self.unicode() {
                        return Err(self.error("Invalid decimal escape"));
                    }
                    // Legacy octal escape: \0 followed by a digit.
                    let value = octal_escape(self, 0)?;
                    return Ok(self.char_node(value));
                }
                Ok(self.char_node(0))
            }
            0x31..=0x39 => {
                let group = (atom - 0x30) as usize;
                if group <= self.total_groups {
                    // A backreference (forward references are valid: the
                    // count is over the whole pattern).
                    let mut index = group;
                    // Annex B: \10 with 10+ groups refers to group 10; consume
                    // a second digit only when it forms a valid group number.
                    if let Some(d) = self.peek()
                        && (0x30..=0x39).contains(&d)
                        && (0x30..=0x39).contains(&atom)
                    {
                        let two = group * 10 + (d - 0x30) as usize;
                        if two <= self.total_groups {
                            self.next();
                            index = two;
                        }
                    }
                    Ok(Node::Backref {
                        indices: vec![index],
                        fold: self.ignore_case,
                    })
                } else if self.unicode() {
                    Err(self.error("Invalid decimal escape"))
                } else if matches!(atom, 0x38 | 0x39) {
                    // `\8`/`\9` have no octal reading and no matching
                    // group: identity escapes (B.1.4 identity-escape).
                    Ok(self.char_node(atom))
                } else {
                    // Legacy octal escape.
                    let value = octal_escape(self, atom - 0x30)?;
                    Ok(self.char_node(value))
                }
            }
            0x78 => {
                if let (Some(h), Some(l)) = (
                    self.peek().and_then(hex_value),
                    self.peek_at(1).and_then(hex_value),
                ) {
                    self.next();
                    self.next();
                    Ok(self.char_node((h << 4) | l))
                } else if self.unicode() {
                    Err(self.error("Invalid \\x escape"))
                } else {
                    // Annex B: `\x` without two hex digits is the identity
                    // escape (the following chars are ordinary atoms).
                    Ok(self.char_node(0x78))
                }
            }
            0x75 => {
                if self.unicode() && self.peek() == Some(0x7B) {
                    self.next();
                    return self.parse_unicode_code_point_escape();
                }
                if let (Some(h), Some(l), Some(h2), Some(l2)) = (
                    self.peek().and_then(hex_value),
                    self.peek_at(1).and_then(hex_value),
                    self.peek_at(2).and_then(hex_value),
                    self.peek_at(3).and_then(hex_value),
                ) {
                    self.next();
                    self.next();
                    self.next();
                    self.next();
                    let code_unit = (h << 12) | (l << 8) | (h2 << 4) | l2;
                    return self.char_from_code_unit(code_unit);
                }
                if self.unicode() {
                    Err(self.error("Invalid \\u escape"))
                } else {
                    // Annex B: `\u` without four hex digits is the identity
                    // escape (the following chars are ordinary atoms).
                    Ok(self.char_node(0x75))
                }
            }
            0x63 => {
                if let Some(letter) = self.peek()
                    && is_control_letter(letter)
                {
                    self.next();
                    Ok(self.char_node(letter % 32))
                } else if self.unicode() {
                    Err(self.error("Invalid control escape"))
                } else {
                    // Annex B: `\c` followed by a non-ControlLetter is not an
                    // escape at all — the backslash and the `c` are separate
                    // atoms. Rewind the cursor so the caller re-parses `c` as
                    // a PatternCharacter (a following quantifier applies only
                    // to it, e.g. `/\cа+/`).
                    self.pos -= 1;
                    Ok(self.char_node(b'\\' as Atom))
                }
            }
            0x6B => {
                // Annex B: outside unicode mode, `\k` is only a named
                // backreference when the pattern contains named groups;
                // otherwise it is the identity escape `k`. In either mode a
                // `\k` that is not followed by `<name>` is an error when the
                // pattern has (or will have) named groups.
                if self.peek() == Some(0x3C) && (self.unicode() || self.has_group_names) {
                    self.parse_named_backreference()
                } else if self.unicode() || self.has_group_names {
                    Err(self.error("Invalid named capture reference"))
                } else {
                    Ok(self.char_node(0x6B))
                }
            }
            // Control escapes (spec 22.2.1 CharacterEscape): `\t` TAB, `\n`
            // LF, `\v` VT, `\f` FF, `\r` CR.
            0x74 => Ok(self.char_node(0x09)),
            0x6E => Ok(self.char_node(0x0A)),
            0x76 => Ok(self.char_node(0x0B)),
            0x66 => Ok(self.char_node(0x0C)),
            0x72 => Ok(self.char_node(0x0D)),
            _ => {
                if self.unicode() && !is_identity_escape_allowed(atom) {
                    Err(self.error("Invalid escape"))
                } else {
                    Ok(self.char_node(atom))
                }
            }
        }
    }

    fn parse_unicode_code_point_escape(&mut self) -> Result<Node, Error> {
        let mut value = 0u32;
        let mut any = false;
        while let Some(h) = self.peek().and_then(hex_value) {
            self.next();
            any = true;
            value = value
                .checked_mul(16)
                .and_then(|v| v.checked_add(h))
                .ok_or_else(|| self.error("Invalid code point escape"))?;
        }
        if !any || !self.eat(0x7D) {
            return Err(self.error("Invalid code point escape"));
        }
        if value > 0x10FFFF {
            return Err(self.error("Invalid code point escape"));
        }
        Ok(self.char_node(value))
    }

    /// A `\uXXXX` escape decodes to a code unit; in unicode mode a leading
    /// surrogate must form a `\uD800\uDC00` pair, per the spec's escape
    /// semantics.
    fn char_from_code_unit(&mut self, code_unit: u32) -> Result<Node, Error> {
        if !self.unicode() {
            return Ok(self.char_node(code_unit));
        }
        if (0xD800..=0xDBFF).contains(&code_unit) {
            if let Some(cp) = self.combine_surrogate_pair(code_unit) {
                return Ok(self.char_node(cp));
            }
            return Err(self.error("Invalid unicode escape"));
        }
        Ok(self.char_node(code_unit))
    }

    /// If the next atoms are a `\u` escape of a trail surrogate, consume them
    /// and return the code point the `lead` surrogate pairs with (spec 22.2.1
    /// `u LeadSurrogate \u TrailSurrogate`).
    fn combine_surrogate_pair(&mut self, lead: u32) -> Option<u32> {
        if self.peek() != Some(0x5C) || self.peek_at(1) != Some(0x75) {
            return None;
        }
        let (Some(h), Some(l)) = (
            self.peek_at(2).and_then(hex_value),
            self.peek_at(3).and_then(hex_value),
        ) else {
            return None;
        };
        let (Some(h2), Some(l2)) = (
            self.peek_at(4).and_then(hex_value),
            self.peek_at(5).and_then(hex_value),
        ) else {
            return None;
        };
        let low = (h << 12) | (l << 8) | (h2 << 4) | l2;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        for _ in 0..6 {
            self.next();
        }
        Some(0x10000 + ((lead - 0xD800) << 10) + (low - 0xDC00))
    }

    /// `\k<name>` — a named backreference.
    fn parse_named_backreference(&mut self) -> Result<Node, Error> {
        self.next(); // <
        let name = self.parse_group_name()?;
        if !self.eat(0x3E) {
            return Err(self.error("Invalid named capture reference"));
        }
        match self
            .named_groups
            .get(&name)
            .or_else(|| self.total_named_groups.get(&name))
        {
            Some(indices) => Ok(Node::Backref {
                indices: indices.clone(),
                fold: self.ignore_case,
            }),
            None => Err(self.error("Invalid named capture reference")),
        }
    }

    /// GroupName :: `<` IdentifierName `>` — `\uXXXX` / `\u{…}` escapes
    /// decode into the name in both modes (spec RegExpIdentifierName).
    fn parse_group_name(&mut self) -> Result<Vec<Atom>, Error> {
        let mut name = Vec::new();
        while let Some(a) = self.peek() {
            if a == 0x3E {
                break;
            }
            if a == 0x5C {
                // Only `\u` escapes may appear in a RegExpIdentifierName.
                self.next();
                if self.next() != Some(0x75) {
                    return Err(self.error("Invalid capture group name"));
                }
                let value = if self.peek() == Some(0x7B) {
                    self.next();
                    let mut value = 0u32;
                    let mut any = false;
                    while let Some(h) = self.peek().and_then(hex_value) {
                        self.next();
                        any = true;
                        value = value
                            .checked_mul(16)
                            .and_then(|v| v.checked_add(h))
                            .ok_or_else(|| self.error("Invalid code point escape"))?;
                    }
                    if !any || !self.eat(0x7D) || value > 0x10FFFF {
                        return Err(self.error("Invalid code point escape"));
                    }
                    value
                } else if let (Some(h), Some(l)) = (
                    self.peek().and_then(hex_value),
                    self.peek_at(1).and_then(hex_value),
                ) {
                    self.next();
                    self.next();
                    let mut value = (h << 12) | (l << 8);
                    if let (Some(h2), Some(l2)) = (
                        self.peek().and_then(hex_value),
                        self.peek_at(1).and_then(hex_value),
                    ) {
                        self.next();
                        self.next();
                        value |= (h2 << 4) | l2;
                    }
                    value
                } else {
                    return Err(self.error("Invalid \\u escape"));
                };
                name.push(value);
            } else {
                name.push(self.next().unwrap());
            }
        }
        if name.is_empty() {
            return Err(self.error("Invalid capture group name"));
        }
        // RegExpIdentifierStart/Part (spec): ID_Start then ID_Continue, with
        // `\uD800\uDC00`-style surrogate pairs counted as one code point.
        let mut cps: Vec<u32> = Vec::with_capacity(name.len());
        let mut i = 0;
        while i < name.len() {
            let hi = name[i];
            if (0xD800..=0xDBFF).contains(&hi)
                && i + 1 < name.len()
                && (0xDC00..=0xDFFF).contains(&name[i + 1])
            {
                let lo = name[i + 1];
                cps.push(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                i += 2;
            } else {
                cps.push(hi);
                i += 1;
            }
        }
        if !unicode::is_identifier_start(cps[0])
            || cps[1..].iter().any(|&cp| !unicode::is_identifier_part(cp))
        {
            return Err(self.error("Invalid capture group name"));
        }
        Ok(name)
    }

    /// Atom :: `(` GroupSpecifier? Disjunction `)` | `(?:` ... | assertions
    fn parse_group(&mut self) -> Result<Node, Error> {
        self.next(); // (
        self.push_depth()?;
        let result = self.parse_group_inner();
        self.pop_depth();
        result
    }

    fn parse_group_inner(&mut self) -> Result<Node, Error> {
        if self.eat(0x3F) {
            match self.peek() {
                Some(0x3A) => {
                    self.next();
                    let body = self.parse_disjunction()?;
                    if !self.eat(0x29) {
                        return Err(self.error("Unterminated group"));
                    }
                    Ok(body)
                }
                Some(0x3D) => {
                    self.next();
                    let body = self.parse_disjunction()?;
                    if !self.eat(0x29) {
                        return Err(self.error("Unterminated group"));
                    }
                    Ok(Node::Lookahead {
                        negate: false,
                        node: Box::new(body),
                    })
                }
                Some(0x21) => {
                    self.next();
                    let body = self.parse_disjunction()?;
                    if !self.eat(0x29) {
                        return Err(self.error("Unterminated group"));
                    }
                    Ok(Node::Lookahead {
                        negate: true,
                        node: Box::new(body),
                    })
                }
                Some(0x3C) => {
                    self.next();
                    match self.peek() {
                        Some(0x3D) => {
                            self.next();
                            let body = self.parse_disjunction()?;
                            if !self.eat(0x29) {
                                return Err(self.error("Unterminated group"));
                            }
                            Ok(Node::Lookbehind {
                                negate: false,
                                node: Box::new(body),
                            })
                        }
                        Some(0x21) => {
                            self.next();
                            let body = self.parse_disjunction()?;
                            if !self.eat(0x29) {
                                return Err(self.error("Unterminated group"));
                            }
                            Ok(Node::Lookbehind {
                                negate: true,
                                node: Box::new(body),
                            })
                        }
                        _ => {
                            // Named capture group.
                            let name = self.parse_group_name()?;
                            if !self.eat(0x3E) {
                                return Err(self.error("Invalid capture group name"));
                            }
                            // Duplicate names are valid only across
                            // alternatives of the same Disjunction (ES2025); a
                            // name used elsewhere in the current alternative
                            // is an early error.
                            if self.alt_names.iter().any(|set| set.contains(&name)) {
                                return Err(self.error("Duplicate named capture group"));
                            }
                            if let Some(top) = self.alt_names.last_mut() {
                                top.insert(name.clone());
                            }
                            self.capturing_groups += 1;
                            let index = self.capturing_groups;
                            // Duplicate names are valid (ES2025): every index
                            // using the name is kept, first occurrence drives
                            // the `groups` key order.
                            let first = !self.named_groups.contains_key(&name);
                            self.named_groups
                                .entry(name.clone())
                                .or_default()
                                .push(index);
                            if first {
                                self.named_group_order.push(name.clone());
                            }
                            self.has_group_names = true;
                            let body = self.parse_disjunction()?;
                            if !self.eat(0x29) {
                                return Err(self.error("Unterminated group"));
                            }
                            Ok(Node::Capture {
                                index,
                                node: Box::new(body),
                            })
                        }
                    }
                }
                Some(a) if a == 0x69 || a == 0x6D || a == 0x73 || a == 0x2D => {
                    // Inline modifiers: (?ims-ims:…) or (?ims-ims), including
                    // a remove-only head (?-ims:…).
                    self.parse_inline_modifiers()
                }
                _ => Err(self.error("Invalid group")),
            }
        } else {
            self.capturing_groups += 1;
            let index = self.capturing_groups;
            let body = self.parse_disjunction()?;
            if !self.eat(0x29) {
                return Err(self.error("Unterminated group"));
            }
            Ok(Node::Capture {
                index,
                node: Box::new(body),
            })
        }
    }

    /// (?ims-ims: Disjunction ) or (?ims-ims) — ES2025 inline modifiers.
    fn parse_inline_modifiers(&mut self) -> Result<Node, Error> {
        let mut add = 0u8;
        let mut remove = 0u8;
        let mut saw_minus = false;
        loop {
            match self.peek() {
                Some(0x69) => {
                    let bit = 1;
                    let slot = if saw_minus { &mut remove } else { &mut add };
                    if *slot & bit != 0 {
                        return Err(self.error("Duplicate modifier"));
                    }
                    *slot |= bit;
                    self.next();
                }
                Some(0x6D) => {
                    let bit = 2;
                    let slot = if saw_minus { &mut remove } else { &mut add };
                    if *slot & bit != 0 {
                        return Err(self.error("Duplicate modifier"));
                    }
                    *slot |= bit;
                    self.next();
                }
                Some(0x73) => {
                    let bit = 4;
                    let slot = if saw_minus { &mut remove } else { &mut add };
                    if *slot & bit != 0 {
                        return Err(self.error("Duplicate modifier"));
                    }
                    *slot |= bit;
                    self.next();
                }
                Some(0x2D) if !saw_minus => {
                    saw_minus = true;
                    self.next();
                }
                Some(0x3A) => {
                    if add & remove != 0 {
                        return Err(self.error("Modifier added and removed"));
                    }
                    if saw_minus && add == 0 && remove == 0 {
                        return Err(self.error("Empty modifiers"));
                    }
                    self.next();
                    let saved = (self.ignore_case, self.multiline, self.dot_all);
                    self.apply_modifiers(add, remove)?;
                    let body = self.parse_disjunction()?;
                    if !self.eat(0x29) {
                        return Err(self.error("Unterminated group"));
                    }
                    self.ignore_case = saved.0;
                    self.multiline = saved.1;
                    self.dot_all = saved.2;
                    return Ok(body);
                }
                Some(0x29) => {
                    if add & remove != 0 {
                        return Err(self.error("Modifier added and removed"));
                    }
                    if saw_minus && add == 0 && remove == 0 {
                        return Err(self.error("Empty modifiers"));
                    }
                    self.next();
                    let saved = (self.ignore_case, self.multiline, self.dot_all);
                    self.apply_modifiers(add, remove)?;
                    // The modifiers apply to the rest of the enclosing
                    // group; parse the remainder and restore afterwards.
                    let body = self.parse_disjunction()?;
                    if !self.eat(0x29) {
                        return Err(self.error("Unterminated group"));
                    }
                    self.ignore_case = saved.0;
                    self.multiline = saved.1;
                    self.dot_all = saved.2;
                    return Ok(body);
                }
                _ => return Err(self.error("Invalid group modifier")),
            }
        }
    }

    fn apply_modifiers(&mut self, add: u8, remove: u8) -> Result<(), Error> {
        if add & 1 != 0 || remove & 1 != 0 {
            self.ignore_case = add & 1 != 0;
        }
        if add & 2 != 0 || remove & 2 != 0 {
            self.multiline = add & 2 != 0;
        }
        if add & 4 != 0 || remove & 4 != 0 {
            self.dot_all = add & 4 != 0;
        }
        Ok(())
    }

    /// CharacterClass :: `[` [lookahead != ^] ClassRanges? `]` | `[^` ClassRanges? `]`
    fn parse_character_class(&mut self, _in_set: bool) -> Result<Node, Error> {
        self.next(); // [
        let negated = self.eat(0x5E);
        if self.flags.v {
            let class = self.parse_class_set(negated)?;
            return Ok(Node::Class(class));
        }
        let mut members: Vec<ClassMember> = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(self.error("Unterminated character class")),
                Some(0x5D) if !first => {
                    self.next();
                    break;
                }
                // `[]` / `[]a`: the `]` closes an (empty) class. In unicode
                // mode the first `]` can never be a literal atom (the Annex B
                // extension does not apply).
                Some(0x5D) if self.unicode() || self.peek_at(1) != Some(0x5D) => {
                    self.next();
                    break;
                }
                Some(0x5D) => {
                    // `[]]`: the first `]` is a literal ClassAtom (Annex B)
                    // when the closing `]` follows.
                    self.next();
                    members.push(ClassMember::Char(0x5D));
                    first = false;
                    continue;
                }
                _ => {}
            }
            first = false;
            let atom = self.parse_class_atom()?;
            if self.peek() == Some(0x2D) && self.peek_at(1).is_some_and(|a| a != 0x5D) {
                self.next();
                let end = self.parse_class_atom()?;
                match (&atom, &end) {
                    (ClassAtom::Char(s), ClassAtom::Char(e)) => {
                        let (s, e) = (*s, *e);
                        if s > e {
                            return Err(self.error("Range out of order in character class"));
                        }
                        members.push(ClassMember::Range(s, e));
                    }
                    _ => {
                        if self.unicode() {
                            return Err(self.error("Invalid character class range"));
                        }
                        // Annex B: `[\\d-a]` treats `-` literally.
                        members.push(ClassMember::Char(0x2D));
                        members.push(ClassMember::from_class_atom(atom));
                        members.push(ClassMember::from_class_atom(end));
                    }
                }
            } else {
                members.push(ClassMember::from_class_atom(atom));
            }
        }
        let class = assemble_class(members, negated, self.ignore_case, self.unicode());
        Ok(Node::Class(class))
    }

    /// ClassAtom :: `-` | ClassAtomNoDash
    fn parse_class_atom(&mut self) -> Result<ClassAtom, Error> {
        match self.peek() {
            None => Err(self.error("Unterminated character class")),
            Some(0x5C) => {
                self.next();
                let Some(atom) = self.next() else {
                    return Err(self.error("\\ at end of character class"));
                };
                self.parse_class_escape(atom)
            }
            Some(atom) => {
                self.next();
                Ok(ClassAtom::Char(atom))
            }
        }
    }

    fn parse_class_escape(&mut self, atom: Atom) -> Result<ClassAtom, Error> {
        match atom {
            0x64 | 0x44 | 0x73 | 0x53 | 0x77 | 0x57 => {
                let predicate = match atom {
                    0x64 => Predicate::Digits,
                    0x44 => Predicate::Digits,
                    0x73 => Predicate::Space,
                    0x53 => Predicate::Space,
                    _ => Predicate::Word,
                };
                let negated = matches!(atom, 0x44 | 0x53 | 0x57);
                Ok(ClassAtom::Escape(self.escape_class(predicate, negated)))
            }
            0x70 | 0x50 if self.unicode() => {
                if !self.eat(0x7B) {
                    return Err(self.error("Invalid property escape"));
                }
                let negated = atom == 0x50;
                Ok(ClassAtom::Escape(self.parse_property_escape(negated)?))
            }
            0x62 => Ok(ClassAtom::Char(0x08)), // \b inside a class is BACKSPACE
            0x78 => {
                if let (Some(h), Some(l)) = (
                    self.peek().and_then(hex_value),
                    self.peek_at(1).and_then(hex_value),
                ) {
                    self.next();
                    self.next();
                    Ok(ClassAtom::Char((h << 4) | l))
                } else if self.unicode() {
                    Err(self.error("Invalid \\x escape"))
                } else {
                    // Annex B: `\x` without two hex digits is the identity
                    // escape inside a class too.
                    Ok(ClassAtom::Char(0x78))
                }
            }
            0x75 => {
                if self.unicode() && self.peek() == Some(0x7B) {
                    self.next();
                    let mut value = 0u32;
                    let mut any = false;
                    while let Some(h) = self.peek().and_then(hex_value) {
                        self.next();
                        any = true;
                        value = value
                            .checked_mul(16)
                            .and_then(|v| v.checked_add(h))
                            .ok_or_else(|| self.error("Invalid code point escape"))?;
                    }
                    if !any || !self.eat(0x7D) || value > 0x10FFFF {
                        return Err(self.error("Invalid code point escape"));
                    }
                    return Ok(ClassAtom::Char(value));
                }
                if let (Some(h), Some(l), Some(h2), Some(l2)) = (
                    self.peek().and_then(hex_value),
                    self.peek_at(1).and_then(hex_value),
                    self.peek_at(2).and_then(hex_value),
                    self.peek_at(3).and_then(hex_value),
                ) {
                    self.next();
                    self.next();
                    self.next();
                    self.next();
                    let code_unit = (h << 12) | (l << 8) | (h2 << 4) | l2;
                    // In unicode mode a `\uXXXX` lead surrogate combines with
                    // a following `\uXXXX` trail surrogate into one class
                    // member (spec 22.2.1); a lone surrogate stays a code
                    // unit, so it can never match half of a code-point pair.
                    if self.unicode()
                        && (0xD800..=0xDBFF).contains(&code_unit)
                        && let Some(cp) = self.combine_surrogate_pair(code_unit)
                    {
                        return Ok(ClassAtom::Char(cp));
                    }
                    Ok(ClassAtom::Char(code_unit))
                } else if self.unicode() {
                    Err(self.error("Invalid \\u escape"))
                } else {
                    Ok(ClassAtom::Char(0x75))
                }
            }
            0x63 => {
                // A control letter is valid in both modes; Annex B adds
                // ClassControlLetter `DecimalDigit`/`_` (non-unicode only);
                // anything else re-parses as the two atoms `\` and `c`.
                if let Some(letter) = self.peek()
                    && is_control_letter(letter)
                {
                    self.next();
                    Ok(ClassAtom::Char(letter % 32))
                } else if let Some(letter) = self.peek()
                    && !self.unicode()
                    && ((0x30..=0x39).contains(&letter) || letter == 0x5F)
                {
                    self.next();
                    Ok(ClassAtom::Char(letter % 32))
                } else if self.unicode() {
                    Err(self.error("Invalid control escape"))
                } else {
                    // Annex B: `[\cX]` with a non-ClassControlLetter X is the
                    // two atoms `\` and `c` (plus X); rewind so the caller
                    // re-parses `c` as its own ClassAtom.
                    self.pos -= 1;
                    Ok(ClassAtom::Char(b'\\' as Atom))
                }
            }
            // Control escapes: `\t` TAB, `\n` LF, `\v` VT, `\f` FF, `\r` CR.
            0x74 => Ok(ClassAtom::Char(0x09)),
            0x6E => Ok(ClassAtom::Char(0x0A)),
            0x76 => Ok(ClassAtom::Char(0x0B)),
            0x66 => Ok(ClassAtom::Char(0x0C)),
            0x72 => Ok(ClassAtom::Char(0x0D)),
            // Annex B decimal escapes: classes have no backreferences, so a
            // digit escape is the legacy octal character (B.1.4 ClassAtomNoDash
            // :: \ DecimalEscape). `\8`/`\9` have no octal reading and stay
            // identity escapes.
            0x30 => {
                if self.peek().is_some_and(|a| (0x30..=0x39).contains(&a)) {
                    if self.unicode() {
                        return Err(self.error("Invalid decimal escape"));
                    }
                    return Ok(ClassAtom::Char(octal_escape(self, 0)?));
                }
                Ok(ClassAtom::Char(0))
            }
            0x31..=0x39 => {
                if self.unicode() {
                    return Err(self.error("Invalid decimal escape"));
                }
                if matches!(atom, 0x38 | 0x39) {
                    return Ok(ClassAtom::Char(atom));
                }
                Ok(ClassAtom::Char(octal_escape(self, atom - 0x30)?))
            }
            _ => {
                if self.unicode() && !is_identity_escape_allowed(atom) && atom != 0x2D {
                    // `-` is its own ClassEscape production (a literal dash);
                    // any other identity escape is invalid in unicode mode.
                    Err(self.error("Invalid escape"))
                } else {
                    Ok(ClassAtom::Char(atom))
                }
            }
        }
    }

    /// /v set arithmetic: ClassSetExpression with nested classes, `&&`,
    /// `--`, and `\q{…}` strings (spec 22.2.2).
    fn parse_class_set(&mut self, negated: bool) -> Result<CharClass, Error> {
        let operand = self.parse_class_set_operand()?;
        let mut class = operand;
        loop {
            match self.peek() {
                Some(0x26) if self.peek_at(1) == Some(0x26) => {
                    self.next();
                    self.next();
                    let right = self.parse_class_set_operand()?;
                    class = intersect_classes(class, right);
                }
                Some(0x2D) if self.peek_at(1) == Some(0x2D) => {
                    self.next();
                    self.next();
                    let right = self.parse_class_set_operand()?;
                    class = difference_classes(class, right);
                }
                Some(0x2D) if self.peek_at(1) != Some(0x5D) => {
                    // ClassSetRange (spec 22.2.1): two ClassSetCharacters
                    // separated by `-`. Both endpoints must be literal single
                    // characters (a char or a char escape like `\x41`); a
                    // range over a class escape (`[\d-a]`) is an early error.
                    let (Some((start, _)),) = (class_ranges_singleton(&class),) else {
                        return Err(self.error("Invalid class set character"));
                    };
                    self.next(); // -
                    let right = self.parse_class_set_operand()?;
                    let Some((_, end)) = class_ranges_singleton(&right) else {
                        return Err(self.error("Invalid class set character"));
                    };
                    if start > end {
                        return Err(self.error("Range out of order in character class"));
                    }
                    class = CharClass::new(false);
                    class.add_range(start, end);
                }
                Some(0x5D) => {
                    self.next();
                    break;
                }
                _ => {
                    // A bare `&`/`-`/char continues the union.
                    let operand = self.parse_class_set_operand()?;
                    class = union_classes(class, operand);
                }
            }
        }
        if negated {
            class.negate();
        }
        if self.ignore_case {
            class.ranges = crate::engine::fold_ranges(&class.ranges, self.unicode());
        }
        Ok(class)
    }

    fn parse_class_set_operand(&mut self) -> Result<CharClass, Error> {
        match self.peek() {
            None => Err(self.error("Unterminated character class")),
            Some(0x5B) => {
                // Nested class: parse it as its own set expression.
                self.next();
                let negated = self.eat(0x5E);
                let class = self.parse_class_set(negated)?;
                Ok(class)
            }
            Some(0x5C) => {
                self.next();
                let Some(atom) = self.next() else {
                    return Err(self.error("\\ at end of character class"));
                };
                match atom {
                    0x64 | 0x44 | 0x73 | 0x53 | 0x77 | 0x57 => {
                        let predicate = match atom {
                            0x64 | 0x44 => Predicate::Digits,
                            0x73 | 0x53 => Predicate::Space,
                            _ => Predicate::Word,
                        };
                        let negated = matches!(atom, 0x44 | 0x53 | 0x57);
                        Ok(self.escape_class(predicate, negated))
                    }
                    0x70 | 0x50 => {
                        if !self.eat(0x7B) {
                            return Err(self.error("Invalid property escape"));
                        }
                        let negated = atom == 0x50;
                        self.parse_property_escape(negated)
                    }
                    0x71 => {
                        if !self.eat(0x7B) {
                            return Err(self.error("Invalid \\q escape"));
                        }
                        self.parse_q_string()
                    }
                    _ => self.parse_class_escape(atom).map(|a| match a {
                        ClassAtom::Char(c) => CharClass::singleton(c),
                        ClassAtom::Escape(class) => class,
                    }),
                }
            }
            Some(atom) => {
                if self.flags.v {
                    // v-mode ClassSetCharacter (spec 22.2.1): the syntax
                    // characters `( ) { } / - ] |` must be escaped, and a
                    // doubled reserved punctuator is an early error (`&&` and
                    // `--` are set operators handled by the caller).
                    if matches!(atom, 0x28 | 0x29 | 0x7B | 0x7D | 0x2F | 0x2D | 0x5D | 0x7C) {
                        return Err(self.error("Invalid class set character"));
                    }
                    if is_double_punctuator(atom) && self.peek_at(1) == Some(atom) {
                        return Err(self.error("Invalid doubled punctuator"));
                    }
                    // `&&` at an operand position is the intersection operator
                    // with a missing left operand (spec 22.2.1).
                    if atom == 0x26 && self.peek_at(1) == Some(0x26) {
                        return Err(self.error("Invalid class set intersection"));
                    }
                }
                self.next();
                Ok(CharClass::singleton(atom))
            }
        }
    }

    /// `\q{…}` in /v mode: a string disjunction (one or more `|`-separated
    /// strings, each one or more characters — spec ClassStringDisjunction).
    fn parse_q_string(&mut self) -> Result<CharClass, Error> {
        let mut strings: Vec<Vec<u32>> = vec![Vec::new()];
        loop {
            match self.peek() {
                None => return Err(self.error("Unterminated \\q escape")),
                Some(0x7D) => {
                    self.next();
                    break;
                }
                Some(0x7C) => {
                    // `|` separates the alternative strings.
                    self.next();
                    strings.push(Vec::new());
                }
                Some(0x5C) => {
                    self.next();
                    let Some(atom) = self.next() else {
                        return Err(self.error("\\ at end of \\q escape"));
                    };
                    match atom {
                        0x78 => {
                            let Some(hi) = self.peek() else {
                                return Err(self.error("Invalid \\x escape"));
                            };
                            let (Some(h), Some(l)) =
                                (hex_value(hi), self.peek_at(1).and_then(hex_value))
                            else {
                                return Err(self.error("Invalid \\x escape"));
                            };
                            self.next();
                            self.next();
                            strings.last_mut().unwrap().push((h << 4) | l);
                        }
                        0x75 => {
                            let Some(hi) = self.peek() else {
                                return Err(self.error("Invalid \\u escape"));
                            };
                            let (Some(h), Some(l)) =
                                (hex_value(hi), self.peek_at(1).and_then(hex_value))
                            else {
                                return Err(self.error("Invalid \\u escape"));
                            };
                            self.next();
                            self.next();
                            let mut value = (h << 12) | (l << 8);
                            if let (Some(h2), Some(l2)) = (
                                self.peek().and_then(hex_value),
                                self.peek_at(1).and_then(hex_value),
                            ) {
                                self.next();
                                self.next();
                                value |= (h2 << 4) | l2;
                            }
                            strings.last_mut().unwrap().push(value);
                        }
                        other => strings.last_mut().unwrap().push(other),
                    }
                }
                Some(atom) => {
                    self.next();
                    strings.last_mut().unwrap().push(atom);
                }
            }
        }
        // A ClassStringDisjunction member is one or more characters; drop any
        // empty alternatives (`\q{9|}` / `\q{|9}` / `\q{}` are errors).
        if strings.iter().any(|s| s.is_empty()) {
            return Err(self.error("Invalid \\q escape"));
        }
        Ok(CharClass {
            ranges: Vec::new(),
            strings,
            negated: false,
            predicate: None,
            fold: self.ignore_case,
        })
    }
}

enum ClassAtom {
    Char(Atom),
    Escape(CharClass),
}

impl Clone for ClassAtom {
    fn clone(&self) -> Self {
        match self {
            ClassAtom::Char(c) => ClassAtom::Char(*c),
            ClassAtom::Escape(class) => ClassAtom::Escape(class.clone()),
        }
    }
}

/// One member of a character class being assembled.
enum ClassMember {
    Char(Atom),
    Range(u32, u32),
    /// An escape set: a predicate (possibly negated) or explicit ranges.
    Set(CharClass),
}

impl ClassMember {
    fn from_class_atom(atom: ClassAtom) -> ClassMember {
        match atom {
            ClassAtom::Char(c) => ClassMember::Char(c),
            ClassAtom::Escape(class) => ClassMember::Set(class),
        }
    }
}

/// Assemble the class members into a single `CharClass`, keeping a single
/// predicate when possible (fast path) and enumerating into explicit ranges
/// otherwise. `ignore_case` folds the explicit ranges; negated predicates are
/// scanned and complemented.
/// Pre-scan the whole pattern: the total capturing-group count (so decimal
/// escapes resolve forward references) and every name → capture indices in
/// source order (so `\k<name>` resolves forward references). Skips escapes,
/// character classes, and non-capturing groups.
fn scan_pattern(atoms: &[Atom]) -> (usize, HashMap<Vec<Atom>, Vec<usize>>) {
    let mut count = 0usize;
    let mut named: HashMap<Vec<Atom>, Vec<usize>> = HashMap::new();
    let mut i = 0;
    while i < atoms.len() {
        match atoms[i] {
            0x5C => i += 2, // escape + escaped code point
            0x5B => {
                i += 1;
                while i < atoms.len() && atoms[i] != 0x5D {
                    i += if atoms[i] == 0x5C { 2 } else { 1 };
                }
                i += 1;
            }
            0x28 => {
                if atoms.get(i + 1) == Some(&0x3F) {
                    match atoms.get(i + 2) {
                        // (?:, (?=, (?!
                        Some(&0x3A) | Some(&0x3D) | Some(&0x21) => i += 3,
                        Some(&0x3C) => {
                            // (?<=, (?<! are lookbehind; otherwise a named
                            // capture `(?<name>`.
                            match atoms.get(i + 3) {
                                Some(&0x3D) | Some(&0x21) => i += 4,
                                _ => {
                                    count += 1;
                                    let index = count;
                                    let mut name = Vec::new();
                                    let mut j = i + 3;
                                    while j < atoms.len() && atoms[j] != 0x3E {
                                        name.push(atoms[j]);
                                        j += 1;
                                    }
                                    named.entry(name).or_default().push(index);
                                    i = j + 1;
                                }
                            }
                        }
                        _ => i += 2,
                    }
                } else {
                    count += 1;
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    (count, named)
}

/// Capture indices within `node` (including nested groups and lookarounds),
/// used to clear the atom's captures before each quantified iteration.
fn collect_captures(node: &Node) -> Vec<usize> {
    fn walk(node: &Node, out: &mut Vec<usize>) {
        match node {
            Node::Capture { index, node } => {
                out.push(*index);
                walk(node, out);
            }
            Node::Sequence(nodes) => nodes.iter().for_each(|n| walk(n, out)),
            Node::Alternate(alts) => alts
                .iter()
                .for_each(|alt| alt.iter().for_each(|n| walk(n, out))),
            Node::Repeat { node, .. } => walk(node, out),
            Node::Lookahead { node, .. } | Node::Lookbehind { node, .. } => walk(node, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// Whether the code point is a /v-mode ClassSetReservedPunctuator whose
/// doubled form is an early error (spec ClassSetReservedDoublePunctuator,
/// minus the `&&`/`--` set operators).
fn is_double_punctuator(atom: Atom) -> bool {
    matches!(
        atom,
        0x21 | 0x23
            | 0x24
            | 0x25
            | 0x2A
            | 0x2B
            | 0x2C
            | 0x2E
            | 0x3A
            | 0x3B
            | 0x3C
            | 0x3D
            | 0x3E
            | 0x3F
            | 0x40
            | 0x5E
            | 0x60
            | 0x7E
    )
}

fn assemble_class(
    members: Vec<ClassMember>,
    negated: bool,
    ignore_case: bool,
    unicode: bool,
) -> CharClass {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut strings: Vec<Vec<u32>> = Vec::new();
    let mut predicate: Option<(Predicate, bool)> = None;
    for member in members {
        match member {
            ClassMember::Char(c) => push_range(&mut ranges, c, c),
            ClassMember::Range(s, e) => push_range(&mut ranges, s, e),
            ClassMember::Set(class) => {
                if let Some(pred) = class.predicate {
                    let entry = (pred, class.negated);
                    match &predicate {
                        None => predicate = Some(entry),
                        Some(existing) if existing == &entry => {}
                        Some(_) => {
                            // Multiple distinct predicates: enumerate all of
                            // them (the second, plus the already-kept one
                            // becomes explicit next time it is checked).
                            let (p2, n2) = entry;
                            let mut set = crate::engine::scan_predicate(&p2, ignore_case, unicode);
                            if n2 {
                                crate::engine::complement(&mut set);
                            }
                            ranges.extend(set);
                            let (p1, n1) = predicate.take().unwrap();
                            let mut set1 = crate::engine::scan_predicate(&p1, ignore_case, unicode);
                            if n1 {
                                crate::engine::complement(&mut set1);
                            }
                            ranges.extend(set1);
                        }
                    }
                } else {
                    let mut set = class.ranges;
                    if class.negated {
                        crate::engine::complement(&mut set);
                    }
                    ranges.extend(set);
                    strings.extend(class.strings);
                }
            }
        }
    }
    let mut class = if let Some((pred, pred_negated)) = predicate {
        if ranges.is_empty() && strings.is_empty() {
            let mut class = CharClass::from_predicate(pred, pred_negated ^ negated);
            class.fold = ignore_case;
            class
        } else {
            // Enumerate the single predicate and combine.
            let mut set = crate::engine::scan_predicate(&pred, ignore_case, unicode);
            if pred_negated {
                crate::engine::complement(&mut set);
            }
            ranges.extend(set);
            CharClass {
                ranges: merge_ranges(ranges),
                strings,
                negated,
                predicate: None,
                fold: ignore_case,
            }
        }
    } else {
        CharClass {
            ranges: merge_ranges(ranges),
            strings,
            negated,
            predicate: None,
            fold: ignore_case,
        }
    };
    if ignore_case && class.predicate.is_none() {
        class.ranges = crate::engine::fold_ranges(&class.ranges, unicode);
    }
    class
}

fn push_range(ranges: &mut Vec<(u32, u32)>, s: u32, e: u32) {
    ranges.push((s, e));
}

fn merge_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if s <= *last_end + 1 => {
                *last_end = (*last_end).max(e);
            }
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Legacy octal escapes (Annex B LegacyOctalEscapeSequence): `\0`-`\3` take
/// up to two more octal digits; `\4`-`\7` take exactly one more. Values never
/// exceed 0xFF under this grammar.
fn octal_escape(parser: &mut Parser<'_>, first: u32) -> Result<u32, Error> {
    let mut value = first;
    let mut remaining = if first <= 3 { 2 } else { 1 };
    while remaining > 0 {
        let Some(d) = parser.peek() else { break };
        if !(0x30..=0x37).contains(&d) {
            break;
        }
        parser.next();
        value = value * 8 + (d - 0x30);
        remaining -= 1;
    }
    Ok(value)
}

fn hex_value(atom: Atom) -> Option<u32> {
    match atom {
        0x30..=0x39 => Some(atom - 0x30),
        0x41..=0x46 => Some(atom - 0x41 + 10),
        0x61..=0x66 => Some(atom - 0x61 + 10),
        _ => None,
    }
}

fn is_control_letter(atom: Atom) -> bool {
    matches!(atom, 0x41..=0x5A | 0x61..=0x7A)
}

fn is_letter_or_underscore_or_digit(atom: Atom) -> bool {
    // `=` separates the property name from its value (`\p{Script=Latin}`).
    matches!(atom, 0x30..=0x39 | 0x3D | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

/// Identity escapes allowed in unicode mode: SyntaxCharacter,
/// `/`, and the legacy `$ & - _ ~` (spec IdentityEscape).
fn is_identity_escape_allowed(atom: Atom) -> bool {
    // spec IdentityEscape[+U]: SyntaxCharacter | `/` (B.1.4 does not apply).
    matches!(
        atom,
        0x24 | 0x28
            | 0x29
            | 0x2A
            | 0x2B
            | 0x2E
            | 0x2F
            | 0x3F
            | 0x5B
            | 0x5C
            | 0x5D
            | 0x5E
            | 0x7B
            | 0x7C
            | 0x7D
    )
}

fn code_points_to_string(cps: &[u32]) -> String {
    let mut out = String::new();
    for &cp in cps {
        if let Some(c) = char::from_u32(cp) {
            out.push(c);
        }
    }
    out
}

/// Map a General_Category name (long or two-letter) to its abbreviation.
fn category_abbreviation(name: &str) -> Option<&'static str> {
    Some(match name {
        "Lu" | "Uppercase_Letter" => "Lu",
        "Ll" | "Lowercase_Letter" => "Ll",
        "Lt" | "Titlecase_Letter" => "Lt",
        "Lm" | "Modifier_Letter" => "Lm",
        "Lo" | "Other_Letter" => "Lo",
        "L" | "Letter" => "L",
        "LC" | "Cased_Letter" => "LC",
        "Mn" | "Nonspacing_Mark" => "Mn",
        "Mc" | "Spacing_Mark" => "Mc",
        "Me" | "Enclosing_Mark" => "Me",
        "M" | "Mark" | "Combining_Mark" => "M",
        "Nd" | "Decimal_Number" | "digit" => "Nd",
        "Nl" | "Letter_Number" => "Nl",
        "No" | "Other_Number" => "No",
        "N" | "Number" => "N",
        "Pc" | "Connector_Punctuation" => "Pc",
        "Pd" | "Dash_Punctuation" => "Pd",
        "Ps" | "Open_Punctuation" => "Ps",
        "Pe" | "Close_Punctuation" => "Pe",
        "Pi" | "Initial_Punctuation" => "Pi",
        "Pf" | "Final_Punctuation" => "Pf",
        "Po" | "Other_Punctuation" => "Po",
        "P" | "Punctuation" | "punct" => "P",
        "Sm" | "Math_Symbol" => "Sm",
        "Sc" | "Currency_Symbol" => "Sc",
        "Sk" | "Modifier_Symbol" => "Sk",
        "So" | "Other_Symbol" => "So",
        "S" | "Symbol" => "S",
        "Zs" | "Space_Separator" => "Zs",
        "Zl" | "Line_Separator" => "Zl",
        "Zp" | "Paragraph_Separator" => "Zp",
        "Z" | "Separator" => "Z",
        "Cc" | "Control" | "cntrl" => "Cc",
        "Cf" | "Format" => "Cf",
        "Cs" | "Surrogate" => "Cs",
        "Co" | "Private_Use" => "Co",
        "Cn" | "Unassigned" => "Cn",
        "C" | "Other" => "C",
        _ => return None,
    })
}

/// Canonical script name (full name with underscores) for a Script escape.
fn canonical_script_name(name: &str) -> Option<&'static str> {
    unicode::canonical_script_name(name)
}

fn binary_property_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "ASCII" => "ASCII",
        "AHex" | "ASCII_Hex_Digit" => "ASCII_Hex_Digit",
        "Alpha" | "Alphabetic" => "Alphabetic",
        "Any" => "Any",
        "Assigned" => "Assigned",
        "Bidi_C" | "Bidi_Control" => "Bidi_Control",
        "Bidi_M" | "Bidi_Mirrored" => "Bidi_Mirrored",
        "CI" | "Case_Ignorable" => "Case_Ignorable",
        "Cased" => "Cased",
        "CWCF" | "Changes_When_Casefolded" => "Changes_When_Casefolded",
        "CWCM" | "Changes_When_Casemapped" => "Changes_When_Casemapped",
        "CWKCF" | "Changes_When_NFKC_Casefolded" => "Changes_When_NFKC_Casefolded",
        "CWL" | "Changes_When_Lowercased" => "Changes_When_Lowercased",
        "CWT" | "Changes_When_Titlecased" => "Changes_When_Titlecased",
        "CWU" | "Changes_When_Uppercased" => "Changes_When_Uppercased",
        "Dash" => "Dash",
        "DI" | "Default_Ignorable_Code_Point" => "Default_Ignorable_Code_Point",
        "Dep" | "Deprecated" => "Deprecated",
        "Dia" | "Diacritic" => "Diacritic",
        "EBase" | "Emoji_Modifier_Base" => "Emoji_Modifier_Base",
        "EComp" | "Emoji_Component" => "Emoji_Component",
        "EMod" | "Emoji_Modifier" => "Emoji_Modifier",
        "EPres" | "Emoji_Presentation" => "Emoji_Presentation",
        "Emoji" => "Emoji",
        "Ext" | "Extender" => "Extender",
        "ExtPict" | "Extended_Pictographic" => "Extended_Pictographic",
        "Gr_Base" | "Grapheme_Base" => "Grapheme_Base",
        "Gr_Ext" | "Grapheme_Extend" => "Grapheme_Extend",
        "Hex" | "Hex_Digit" => "Hex_Digit",
        "IDC" | "ID_Continue" => "ID_Continue",
        "IDS" | "ID_Start" => "ID_Start",
        "IDSB" | "IDS_Binary_Operator" => "IDS_Binary_Operator",
        "IDST" | "IDS_Trinary_Operator" => "IDS_Trinary_Operator",
        "Ideo" | "Ideographic" => "Ideographic",
        "Join_C" | "Join_Control" => "Join_Control",
        "LOE" | "Logical_Order_Exception" => "Logical_Order_Exception",
        "Lower" | "Lowercase" => "Lowercase",
        "Math" => "Math",
        "NChar" | "Noncharacter_Code_Point" => "Noncharacter_Code_Point",
        "Pat_Syn" | "Pattern_Syntax" => "Pattern_Syntax",
        "Pat_WS" | "Pattern_White_Space" => "Pattern_White_Space",
        "QMark" | "Quotation_Mark" => "Quotation_Mark",
        "Radical" => "Radical",
        "RI" | "Regional_Indicator" => "Regional_Indicator",
        "SD" | "Soft_Dotted" => "Soft_Dotted",
        "STerm" | "Sentence_Terminal" => "Sentence_Terminal",
        "Term" | "Terminal_Punctuation" => "Terminal_Punctuation",
        "UIdeo" | "Unified_Ideograph" => "Unified_Ideograph",
        "Upper" | "Uppercase" => "Uppercase",
        "VS" | "Variation_Selector" => "Variation_Selector",
        "space" | "WSpace" | "White_Space" => "White_Space",
        "XIDC" | "XID_Continue" => "XID_Continue",
        "XIDS" | "XID_Start" => "XID_Start",
        _ => return None,
    })
}

/// The single (start, end) pair of a class that is exactly one character
/// (no strings, no predicate) — the ClassSetRange endpoint case. `None` for
/// anything else (a range over `\d`, `\q{…}`, or a nested class is an
/// early error in /v mode).
fn class_ranges_singleton(class: &CharClass) -> Option<(u32, u32)> {
    if class.predicate.is_none()
        && class.strings.is_empty()
        && class.ranges.len() == 1
        && class.ranges[0].0 == class.ranges[0].1
    {
        Some((class.ranges[0].0, class.ranges[0].1))
    } else {
        None
    }
}

/// Whether `cp` is in a sorted inclusive range list (binary search).
fn ranges_contain_sorted(ranges: &[(u32, u32)], cp: u32) -> bool {
    ranges
        .binary_search_by(|&(s, e)| {
            if cp < s {
                std::cmp::Ordering::Greater
            } else if cp > e {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Union of two classes (both explicit after /v scanning).
fn union_classes(mut a: CharClass, b: CharClass) -> CharClass {
    a.ranges.extend(b.ranges);
    a.strings.extend(b.strings);
    a.ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (s, e) in a.ranges {
        match merged.last_mut() {
            Some((_, last_end)) if s <= *last_end + 1 => {
                *last_end = (*last_end).max(e);
            }
            _ => merged.push((s, e)),
        }
    }
    a.ranges = merged;
    a
}

fn intersect_classes(a: CharClass, b: CharClass) -> CharClass {
    let mut out = CharClass::new(false);
    for &(as_, ae) in &a.ranges {
        for &(bs, be) in &b.ranges {
            let s = as_.max(bs);
            let e = ae.min(be);
            if s <= e {
                out.add_range(s, e);
            }
        }
    }
    // A single-character string in one set whose character is in the other
    // set's ranges survives the intersection as that character (spec
    // ClassSetExpression semantics: `[\d&&\q{0|2|4|9\uFE0F\u20E3}]` keeps
    // "0"/"2"/"4" and drops the multi-char "9\uFE0F\u20E3").
    for sa in &a.strings {
        if sa.len() == 1 && ranges_contain_sorted(&b.ranges, sa[0]) {
            out.add_range(sa[0], sa[0]);
        }
        if b.strings.iter().any(|sb| sb == sa) {
            out.strings.push(sa.clone());
        }
    }
    for sb in &b.strings {
        if sb.len() == 1 && ranges_contain_sorted(&a.ranges, sb[0]) {
            out.add_range(sb[0], sb[0]);
        }
    }
    out
}

fn difference_classes(a: CharClass, b: CharClass) -> CharClass {
    let mut out = CharClass::new(false);
    for &(as_, ae) in &a.ranges {
        let mut start = as_;
        for &(bs, be) in &b.ranges {
            if be < start {
                continue;
            }
            if bs > ae {
                break;
            }
            if bs > start {
                out.add_range(start, bs.saturating_sub(1));
            }
            if be >= ae {
                start = ae + 1;
                break;
            }
            start = be.saturating_add(1).max(start);
        }
        if start <= ae {
            out.add_range(start, ae);
        }
    }
    out.strings = a
        .strings
        .into_iter()
        .filter(|s| !b.strings.iter().any(|bs| bs == s))
        .collect();
    out
}
