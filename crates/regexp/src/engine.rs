//! The backtracking engine: a recursive matcher over the compiled AST with an
//! undo-log (trail) for captures. Choices are tried in spec order — leftmost
//! alternative first, greedy quantifiers consume as much as possible, and
//! lookarounds commit to their first success.

use crate::{CharClass, Node, Predicate, Regex};

pub(crate) type MatchResult = Result<usize, ()>;

/// Canonicalize (spec 22.2.3.2.2): simple case folding under `u`/`v`,
/// toUppercase-based otherwise.
pub(crate) fn canonicalize(unicode: bool, cp: u32) -> u32 {
    if unicode {
        unicode::simple_case_fold(cp)
    } else {
        unicode::non_unicode_canonicalize(cp)
    }
}

/// The effective membership of a predicate when scanning a class: `\\w`
/// includes the unicode+ignore-case extras (chars whose fold is an ASCII
/// word char).
fn pred_effective(pred: &Predicate, fold: bool, unicode: bool, cp: u32) -> bool {
    match pred {
        Predicate::Word => {
            unicode::is_ascii_word_char(cp)
                || (fold && unicode && unicode::is_ascii_word_char(canonicalize(true, cp)))
        }
        _ => predicate_matches(pred, cp),
    }
}

/// The plain predicate membership used at match time (the input character is
/// canonicalized before this when the class folds).
pub(crate) fn predicate_matches(pred: &Predicate, cp: u32) -> bool {
    match pred {
        Predicate::Digits => (0x30..=0x39).contains(&cp),
        Predicate::Word => unicode::is_ascii_word_char(cp),
        Predicate::Space => unicode::is_white_space(cp) || unicode::is_line_terminator(cp),
        Predicate::GeneralCategory(abbr) => {
            let gc = unicode::general_category(cp);
            if abbr.len() == 1 {
                gc.starts_with(abbr)
            } else {
                gc == *abbr
            }
        }
        Predicate::Script(name) => unicode::script(cp) == Some(*name),
        Predicate::ScriptExtensions(name) => {
            unicode::script_extensions(cp).iter().any(|s| s == name)
        }
        Predicate::Binary(name) => unicode::binary_property(cp, name).unwrap_or(false),
    }
}

/// Enumerate a predicate into explicit ranges; with `fold` each member's
/// canonical form joins too (F = S ∪ canon(S)).
pub(crate) fn scan_predicate(pred: &Predicate, fold: bool, unicode: bool) -> Vec<(u32, u32)> {
    let mut members = Vec::new();
    for cp in 0u32..=0x10FFFF {
        if pred_effective(pred, fold, unicode, cp) {
            members.push(cp);
            if fold {
                members.push(canonicalize(unicode, cp));
            }
        }
    }
    ranges_of(&members)
}

/// Compress a member list into inclusive ranges.
pub(crate) fn ranges_of(members: &[u32]) -> Vec<(u32, u32)> {
    let mut sorted: Vec<u32> = members.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for cp in sorted {
        match out.last_mut() {
            Some((_, end)) if cp == *end + 1 => *end = cp,
            _ => out.push((cp, cp)),
        }
    }
    out
}

/// The complement of a range list over the full code point space.
pub(crate) fn complement(ranges: &mut Vec<(u32, u32)>) {
    let mut next = 0u32;
    let mut out = Vec::new();
    for (s, e) in ranges.iter() {
        if next < *s {
            out.push((next, s - 1));
        }
        next = e.saturating_add(1);
    }
    if next <= 0x10FFFF {
        out.push((next, 0x10FFFF));
    }
    *ranges = out;
}

/// Fold explicit ranges: F = S ∪ canon(S).
pub(crate) fn fold_ranges(ranges: &[(u32, u32)], unicode: bool) -> Vec<(u32, u32)> {
    let mut members = Vec::new();
    for &(s, e) in ranges {
        for cp in s..=e {
            members.push(cp);
            members.push(canonicalize(unicode, cp));
        }
    }
    ranges_of(&members)
}

fn ranges_contain(ranges: &[(u32, u32)], cp: u32) -> bool {
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

struct Ctx<'i> {
    input: &'i [u16],
    unicode: bool,
}

/// Capture buffer with an undo log so backtracking restores group values.
struct Caps {
    values: Vec<Option<(usize, usize)>>,
    trail: Vec<(usize, Option<(usize, usize)>)>,
}

impl Caps {
    fn new(groups: usize) -> Self {
        Caps {
            values: vec![None; groups + 1],
            trail: Vec::new(),
        }
    }

    fn mark(&self) -> usize {
        self.trail.len()
    }

    fn rollback(&mut self, mark: usize) {
        while self.trail.len() > mark {
            let (index, old) = self.trail.pop().unwrap();
            self.values[index] = old;
        }
    }

    fn set(&mut self, index: usize, value: (usize, usize)) {
        self.trail.push((index, self.values[index]));
        self.values[index] = Some(value);
    }
}

/// The public entry: match the program at `start`, returning capture spans in
/// code units.
pub(crate) fn exec(re: &Regex, input: &[u16], start: usize) -> Option<crate::Match> {
    let ctx = Ctx {
        input,
        unicode: re.flags.has_unicode(),
    };
    let mut caps = Caps::new(re.capturing_groups);
    match match_node(&re.program, &ctx, &mut caps, start, &mut |_, pos| Ok(pos)) {
        Ok(end) => {
            caps.values[0] = Some((start, end));
            Some(caps.values)
        }
        Err(_) => None,
    }
}

fn match_node<'i>(
    node: &Node,
    ctx: &Ctx<'i>,
    caps: &mut Caps,
    pos: usize,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    match node {
        Node::Empty => cont(caps, pos),
        Node::Char { cp, fold } => match read_char(ctx, pos) {
            Some((c, len)) => {
                let c = if *fold {
                    canonicalize(ctx.unicode, c)
                } else {
                    c
                };
                if c == *cp {
                    cont(caps, pos + len)
                } else {
                    Err(())
                }
            }
            None => Err(()),
        },
        Node::Any { dot_all } => match read_char(ctx, pos) {
            Some((c, len)) if *dot_all || !unicode::is_line_terminator(c) => cont(caps, pos + len),
            _ => Err(()),
        },
        Node::Start { multiline } => {
            if pos == 0 || (*multiline && pos > 0 && is_line_terminator_at(ctx, pos - 1)) {
                cont(caps, pos)
            } else {
                Err(())
            }
        }
        Node::End { multiline } => {
            let len = ctx.input.len();
            if pos == len || (*multiline && pos < len && is_line_terminator_at(ctx, pos)) {
                cont(caps, pos)
            } else {
                Err(())
            }
        }
        Node::WordBoundary { extra_folded } => {
            let before = pos > 0 && is_word_char_at(ctx, pos - 1, *extra_folded);
            let after = pos < ctx.input.len() && is_word_char_at(ctx, pos, *extra_folded);
            if before != after {
                cont(caps, pos)
            } else {
                Err(())
            }
        }
        Node::NotWordBoundary { extra_folded } => {
            let before = pos > 0 && is_word_char_at(ctx, pos - 1, *extra_folded);
            let after = pos < ctx.input.len() && is_word_char_at(ctx, pos, *extra_folded);
            if before == after {
                cont(caps, pos)
            } else {
                Err(())
            }
        }
        Node::Class(class) => match read_char(ctx, pos) {
            Some((c, len)) => {
                if class_matches(ctx, class, pos, c) {
                    cont(caps, pos + len)
                } else {
                    Err(())
                }
            }
            None => Err(()),
        },
        Node::Sequence(nodes) => match_sequence(nodes, 0, ctx, caps, pos, cont),
        Node::Alternate(alts) => {
            for alt in alts {
                let mark = caps.mark();
                match match_sequence(alt, 0, ctx, caps, pos, cont) {
                    Ok(end) => return Ok(end),
                    Err(_) => caps.rollback(mark),
                }
            }
            Err(())
        }
        Node::Repeat {
            node,
            min,
            max,
            greedy,
        } => repeat_loop(node, *min, *max, *greedy, ctx, caps, pos, 0, cont),
        Node::Capture { index, node } => {
            caps.set(*index, (pos, pos));
            let start = pos;
            let mut inner = |caps: &mut Caps, next: usize| {
                caps.set(*index, (start, next));
                cont(caps, next)
            };
            match_node(node, ctx, caps, pos, &mut inner)
        }
        Node::Backref { index, fold } => match caps.values[*index] {
            Some((s, e)) => {
                let len = e - s;
                if pos + len > ctx.input.len() {
                    return Err(());
                }
                let captured = &ctx.input[s..e];
                let window = &ctx.input[pos..pos + len];
                if units_eq(ctx, captured, window, *fold) {
                    cont(caps, pos + len)
                } else {
                    Err(())
                }
            }
            None => Err(()),
        },
        Node::Lookahead { negate, node } => {
            let mark = caps.mark();
            let mut inner = |_: &mut Caps, _next: usize| Ok(pos);
            match match_node(node, ctx, caps, pos, &mut inner) {
                Ok(_) => {
                    if *negate {
                        caps.rollback(mark);
                        Err(())
                    } else {
                        cont(caps, pos)
                    }
                }
                Err(_) => {
                    caps.rollback(mark);
                    if *negate { cont(caps, pos) } else { Err(()) }
                }
            }
        }
        Node::Lookbehind {
            negate,
            node,
            length,
        } => {
            let Some(start) = step_back(ctx, pos, *length as usize) else {
                // Not enough input before the position.
                if *negate {
                    return cont(caps, pos);
                }
                return Err(());
            };
            let mark = caps.mark();
            let mut inner = |_: &mut Caps, end: usize| {
                if end == pos { Ok(pos) } else { Err(()) }
            };
            match match_node(node, ctx, caps, start, &mut inner) {
                Ok(_) => {
                    if *negate {
                        caps.rollback(mark);
                        Err(())
                    } else {
                        cont(caps, pos)
                    }
                }
                Err(_) => {
                    caps.rollback(mark);
                    if *negate { cont(caps, pos) } else { Err(()) }
                }
            }
        }
    }
}

fn match_sequence<'i>(
    nodes: &[Node],
    index: usize,
    ctx: &Ctx<'i>,
    caps: &mut Caps,
    pos: usize,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    if index == nodes.len() {
        return cont(caps, pos);
    }
    let mut inner =
        |caps: &mut Caps, next: usize| match_sequence(nodes, index + 1, ctx, caps, next, cont);
    match_node(&nodes[index], ctx, caps, pos, &mut inner)
}

#[allow(clippy::too_many_arguments)]
fn repeat_loop<'i>(
    node: &Node,
    min: u32,
    max: Option<u32>,
    greedy: bool,
    ctx: &Ctx<'i>,
    caps: &mut Caps,
    pos: usize,
    count: u32,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    if greedy {
        if max.is_none_or(|m| count < m) {
            let mark = caps.mark();
            // A zero-progress iteration (the atom matched the empty string) is
            // the last one: it counts toward `min`, but recursing again could
            // never advance the position, so stop once the minimum is met.
            let mut inner = |caps: &mut Caps, next: usize| {
                if next == pos && count + 1 >= min {
                    cont(caps, next)
                } else {
                    repeat_loop(node, min, max, greedy, ctx, caps, next, count + 1, cont)
                }
            };
            match match_node(node, ctx, caps, pos, &mut inner) {
                Ok(end) => return Ok(end),
                Err(_) => caps.rollback(mark),
            }
        }
        if count >= min {
            cont(caps, pos)
        } else {
            Err(())
        }
    } else if count >= min {
        let mark = caps.mark();
        match cont(caps, pos) {
            Ok(end) => return Ok(end),
            Err(_) => caps.rollback(mark),
        }
        if max.is_some_and(|m| count >= m) {
            return Err(());
        }
        let mark = caps.mark();
        let mut inner = |caps: &mut Caps, next: usize| {
            if next == pos && count + 1 >= min {
                cont(caps, next)
            } else {
                repeat_loop(node, min, max, greedy, ctx, caps, next, count + 1, cont)
            }
        };
        match match_node(node, ctx, caps, pos, &mut inner) {
            Ok(end) => Ok(end),
            Err(_) => {
                caps.rollback(mark);
                Err(())
            }
        }
    } else {
        // count < min: must match more.
        if max.is_some_and(|m| count >= m) {
            return Err(());
        }
        let mut inner = |caps: &mut Caps, next: usize| {
            repeat_loop(node, min, max, greedy, ctx, caps, next, count + 1, cont)
        };
        match_node(node, ctx, caps, pos, &mut inner)
    }
}

/// Whether the class matches the character at `pos` (given `c` = that char).
fn class_matches(ctx: &Ctx<'_>, class: &CharClass, pos: usize, c: u32) -> bool {
    let cc = if class.fold {
        canonicalize(ctx.unicode, c)
    } else {
        c
    };
    let in_set = match &class.predicate {
        Some(pred) => predicate_matches(pred, cc),
        None => ranges_contain(&class.ranges, cc),
    } || class.strings.iter().any(|s| string_at(ctx, pos, s));
    in_set != class.negated
}

/// A `\q{…}` string atom: the code points at `pos` equal the string.
fn string_at(ctx: &Ctx<'_>, pos: usize, string: &[u32]) -> bool {
    let mut index = pos;
    for &want in string {
        match read_char(ctx, index) {
            Some((c, len)) if c == want => index += len,
            _ => return false,
        }
    }
    true
}

/// Read one input character at `pos`: a code unit in legacy mode, a full code
/// point in unicode mode.
fn read_char(ctx: &Ctx<'_>, pos: usize) -> Option<(u32, usize)> {
    if ctx.unicode {
        let (cp, _, count) = crate::crux_code_point_at(ctx.input, pos);
        if count == 0 { None } else { Some((cp, count)) }
    } else {
        ctx.input.get(pos).map(|&u| (u as u32, 1))
    }
}

fn is_line_terminator_at(ctx: &Ctx<'_>, pos: usize) -> bool {
    match read_char(ctx, pos) {
        Some((c, _)) => unicode::is_line_terminator(c),
        None => false,
    }
}

/// IsWordChar (spec 22.2.3.3): WordCharacters membership of the raw char.
fn is_word_char_at(ctx: &Ctx<'_>, pos: usize, extra_folded: bool) -> bool {
    match read_char(ctx, pos) {
        Some((c, _)) => {
            unicode::is_ascii_word_char(c)
                || (extra_folded && unicode::is_ascii_word_char(canonicalize(true, c)))
        }
        None => false,
    }
}

/// Compare two equal-length unit windows, canonicalizing under ignore-case.
fn units_eq(ctx: &Ctx<'_>, a: &[u16], b: &[u16], fold: bool) -> bool {
    if !fold {
        return a == b;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if canonicalize(ctx.unicode, *x as u32) != canonicalize(ctx.unicode, *y as u32) {
            return false;
        }
    }
    true
}

/// Step back `count` characters (input elements) from `pos`, returning the
/// code-unit index.
fn step_back(ctx: &Ctx<'_>, pos: usize, count: usize) -> Option<usize> {
    if count == 0 {
        return Some(pos);
    }
    if !ctx.unicode {
        return pos.checked_sub(count);
    }
    let mut index = pos;
    for _ in 0..count {
        if index == 0 {
            return None;
        }
        // Find the start of the code point ending at `index`.
        if (0xDC00..=0xDFFF).contains(&ctx.input[index - 1])
            && index >= 2
            && (0xD800..=0xDBFF).contains(&ctx.input[index - 2])
        {
            index -= 2;
        } else {
            index -= 1;
        }
    }
    Some(index)
}
