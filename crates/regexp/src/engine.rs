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
            match *abbr {
                // LC / Cased_Letter: the union of the cased letters (spec
                // 22.2.3.13 Table 65).
                "LC" => matches!(gc, "Lu" | "Ll" | "Lt"),
                abbr if abbr.len() == 1 => gc.starts_with(abbr),
                _ => gc == *abbr,
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
    match match_node(&re.program, &ctx, &mut caps, start, 1, &mut |_, pos| {
        Ok(pos)
    }) {
        Ok(end) => {
            caps.values[0] = Some((start, end));
            Some(caps.values)
        }
        Err(_) => None,
    }
}

/// A position advanced by `len` input elements in the match direction.
fn advance(pos: usize, dir: i32, len: usize) -> usize {
    if dir > 0 { pos + len } else { pos - len }
}

fn match_node<'i>(
    node: &Node,
    ctx: &Ctx<'i>,
    caps: &mut Caps,
    pos: usize,
    dir: i32,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    match node {
        Node::Empty => cont(caps, pos),
        Node::Char { cp, fold } => match read_char_dir(ctx, pos, dir) {
            Some((c, len)) => {
                let c = if *fold {
                    canonicalize(ctx.unicode, c)
                } else {
                    c
                };
                if c == *cp {
                    cont(caps, advance(pos, dir, len))
                } else {
                    Err(())
                }
            }
            None => Err(()),
        },
        Node::Any { dot_all } => match read_char_dir(ctx, pos, dir) {
            Some((c, len)) if *dot_all || !unicode::is_line_terminator(c) => {
                cont(caps, advance(pos, dir, len))
            }
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
        Node::Class(class) => {
            // A string member (`\q{…}`, spec ClassStringDisjunction) consumes
            // its full code-unit length, not one character: try the strings
            // first (a non-negated class), then fall back to the single-char
            // membership test. Negated classes never consume strings (the
            // complement of a set containing a string has no string members).
            if !class.negated {
                for string in &class.strings {
                    if string_at_dir(ctx, pos, string, dir) {
                        return cont(caps, advance(pos, dir, string_units(ctx, string)));
                    }
                }
            }
            match read_char_dir(ctx, pos, dir) {
                Some((c, len)) => {
                    if class_matches(ctx, class, pos, c, dir) {
                        cont(caps, advance(pos, dir, len))
                    } else {
                        Err(())
                    }
                }
                None => Err(()),
            }
        }
        Node::Sequence(nodes) => match_sequence(nodes, 0, ctx, caps, pos, dir, cont),
        Node::Alternate(alts) => {
            for alt in alts {
                let mark = caps.mark();
                match match_sequence(alt, 0, ctx, caps, pos, dir, cont) {
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
            owned_captures,
        } => repeat_loop(
            node,
            owned_captures,
            *min,
            *max,
            *greedy,
            ctx,
            caps,
            pos,
            dir,
            0,
            cont,
        ),
        Node::Capture { index, node } => {
            caps.set(*index, (pos, pos));
            let start = pos;
            let mut inner = |caps: &mut Caps, next: usize| {
                caps.set(*index, (start.min(next), start.max(next)));
                cont(caps, next)
            };
            match_node(node, ctx, caps, pos, dir, &mut inner)
        }
        Node::Backref { indices, fold } => {
            // A duplicate name resolves to the last of its groups that
            // participated (spec: the match of the last matching group).
            let chosen = indices
                .iter()
                .rev()
                .find(|&&i| caps.values[i].is_some())
                .copied();
            match chosen.and_then(|i| caps.values[i]) {
                Some((s, e)) => {
                    let len = e - s;
                    let (window_start, window_end) = if dir > 0 {
                        (pos, pos + len)
                    } else {
                        (pos - len, pos)
                    };
                    if window_end > ctx.input.len() || window_start > window_end {
                        return Err(());
                    }
                    let captured = &ctx.input[s..e];
                    let window = &ctx.input[window_start..window_end];
                    if units_eq(ctx, captured, window, *fold) {
                        cont(caps, advance(pos, dir, len))
                    } else {
                        Err(())
                    }
                }
                // A backreference to a group that has not participated matches
                // the empty string (spec BackreferenceMatcher, `captures[cp]`
                // undefined).
                None => cont(caps, pos),
            }
        }
        Node::Lookahead { negate, node } => {
            let mark = caps.mark();
            let mut inner = |_: &mut Caps, _next: usize| Ok(pos);
            // The lookahead's subexpression always matches forward, even
            // inside a lookbehind (the assertion is evaluated at its own
            // position in the input).
            match match_node(node, ctx, caps, pos, 1, &mut inner) {
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
        Node::Lookbehind { negate, node } => {
            // The subexpression runs right-to-left from the current position
            // (variable-length lookbehind); on success the position resets.
            let mark = caps.mark();
            let mut inner = |_: &mut Caps, _next: usize| Ok(pos);
            match match_node(node, ctx, caps, pos, -1, &mut inner) {
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
    dir: i32,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    let len = nodes.len();
    if index == len {
        return cont(caps, pos);
    }
    // In reverse direction the terms match right-to-left.
    let node_index = if dir > 0 { index } else { len - 1 - index };
    let mut inner =
        |caps: &mut Caps, next: usize| match_sequence(nodes, index + 1, ctx, caps, next, dir, cont);
    match_node(&nodes[node_index], ctx, caps, pos, dir, &mut inner)
}

#[allow(clippy::too_many_arguments)]
fn repeat_loop<'i>(
    node: &Node,
    owned: &[usize],
    min: u32,
    max: Option<u32>,
    greedy: bool,
    ctx: &Ctx<'i>,
    caps: &mut Caps,
    pos: usize,
    dir: i32,
    count: u32,
    cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult,
) -> MatchResult {
    // Fast path: a greedy repeat of a single-atom (literal, dot, or character
    // class — with or without `\q{…}` string members) with no captures. The
    // recursive path consumes one atom per stack frame, which overflows on
    // multi-megabyte inputs (the generated property-escape fixtures build
    // ~2M-unit match strings); consume iteratively instead, then try the
    // continuation from the furthest position backward, backtracking one atom
    // at a time (spec RepeatMatcher). A class with string members may match
    // several alternatives at one position (`[\q{a|ab}]+` on "ab"), so each
    // consumed position records the untried alternatives to explore on
    // backtrack.
    if greedy
        && owned.is_empty()
        && matches!(node, Node::Char { .. } | Node::Any { .. } | Node::Class(_))
    {
        let mark = caps.mark();
        let mut cur = pos;
        let mut cnt = count;
        // Each consumed atom: (position, remaining alternative end positions).
        let mut consumed: Vec<(usize, Vec<usize>)> = Vec::new();
        while max.is_none_or(|m| cnt < m) {
            let alternatives = atom_match_ends(ctx, node, cur, dir);
            if alternatives.is_empty() {
                break;
            }
            let next = alternatives[0];
            consumed.push((cur, alternatives[1..].to_vec()));
            cur = next;
            cnt += 1;
        }
        loop {
            if cnt >= min {
                caps.rollback(mark);
                if let Ok(end) = cont(caps, cur) {
                    return Ok(end);
                }
            }
            // Backtrack one step: try the next alternative at the most recent
            // consumed position, or (when its alternatives are exhausted) undo
            // that atom and move back to its start. Either way the loop tries
            // the continuation again at the new `cur`.
            match consumed.last_mut() {
                None => return Err(()),
                Some((_, remaining)) => {
                    if let Some(alt) = remaining.pop() {
                        // The atom is still consumed (it matched, just to a
                        // different endpoint); re-consume greedily from there.
                        cur = alt;
                        while max.is_none_or(|m| cnt < m) {
                            let alternatives = atom_match_ends(ctx, node, cur, dir);
                            if alternatives.is_empty() {
                                break;
                            }
                            let next = alternatives[0];
                            consumed.push((cur, alternatives[1..].to_vec()));
                            cur = next;
                            cnt += 1;
                        }
                    } else {
                        let (start, _) = consumed.pop().unwrap();
                        cur = start;
                        cnt -= 1;
                    }
                }
            }
        }
    }
    // Each iteration re-matches the atom from scratch, so captures owned by
    // the atom from earlier iterations must not leak into this one (spec
    // RepeatMatcher clears the atom's captures on the copy before matching).
    if greedy {
        if max.is_none_or(|m| count < m) {
            let mark = caps.mark();
            clear_owned(caps, owned);
            // A zero-progress iteration is discarded when it was optional
            // (spec RepeatMatcher step 2.b: an empty match after the minimum
            // is reached fails, so the atom backtracks into a longer match);
            // a forced one only counts toward the minimum.
            let mut inner = |caps: &mut Caps, next: usize| {
                if next == pos && count >= min {
                    Err(())
                } else if next == pos {
                    if count + 1 >= min {
                        cont(caps, next)
                    } else {
                        repeat_loop(
                            node,
                            owned,
                            min,
                            max,
                            greedy,
                            ctx,
                            caps,
                            next,
                            dir,
                            count + 1,
                            cont,
                        )
                    }
                } else {
                    repeat_loop(
                        node,
                        owned,
                        min,
                        max,
                        greedy,
                        ctx,
                        caps,
                        next,
                        dir,
                        count + 1,
                        cont,
                    )
                }
            };
            match match_node(node, ctx, caps, pos, dir, &mut inner) {
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
        clear_owned(caps, owned);
        let mut inner = |caps: &mut Caps, next: usize| {
            if next == pos && count >= min {
                Err(())
            } else if next == pos {
                if count + 1 >= min {
                    cont(caps, next)
                } else {
                    repeat_loop(
                        node,
                        owned,
                        min,
                        max,
                        greedy,
                        ctx,
                        caps,
                        next,
                        dir,
                        count + 1,
                        cont,
                    )
                }
            } else {
                repeat_loop(
                    node,
                    owned,
                    min,
                    max,
                    greedy,
                    ctx,
                    caps,
                    next,
                    dir,
                    count + 1,
                    cont,
                )
            }
        };
        match match_node(node, ctx, caps, pos, dir, &mut inner) {
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
        clear_owned(caps, owned);
        let mut inner = |caps: &mut Caps, next: usize| {
            repeat_loop(
                node,
                owned,
                min,
                max,
                greedy,
                ctx,
                caps,
                next,
                dir,
                count + 1,
                cont,
            )
        };
        match_node(node, ctx, caps, pos, dir, &mut inner)
    }
}

/// Clear the captures owned by a repeated atom, recording the previous
/// values on the undo log so backtracking restores them.
fn clear_owned(caps: &mut Caps, owned: &[usize]) {
    for &index in owned {
        if let Some(old) = caps.values[index] {
            caps.trail.push((index, Some(old)));
            caps.values[index] = None;
        }
    }
}

/// Whether the class matches the character at `pos` (given `c` = that char);
/// `pos` is the character's start (forward) or end (backward) offset.
fn class_matches(ctx: &Ctx<'_>, class: &CharClass, pos: usize, c: u32, dir: i32) -> bool {
    let cc = if class.fold {
        canonicalize(ctx.unicode, c)
    } else {
        c
    };
    let in_set = match &class.predicate {
        Some(pred) => predicate_matches(pred, cc),
        None => ranges_contain(&class.ranges, cc),
    } || class
        .strings
        .iter()
        .any(|s| string_at_dir(ctx, pos, s, dir));
    in_set != class.negated
}

/// All the ways a single atom (literal, dot, or character class — including
/// its `\q{…}` string members) matches at `pos`: every endpoint the atom
/// can advance to, in priority order (the matching alternative first, then
/// the rest). A string class may match several alternatives at one position
/// (`[\q{a|ab}]` at "ab" matches both "a" and "ab"), and the repeat fast
/// path explores them in order on backtrack. Returns an empty vector when
/// the atom cannot match here.
fn atom_match_ends(ctx: &Ctx<'_>, node: &Node, pos: usize, dir: i32) -> Vec<usize> {
    match node {
        Node::Char { cp, fold } => {
            let Some((c, len)) = read_char_dir(ctx, pos, dir) else {
                return Vec::new();
            };
            let c = if *fold {
                canonicalize(ctx.unicode, c)
            } else {
                c
            };
            if c == *cp {
                vec![advance(pos, dir, len)]
            } else {
                Vec::new()
            }
        }
        Node::Any { dot_all } => {
            let Some((c, len)) = read_char_dir(ctx, pos, dir) else {
                return Vec::new();
            };
            if *dot_all || !unicode::is_line_terminator(c) {
                vec![advance(pos, dir, len)]
            } else {
                Vec::new()
            }
        }
        Node::Class(class) => {
            let mut ends = Vec::new();
            // A string member (`\q{…}`) consumes its full code-unit length;
            // a non-negated class tries the strings first, then the
            // single-char membership test (negated classes never consume
            // strings).
            if !class.negated {
                for string in &class.strings {
                    if string_at_dir(ctx, pos, string, dir) {
                        ends.push(advance(pos, dir, string_units(ctx, string)));
                    }
                }
            }
            if let Some((c, len)) = read_char_dir(ctx, pos, dir)
                && class_matches(ctx, class, pos, c, dir)
            {
                ends.push(advance(pos, dir, len));
            }
            ends
        }
        _ => Vec::new(),
    }
}

/// A `\q{…}` string atom: the code points adjacent to `pos` equal the string,
/// in the match direction.
fn string_at_dir(ctx: &Ctx<'_>, pos: usize, string: &[u32], dir: i32) -> bool {
    if dir > 0 {
        string_at(ctx, pos, string)
    } else {
        string_at_back(ctx, pos, string)
    }
}

/// The code-unit length of a `\q{…}` string at match time: a non-BMP code
/// point is two UTF-16 units in unicode mode, one in legacy mode (strings
/// only arise under `/v`, which is unicode, but keep the legacy path total).
fn string_units(ctx: &Ctx<'_>, string: &[u32]) -> usize {
    if ctx.unicode {
        string
            .iter()
            .map(|&cp| if cp > 0xFFFF { 2 } else { 1 })
            .sum()
    } else {
        string.len()
    }
}

/// A `\q{…}` string atom matching leftward: the code points ending at `pos`.
fn string_at_back(ctx: &Ctx<'_>, pos: usize, string: &[u32]) -> bool {
    let mut index = pos;
    for &want in string.iter().rev() {
        match read_char_back(ctx, index) {
            Some((c, len)) if c == want => index -= len,
            _ => return false,
        }
    }
    true
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

/// Read one input character in the match direction: the character at `pos`
/// (forward) or the one ending at `pos` (backward).
fn read_char_dir(ctx: &Ctx<'_>, pos: usize, dir: i32) -> Option<(u32, usize)> {
    if dir > 0 {
        read_char(ctx, pos)
    } else {
        read_char_back(ctx, pos)
    }
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

/// Read one input character ending at `pos` (the previous character, for
/// backward matching).
fn read_char_back(ctx: &Ctx<'_>, pos: usize) -> Option<(u32, usize)> {
    if pos == 0 {
        return None;
    }
    let u = ctx.input[pos - 1];
    if ctx.unicode
        && (0xDC00..=0xDFFF).contains(&u)
        && pos >= 2
        && (0xD800..=0xDBFF).contains(&ctx.input[pos - 2])
    {
        let hi = ctx.input[pos - 2] as u32;
        return Some((0x10000 + ((hi - 0xD800) << 10) + (u as u32 - 0xDC00), 2));
    }
    Some((u as u32, 1))
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
