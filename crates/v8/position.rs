//! The position a `v8::Message` answers from.
//!
//! V8 hangs it off the error object: `Isolate::CreateMessage` reads a start
//! position, an end position and the script back off the exception
//! (`ComputeLocationFromException`, `v8/src/execution/isolate.cc:3640`), and
//! the message answers its line and column from them. Slag's error objects
//! carry no such properties — the engine records a stack *string* per error and
//! no position — and the script's name is the bridge's to know, because a
//! `ScriptOrigin` never reaches the engine. So this is the same record at the
//! bridge's level, keyed the way the engine keys its own per-object tables: by
//! object identity.
//!
//! # Only a failed compile records one
//!
//! [`JsError::span`](crux::error::JsError::span) indexes whichever source the
//! failing code was parsed from, and on its own says nothing about *which*
//! source that was: an error raised while a script runs can carry a span into
//! any other script, and recording it against the running one would put a
//! wrong position on an error. At a compile the error necessarily came from
//! the text in hand, so that is the only place the record is honest. A runtime
//! error keeps answering "no position" — `get_line_number` is `None` — rather
//! than a position that may name another script.

use std::rc::Rc;

use crux::Span;
use runtime::api;

use crate::Isolate;
use crate::script::ScriptOrigin;

/// What a host told the engine about a script, of which a position needs the
/// name and the two offsets.
///
/// The name has to be text for this bridge to carry it: where the crate we
/// stand in for hands back whatever value the host gave, a record here holds an
/// `Rc<str>`, so a resource name that is not a string is one the messages made
/// from this script cannot answer.
#[derive(Debug, Clone, Default)]
pub(crate) struct Origin {
    pub name: Option<Rc<str>>,
    pub line_offset: i32,
    pub column_offset: i32,
}

impl Origin {
    /// The parts of a host's origin a position is built with.
    pub(crate) fn of(origin: Option<&ScriptOrigin<'_>>) -> Self {
        let Some(origin) = origin else {
            return Self::default();
        };
        Self {
            name: origin.resource_name.engine().as_string().map(Rc::from),
            line_offset: origin.resource_line_offset,
            column_offset: origin.resource_column_offset,
        }
    }
}

/// Where an error came from: the script's name when there was one, and the
/// position `v8::Message` reports — line 1-based, column 0-based.
#[derive(Debug, Clone)]
pub(crate) struct Position {
    pub name: Option<Rc<str>>,
    pub line: u32,
    pub column: u32,
    /// The text of the line the position is on, without its terminator — what
    /// `v8::Message::GetSourceLine` answers.
    pub line_text: Rc<str>,
}

impl Position {
    /// The position `span` names in `source`, with `origin`'s offsets applied.
    ///
    /// The offsets are V8's own (`Script::AddPositionInfoOffset`,
    /// `v8/src/objects/script.cc:324`): the column takes them only on the first
    /// line, because what they say is "this text is line N of a larger file",
    /// and the line takes them always. Both saturate rather than wrapping,
    /// which is the one place this differs from V8's `int` arithmetic — a host
    /// that passes offsets underflowing past the start of the file gets line 1,
    /// column 0 instead of a position in a file before this one.
    pub(crate) fn at(source: &str, span: Span, origin: &Origin) -> Self {
        let (line_in_script, column_in_script) = line_and_column(source, span.start);
        let line = i64::from(line_in_script) + i64::from(origin.line_offset);
        let mut column = i64::from(column_in_script);
        if line_in_script == 1 {
            column += i64::from(origin.column_offset);
        }
        Self {
            name: origin.name.clone(),
            line: line.clamp(1, i64::from(u32::MAX)) as u32,
            column: column.clamp(0, i64::from(u32::MAX)) as u32,
            line_text: line_text(source, span.start),
        }
    }
}

/// The text of the line `offset` names in `source`, without its line
/// terminator: `v8::Message::GetSourceLine`'s answer.
///
/// The offset is in UTF-16 code units, as the parser's spans are, so the walk
/// runs over code units like [`line_and_column`]'s — and the terminator's last
/// unit is where a line ends, so `\r\n` yields a line ending before the `\r`.
pub(crate) fn line_text(source: &str, offset: u32) -> Rc<str> {
    let units: Vec<u16> = source.encode_utf16().collect();
    let position = (offset as usize).min(units.len());
    let mut start = 0usize;
    for i in 0..position {
        if ends_line(&units, i) {
            start = i + 1;
        }
    }
    let mut end = position;
    while end < units.len() && !ends_line(&units, end) {
        end += 1;
    }
    Rc::from(String::from_utf16_lossy(&units[start..end]))
}

/// Record where the error `thrown` came from, for a `v8::Message` made from it
/// later.
///
/// `span` is the error's own, and an error whose kind carries none records
/// nothing.
pub(crate) fn record(
    isolate: &Isolate,
    thrown: &api::Local,
    source: &str,
    span: Option<Span>,
    origin: &Origin,
) {
    let Some(span) = span else {
        return;
    };
    isolate.record_position(thrown, Position::at(source, span, origin));
}

/// The line (1-based) and column (0-based) that `offset` names in `source`.
///
/// The offset is in UTF-16 code units, which is what the parser's spans are
/// (they index the source as code units, as `capture_source` reads them), so
/// the count runs over the source's code units and not its characters.
pub(crate) fn line_and_column(source: &str, offset: u32) -> (u32, u32) {
    let units: Vec<u16> = source.encode_utf16().collect();
    let position = (offset as usize).min(units.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    for i in 0..position {
        if ends_line(&units, i) {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, (position - line_start) as u32)
}

/// Whether the code unit at `i` is the last one of a LineTerminatorSequence
/// (spec 12.3): `\n`, `\u{2028}`, `\u{2029}`, or a `\r` that `\n` does not
/// follow.
///
/// The boundary sits on the sequence's *last* unit, which is what makes `\r\n`
/// end its line at the `\n`: V8 takes a position's column as the distance from
/// one past the previous line's end (`Script::PositionInfo`, with
/// `line_start = GetLineEnd(line - 1) + 1`), so the next line has to begin
/// after the whole sequence.
fn ends_line(units: &[u16], i: usize) -> bool {
    match units[i] {
        0x0A | 0x2028 | 0x2029 => true,
        0x0D => units.get(i + 1) != Some(&0x0A),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position counts lines from the terminators before it and columns from
    /// the start of the line it is on.
    #[test]
    fn a_position_counts_lines_and_columns() {
        let source = "ab\ncd";
        assert_eq!(line_and_column(source, 0), (1, 0));
        assert_eq!(line_and_column(source, 1), (1, 1));
        assert_eq!(line_and_column(source, 2), (1, 2));
        assert_eq!(line_and_column(source, 3), (2, 0));
        assert_eq!(line_and_column(source, 4), (2, 1));
    }

    /// Every line terminator ends a line, and `\r\n` is one terminator rather
    /// than two: the `\n` ends the line, so the next one starts after it.
    #[test]
    fn every_line_terminator_ends_a_line() {
        for source in ["a\u{2028}b", "a\u{2029}b"] {
            assert_eq!(line_and_column(source, 2), (2, 0), "in {source:?}");
            assert_eq!(line_and_column(source, 3), (2, 1), "in {source:?}");
        }
        // `\r\n` is one terminator, so the `\n` is still on the first line and
        // the line after starts past it.
        assert_eq!(line_and_column("a\r\nb", 2), (1, 2));
        assert_eq!(line_and_column("a\r\nb", 3), (2, 0));
        // A lone `\r` is a terminator of its own, so the index before it is the
        // line's last character and the character after it starts the next.
        assert_eq!(line_and_column("a\rb", 1), (1, 1));
        assert_eq!(line_and_column("a\rb", 2), (2, 0));
        // Two terminators in a row are two lines.
        assert_eq!(line_and_column("\n\nx", 2), (3, 0));
    }

    /// The count is over code units, not characters: a surrogate pair is two.
    #[test]
    fn a_surrogate_pair_counts_as_two_columns() {
        assert_eq!(line_and_column("\u{1F600}x", 2), (1, 2));
    }

    /// An offset past the end of the source names the end of its last line
    /// rather than panicking: the parser reports the end of input that way.
    #[test]
    fn an_offset_past_the_end_clamps() {
        assert_eq!(line_and_column("ab", 99), (1, 2));
        assert_eq!(line_and_column("ab\n", 99), (2, 0));
        assert_eq!(line_and_column("", 99), (1, 0));
    }

    /// The origin's offsets are added as V8 adds them: the line always, the
    /// column only on the script's first line.
    #[test]
    fn the_origin_offsets_apply_the_way_v8_applies_them() {
        let origin = Origin {
            name: Some(Rc::from("file.ts")),
            line_offset: 10,
            column_offset: 5,
        };
        let first_line = Position::at("ab\ncd", Span::new(1, 1), &origin);
        assert_eq!((first_line.line, first_line.column), (11, 6));
        let second_line = Position::at("ab\ncd", Span::new(4, 4), &origin);
        assert_eq!((second_line.line, second_line.column), (12, 1));
        assert_eq!(second_line.name.as_deref(), Some("file.ts"));

        // Offsets that would run before the start of the file saturate, so a
        // host never sees a line of zero or a negative column.
        let underflowing = Origin {
            name: None,
            line_offset: -10,
            column_offset: -10,
        };
        let position = Position::at("ab", Span::new(1, 1), &underflowing);
        assert_eq!((position.line, position.column), (1, 0));
    }
}
