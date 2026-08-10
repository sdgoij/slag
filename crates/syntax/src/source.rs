//! ECMAScript source text: UTF-16 code units plus line-start offsets for
//! position mapping (spec ch. 11).

use crux::{JsString, SourceLocation};

/// ECMAScript source text.
///
/// Regardless of the external encoding, the engine processes source text as a
/// sequence of UTF-16 code units (spec 11.1).
#[derive(Debug, Clone)]
pub struct SourceText {
    units: Vec<u16>,
    line_starts: Vec<u32>,
}

impl SourceText {
    pub fn from_utf8(text: &str) -> Self {
        Self::from_utf16(text.encode_utf16().collect())
    }

    pub fn from_utf16(units: Vec<u16>) -> Self {
        let mut line_starts = vec![0u32];
        let mut i = 0;
        while i < units.len() {
            match units[i] {
                // CR, optionally followed by LF, ends a line.
                0x000D => {
                    if units.get(i + 1) == Some(&0x000A) {
                        i += 1;
                    }
                    line_starts.push(i as u32 + 1);
                }
                0x000A | 0x2028 | 0x2029 => {
                    line_starts.push(i as u32 + 1);
                }
                _ => {}
            }
            i += 1;
        }
        Self { units, line_starts }
    }

    /// The number of UTF-16 code units.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn code_unit(&self, index: usize) -> Option<u16> {
        self.units.get(index).copied()
    }

    pub fn as_slice(&self) -> &[u16] {
        &self.units
    }

    /// The source text in the half-open unit range `[start, end)`.
    pub fn substring(&self, start: u32, end: u32) -> JsString {
        let start = (start as usize).min(self.units.len());
        let end = (end as usize).min(self.units.len());
        JsString::from_utf16(&self.units[start.min(end)..end.max(start)])
    }

    /// 1-based line and column for a code-unit offset (clamped to the end).
    pub fn line_column(&self, offset: u32) -> SourceLocation {
        let offset = (offset as usize).min(self.units.len()) as u32;
        // line_starts[0] = 0 always precedes `offset`.
        let line_idx = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let line = line_idx as u32 + 1;
        let column = offset - self.line_starts[line_idx] + 1;
        SourceLocation::new(line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_utf16_round_trip() {
        let s = SourceText::from_utf8("a\u{1F600}b");
        assert_eq!(s.len(), 4);
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(1), Some(0xD83D));
        assert_eq!(s.code_unit(3), Some(b'b' as u16));
        assert_eq!(s.substring(0, 2).as_slice(), &[b'a' as u16, 0xD83D]);
    }

    #[test]
    fn line_column_tracks_line_breaks() {
        // A line terminator ends its line; the next line starts after it.
        // The CRLF pair counts as a single break.
        let s = SourceText::from_utf8("ab\ncd\r\nef\rg\u{2028}h");
        assert_eq!(s.line_column(0), SourceLocation::new(1, 1));
        assert_eq!(s.line_column(1), SourceLocation::new(1, 2));
        assert_eq!(s.line_column(2), SourceLocation::new(1, 3)); // \n ends line 1
        assert_eq!(s.line_column(3), SourceLocation::new(2, 1)); // c starts line 2
        assert_eq!(s.line_column(5), SourceLocation::new(2, 3)); // \r of the pair
        assert_eq!(s.line_column(6), SourceLocation::new(2, 4)); // \n of the pair
        assert_eq!(s.line_column(7), SourceLocation::new(3, 1)); // e starts line 3
        assert_eq!(s.line_column(8), SourceLocation::new(3, 2));
        assert_eq!(s.line_column(9), SourceLocation::new(3, 3)); // lone \r ends line 3
        assert_eq!(s.line_column(10), SourceLocation::new(4, 1)); // g starts line 4
        assert_eq!(s.line_column(11), SourceLocation::new(4, 2)); // U+2028 ends line 4
        assert_eq!(s.line_column(12), SourceLocation::new(5, 1)); // h starts line 5
    }

    #[test]
    fn line_column_clamps_to_end() {
        let s = SourceText::from_utf8("hi");
        assert_eq!(s.line_column(100), SourceLocation::new(1, 3));
        assert_eq!(s.line_column(2), SourceLocation::new(1, 3));
    }
}
