//! The pattern parser (spec 22.2.1 grammar, Annex B amendments): a recursive
//! descent over the pattern's code points (unicode mode) or code units
//! (legacy mode), parameterized by the `u`/`v` flags and inline modifiers.

use std::collections::HashMap;

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
    named_groups: HashMap<Vec<Atom>, usize>,
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
    let mut parser = Parser {
        atoms: &atoms,
        pos: 0,
        flags,
        ignore_case: flags.i,
        multiline: flags.m,
        dot_all: flags.s,
        capturing_groups: 0,
        named_groups: HashMap::new(),
        has_group_names: false,
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

    /// Pattern :: Disjunction
    fn parse_disjunction(&mut self) -> Result<Node, Error> {
        let mut alternatives = vec![self.parse_alternative()?];
        while self.eat(0x7C) {
            alternatives.push(self.parse_alternative()?);
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
            Some(atom) => {
                self.next();
                self.char_node(atom)
            }
            None => return Err(self.error("Unexpected end of pattern")),
        };
        let quantifier = self.parse_quantifier()?;
        match quantifier {
            None => Ok(node),
            Some((min, max, greedy)) => Ok(Node::Repeat {
                node: Box::new(node),
                min,
                max,
                greedy,
            }),
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
    fn parse_decimal_digits(&mut self) -> Result<u32, Error> {
        let mut value = 0u32;
        let mut any = false;
        while let Some(d) = self.peek()
            && (0x30..=0x39).contains(&d)
        {
            self.next();
            any = true;
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(d - 0x30))
                .ok_or_else(|| self.error("Quantifier number too large"))?;
        }
        if !any {
            return Err(self.error("Expected decimal digits"));
        }
        Ok(value)
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
    /// which force enumeration into explicit ranges.
    fn escape_class(&mut self, predicate: Predicate, negated: bool) -> CharClass {
        if self.ignore_case || self.flags.v {
            let fold = self.ignore_case;
            let ranges = crate::engine::scan_predicate(&predicate, fold, self.unicode());
            CharClass {
                ranges,
                strings: Vec::new(),
                negated,
                predicate: None,
                fold,
            }
        } else {
            let mut class = CharClass::from_predicate(predicate, negated);
            class.fold = false;
            class
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
            "General_Category" => {
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
        if self.ignore_case || self.flags.v {
            let fold = self.ignore_case;
            let ranges = crate::engine::scan_predicate(&predicate, fold, self.unicode());
            CharClass {
                ranges,
                strings: Vec::new(),
                negated,
                predicate: None,
                fold,
            }
        } else {
            let mut class = CharClass::from_predicate(predicate, negated);
            class.fold = false;
            class
        }
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
                if group <= self.capturing_groups {
                    // A backreference.
                    let mut index = group;
                    // Annex B: \10 with 10+ groups refers to group 10; consume
                    // a second digit only when it forms a valid group number.
                    if let Some(d) = self.peek()
                        && (0x30..=0x39).contains(&d)
                        && (0x30..=0x39).contains(&atom)
                    {
                        let two = group * 10 + (d - 0x30) as usize;
                        if two <= self.capturing_groups {
                            self.next();
                            index = two;
                        }
                    }
                    Ok(Node::Backref {
                        index,
                        fold: self.ignore_case,
                    })
                } else if self.unicode() {
                    Err(self.error("Invalid decimal escape"))
                } else {
                    // Legacy octal escape.
                    let value = octal_escape(self, atom - 0x30)?;
                    Ok(self.char_node(value))
                }
            }
            0x78 => {
                let Some(hi) = self.peek() else {
                    return Err(self.error("Invalid \\x escape"));
                };
                if let (Some(h), Some(l)) = (hex_value(hi), self.peek_at(1).and_then(hex_value)) {
                    self.next();
                    self.next();
                    Ok(self.char_node((h << 4) | l))
                } else if self.unicode() {
                    Err(self.error("Invalid \\x escape"))
                } else {
                    Ok(self.char_node(0x78))
                }
            }
            0x75 => {
                if self.unicode() && self.peek() == Some(0x7B) {
                    self.next();
                    return self.parse_unicode_code_point_escape();
                }
                let Some(hi) = self.peek() else {
                    return Err(self.error("Invalid \\u escape"));
                };
                if let (Some(h), Some(l)) = (hex_value(hi), self.peek_at(1).and_then(hex_value)) {
                    self.next();
                    self.next();
                    let value = (h << 12) | (l << 8);
                    if let (Some(h2), Some(l2)) = (
                        self.peek().and_then(hex_value),
                        self.peek_at(1).and_then(hex_value),
                    ) {
                        self.next();
                        self.next();
                        let code_unit = value | (h2 << 4) | l2;
                        return self.char_from_code_unit(code_unit);
                    }
                    return Ok(self.char_node(value as Atom));
                }
                if self.unicode() {
                    Err(self.error("Invalid \\u escape"))
                } else {
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
                    // Annex B: `\c` without a control letter matches `\` then
                    // the letter `c` is a literal; the following char (if
                    // any) is consumed by the caller's normal flow.
                    Ok(self.char_node(b'\\' as Atom))
                }
            }
            0x6B => {
                if self.peek() == Some(0x3C) {
                    self.parse_named_backreference()
                } else if self.unicode() {
                    Err(self.error("Invalid named capture reference"))
                } else {
                    Ok(self.char_node(0x6B))
                }
            }
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
    /// surrogate must form a pair (or be a lone surrogate only if followed by
    /// nothing), per the spec's escape semantics.
    fn char_from_code_unit(&mut self, code_unit: u32) -> Result<Node, Error> {
        if !self.unicode() {
            return Ok(self.char_node(code_unit));
        }
        if (0xD800..=0xDBFF).contains(&code_unit) {
            // Leading surrogate: only valid as part of a pair.
            return Err(self.error("Invalid unicode escape"));
        }
        Ok(self.char_node(code_unit))
    }

    /// `\k<name>` — a named backreference.
    fn parse_named_backreference(&mut self) -> Result<Node, Error> {
        self.next(); // <
        let name = self.parse_group_name()?;
        if !self.eat(0x3E) {
            return Err(self.error("Invalid named capture reference"));
        }
        match self.named_groups.get(&name) {
            Some(&index) => Ok(Node::Backref {
                index,
                fold: self.ignore_case,
            }),
            None => Err(self.error("Invalid named capture reference")),
        }
    }

    /// GroupName :: `<` IdentifierName `>`
    fn parse_group_name(&mut self) -> Result<Vec<Atom>, Error> {
        let mut name = Vec::new();
        while let Some(a) = self.peek() {
            if a == 0x3E {
                break;
            }
            name.push(self.next().unwrap());
        }
        if name.is_empty() {
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
                            let length = self.fixed_length(&body)?;
                            Ok(Node::Lookbehind {
                                negate: false,
                                node: Box::new(body),
                                length,
                            })
                        }
                        Some(0x21) => {
                            self.next();
                            let body = self.parse_disjunction()?;
                            if !self.eat(0x29) {
                                return Err(self.error("Unterminated group"));
                            }
                            let length = self.fixed_length(&body)?;
                            Ok(Node::Lookbehind {
                                negate: true,
                                node: Box::new(body),
                                length,
                            })
                        }
                        _ => {
                            // Named capture group.
                            let name = self.parse_group_name()?;
                            if !self.eat(0x3E) {
                                return Err(self.error("Invalid capture group name"));
                            }
                            self.capturing_groups += 1;
                            let index = self.capturing_groups;
                            if self.named_groups.insert(name, index).is_some() {
                                return Err(self.error("Duplicate capture group name"));
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
                Some(a) if a == 0x69 || a == 0x6D || a == 0x73 => {
                    // Inline modifiers: (?ims-ims:…) or (?ims-ims).
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
                    if saw_minus {
                        remove |= 1;
                    } else {
                        add |= 1;
                    }
                    self.next();
                }
                Some(0x6D) => {
                    if saw_minus {
                        remove |= 2;
                    } else {
                        add |= 2;
                    }
                    self.next();
                }
                Some(0x73) => {
                    if saw_minus {
                        remove |= 4;
                    } else {
                        add |= 4;
                    }
                    self.next();
                }
                Some(0x2D) if !saw_minus => {
                    saw_minus = true;
                    self.next();
                }
                Some(0x3A) => {
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

    /// Fixed length of a subpattern in characters (input elements), for
    /// lookbehind (spec: the lookbehind Disjunction must be fixed-length).
    fn fixed_length(&self, node: &Node) -> Result<u32, Error> {
        match node {
            Node::Empty
            | Node::Start { .. }
            | Node::End { .. }
            | Node::WordBoundary { .. }
            | Node::NotWordBoundary { .. }
            | Node::Lookahead { .. }
            | Node::Lookbehind { .. } => Ok(0),
            Node::Char { .. } | Node::Any { .. } | Node::Class(_) => Ok(1),
            Node::Backref { .. } => Err(self.error("Lookbehind assertion is not fixed length")),
            Node::Sequence(nodes) => {
                let mut total = 0u32;
                for n in nodes {
                    total = total
                        .checked_add(self.fixed_length(n)?)
                        .ok_or_else(|| self.error("Lookbehind too long"))?;
                }
                Ok(total)
            }
            Node::Alternate(alts) => {
                let mut length = None;
                for alt in alts {
                    let l = self.fixed_length(&Node::Sequence(alt.clone()))?;
                    match length {
                        None => length = Some(l),
                        Some(prev) if prev != l => {
                            return Err(self.error("Lookbehind assertion is not fixed length"));
                        }
                        _ => {}
                    }
                }
                Ok(length.unwrap_or(0))
            }
            Node::Capture { node, .. } => self.fixed_length(node),
            Node::Repeat { node, min, max, .. } => {
                if max.is_none() || *min != max.unwrap() {
                    return Err(self.error("Lookbehind assertion is not fixed length"));
                }
                self.fixed_length(node)?
                    .checked_mul(*min)
                    .ok_or_else(|| self.error("Lookbehind too long"))
            }
        }
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
                Some(0x5D) => {
                    // `[]]`: the first `]` is a literal ClassAtom (Annex B).
                    self.next();
                    members.push(ClassMember::Char(0x5D));
                    first = false;
                    continue;
                }
                _ => {}
            }
            first = false;
            let atom = self.parse_class_atom()?;
            if self.peek() == Some(0x2D) && !self.peek_at(1).is_none_or(|a| a == 0x5D) {
                self.next();
                let end = self.parse_class_atom()?;
                match (&atom, &end) {
                    (ClassAtom::Char(s), ClassAtom::Char(e)) => {
                        let (s, e) = (*s, *e);
                        if self.unicode() && s > e {
                            return Err(self.error("Range out of order in character class"));
                        }
                        if s <= e {
                            members.push(ClassMember::Range(s, e));
                        }
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
            0x62 => {
                if self.unicode() {
                    return Err(self.error("\\b in character class is invalid in unicode mode"));
                }
                Ok(ClassAtom::Char(0x08)) // backspace
            }
            0x78 => {
                let Some(hi) = self.peek() else {
                    return Err(self.error("Invalid \\x escape"));
                };
                if let (Some(h), Some(l)) = (hex_value(hi), self.peek_at(1).and_then(hex_value)) {
                    self.next();
                    self.next();
                    Ok(ClassAtom::Char((h << 4) | l))
                } else if self.unicode() {
                    Err(self.error("Invalid \\x escape"))
                } else {
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
                let Some(hi) = self.peek() else {
                    return Err(self.error("Invalid \\u escape"));
                };
                if let (Some(h), Some(l)) = (hex_value(hi), self.peek_at(1).and_then(hex_value)) {
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
                    Ok(ClassAtom::Char(value))
                } else if self.unicode() {
                    Err(self.error("Invalid \\u escape"))
                } else {
                    Ok(ClassAtom::Char(0x75))
                }
            }
            0x63 => {
                if let Some(letter) = self.peek()
                    && is_control_letter(letter)
                {
                    self.next();
                    Ok(ClassAtom::Char(letter % 32))
                } else if self.unicode() {
                    Err(self.error("Invalid control escape"))
                } else {
                    Ok(ClassAtom::Char(b'\\' as Atom))
                }
            }
            _ => {
                if self.unicode() && !is_identity_escape_allowed(atom) {
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
                self.next();
                Ok(CharClass::singleton(atom))
            }
        }
    }

    /// `\q{…}` in /v mode: a string atom (one or more characters).
    fn parse_q_string(&mut self) -> Result<CharClass, Error> {
        let mut string = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.error("Unterminated \\q escape")),
                Some(0x7D) => {
                    self.next();
                    break;
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
                            string.push((h << 4) | l);
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
                            string.push(value);
                        }
                        other => string.push(other),
                    }
                }
                Some(atom) => {
                    self.next();
                    string.push(atom);
                }
            }
        }
        if string.is_empty() {
            return Err(self.error("Invalid \\q escape"));
        }
        Ok(CharClass {
            ranges: Vec::new(),
            strings: vec![string],
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

/// Legacy octal escapes (Annex B): up to 3 octal digits, value capped at 0xFF.
fn octal_escape(parser: &mut Parser<'_>, first: u32) -> Result<u32, Error> {
    let mut value = first;
    let mut count = 1;
    while count < 3 {
        let Some(d) = parser.peek() else { break };
        if !(0x30..=0x37).contains(&d) {
            break;
        }
        parser.next();
        value = value * 8 + (d - 0x30);
        if value > 0xFF {
            value -= 0x100;
        }
        count += 1;
    }
    if value > 0xFF {
        value %= 0x100;
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
    matches!(atom, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

/// Identity escapes allowed in unicode mode: SyntaxCharacter,
/// `/`, and the legacy `$ & - _ ~` (spec IdentityEscape).
fn is_identity_escape_allowed(atom: Atom) -> bool {
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
            | 0x2D
            | 0x5F
            | 0x7E
            | 0x26
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
        "Mn" | "Nonspacing_Mark" => "Mn",
        "Mc" | "Spacing_Mark" => "Mc",
        "Me" | "Enclosing_Mark" => "Me",
        "M" | "Mark" => "M",
        "Nd" | "Decimal_Number" => "Nd",
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
        "P" | "Punctuation" => "P",
        "Sm" | "Math_Symbol" => "Sm",
        "Sc" | "Currency_Symbol" => "Sc",
        "Sk" | "Modifier_Symbol" => "Sk",
        "So" | "Other_Symbol" => "So",
        "S" | "Symbol" => "S",
        "Zs" | "Space_Separator" => "Zs",
        "Zl" | "Line_Separator" => "Zl",
        "Zp" | "Paragraph_Separator" => "Zp",
        "Z" | "Separator" => "Z",
        "Cc" | "Control" => "Cc",
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
        "ASCII_Hex_Digit" => "ASCII_Hex_Digit",
        "Alphabetic" => "Alphabetic",
        "Any" => "Any",
        "Assigned" => "Assigned",
        "ID_Continue" => "ID_Continue",
        "ID_Start" => "ID_Start",
        "Lowercase" => "Lowercase",
        "Uppercase" => "Uppercase",
        "White_Space" => "White_Space",
        "XID_Continue" => "XID_Continue",
        "XID_Start" => "XID_Start",
        _ => return None,
    })
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
    for sa in &a.strings {
        if b.strings.iter().any(|sb| sb == sa) {
            out.strings.push(sa.clone());
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
