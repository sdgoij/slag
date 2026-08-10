//! String, template, and regexp literal scanning (spec 12.9.5-12.9.7).

use crux::{JsError, JsString};
use syntax::TokenKind;
use unicode::is_line_terminator;

use crate::lexer::Lexer;

/// Where a template literal starts: at the opening backtick, or at a `}`
/// continuing an existing template (spec 12.9.6).
pub(crate) enum TemplateKind {
    Start,
    Continuation,
}

struct CookedEscape {
    units: Vec<u16>,
    legacy: bool,
}

impl Lexer<'_> {
    /// Lexes a string literal; the cursor is at the opening quote.
    pub(crate) fn lex_string(&mut self, quote: u16) -> Result<TokenKind, JsError> {
        let start = self.pos;
        self.pos += 1;
        let mut cooked: Vec<u16> = Vec::new();
        let mut legacy_octal = false;
        loop {
            let Some(u) = self.peek() else {
                return Err(self.error_at(start, "Unterminated string literal"));
            };
            match u {
                u if u == quote => {
                    self.pos += 1;
                    break;
                }
                // Only LF and CR terminate a string literal (spec 12.9.5:
                // <LS> and <PS> are ordinary DoubleStringCharacters).
                0x0A | 0x0D => {
                    return Err(self.error_at(start, "Unterminated string literal"));
                }
                0x5C => {
                    self.pos += 1;
                    if self.peek().is_some_and(|x| is_line_terminator(x as u32)) {
                        self.consume_line_terminator();
                        continue;
                    }
                    let esc = self.cook_string_escape(start)?;
                    legacy_octal |= esc.legacy;
                    cooked.extend(esc.units);
                }
                _ => {
                    cooked.push(u);
                    self.pos += 1;
                }
            }
        }
        Ok(TokenKind::StringLiteral {
            value: JsString::from_utf16(&cooked),
            legacy_octal,
        })
    }

    /// Lexes a template literal. `Start` begins at the opening backtick;
    /// `Continuation` begins at the `}` that follows a substitution
    /// expression. The terminator (`` ` `` or `${`) decides which token kind
    /// is produced.
    pub(crate) fn lex_template(&mut self, kind: TemplateKind) -> Result<TokenKind, JsError> {
        let start = self.pos;
        self.pos += 1; // consume '`' or '}'
        let mut raw: Vec<u16> = Vec::new();
        let mut cooked: Vec<u16> = Vec::new();
        let mut invalid = false;
        loop {
            let Some(u) = self.peek() else {
                return Err(self.error_at(start, "Unterminated template literal"));
            };
            match u {
                0x60 => {
                    self.pos += 1;
                    break;
                }
                0x24 if self.peek_n(1) == Some(0x7B) => {
                    self.pos += 2;
                    let cooked = if invalid {
                        None
                    } else {
                        Some(JsString::from_utf16(&cooked))
                    };
                    let raw = JsString::from_utf16(&raw);
                    return Ok(match kind {
                        TemplateKind::Start => TokenKind::TemplateHead { cooked, raw },
                        TemplateKind::Continuation => TokenKind::TemplateMiddle { cooked, raw },
                    });
                }
                0x5C => {
                    // Capture the span before consuming `\` so the raw value
                    // keeps it (spec 12.9.6.3: TRV of EscapeSequence starts
                    // with the REVERSE SOLIDUS).
                    let raw_start = self.pos;
                    self.pos += 1;
                    if self.peek().is_some_and(|x| is_line_terminator(x as u32)) {
                        // Line continuation: cooked is empty, raw keeps `\` plus
                        // the LineTerminatorSequence with CR/CRLF normalized to
                        // LF (spec 12.9.6.3: TRV of LineContinuation).
                        raw.push(0x5C);
                        let t = self.peek().unwrap();
                        raw.push(if t == 0x0D { 0x0A } else { t });
                        self.consume_line_terminator();
                        continue;
                    }
                    match self.cook_template_escape() {
                        Ok(esc) => cooked.extend(esc.units),
                        Err(()) => invalid = true,
                    }
                    raw.extend(self.source_units(raw_start, self.pos));
                }
                0x0D => {
                    raw.push(0x0A);
                    cooked.push(0x0A);
                    self.consume_line_terminator();
                }
                0x0A | 0x2028 | 0x2029 => {
                    raw.push(u);
                    cooked.push(u);
                    self.pos += 1;
                }
                _ => {
                    raw.push(u);
                    cooked.push(u);
                    self.pos += 1;
                }
            }
        }
        let cooked = if invalid {
            None
        } else {
            Some(JsString::from_utf16(&cooked))
        };
        let raw = JsString::from_utf16(&raw);
        Ok(match kind {
            TemplateKind::Start => TokenKind::NoSubstitutionTemplate { cooked, raw },
            TemplateKind::Continuation => TokenKind::TemplateTail { cooked, raw },
        })
    }

    /// Lexes a regexp literal; the cursor is at the opening `/`.
    pub(crate) fn lex_regexp(&mut self) -> Result<TokenKind, JsError> {
        let start = self.pos;
        self.pos += 1;
        let mut in_class = false;
        loop {
            let Some(u) = self.peek() else {
                return Err(self.error_at(start, "Unterminated regular expression"));
            };
            match u {
                0x0A | 0x0D | 0x2028 | 0x2029 => {
                    return Err(self.error_at(start, "Unterminated regular expression"));
                }
                0x5C => {
                    self.pos += 1;
                    if self.peek().is_none()
                        || self.peek().is_some_and(|x| is_line_terminator(x as u32))
                    {
                        return Err(self.error_at(start, "Unterminated regular expression"));
                    }
                    self.pos += 1;
                }
                0x5B => {
                    in_class = true;
                    self.pos += 1;
                }
                0x5D => {
                    in_class = false;
                    self.pos += 1;
                }
                0x2F if !in_class => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += 1,
            }
        }
        let pattern = self.source_units(start + 1, self.pos - 1);
        let flags_start = self.pos;
        while self
            .peek()
            .is_some_and(|u| unicode::is_identifier_part(u as u32))
        {
            self.pos += 1;
        }
        let flags = self.source_units(flags_start, self.pos);
        Ok(TokenKind::RegExpLiteral {
            pattern: JsString::from_utf16(&pattern),
            flags: JsString::from_utf16(&flags),
        })
    }

    /// Cooks a `\`-escape for a string literal; the cursor is past the
    /// backslash. Invalid escapes are hard errors.
    fn cook_string_escape(&mut self, start: usize) -> Result<CookedEscape, JsError> {
        let c = self
            .peek()
            .ok_or_else(|| self.error_at(start, "Invalid escape sequence"))?;
        self.pos += 1;
        match c {
            0x27 | 0x22 | 0x5C => Ok(CookedEscape {
                units: vec![c],
                legacy: false,
            }),
            0x62 => Ok(single(0x08)),
            0x66 => Ok(single(0x0C)),
            0x6E => Ok(single(0x0A)),
            0x72 => Ok(single(0x0D)),
            0x74 => Ok(single(0x09)),
            0x76 => Ok(single(0x0B)),
            0x30..=0x39 => {
                if c == 0x30 {
                    if matches!(self.peek(), Some(0x38 | 0x39)) {
                        // Annex B: `\0` followed by 8 or 9 is NUL; the digit
                        // continues as a plain character.
                        return Ok(CookedEscape {
                            units: vec![0],
                            legacy: true,
                        });
                    }
                    if !self.peek().is_some_and(|u| (0x30..=0x37).contains(&u)) {
                        // `0` not followed by a decimal digit.
                        return Ok(CookedEscape {
                            units: vec![0],
                            legacy: false,
                        });
                    }
                } else if c >= 0x38 {
                    // Annex B: `\8` and `\9` are NonOctalDecimalEscapeSequence;
                    // the SV is the digit itself and following digits are plain
                    // characters.
                    return Ok(CookedEscape {
                        units: vec![c],
                        legacy: true,
                    });
                }
                // Legacy octal (Annex B): 0-3 first digit allows two more
                // octal digits; 4-7 allows one more.
                let mut value = (c - 0x30) as u32;
                let max_extra = if c <= 0x33 { 2 } else { 1 };
                let mut count = 0;
                while count < max_extra {
                    let Some(u) = self.peek() else { break };
                    if !(0x30..=0x37).contains(&u) {
                        break;
                    }
                    value = value * 8 + (u - 0x30) as u32;
                    self.pos += 1;
                    count += 1;
                }
                Ok(CookedEscape {
                    units: vec![value as u16],
                    legacy: true,
                })
            }
            0x78 => {
                let hi = self
                    .take_hex_digit()
                    .ok_or_else(|| self.error_at(start, "Invalid hexadecimal escape"))?;
                let lo = self
                    .take_hex_digit()
                    .ok_or_else(|| self.error_at(start, "Invalid hexadecimal escape"))?;
                Ok(single((hi << 4) | lo))
            }
            0x75 => self
                .cook_unicode_escape()
                .map_err(|()| self.error_at(start, "Invalid Unicode escape")),
            _ => Ok(CookedEscape {
                units: vec![c],
                legacy: false,
            }),
        }
    }

    /// Cooks a `\`-escape for a template literal; the cursor is past the
    /// backslash. Invalid escapes (NotEscapeSequence) yield Err so the cooked
    /// value becomes undefined.
    fn cook_template_escape(&mut self) -> Result<CookedEscape, ()> {
        let c = self.peek().ok_or(())?;
        self.pos += 1;
        match c {
            0x27 | 0x22 | 0x5C => Ok(CookedEscape {
                units: vec![c],
                legacy: false,
            }),
            0x62 => Ok(single(0x08)),
            0x66 => Ok(single(0x0C)),
            0x6E => Ok(single(0x0A)),
            0x72 => Ok(single(0x0D)),
            0x74 => Ok(single(0x09)),
            0x76 => Ok(single(0x0B)),
            0x30 => {
                // `0` [lookahead ∉ DecimalDigit]
                if self.peek().is_some_and(|u| (0x30..=0x39).contains(&u)) {
                    return Err(());
                }
                Ok(CookedEscape {
                    units: vec![0],
                    legacy: false,
                })
            }
            0x31..=0x39 => Err(()), // NotEscapeSequence: DecimalDigit but not 0
            0x78 => {
                let hi = self.take_hex_digit().ok_or(())?;
                let lo = self.take_hex_digit().ok_or(())?;
                Ok(single((hi << 4) | lo))
            }
            0x75 => self.cook_unicode_escape(),
            _ => Ok(CookedEscape {
                units: vec![c],
                legacy: false,
            }),
        }
    }

    /// Cooks `\uXXXX` or `\u{...}`; the cursor is past the `u`.
    fn cook_unicode_escape(&mut self) -> Result<CookedEscape, ()> {
        if self.peek() == Some(0x7B) {
            self.pos += 1;
            let mut value: u32 = 0;
            let mut digits = 0;
            loop {
                match self.peek() {
                    Some(u) if is_hex_digit(u) => {
                        value = value * 16 + hex_value(u);
                        self.pos += 1;
                        digits += 1;
                        if digits > 6 {
                            return Err(());
                        }
                    }
                    Some(0x7D) => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(()),
                }
            }
            if digits == 0 || value > 0x10FFFF {
                return Err(());
            }
            Ok(CookedEscape {
                units: utf16_encode(value),
                legacy: false,
            })
        } else {
            let mut value: u32 = 0;
            for _ in 0..4 {
                let u = self.peek().ok_or(())?;
                if !is_hex_digit(u) {
                    return Err(());
                }
                value = value * 16 + hex_value(u);
                self.pos += 1;
            }
            Ok(CookedEscape {
                units: vec![value as u16],
                legacy: false,
            })
        }
    }

    fn take_hex_digit(&mut self) -> Option<u16> {
        let u = self.peek()?;
        if !is_hex_digit(u) {
            return None;
        }
        self.pos += 1;
        Some(hex_value(u) as u16)
    }
}

fn single(unit: u16) -> CookedEscape {
    CookedEscape {
        units: vec![unit],
        legacy: false,
    }
}

fn is_hex_digit(u: u16) -> bool {
    (0x30..=0x39).contains(&u) || (0x61..=0x66).contains(&u) || (0x41..=0x46).contains(&u)
}

fn hex_value(u: u16) -> u32 {
    match u {
        0x30..=0x39 => (u - 0x30) as u32,
        0x61..=0x66 => (u - 0x61 + 10) as u32,
        0x41..=0x46 => (u - 0x41 + 10) as u32,
        _ => unreachable!("validated by is_hex_digit"),
    }
}

fn utf16_encode(cp: u32) -> Vec<u16> {
    if cp <= 0xFFFF {
        vec![cp as u16]
    } else {
        let x = cp - 0x10000;
        vec![0xD800 + (x >> 10) as u16, 0xDC00 + (x & 0x3FF) as u16]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::LexGoal;

    fn token(source: &str, goal: LexGoal) -> TokenKind {
        let src = syntax::SourceText::from_utf8(source);
        let mut lexer = Lexer::new(&src, goal, false);
        lexer.next_token().expect("lexing failed").kind
    }

    fn is_error(source: &str, goal: LexGoal) -> bool {
        let src = syntax::SourceText::from_utf8(source);
        let mut lexer = Lexer::new(&src, goal, false);
        lexer.next_token().is_err()
    }

    #[test]
    fn string_literal_cooks_basic_escapes() {
        let t = token(r#""a\n\t\r\b\f\v\"\'\\b""#, LexGoal::Div);
        let TokenKind::StringLiteral {
            value,
            legacy_octal,
        } = t
        else {
            panic!("expected string literal")
        };
        assert!(!legacy_octal);
        assert_eq!(
            value.as_slice(),
            &[
                0x61, 0x0A, 0x09, 0x0D, 0x08, 0x0C, 0x0B, 0x22, 0x27, 0x5C, 0x62
            ]
        );
    }

    #[test]
    fn string_literal_hex_and_unicode_escapes() {
        let t = token(r#""\x41\u0042\u{1F600}""#, LexGoal::Div);
        let TokenKind::StringLiteral {
            value,
            legacy_octal,
        } = t
        else {
            panic!("expected string literal")
        };
        assert!(!legacy_octal);
        assert_eq!(value.as_slice(), &[0x41, 0x42, 0xD83D, 0xDE00]);
    }

    #[test]
    fn string_literal_line_continuation_cooks_empty() {
        // "a\<LF>b" and "a\<CRLF>b" and "a\<LS>b" all cook to "ab".
        for src in ["\"a\\\nb\"", "\"a\\\r\nb\"", "\"a\\\u{2028}b\""] {
            let t = token(src, LexGoal::Div);
            let TokenKind::StringLiteral {
                value,
                legacy_octal,
            } = t
            else {
                panic!("expected string literal")
            };
            assert!(!legacy_octal);
            assert_eq!(value.as_slice(), &[0x61, 0x62]);
        }
    }

    #[test]
    fn string_literal_allows_ls_and_ps_raw() {
        // <LS> and <PS> are ordinary string characters (spec 12.9.5);
        // only LF and CR are forbidden.
        let t = token("\"a\u{2028}b\u{2029}c\"", LexGoal::Div);
        let TokenKind::StringLiteral { value, .. } = t else {
            panic!("expected string literal")
        };
        assert_eq!(value.as_slice(), &[0x61, 0x2028, 0x62, 0x2029, 0x63]);
    }

    #[test]
    fn string_literal_legacy_octal_and_non_octal_escapes() {
        let cases: &[(&str, &[u16], bool)] = &[
            (r#""\1""#, &[1], true),
            (r#""\12""#, &[10], true),
            (r#""\123""#, &[0x53], true), // 'S'
            (r#""\4""#, &[4], true),
            (r#""\47""#, &[0x27], true), // "'"
            (r#""\0""#, &[0], false),    // \0 alone is not legacy
            (r#""\08""#, &[0, 0x38], true),
            (r#""\09""#, &[0, 0x39], true),
            // \8/\9 are NonOctalDecimalEscapeSequence: the digit only, no
            // following digits consumed.
            (r#""\8""#, &[0x38], true),
            (r#""\9""#, &[0x39], true),
            (r#""\81""#, &[0x38, 0x31], true),
            (r#""\90""#, &[0x39, 0x30], true),
        ];
        for (src, expected, legacy) in cases {
            let t = token(src, LexGoal::Div);
            let TokenKind::StringLiteral {
                value,
                legacy_octal,
            } = t
            else {
                panic!("expected string literal for {src}")
            };
            assert_eq!(value.as_slice(), *expected, "wrong value for {src}");
            assert_eq!(legacy_octal, *legacy, "wrong legacy flag for {src}");
        }
    }

    #[test]
    fn string_literal_errors() {
        for bad in [
            "\"abc",     // unterminated
            "'abc",      // unterminated
            "\"abc\\",   // backslash at EOF
            "\"ab\nc\"", // LF inside string
            "\"ab\rc\"", // CR inside string
            "\"\\x4\"",  // short hex escape
            "\"\\u00\"", // short unicode escape
        ] {
            assert!(is_error(bad, LexGoal::Div), "expected error for {bad:?}");
        }
    }

    #[test]
    fn template_no_substitution_cooked_and_raw() {
        let t = token("`a\\n`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x0A]);
        assert_eq!(raw.as_slice(), &[0x61, 0x5C, 0x6E]);
    }

    #[test]
    fn template_head_middle_and_tail_by_goal() {
        // Opening backtick: `${` ends the head, `` ` `` ends the template.
        let t = token("`a${", LexGoal::Div);
        let TokenKind::TemplateHead { cooked, raw } = t else {
            panic!("expected template head")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61]);
        assert_eq!(raw.as_slice(), &[0x61]);

        // Continuation goals: `}` plus ` and `${`.
        let t = token("}x`", LexGoal::TemplateTail);
        let TokenKind::TemplateTail { cooked, raw } = t else {
            panic!("expected template tail")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x78]);
        assert_eq!(raw.as_slice(), &[0x78]);

        let t = token("}x${", LexGoal::TemplateTail);
        let TokenKind::TemplateMiddle { cooked, raw } = t else {
            panic!("expected template middle")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x78]);
        assert_eq!(raw.as_slice(), &[0x78]);

        let t = token("}`", LexGoal::RegExpOrTemplateTail);
        let TokenKind::TemplateTail { cooked, raw } = t else {
            panic!("expected template tail")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[]);
        assert_eq!(raw.as_slice(), &[]);

        // Outside a continuation goal a `}` is a punctuator.
        assert_eq!(token("}", LexGoal::Div), TokenKind::RightBrace);
    }

    #[test]
    fn template_not_escape_sequence_yields_undefined_cooked() {
        // `\8` is a NotEscapeSequence: cooked is undefined, raw keeps the text.
        let t = token("`\\8`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked, None);
        assert_eq!(raw.as_slice(), &[0x5C, 0x38]);

        let t = token("`a\\8b`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, .. } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked, None);
    }

    #[test]
    fn template_line_terminators_normalize_to_lf() {
        // CR and CRLF cook and raw-normalize to LF.
        for src in ["`a\rb`", "`a\r\nb`"] {
            let t = token(src, LexGoal::Div);
            let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
                panic!("expected template")
            };
            assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x0A, 0x62]);
            assert_eq!(raw.as_slice(), &[0x61, 0x0A, 0x62]);
        }
        // LS and PS stay as themselves.
        let t = token("`a\u{2028}b`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x2028, 0x62]);
        assert_eq!(raw.as_slice(), &[0x61, 0x2028, 0x62]);
    }

    #[test]
    fn template_line_continuation_cooks_empty_and_keeps_raw() {
        // Cooked drops the continuation; raw keeps `\` + the normalized
        // terminator (LF for LF/CR/CRLF, LS/PS for themselves).
        let t = token("`a\\\nb`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x62]);
        assert_eq!(raw.as_slice(), &[0x61, 0x5C, 0x0A, 0x62]);

        let t = token("`a\\\r\nb`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x62]);
        assert_eq!(raw.as_slice(), &[0x61, 0x5C, 0x0A, 0x62]);

        let t = token("`a\\\u{2028}b`", LexGoal::Div);
        let TokenKind::NoSubstitutionTemplate { cooked, raw } = t else {
            panic!("expected template")
        };
        assert_eq!(cooked.unwrap().as_slice(), &[0x61, 0x62]);
        assert_eq!(raw.as_slice(), &[0x61, 0x5C, 0x2028, 0x62]);
    }

    #[test]
    fn template_errors() {
        // Unterminated at EOF, and a backslash at EOF.
        assert!(is_error("`abc", LexGoal::Div));
        assert!(is_error("`abc\\", LexGoal::Div));
    }

    #[test]
    fn regexp_literal_pattern_and_flags() {
        let t = token(r"/ab+c/gi", LexGoal::RegExp);
        let TokenKind::RegExpLiteral { pattern, flags } = t else {
            panic!("expected regexp literal")
        };
        assert_eq!(pattern.as_slice(), &[0x61, 0x62, 0x2B, 0x63]);
        assert_eq!(flags.as_slice(), &[0x67, 0x69]);
    }

    #[test]
    fn regexp_literal_class_and_escaped_slash() {
        // `/` inside a character class does not end the literal.
        let t = token(r"/[/]/", LexGoal::RegExp);
        let TokenKind::RegExpLiteral { pattern, flags } = t else {
            panic!("expected regexp literal")
        };
        assert_eq!(pattern.as_slice(), &[0x5B, 0x2F, 0x5D]);
        assert_eq!(flags.as_slice(), &[]);

        // Escaped `/` outside a class is also fine.
        let t = token(r"/a\/b/", LexGoal::RegExp);
        let TokenKind::RegExpLiteral { pattern, flags } = t else {
            panic!("expected regexp literal")
        };
        assert_eq!(pattern.as_slice(), &[0x61, 0x5C, 0x2F, 0x62]);
        assert_eq!(flags.as_slice(), &[]);
    }

    #[test]
    fn regexp_literal_errors() {
        for bad in ["/abc", "/a\nb/", "/[a\nb]/", "/a\\"] {
            assert!(is_error(bad, LexGoal::RegExp), "expected error for {bad:?}");
        }
    }
}
