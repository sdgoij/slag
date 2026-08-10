//! Numeric literal scanning (spec 12.9.3).

use crux::{BigInt, JsError};
use syntax::{NumericLiteral, TokenKind};

use crate::lexer::Lexer;

impl Lexer<'_> {
    /// Lexes a numeric literal. The cursor is at a digit, or at `.` followed
    /// by a digit.
    pub(crate) fn lex_numeric(&mut self) -> Result<TokenKind, JsError> {
        let start = self.pos;
        if self.peek() == Some(b'.' as u16) {
            return self.lex_decimal(start);
        }
        if self.peek() == Some(b'0' as u16) {
            match self.peek_n(1) {
                Some(0x78) | Some(0x58) => return self.lex_non_decimal(start, 16),
                Some(0x6F) | Some(0x4F) => return self.lex_non_decimal(start, 8),
                Some(0x62) | Some(0x42) => return self.lex_non_decimal(start, 2),
                _ => {}
            }
        }
        self.lex_decimal(start)
    }

    /// Consumes digits of `radix` with optional `_` separators strictly
    /// between digits; returns the cleaned digit characters.
    fn scan_digits(&mut self, radix: u32) -> Result<Vec<u16>, JsError> {
        let mut out = Vec::new();
        let mut expect_digit = true;
        loop {
            match self.peek() {
                Some(u) if digit_value(u, radix).is_some() => {
                    out.push(u);
                    self.pos += 1;
                    expect_digit = false;
                }
                Some(u) if u == b'_' as u16 => {
                    if expect_digit {
                        return Err(self.error_here("Invalid numeric separator"));
                    }
                    self.pos += 1;
                    expect_digit = true;
                }
                _ => break,
            }
        }
        if expect_digit && !out.is_empty() {
            return Err(self.error_here("Invalid numeric separator"));
        }
        Ok(out)
    }

    fn lex_decimal(&mut self, start: usize) -> Result<TokenKind, JsError> {
        let int_digits = self.scan_digits(10)?;
        let mut has_point = false;
        let mut frac_digits: Vec<u16> = Vec::new();
        if self.peek() == Some(b'.' as u16) {
            has_point = true;
            self.pos += 1;
            frac_digits = self.scan_digits(10)?;
        }
        let mut has_exponent = false;
        let mut exponent: Vec<u16> = Vec::new();
        if matches!(self.peek(), Some(0x65) | Some(0x45)) {
            has_exponent = true;
            let exp_start = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(0x2B) | Some(0x2D)) {
                exponent.push(self.peek().unwrap());
                self.pos += 1;
            }
            let digits = self.scan_digits(10)?;
            if digits.is_empty() {
                return Err(self.error_at(exp_start, "Invalid exponent"));
            }
            exponent.extend(digits);
        }

        let leading_zero = int_digits.first() == Some(&0x30) && int_digits.len() > 1;
        if leading_zero && (has_point || has_exponent) {
            // StrictlyDecimalLiteral: leading-zero integers cannot carry a
            // fractional part or exponent (spec 12.9.3 early errors).
            return Err(self.error_at(start, "Unexpected number"));
        }

        // BigInt suffix applies only to plain integer forms.
        if self.peek() == Some(b'n' as u16) && !leading_zero && !has_point && !has_exponent {
            self.pos += 1;
            let text = units_to_string(&int_digits);
            let value = BigInt::parse_str(&text, 10)
                .ok_or_else(|| self.error_at(start, "Invalid BigInt literal"))?;
            return Ok(TokenKind::NumericLiteral(NumericLiteral::BigInt(value)));
        }

        let value = if leading_zero && int_digits.iter().all(|d| (0x30..=0x37).contains(d)) {
            // LegacyOctalIntegerLiteral (sloppy mode only; Annex B).
            let text = units_to_string(&int_digits);
            let bigint = BigInt::parse_str(&text, 8)
                .ok_or_else(|| self.error_at(start, "Invalid number"))?;
            bigint.to_f64()
        } else {
            let mut text = String::new();
            text.push_str(&units_to_string(&int_digits));
            if has_point {
                text.push('.');
                text.push_str(&units_to_string(&frac_digits));
            }
            if has_exponent {
                text.push('e');
                text.push_str(&units_to_string(&exponent));
            }
            text.parse::<f64>()
                .map_err(|_| self.error_at(start, "Invalid number"))?
        };
        Ok(TokenKind::NumericLiteral(NumericLiteral::Number(value)))
    }

    fn lex_non_decimal(&mut self, start: usize, radix: u32) -> Result<TokenKind, JsError> {
        self.pos += 2; // skip the 0x/0o/0b prefix
        let digits = self.scan_digits(radix)?;
        if digits.is_empty() {
            return Err(self.error_at(start, "Invalid numeric literal"));
        }
        let text = units_to_string(&digits);
        let value = BigInt::parse_str(&text, radix)
            .ok_or_else(|| self.error_at(start, "Invalid numeric literal"))?;
        if self.peek() == Some(b'n' as u16) {
            self.pos += 1;
            return Ok(TokenKind::NumericLiteral(NumericLiteral::BigInt(value)));
        }
        Ok(TokenKind::NumericLiteral(NumericLiteral::Number(
            value.to_f64(),
        )))
    }
}

fn digit_value(u: u16, radix: u32) -> Option<u32> {
    let d = match u {
        0x30..=0x39 => (u - 0x30) as u32,
        0x61..=0x7A => (u - 0x61 + 10) as u32,
        0x41..=0x5A => (u - 0x41 + 10) as u32,
        _ => return None,
    };
    (d < radix).then_some(d)
}

fn units_to_string(units: &[u16]) -> String {
    units.iter().map(|&u| u as u8 as char).collect()
}
