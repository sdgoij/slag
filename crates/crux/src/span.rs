//! Source locations and spans (spec ch. 5 conventions).

/// A 1-based source location: line and column in the original source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    pub line: u32,
    pub column: u32,
}

impl SourceLocation {
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// A half-open range of byte offsets into the original source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub const fn empty(at: u32) -> Self {
        Self { start: at, end: at }
    }

    pub fn contains(&self, offset: u32) -> bool {
        self.start <= offset && offset < self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_new_orders_offsets() {
        let span = Span::new(10, 20);
        assert_eq!((span.start, span.end), (10, 20));
        assert_eq!(span, Span::new(10, 20));
        assert_ne!(span, Span::new(10, 21));
    }

    #[test]
    fn span_contains_is_half_open() {
        let span = Span::new(10, 20);
        assert!(span.contains(10));
        assert!(span.contains(19));
        assert!(!span.contains(20));
        assert!(!span.contains(9));
    }

    #[test]
    fn span_empty_has_zero_width() {
        let at = 7;
        let span = Span::empty(at);
        assert_eq!(span, Span::new(at, at));
        assert!(!span.contains(at));
    }

    #[test]
    fn source_location_is_1_based_and_equatable() {
        let loc = SourceLocation::new(1, 1);
        assert_eq!((loc.line, loc.column), (1, 1));
        assert_eq!(loc, SourceLocation::new(1, 1));
        assert_ne!(loc, SourceLocation::new(1, 2));
    }
}
