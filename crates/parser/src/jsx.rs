//! JSX element parsing, gated behind `Parser.jsx` (default off, so spec
//! parsing is untouched).
//!
//! Each element desugars at parse time into an `rlx.h(type, props,
//! ...children)` call expression, so the existing expression/bytecode
//! pipeline needs no new IR.
//!
//! Supported today:
//!   - element names: single identifiers (`<div>`), dashed/namespaced names
//!     (`<my-element>`, `<svg:path>` — intrinsic string types), and dotted
//!     member paths (`<Form.Field>`). A single capitalized identifier
//!     desugars to an identifier reference (a component); everything else to
//!     a string literal (an intrinsic).
//!   - attributes: `name` (true), `name="string"`, `name={expr}`, dashed
//!     names (`data-id`), and spread attributes (`{...props}`).
//!   - children: JSXText, nested elements, `{expr}` containers, and
//!     fragments (`<>...</>`, desugared to an array).
//!
//! JSXText is scanned from the raw source between constructs (never
//! tokenized), so text that would lex as comments, strings, or operators is
//! ordinary text; the cleaned text follows Babel's JSX whitespace rules
//! (indented blank lines collapse, in-line single spaces survive). After a
//! text run the lexer is repositioned (`TokenStream::seek`) at the next
//! `<`/`{` boundary. Name separators (`.`, `-`, `:`) are only treated as
//! name characters when they abut the preceding identifier (no whitespace),
//! matching the JSX grammar.

use crux::{AtomId, JsError, JsString, Span, intern_utf8};
use syntax::{
    Argument, ArrayElement, ArrayLiteral, CallExpr, Expr, ExprKind, LexGoal, Literal, MemberExpr,
    MemberProperty, ObjectLiteral, ObjectProperty, PropertyName, TokenKind,
};

use crate::expr::parse_assignment;
use crate::parser::Parser;

/// A parsed JSX name: its segments (for closing-tag matching and member
/// paths), whether it was a dotted member path, and its full raw text
/// (including any `-`/`:` separators).
struct JsxName {
    /// One interned identifier per segment (`a.b-c` → `[a, b, c]`).
    parts: Vec<AtomId>,
    /// True when `.` joined the segments: desugars to a member expression.
    is_member: bool,
    /// The full name text, separators included.
    text: Vec<u16>,
}

/// Parse one JSX element or fragment (the caller has confirmed `jsx` is
/// enabled and the current token is `<`).
pub(crate) fn parse_element(parser: &mut Parser<'_>) -> Result<Expr, JsError> {
    let start = parser.peek()?.span.start;
    if next_unit_is(parser, b'>')? {
        // A fragment: `<>…</>` desugars to an array literal (render and `h`
        // already flatten arrays).
        parser.next()?; // `<`
        parser.next()?; // `>`
        let mut children: Vec<Expr> = Vec::new();
        let end = parse_children(parser, &[], &mut children)?;
        return Ok(fragment_expr(children, start, end));
    }
    parser.next()?; // `<`
    let name = read_name(parser)?;
    let (props, self_closing, open_end) = parse_attributes(parser)?;

    let mut children: Vec<Expr> = Vec::new();
    let end = if self_closing {
        open_end
    } else {
        parse_children(parser, &name.parts, &mut children)?
    };

    let tag = element_tag(&name, start, end);
    let props_expr = Expr {
        span: Span::new(start, end),
        kind: ExprKind::Object(ObjectLiteral {
            props,
            rest_trailing_comma: false,
            span: Span::new(start, end),
        }),
    };
    let mut args = vec![Argument::Expr(tag), Argument::Expr(props_expr)];
    for child in children {
        args.push(Argument::Expr(child));
    }
    let callee = member_path(
        ident_expr(intern_utf8("rlx"), start, end),
        &[intern_utf8("h")],
        end,
    );
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Call(CallExpr {
            callee: Box::new(callee),
            args,
            optional: false,
            span: Span::new(start, end),
        }),
    })
}

/// Whether the code unit right after the peeked token is `want`. The token
/// itself is not consumed; used to tell `</` (close) and `<>` (fragment)
/// from element names without lexing the following `/` under the RegExp
/// goal.
fn next_unit_is(parser: &mut Parser<'_>, want: u8) -> Result<bool, JsError> {
    let tok = parser.peek()?.clone();
    let next = parser.source_slice(Span::new(tok.span.end, tok.span.end + 1));
    Ok(next.first() == Some(&(want as u16)))
}

/// One identifier segment of a JSX name: keywords are fine in JSX name
/// positions (they are plain Identifier tokens with a keyword
/// classification). Returns the interned name and its raw source text.
fn read_name_part(parser: &mut Parser<'_>) -> Result<(AtomId, Vec<u16>), JsError> {
    let tok = parser.peek()?.clone();
    let TokenKind::Identifier(atom) = tok.kind else {
        return Err(parser.unexpected(&tok));
    };
    parser.next()?;
    let units = parser.source_slice(tok.span);
    Ok((atom, units))
}

/// Whether `parser.prev` (the last consumed token) ends exactly where the
/// peeked token starts: no whitespace between them. JSX name separators must
/// abut the segments they join.
fn abuts_prev(parser: &mut Parser<'_>) -> Result<bool, JsError> {
    let start = parser.peek()?.span.start;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(start == end)
}

/// Reads a JSX name: `name`, `name-name`, `name:name`, or `a.b.c`.
fn read_name(parser: &mut Parser<'_>) -> Result<JsxName, JsError> {
    let first_start = parser.peek()?.span.start;
    let (first, _) = read_name_part(parser)?;
    let mut parts = vec![first];
    let mut is_member = false;
    loop {
        let separator = if abuts_prev(parser)? && parser.at_punct(TokenKind::Dot)?
            || abuts_prev(parser)? && parser.at_punct(TokenKind::Minus)?
            || abuts_prev(parser)? && parser.at_punct(TokenKind::Colon)?
        {
            let tok = parser.peek()?.clone();
            parser.next()?;
            Some(tok.kind)
        } else {
            None
        };
        let Some(kind) = separator else { break };
        if kind == TokenKind::Dot {
            is_member = true;
        }
        let (part, _) = read_name_part(parser)?;
        parts.push(part);
    }
    let last_end = parser.prev.as_ref().unwrap().span.end;
    let text = parser.source_slice(Span::new(first_start, last_end));
    Ok(JsxName {
        parts,
        is_member,
        text,
    })
}

/// Reads the attribute list, ending at `>` (children follow) or `/>`
/// (self-closing). Returns the properties, whether the element is
/// self-closing, and the offset just past the `>`.
fn parse_attributes(parser: &mut Parser<'_>) -> Result<(Vec<ObjectProperty>, bool, u32), JsError> {
    let mut props: Vec<ObjectProperty> = Vec::new();
    loop {
        if parser.at_punct(TokenKind::GreaterThan)? {
            let gt = parser.peek()?.clone();
            parser.next()?;
            return Ok((props, false, gt.span.end));
        }
        if parser.at_punct(TokenKind::Slash)? {
            parser.next()?; // `/`
            let gt = parser.peek()?.clone();
            parser.expect_punct(TokenKind::GreaterThan)?;
            return Ok((props, true, gt.span.end));
        }
        if parser.at_punct(TokenKind::LeftBrace)? {
            // Spread attribute: `{...expr}`.
            parser.next()?; // `{`
            if !parser.eat_punct(TokenKind::Ellipsis)? {
                let tok = parser.peek()?.clone();
                return Err(parser.unexpected(&tok));
            }
            let expr = parse_assignment(parser, true)?;
            parser.expect_punct(TokenKind::RightBrace)?;
            props.push(ObjectProperty::Spread(expr));
            continue;
        }
        let name_tok = parser.peek()?.clone();
        let TokenKind::Identifier(atom) = name_tok.kind else {
            return Err(parser.unexpected(&name_tok));
        };
        let name = read_name(parser)?;
        let key = name_key(&name, atom);
        let value = if parser.eat_punct(TokenKind::Equal)? {
            if matches!(parser.peek()?.kind, TokenKind::StringLiteral { .. }) {
                let tok = parser.peek()?.clone();
                parser.next()?;
                let TokenKind::StringLiteral { value, .. } = tok.kind else {
                    unreachable!("peeked a string literal")
                };
                Expr {
                    span: tok.span,
                    kind: ExprKind::Literal(Literal::Str(value)),
                }
            } else if parser.at_punct(TokenKind::LeftBrace)? {
                parser.next()?; // `{`
                let expr = parse_assignment(parser, true)?;
                parser.expect_punct(TokenKind::RightBrace)?;
                expr
            } else {
                let tok = parser.peek()?.clone();
                return Err(parser.unexpected(&tok));
            }
        } else {
            // A bare attribute name is shorthand for `true`.
            Expr {
                span: Span::new(name_tok.span.start, name_tok.span.start),
                kind: ExprKind::Literal(Literal::Boolean(true)),
            }
        };
        props.push(ObjectProperty::Init {
            key,
            value,
            shorthand: false,
        });
    }
}

/// The property key for an attribute: an identifier for plain names, a
/// string for dashed/namespaced ones.
fn name_key(name: &JsxName, single_atom: AtomId) -> PropertyName {
    if name.parts.len() == 1 && !name.is_member {
        PropertyName::Ident(single_atom)
    } else {
        PropertyName::Str(JsString::from_utf16(&name.text))
    }
}

/// After an open tag's `>`, parses children until the matching close tag.
/// Each gap between constructs is scanned as raw JSXText from the source
/// (never tokenized), then the lexer is repositioned at the `<`/`{` that
/// ends it. Returns the offset past the close tag's `>`.
fn parse_children(
    parser: &mut Parser<'_>,
    open_parts: &[AtomId],
    out: &mut Vec<Expr>,
) -> Result<u32, JsError> {
    loop {
        let pos = parser.prev.as_ref().unwrap().span.end as usize;
        let tail = parser.source_slice(Span::new(pos as u32, parser.source.len() as u32));
        // The first `<` or `{` ends the text run (raw text may contain
        // anything else, including comment-like and string-like characters).
        let mut text_end: Option<usize> = None;
        for (offset, &unit) in tail.iter().enumerate() {
            if unit == b'<' as u16 || unit == b'{' as u16 {
                text_end = Some(pos + offset);
                break;
            }
        }
        match text_end {
            None => {
                // Ran out of source before a close tag could appear.
                return Err(parser.error_at(pos as u32, "Unterminated JSX element"));
            }
            Some(end) => {
                let cleaned = clean_text(&tail[..end - pos]);
                if !cleaned.is_empty() {
                    out.push(Expr {
                        span: Span::new(pos as u32, end as u32),
                        kind: ExprKind::Literal(Literal::Str(JsString::from_utf16(&cleaned))),
                    });
                }
                let kind = tail[end - pos];
                let after = tail.get(end - pos + 1).copied();
                if kind == b'{' as u16 {
                    parser.stream.seek(end);
                    parser.next()?; // `{`
                    let expr = parse_assignment(parser, true)?;
                    parser.expect_punct(TokenKind::RightBrace)?;
                    out.push(expr);
                } else if after == Some(b'/' as u16) {
                    parser.stream.seek(end);
                    return parse_close_tag(parser, open_parts);
                } else if after == Some(b'>' as u16) {
                    // A nested fragment: `<>…</>`.
                    parser.stream.seek(end);
                    let start = parser.next()?.span.start; // `<`
                    parser.next()?; // `>`
                    let mut fragment: Vec<Expr> = Vec::new();
                    let fragment_end = parse_children(parser, &[], &mut fragment)?;
                    out.push(fragment_expr(fragment, start, fragment_end));
                } else {
                    parser.stream.seek(end);
                    out.push(parse_element(parser)?);
                }
            }
        }
    }
}

/// `</name>` (or `</>` for a fragment) — validates the name against the open
/// tag and consumes the `>`.
fn parse_close_tag(parser: &mut Parser<'_>, open_parts: &[AtomId]) -> Result<u32, JsError> {
    parser.next()?; // `<`
    let slash = parser.next_with_goal(LexGoal::Div)?;
    if slash.kind != TokenKind::Slash {
        return Err(parser.unexpected(&slash));
    }
    if open_parts.is_empty() {
        // Fragment close: `</>` carries no name to validate.
        let gt = parser.peek()?.clone();
        parser.expect_punct(TokenKind::GreaterThan)?;
        return Ok(gt.span.end);
    }
    let name = read_name(parser)?;
    if name.parts != open_parts {
        return Err(parser.error_at(
            slash.span.start,
            "Expected closing tag </...> to match the opening tag",
        ));
    }
    let gt = parser.peek()?.clone();
    parser.expect_punct(TokenKind::GreaterThan)?;
    Ok(gt.span.end)
}

/// A fragment (`<>…</>`) desugars to an array literal: `h` flattens array
/// children and `render` walks array roots/fragments, so no special element
/// type is needed.
fn fragment_expr(children: Vec<Expr>, start: u32, end: u32) -> Expr {
    let elements = children.into_iter().map(ArrayElement::Expr).collect();
    Expr {
        span: Span::new(start, end),
        kind: ExprKind::Array(ArrayLiteral {
            elements,
            rest_trailing_comma: false,
            span: Span::new(start, end),
        }),
    }
}

/// Cleans raw JSXText with Babel's rules: tabs become spaces, leading
/// whitespace is trimmed on continuation lines, trailing whitespace is
/// trimmed on every line but the last, and blank/whitespace-only lines
/// collapse. In-line single spaces survive, so `<a>x</a> <a>y</a>` keeps
/// the space while indentation between block elements does not.
fn clean_text(raw: &[u16]) -> Vec<u16> {
    // Split into lines on \n, \r, and \r\n.
    let mut lines: Vec<&[u16]> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\n' as u16 || raw[i] == b'\r' as u16 {
            lines.push(&raw[start..i]);
            if raw[i] == b'\r' as u16 && i + 1 < raw.len() && raw[i + 1] == b'\n' as u16 {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    lines.push(&raw[start..]);

    let space = b' ' as u16;
    let mut last_non_empty = 0;
    for (idx, line) in lines.iter().enumerate() {
        if line
            .iter()
            .any(|&unit| unit != space && unit != b'\t' as u16)
        {
            last_non_empty = idx;
        }
    }
    let mut out: Vec<u16> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let is_first = idx == 0;
        let is_last = idx + 1 == lines.len();
        let mut cleaned: Vec<u16> = Vec::with_capacity(line.len());
        for &unit in line.iter() {
            cleaned.push(if unit == b'\t' as u16 { space } else { unit });
        }
        if !is_first {
            while cleaned.first() == Some(&space) {
                cleaned.remove(0);
            }
        }
        if !is_last {
            while cleaned.last() == Some(&space) {
                cleaned.pop();
            }
        }
        if !cleaned.is_empty() {
            if idx != last_non_empty {
                out.extend_from_slice(&cleaned);
                out.push(space);
            } else {
                out.extend_from_slice(&cleaned);
            }
        }
    }
    out
}

/// The type argument for an element: a member path for dotted names, an
/// identifier reference for single capitalized names (a component), and a
/// string literal (the intrinsic name) otherwise.
fn element_tag(name: &JsxName, start: u32, end: u32) -> Expr {
    if name.is_member {
        return member_path(ident_expr(name.parts[0], start, end), &name.parts[1..], end);
    }
    if name.parts.len() == 1 {
        let uppercase = name
            .text
            .first()
            .map(|&unit| (b'A' as u16..=b'Z' as u16).contains(&unit))
            .unwrap_or(false);
        if uppercase {
            return ident_expr(name.parts[0], start, end);
        }
    }
    Expr {
        span: Span::new(start, end),
        kind: ExprKind::Literal(Literal::Str(JsString::from_utf16(&name.text))),
    }
}

fn ident_expr(name: AtomId, start: u32, end: u32) -> Expr {
    Expr {
        span: Span::new(start, end),
        kind: ExprKind::Ident(name),
    }
}

/// Builds `base.p1.p2…` as a MemberExpression chain.
fn member_path(mut base: Expr, properties: &[AtomId], end: u32) -> Expr {
    let start = base.span.start;
    for &property in properties {
        base = Expr {
            span: Span::new(start, end),
            kind: ExprKind::Member(MemberExpr {
                object: Box::new(base),
                property: MemberProperty::Name(property),
                optional: false,
                span: Span::new(start, end),
            }),
        };
    }
    base
}

#[cfg(test)]
mod tests {
    use crate::{parse_script, parse_script_jsx};

    #[test]
    fn jsx_is_off_by_default() {
        // Without the jsx entry point, `<` is only ever an operator, so a
        // script starting with one is a syntax error (spec parsing intact).
        assert!(parse_script("const v = <box/>;").is_err());
        // Relational expressions keep working under the default goal.
        assert!(parse_script("const r = a < b;").is_ok());
    }

    #[test]
    fn self_closing_elements_parse() {
        assert!(parse_script_jsx("const v = <box/>;").is_ok());
        assert!(parse_script_jsx("const v = <box />;").is_ok());
        // A capitalized name is a component reference, a dotted name a member.
        assert!(parse_script_jsx("const v = <Demo/>;").is_ok());
        assert!(parse_script_jsx("const v = <Form.Field/>;").is_ok());
    }

    #[test]
    fn dashed_and_namespaced_names_parse() {
        assert!(parse_script_jsx("const v = <my-box data-id=\"x\"/>;").is_ok());
        assert!(parse_script_jsx("const v = <my-element>text</my-element>;").is_ok());
        assert!(parse_script_jsx("const v = <svg:path/>;").is_ok());
    }

    #[test]
    fn attributes_accept_all_value_forms_and_spreads() {
        assert!(parse_script_jsx("const v = <box a=\"x\" b={1} c/>;").is_ok());
        assert!(parse_script_jsx("const v = <box value={speed} min={0} max={400}/>;").is_ok());
        assert!(parse_script_jsx("const v = <box {...props}/>;").is_ok());
        assert!(parse_script_jsx("const v = <box {...props} a={1} data-n=\"n\"/>;").is_ok());
    }

    #[test]
    fn fragments_parse() {
        assert!(parse_script_jsx("const v = <>a<b/>c</>;").is_ok());
        assert!(parse_script_jsx("const v = <panel><>nested</></panel>;").is_ok());
        assert!(parse_script_jsx("const v = <></>;").is_ok());
    }

    #[test]
    fn elements_nest_and_contain_expressions() {
        assert!(parse_script_jsx(
            "const v = (\n  <panel>\n    <label>{\"Settings\"}</label>\n    <button>{\"Go\"}</button>\n  </panel>\n);"
        )
        .is_ok());
        // Expression containers may themselves hold JSX.
        assert!(parse_script_jsx("const v = <box>{<inner/>}</box>;").is_ok());
    }

    #[test]
    fn text_children_parse() {
        assert!(parse_script_jsx("const v = <box>hello world</box>;").is_ok());
        // Comment-like and string-like text is raw, not lexed as JS.
        assert!(parse_script_jsx("const v = <box>50% off // sale</box>;").is_ok());
        assert!(parse_script_jsx("const v = <box>say \"hi\"</box>;").is_ok());
        // Indented blank lines between elements collapse; text flows.
        assert!(
            parse_script_jsx("const v = (\n  <box>\n    hello\n    <b>world</b>\n  </box>\n);")
                .is_ok()
        );
    }

    #[test]
    fn mismatched_close_tags_error() {
        let error = parse_script_jsx("const v = <box></other>;").unwrap_err();
        assert!(error.to_string().contains("closing tag"), "{error}");
    }

    #[test]
    fn unterminated_elements_error() {
        assert!(parse_script_jsx("const v = <box>hello").is_err());
        assert!(parse_script_jsx("const v = <>hello").is_err());
    }

    #[test]
    fn jsx_mode_keeps_relational_operators_working() {
        assert!(parse_script_jsx("const r = a < b && c > d;").is_ok());
    }
}
