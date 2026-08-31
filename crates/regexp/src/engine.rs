//! The backtracking engine: an explicit-stack matcher over the compiled AST
//! with an undo-log (trail) for captures. Choices are tried in spec order —
//! leftmost alternative first, greedy quantifiers consume as much as
//! possible, and lookarounds commit to their first success. A single loop
//! drives a task stack (the forward path) and a backtrack stack (the choice
//! points), so recursion depth tracks pattern structure, never input length.

use crate::{CharClass, Node, Predicate, Regex};

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

/// Compile-time cache of predicate → explicit code-point ranges (the
/// non-folded membership set, which is unicode-mode independent). Enumerating
/// a predicate over the full code point space is ~1M membership tests, so
/// each unique predicate pays it once per process instead of once per
/// compile.
type PredicateRanges = std::sync::Arc<Vec<(u32, u32)>>;
static PREDICATE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<Predicate, PredicateRanges>>,
> = std::sync::OnceLock::new();

fn predicate_ranges(pred: &Predicate) -> PredicateRanges {
    let cache =
        PREDICATE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Some(ranges) = cache.lock().unwrap().get(pred) {
        return ranges.clone();
    }
    let ranges = std::sync::Arc::new(scan_predicate(pred, false, false));
    cache.lock().unwrap().insert(pred.clone(), ranges.clone());
    ranges
}

/// Reverse fold index per mode: canonical code point -> the code-point ranges
/// folding to it. Built lazily once per process by enumerating the code point
/// space (same pattern as `predicate_ranges`); the search prefilter's
/// leading-char set for a folded literal is the equivalence class of its
/// canonical form, and a missing member would let the search skip a real
/// match (e.g. U+212A KELVIN SIGN folds to `k`).
type FoldClasses = std::collections::HashMap<u32, std::sync::Arc<Vec<(u32, u32)>>>;
static FOLD_CLASSES: [std::sync::OnceLock<std::sync::Mutex<FoldClasses>>; 2] =
    [std::sync::OnceLock::new(), std::sync::OnceLock::new()];

/// Every code point `c` with `canonicalize(mode, c) == canonicalize(mode, cp)`
/// — the `i`-fold equivalence class of `cp` — as ranges.
fn fold_class(cp: u32, unicode: bool) -> std::sync::Arc<Vec<(u32, u32)>> {
    let slot = usize::from(unicode);
    let cache = FOLD_CLASSES[slot].get_or_init(|| {
        let mut preimage: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        for c in 0u32..=0x10FFFF {
            let f = canonicalize(unicode, c);
            if f != c {
                preimage.entry(f).or_default().push(c);
            }
        }
        std::sync::Mutex::new(
            preimage
                .into_iter()
                .map(|(canon, mut members)| {
                    members.push(canon);
                    (canon, std::sync::Arc::new(ranges_of(&members)))
                })
                .collect(),
        )
    });
    let canon = canonicalize(unicode, cp);
    match cache.lock().unwrap().get(&canon) {
        Some(class) => class.clone(),
        // No other code point folds to this canonical form.
        None => std::sync::Arc::new(vec![(canon, canon)]),
    }
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

/// Union of two sorted, non-overlapping range lists into one sorted, merged
/// list.
fn ranges_union(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = a.to_vec();
    out.extend_from_slice(b);
    out.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(out.len());
    for (s, e) in out {
        match merged.last_mut() {
            Some((_, end)) if s <= *end + 1 => *end = (*end).max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
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

/// What the matcher is currently working toward: the forward path. Popped
/// LIFO; an empty stack is a successful full match at the current position.
#[derive(Clone)]
enum Task<'a> {
    /// Match this node, then pop.
    Node(&'a Node),
    /// Match nodes[i..] of a sequence (right-to-left when matching
    /// backward), then pop.
    SeqElem(&'a [Node], usize),
    /// Close a capture: record (start, pos) normalized, then pop.
    CaptureClose { index: usize, start: usize },
    /// Greedy repeat with `count` iterations done: try one more.
    GreedyRepeat {
        node: &'a Node,
        owned: &'a [usize],
        min: u32,
        max: Option<u32>,
        count: u32,
    },
    /// Lazy repeat with `count` iterations done: try the continuation first
    /// (when count >= min), else iterate.
    LazyRepeat {
        node: &'a Node,
        owned: &'a [usize],
        min: u32,
        max: Option<u32>,
        count: u32,
    },
    /// A repeat iteration just completed (it started at `iter_start`; `count`
    /// iterations preceded it): enforce the empty-iteration rule, then loop.
    RepeatIterDone {
        node: &'a Node,
        owned: &'a [usize],
        min: u32,
        max: Option<u32>,
        greedy: bool,
        count: u32,
        iter_start: usize,
    },
    /// A lookaround sub-match succeeded: commit (positive) or fail (negated).
    AssertDone {
        negate: bool,
        start: usize,
        outer_dir: i32,
        mark: usize,
        cut: usize,
    },
    /// Resumed by a greedy repeat's iteration choice: the repeat is done with
    /// `count` iterations; check the minimum.
    RepeatDone { min: u32, count: u32 },
    /// Resumed by a lazy repeat's continuation-failure choice: try one
    /// iteration.
    LazyIterate {
        node: &'a Node,
        owned: &'a [usize],
        min: u32,
        max: Option<u32>,
        count: u32,
    },
    /// Resumed by a lookaround's guard choice: the sub-match failed (a
    /// negative assertion turns that into success).
    AssertFailed {
        negate: bool,
        start: usize,
        outer_dir: i32,
    },
    /// Resumed by a class's alternative choice: consume `len` input units
    /// (a `\q` string member or the single-char endpoint tried on
    /// backtrack).
    ClassAdvance { len: usize },
    /// Resumed by the simple-atom repeat fast path when the continuation
    /// failed at `level` iterations: shrink to `level - 1`, record the
    /// iteration's captures, and re-arm the choice for the next shrink.
    FastShrink {
        atom: &'a Node,
        consumed: std::rc::Rc<[usize]>,
        entry_pos: usize,
        owned: &'a [usize],
        min: u32,
        base_mark: usize,
        cont: std::rc::Rc<[Task<'a>]>,
        level: usize,
    },
}

/// A choice point: the forward path to resume from if the current attempt
/// fails. The continuation below the choice (`cont`, shared via `Rc` — it is
/// stable while a repeat's iterations run) is restored, then `resume` (if
/// any) runs on top of it. A bare stack length is not enough: the forward
/// path consumes continuation frames below the choice before it is popped,
/// so the exact stack content at the choice point is needed.
struct Choice<'a> {
    pos: usize,
    mark: usize,
    cont: std::rc::Rc<[Task<'a>]>,
    resume: Option<Task<'a>>,
}

/// The explicit-stack backtracking matcher.
struct Matcher<'a, 'i> {
    re: &'a Regex,
    ctx: Ctx<'i>,
    caps: Caps,
    tasks: Vec<Task<'a>>,
    choices: Vec<Choice<'a>>,
    pos: usize,
    dir: i32,
    /// Whether the pattern contains no backreferences. When true, a
    /// repeat-atom failure memoization is sound: the acceptance of the
    /// continuation after a repeat is capture-independent (backrefs are the
    /// only capture reads), so "the repeat at (atom, pos) exhausts" holds
    /// for every re-entry at that position.
    backref_free: bool,
    /// The peeled atoms of repeats whose enclosing repeats all have `min <= 1`
    /// (computed once at compile time and stored on the `Regex`): such an
    /// enclosing repeat can stop at any iteration count (its stop choice always
    /// passes the minimum), so the continuation below the repeat is
    /// acceptance-equivalent at every re-entry; a `{2,}`-or-tighter ancestor
    /// makes the continuation depend on its own iteration count, which the
    /// (atom, position) memo key does not carry.
    memo_safe: &'a std::collections::HashSet<usize>,
    /// R5.2 failure memo: (atom address, position) -> the repeat exhausted
    /// every shrink level (or could not reach its minimum) there. Entries are
    /// recorded only when the conditions above make them sound, and the search
    /// loop reuses them across position attempts (the continuation is
    /// pattern-determined).
    memo: std::collections::HashMap<(usize, usize), ()>,
}

impl<'a, 'i> Matcher<'a, 'i> {
    fn new(re: &'a Regex, input: &'i [u16]) -> Self {
        Matcher {
            backref_free: re.backref_free,
            memo_safe: &re.memo_safe,
            memo: std::collections::HashMap::new(),
            re,
            ctx: Ctx {
                input,
                unicode: re.flags.has_unicode(),
            },
            caps: Caps::new(re.capturing_groups),
            tasks: Vec::new(),
            choices: Vec::new(),
            pos: 0,
            dir: 1,
        }
    }

    /// Match the whole program from `start`, resetting all matcher state
    /// (the search loop reuses one `Matcher` across position attempts).
    fn match_at(&mut self, start: usize) -> Option<usize> {
        self.pos = start;
        self.dir = 1;
        self.tasks.clear();
        self.choices.clear();
        // rollback(0) restores every capture to its initial None: the trail
        // entries chain back to the untouched state.
        self.caps.rollback(0);
        self.tasks.push(Task::Node(&self.re.program));
        self.run()
    }

    fn run(&mut self) -> Option<usize> {
        loop {
            let task = match self.tasks.pop() {
                Some(task) => task,
                None => return Some(self.pos),
            };
            if !self.dispatch(task) && !self.backtrack() {
                return None;
            }
        }
    }

    /// Record a choice point: snapshot the current forward path and run
    /// `resume` on top of it when the choice is restored.
    fn push_choice(&mut self, resume: Task<'a>) {
        self.choices.push(Choice {
            pos: self.pos,
            mark: self.caps.mark(),
            cont: std::rc::Rc::from(self.tasks.as_slice()),
            resume: Some(resume),
        });
    }

    /// Execute one task. Returns false only for an atomic failure or a
    /// structural dead end; the caller then backtracks.
    fn dispatch(&mut self, task: Task<'a>) -> bool {
        match task {
            Task::Node(node) => self.step_node(node),
            Task::SeqElem(nodes, index) => {
                if index < nodes.len() {
                    // In reverse direction the terms match right-to-left.
                    let node_index = if self.dir > 0 {
                        index
                    } else {
                        nodes.len() - 1 - index
                    };
                    self.tasks.push(Task::SeqElem(nodes, index + 1));
                    self.tasks.push(Task::Node(&nodes[node_index]));
                }
                true
            }
            Task::CaptureClose { index, start } => {
                // Normalize the span: in a lookbehind the capture body runs
                // right-to-left, so the start is past the current position.
                self.caps
                    .set(index, (start.min(self.pos), start.max(self.pos)));
                true
            }
            Task::GreedyRepeat {
                node,
                owned,
                min,
                max,
                count,
            } => {
                if max.is_some_and(|m| count >= m) {
                    // No more iterations allowed; the repeat is done with
                    // `count` iterations.
                    count >= min
                } else if let Some(simple) = simple_atom(node) {
                    if self
                        .memo
                        .contains_key(&(simple as *const Node as usize, self.pos))
                    {
                        // An earlier attempt exhausted this repeat at this
                        // position (or showed it cannot reach its minimum);
                        // re-running it can only repeat the same failures.
                        false
                    } else {
                        // A simple (non-branching) atom consumes iteratively
                        // without a choice per iteration.
                        self.greedy_simple(simple, owned, min, max, count)
                    }
                } else {
                    let start = self.pos;
                    // The choice is recorded before clearing the owned
                    // captures so backtracking restores them.
                    self.push_choice(Task::RepeatDone { min, count });
                    clear_owned(&mut self.caps, owned);
                    self.tasks.push(Task::RepeatIterDone {
                        node,
                        owned,
                        min,
                        max,
                        greedy: true,
                        count,
                        iter_start: start,
                    });
                    self.tasks.push(Task::Node(node));
                    true
                }
            }
            Task::LazyRepeat {
                node,
                owned,
                min,
                max,
                count,
            } => {
                if count >= min {
                    // Try the continuation below; on failure the choice
                    // resumes with one iteration.
                    self.push_choice(Task::LazyIterate {
                        node,
                        owned,
                        min,
                        max,
                        count,
                    });
                    true
                } else if max.is_some_and(|m| count >= m) {
                    false
                } else {
                    clear_owned(&mut self.caps, owned);
                    let start = self.pos;
                    self.tasks.push(Task::RepeatIterDone {
                        node,
                        owned,
                        min,
                        max,
                        greedy: false,
                        count,
                        iter_start: start,
                    });
                    self.tasks.push(Task::Node(node));
                    true
                }
            }
            Task::RepeatIterDone {
                node,
                owned,
                min,
                max,
                greedy,
                count,
                iter_start,
            } => {
                // An iteration that made no progress terminates the loop: a
                // greedy repeat must not spin on empty matches once the
                // minimum is satisfied (spec RepeatMatcher step 2.b).
                let empty = self.pos == iter_start;
                if empty && count >= min {
                    // Reject the empty iteration: the repeat is done with
                    // `count` iterations; backtracking shrinks it.
                    false
                } else if empty && count + 1 >= min {
                    // The empty iteration counts toward the minimum; the
                    // repeat stops here.
                    true
                } else {
                    let next = if greedy {
                        Task::GreedyRepeat {
                            node,
                            owned,
                            min,
                            max,
                            count: count + 1,
                        }
                    } else {
                        Task::LazyRepeat {
                            node,
                            owned,
                            min,
                            max,
                            count: count + 1,
                        }
                    };
                    self.tasks.push(next);
                    true
                }
            }
            Task::AssertDone {
                negate,
                start,
                outer_dir,
                mark,
                cut,
            } => {
                self.dir = outer_dir;
                if negate {
                    // The sub-match succeeded but the assertion is negated:
                    // fail, discarding the sub-match's choices and captures.
                    self.caps.rollback(mark);
                    self.choices.truncate(cut);
                    false
                } else {
                    // Positive assertion: commit to this sub-match path
                    // (captures persist), reset the position, and drop the
                    // sub-match's choice points.
                    self.pos = start;
                    self.choices.truncate(cut);
                    true
                }
            }
            Task::RepeatDone { min, count } => count >= min,
            Task::LazyIterate {
                node,
                owned,
                min,
                max,
                count,
            } => {
                if max.is_some_and(|m| count >= m) {
                    false
                } else {
                    clear_owned(&mut self.caps, owned);
                    let start = self.pos;
                    self.tasks.push(Task::RepeatIterDone {
                        node,
                        owned,
                        min,
                        max,
                        greedy: false,
                        count,
                        iter_start: start,
                    });
                    self.tasks.push(Task::Node(node));
                    true
                }
            }
            Task::AssertFailed {
                negate,
                start,
                outer_dir,
            } => {
                self.pos = start;
                self.dir = outer_dir;
                negate
            }
            Task::ClassAdvance { len } => {
                self.pos = advance(self.pos, self.dir, len);
                true
            }
            Task::FastShrink {
                atom,
                consumed,
                entry_pos,
                owned,
                min,
                base_mark,
                cont,
                level,
            } => {
                // The continuation at `level` iterations failed; shrink to
                // `level - 1` (no lower level below the minimum exists).
                if (level as u32) <= min {
                    // Every shrink level is dead. Memoize the exhaustion only
                    // when it is sound for all re-entries: no backrefs (the
                    // continuation's acceptance is capture-independent) and no
                    // `{2,}`-or-tighter enclosing repeat (the continuation is
                    // acceptance-equivalent at every re-entry).
                    if self.backref_free && self.memo_safe.contains(&(atom as *const Node as usize))
                    {
                        self.memo
                            .insert((atom as *const Node as usize, entry_pos), ());
                    }
                    return false;
                }
                let level = level - 1;
                let pos = if level == 0 {
                    entry_pos
                } else {
                    consumed[level]
                };
                self.pos = pos;
                if level > 0 {
                    // The last iteration of the shrunk repeat spans
                    // (consumed[level-1], consumed[level]); normalize for
                    // right-to-left matching in a lookbehind.
                    let span = (
                        consumed[level - 1].min(consumed[level]),
                        consumed[level - 1].max(consumed[level]),
                    );
                    for &index in owned {
                        self.caps.set(index, span);
                    }
                }
                // Re-arm the shrink choice for the next failure.
                self.choices.push(Choice {
                    pos,
                    mark: base_mark,
                    cont: cont.clone(),
                    resume: Some(Task::FastShrink {
                        atom,
                        consumed: consumed.clone(),
                        entry_pos,
                        owned,
                        min,
                        base_mark,
                        cont,
                        level,
                    }),
                });
                true
            }
        }
    }

    /// Execute one node: structural nodes push follow-up tasks; atomic nodes
    /// consume input and report success or failure.
    fn step_node(&mut self, node: &'a Node) -> bool {
        match node {
            Node::Empty => true,
            Node::Char { cp, fold } => match read_char_dir(&self.ctx, self.pos, self.dir) {
                Some((c, len)) => {
                    let c = if *fold {
                        canonicalize(self.ctx.unicode, c)
                    } else {
                        c
                    };
                    if c == *cp {
                        self.pos = advance(self.pos, self.dir, len);
                        true
                    } else {
                        false
                    }
                }
                None => false,
            },
            Node::Any { dot_all } => match read_char_dir(&self.ctx, self.pos, self.dir) {
                Some((c, len)) if *dot_all || !unicode::is_line_terminator(c) => {
                    self.pos = advance(self.pos, self.dir, len);
                    true
                }
                _ => false,
            },
            Node::Start { multiline } => {
                self.pos == 0
                    || (*multiline
                        && self.pos > 0
                        && is_line_terminator_at(&self.ctx, self.pos - 1))
            }
            Node::End { multiline } => {
                let len = self.ctx.input.len();
                self.pos == len
                    || (*multiline && self.pos < len && is_line_terminator_at(&self.ctx, self.pos))
            }
            Node::WordBoundary { extra_folded } => {
                let before =
                    self.pos > 0 && is_word_char_at(&self.ctx, self.pos - 1, *extra_folded);
                let after = self.pos < self.ctx.input.len()
                    && is_word_char_at(&self.ctx, self.pos, *extra_folded);
                before != after
            }
            Node::NotWordBoundary { extra_folded } => {
                let before =
                    self.pos > 0 && is_word_char_at(&self.ctx, self.pos - 1, *extra_folded);
                let after = self.pos < self.ctx.input.len()
                    && is_word_char_at(&self.ctx, self.pos, *extra_folded);
                before == after
            }
            Node::Class(class) => {
                // A string member (`\q{…}`, spec ClassStringDisjunction)
                // consumes its full code-unit length, not one character.
                // The first matching string commits, and each later one
                // becomes a backtracking alternative (a class can match
                // several strings at one position, e.g. `[\q{a|ab}]` at
                // "ab"); the single-char membership test is the final
                // alternative. Negated classes never consume strings (the
                // complement of a set containing a string has no string
                // members).
                if !class.negated {
                    let mut committed: Option<usize> = None;
                    for string in &class.strings {
                        if string_at_dir(&self.ctx, self.pos, string, self.dir) {
                            let len = string_units(&self.ctx, string);
                            match committed {
                                None => committed = Some(len),
                                Some(_) => self.push_choice(Task::ClassAdvance { len }),
                            }
                        }
                    }
                    if let Some(len) = committed {
                        self.pos = advance(self.pos, self.dir, len);
                        return true;
                    }
                }
                match read_char_dir(&self.ctx, self.pos, self.dir) {
                    Some((c, len)) if class_matches(&self.ctx, class, self.pos, c, self.dir) => {
                        self.pos = advance(self.pos, self.dir, len);
                        true
                    }
                    _ => false,
                }
            }
            Node::Sequence(nodes) => {
                self.tasks.push(Task::SeqElem(nodes, 0));
                true
            }
            Node::Alternate(alts) => {
                for alt in alts.iter().skip(1).rev() {
                    self.push_choice(Task::SeqElem(alt, 0));
                }
                self.tasks.push(Task::SeqElem(&alts[0], 0));
                true
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
                owned_captures,
            } => {
                let task = if *greedy {
                    Task::GreedyRepeat {
                        node,
                        owned: owned_captures,
                        min: *min,
                        max: *max,
                        count: 0,
                    }
                } else {
                    Task::LazyRepeat {
                        node,
                        owned: owned_captures,
                        min: *min,
                        max: *max,
                        count: 0,
                    }
                };
                self.tasks.push(task);
                true
            }
            Node::Capture { index, node } => {
                let start = self.pos;
                // Record the capture as empty before the body runs so a
                // backreference inside it sees the (pos, pos) span (spec
                // CaptureMatcher).
                self.caps.set(*index, (start, start));
                self.tasks.push(Task::CaptureClose {
                    index: *index,
                    start,
                });
                self.tasks.push(Task::Node(node));
                true
            }
            Node::Backref { indices, fold } => {
                // A duplicate name resolves to the last of its groups that
                // participated (spec: the match of the last matching group).
                let chosen = indices
                    .iter()
                    .rev()
                    .find(|&&i| self.caps.values[i].is_some())
                    .copied();
                match chosen.and_then(|i| self.caps.values[i]) {
                    Some((s, e)) => {
                        let len = e - s;
                        let (window_start, window_end) = if self.dir > 0 {
                            (self.pos, self.pos + len)
                        } else {
                            (self.pos - len, self.pos)
                        };
                        if window_end > self.ctx.input.len() || window_start > window_end {
                            return false;
                        }
                        let captured = &self.ctx.input[s..e];
                        let window = &self.ctx.input[window_start..window_end];
                        if units_eq(&self.ctx, captured, window, *fold) {
                            self.pos = advance(self.pos, self.dir, len);
                            true
                        } else {
                            false
                        }
                    }
                    // A backreference to a group that has not participated
                    // matches the empty string (spec BackreferenceMatcher,
                    // `captures[cp]` undefined).
                    None => true,
                }
            }
            Node::Lookahead { negate, node } => self.assert_start(*negate, node, 1),
            Node::Lookbehind { negate, node } => self.assert_start(*negate, node, -1),
        }
    }

    /// Begin a lookaround: run the sub-match at `sub_dir`, committing to its
    /// first success (positive) or failing on it (negated).
    fn assert_start(&mut self, negate: bool, node: &'a Node, sub_dir: i32) -> bool {
        let mark = self.caps.mark();
        let cut = self.choices.len();
        let start = self.pos;
        // A guard choice catches the sub-match's failure: the negative
        // assertion turns it into success, the positive one propagates it.
        self.push_choice(Task::AssertFailed {
            negate,
            start,
            outer_dir: self.dir,
        });
        self.tasks.push(Task::AssertDone {
            negate,
            start,
            outer_dir: self.dir,
            mark,
            cut,
        });
        self.tasks.push(Task::Node(node));
        self.dir = sub_dir;
        true
    }

    /// Pop the top choice point and restore its forward path; returns false
    /// when the choice stack is exhausted (total failure).
    fn backtrack(&mut self) -> bool {
        let Some(choice) = self.choices.pop() else {
            return false;
        };
        self.pos = choice.pos;
        self.caps.rollback(choice.mark);
        // Reuse the task buffer (the continuation is shallow, so this rarely
        // allocates after the first restore).
        self.tasks.clear();
        self.tasks.extend_from_slice(&choice.cont);
        if let Some(resume) = choice.resume {
            self.tasks.push(resume);
        }
        true
    }

    /// The simple-atom repeat fast path: a greedy repeat of a literal, dot,
    /// character class (without `\q` string members), or a linear sequence
    /// of them — possibly wrapped in captures — consumes iteratively, then
    /// records one choice per shrink level, all sharing the continuation
    /// snapshot (which is stable while the repeat runs). This avoids a
    /// choice (and its continuation clone) per consumed character, which
    /// dominates on multi-megabyte inputs (the generated property-escape
    /// fixtures build ~2M-unit match strings).
    fn greedy_simple(
        &mut self,
        atom: &'a Node,
        owned: &'a [usize],
        min: u32,
        max: Option<u32>,
        count: u32,
    ) -> bool {
        let entry_pos = self.pos;
        clear_owned(&mut self.caps, owned);
        // Consume iteratively, recording each iteration's start position.
        let mut consumed: Vec<usize> = Vec::new();
        let mut cur = self.pos;
        let mut cnt = count;
        while max.is_none_or(|m| cnt < m) {
            match self.simple_end(atom, cur) {
                Some(end) => {
                    consumed.push(cur);
                    cur = end;
                    cnt += 1;
                }
                None => break,
            }
        }
        self.pos = cur;
        if cnt < min {
            // The atom cannot satisfy the minimum from here. The atom's
            // consumption is deterministic, so this failure is position-only:
            // skip the repeat on any re-entry at this position.
            self.memo
                .insert((atom as *const Node as usize, entry_pos), ());
            return false;
        }
        if cnt > min {
            // A single choice per shrink level, re-armed by `FastShrink`;
            // the continuation below the repeat is stable while it runs, so
            // one snapshot serves every shrink. The mark is taken before the
            // capture write so backtracking restores it.
            let base_mark = self.caps.mark();
            self.set_fast_captures(owned, &consumed, cur);
            let consumed: std::rc::Rc<[usize]> = std::rc::Rc::from(consumed.as_slice());
            let cont: std::rc::Rc<[Task<'a>]> = std::rc::Rc::from(self.tasks.as_slice());
            self.choices.push(Choice {
                pos: cur,
                mark: base_mark,
                cont: cont.clone(),
                resume: Some(Task::FastShrink {
                    atom,
                    consumed: consumed.clone(),
                    entry_pos,
                    owned,
                    min,
                    base_mark,
                    cont,
                    level: cnt as usize,
                }),
            });
        } else {
            self.set_fast_captures(owned, &consumed, cur);
        }
        // Continue with the continuation (the tasks below the repeat).
        true
    }

    /// The capture values visible to the continuation after a simple repeat:
    /// the last iteration's span, or None when the repeat matched zero
    /// iterations (spec RepeatMatcher).
    fn set_fast_captures(&mut self, owned: &[usize], consumed: &[usize], cur: usize) {
        if owned.is_empty() {
            return;
        }
        match consumed.last() {
            Some(&start) => {
                let span = (start.min(cur), start.max(cur));
                for &index in owned {
                    self.caps.set(index, span);
                }
            }
            None => {
                for &index in owned {
                    self.caps.values[index] = None;
                }
            }
        }
    }

    /// The endpoint a simple atom matches at `pos`, or None. Only called
    /// with atoms admitted by `simple_atom` (deterministic, consuming).
    fn simple_end(&mut self, atom: &Node, pos: usize) -> Option<usize> {
        match atom {
            Node::Char { cp, fold } => {
                let (c, len) = read_char_dir(&self.ctx, pos, self.dir)?;
                let c = if *fold {
                    canonicalize(self.ctx.unicode, c)
                } else {
                    c
                };
                (c == *cp).then(|| advance(pos, self.dir, len))
            }
            Node::Any { dot_all } => {
                let (c, len) = read_char_dir(&self.ctx, pos, self.dir)?;
                (*dot_all || !unicode::is_line_terminator(c)).then(|| advance(pos, self.dir, len))
            }
            Node::Class(class) => {
                let (c, len) = read_char_dir(&self.ctx, pos, self.dir)?;
                class_matches(&self.ctx, class, pos, c, self.dir)
                    .then(|| advance(pos, self.dir, len))
            }
            Node::Sequence(nodes) => {
                let mut p = pos;
                for n in nodes {
                    p = self.simple_end(n, p)?;
                }
                Some(p)
            }
            _ => unreachable!("simple_atom admits only Char/Any/Class/linear sequences"),
        }
    }
}

/// Whether the pattern contains no backreferences anywhere. Without them the
/// continuation after a repeat never reads captures, so the repeat's failure
/// at a position is independent of the capture state backtracking left behind
/// — the prerequisite for memoizing "exhausted" per (atom, position).
/// Computed once at compile time and stored on the `Regex`.
pub(crate) fn is_backref_free(node: &Node) -> bool {
    match node {
        Node::Empty
        | Node::Char { .. }
        | Node::Any { .. }
        | Node::Start { .. }
        | Node::End { .. }
        | Node::WordBoundary { .. }
        | Node::NotWordBoundary { .. }
        | Node::Class(_) => true,
        Node::Sequence(nodes) => nodes.iter().all(is_backref_free),
        Node::Alternate(alts) => alts.iter().flatten().all(is_backref_free),
        Node::Repeat { node, .. }
        | Node::Capture { node, .. }
        | Node::Lookahead { node, .. }
        | Node::Lookbehind { node, .. } => is_backref_free(node),
        Node::Backref { .. } => false,
    }
}

/// The peeled atoms of the repeats eligible for the exhausted memo: repeats
/// whose enclosing repeats all have `min <= 1` (see `Matcher::memo_safe`).
/// Computed once at compile time and stored on the `Regex`.
pub(crate) fn memo_safe_atoms(node: &Node) -> std::collections::HashSet<usize> {
    fn walk(node: &Node, encl_min_ok: bool, out: &mut std::collections::HashSet<usize>) {
        match node {
            Node::Repeat { node, min, .. } => {
                if encl_min_ok && let Some(atom) = simple_atom(node) {
                    out.insert(atom as *const Node as usize);
                }
                walk(node, encl_min_ok && *min <= 1, out);
            }
            Node::Sequence(nodes) => {
                for n in nodes {
                    walk(n, encl_min_ok, out);
                }
            }
            Node::Alternate(alts) => {
                for alt in alts {
                    for n in alt {
                        walk(n, encl_min_ok, out);
                    }
                }
            }
            Node::Capture { node, .. }
            | Node::Lookahead { node, .. }
            | Node::Lookbehind { node, .. } => walk(node, encl_min_ok, out),
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    walk(node, true, &mut out);
    out
}

/// The atom a greedy repeat can consume iteratively: a literal, dot, or
/// character class without `\q` string members, unwrapped through captures
/// and one-element sequences, or a non-empty linear sequence of such atoms.
/// String classes can match several alternatives at one position and anything
/// branching needs the general path.
fn simple_atom(node: &Node) -> Option<&Node> {
    match node {
        Node::Char { .. } | Node::Any { .. } => Some(node),
        Node::Class(class) if class.strings.is_empty() => Some(node),
        Node::Capture { node: inner, .. } => simple_atom(inner),
        Node::Sequence(nodes) if nodes.len() == 1 => simple_atom(&nodes[0]),
        Node::Sequence(nodes) if !nodes.is_empty() && nodes.iter().all(is_linear_atom) => {
            Some(node)
        }
        _ => None,
    }
}

/// A single atom with at most one match endpoint per position.
fn is_linear_atom(node: &Node) -> bool {
    matches!(node, Node::Char { .. } | Node::Any { .. })
        || matches!(node, Node::Class(class) if class.strings.is_empty())
}

/// The public entry: match the program at `start`, returning capture spans in
/// code units.
pub(crate) fn exec(re: &Regex, input: &[u16], start: usize) -> Option<crate::Match> {
    let mut matcher = Matcher::new(re, input);
    let end = matcher.match_at(start)?;
    matcher.caps.values[0] = Some((start, end));
    Some(matcher.caps.values)
}

/// Leftmost search with a leading-char prefilter and a single capture buffer
/// reused across position attempts (spec RegExpBuiltinExec's loop). Returns
/// the match and the index it started at.
pub(crate) fn search(re: &Regex, input: &[u16], start: usize) -> Option<(usize, crate::Match)> {
    let mut matcher = Matcher::new(re, input);
    let mut index = start;
    while index <= input.len() {
        if let Some(set) = &re.prefilter
            && index < input.len()
            && !ranges_contain(set, input[index] as u32)
        {
            // Skip in lockstep with AdvanceStringIndex: under `/u` a skipped
            // position may be the low half of a surrogate pair that the
            // spec's loop never visits (matching there would be a spurious
            // result, e.g. `\udf06/u` on a surrogate pair).
            index = re.advance_string_index(input, index);
            continue;
        }
        if let Some(end) = matcher.match_at(index) {
            matcher.caps.values[0] = Some((index, end));
            return Some((index, matcher.caps.values));
        }
        if index >= input.len() {
            break;
        }
        index = re.advance_string_index(input, index);
    }
    None
}

/// A sound prefilter for the search loop: the UTF-16 units that any match
/// starting at a position must begin with, or None when the pattern can match
/// empty or its first consumed character is unconstrained. Computed once at
/// compile time and stored on the `Regex`. Positions whose first unit is not
/// in the set are skipped; false positives are fine (the matcher re-verifies),
/// false negatives would be bugs.
pub(crate) fn search_prefilter(program: &Node, unicode: bool) -> Option<Vec<(u32, u32)>> {
    let (can_match_empty, first_cps) = first_char_analysis(program, unicode);
    if can_match_empty {
        return None;
    }
    let cps = first_cps?;
    let mut units: Vec<(u32, u32)> = Vec::new();
    for &(a, b) in &cps {
        if a <= 0xFFFF {
            units.push((a, b.min(0xFFFF)));
        }
        if b > 0xFFFF {
            if !unicode {
                // A legacy-mode range beyond the BMP would make the unit set
                // incomplete; refuse the prefilter rather than skip wrongly.
                return None;
            }
            // Non-BMP code points start with their high surrogate.
            let lo = 0xD800 + ((a.max(0x10000) - 0x10000) >> 10);
            let hi = 0xD800 + ((b - 0x10000) >> 10);
            units.push((lo, hi));
        }
    }
    Some(ranges_union(&units, &[]))
}

/// (can_match_empty, first_chars): whether `node` can match the empty string,
/// and the set of code points its first consumed character can be when it
/// consumes (None = unconstrained). Both must be sound over-approximations:
/// `first_chars` is the prefilter's skip set, so a missing member would drop
/// a real match.
fn first_char_analysis(node: &Node, unicode: bool) -> (bool, Option<Vec<(u32, u32)>>) {
    match node {
        Node::Sequence(nodes) => first_char_analysis_seq(nodes, unicode),
        Node::Alternate(alts) => {
            let mut acc: Vec<(u32, u32)> = Vec::new();
            let mut any_empty = false;
            for alt in alts {
                let (can_empty, set) = first_char_analysis_seq(alt, unicode);
                any_empty |= can_empty;
                match set {
                    Some(s) => acc = ranges_union(&acc, &s),
                    None => return (any_empty, None),
                }
            }
            (any_empty, Some(acc))
        }
        Node::Empty => (true, None),
        Node::Char { cp, fold } => {
            if *fold {
                // The parser pre-canonicalizes the pattern char, so the
                // leading set is its full fold class (the preimage of its
                // canonical form, e.g. `k`/`K`/U+212A under `/iu`).
                (false, Some((*fold_class(*cp, unicode)).clone()))
            } else {
                (false, Some(vec![(*cp, *cp)]))
            }
        }
        Node::Any { .. } => (false, None),
        // Zero-width assertions constrain nothing about the consumed input.
        Node::Start { .. }
        | Node::End { .. }
        | Node::WordBoundary { .. }
        | Node::NotWordBoundary { .. } => (true, None),
        Node::Class(class) => {
            if class.negated || !class.strings.is_empty() {
                (false, None)
            } else if class.fold {
                match &class.predicate {
                    // A folded predicate's preimage needs the full code-point
                    // enumeration per compile; keep it bounded by refusing.
                    Some(_) => (false, None),
                    None => {
                        // The stored ranges are already forward-folded; the
                        // leading set of a folded class is the preimage of
                        // those canonicals, so union each member's class.
                        let mut acc: Vec<(u32, u32)> = Vec::new();
                        let mut members = 0usize;
                        for &(s, e) in &class.ranges {
                            members += (e - s + 1) as usize;
                            // Bound the per-member lookups so a huge class
                            // (e.g. a folded complement) does not slow
                            // compile; refusing keeps the search correct.
                            if members > 4096 {
                                return (false, None);
                            }
                            for cp in s..=e {
                                acc = ranges_union(&acc, &fold_class(cp, unicode));
                            }
                        }
                        (false, Some(acc))
                    }
                }
            } else {
                match &class.predicate {
                    Some(pred) => {
                        let ranges = predicate_ranges(pred);
                        // A predicate covering the whole space skips nothing;
                        // treat it as unconstrained.
                        if ranges.len() == 1 && ranges[0] == (0, 0x10FFFF) {
                            (false, None)
                        } else {
                            (false, Some((*ranges).clone()))
                        }
                    }
                    None => (false, Some(class.ranges.clone())),
                }
            }
        }
        Node::Repeat { node, min, .. } => {
            let (atom_empty, set) = first_char_analysis(node, unicode);
            // A zero-minimum repeat can always match empty; a positive one
            // matches empty exactly when its atom can.
            let can_empty = if *min == 0 { true } else { atom_empty };
            (can_empty, set)
        }
        Node::Capture { node, .. } => first_char_analysis(node, unicode),
        Node::Backref { .. } => (false, None),
        Node::Lookahead { .. } | Node::Lookbehind { .. } => (true, None),
    }
}

/// The sequence half of `first_char_analysis`, shared by `Sequence` nodes and
/// the bare alternatives of `Alternate`.
fn first_char_analysis_seq(nodes: &[Node], unicode: bool) -> (bool, Option<Vec<(u32, u32)>>) {
    let mut acc: Vec<(u32, u32)> = Vec::new();
    for n in nodes {
        // Zero-width, unconstrained terms are transparent to the first
        // consumed character.
        if matches!(
            n,
            Node::Empty
                | Node::Start { .. }
                | Node::End { .. }
                | Node::WordBoundary { .. }
                | Node::NotWordBoundary { .. }
                | Node::Lookahead { .. }
                | Node::Lookbehind { .. }
        ) {
            continue;
        }
        let (can_empty, set) = first_char_analysis(n, unicode);
        if can_empty {
            match set {
                Some(s) => acc = ranges_union(&acc, &s),
                // An empty-matching element with an unconstrained first char:
                // the rest could supply anything.
                None => return (false, None),
            }
        } else {
            let first = match set {
                Some(s) => ranges_union(&acc, &s),
                None => return (false, None),
            };
            return (false, Some(first));
        }
    }
    (true, None)
}

/// A position advanced by `len` input elements in the match direction.
fn advance(pos: usize, dir: i32, len: usize) -> usize {
    if dir > 0 { pos + len } else { pos - len }
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
        // Digits/Word/Space are cheap inline checks. The property predicates
        // (General_Category, Script, Script_Extensions, binary properties)
        // match against the process-cached per-predicate code-point ranges
        // (built once per predicate) instead of a per-character property
        // table lookup — the property-escape fixtures run \p{…} over
        // ~2.1M-char strings, so the name lookup per character dominates.
        Some(pred) if matches!(pred, Predicate::Digits | Predicate::Word | Predicate::Space) => {
            predicate_matches(pred, cc)
        }
        Some(pred) => ranges_contain(&predicate_ranges(pred), cc),
        None => ranges_contain(&class.ranges, cc),
    } || class
        .strings
        .iter()
        .any(|s| string_at_dir(ctx, pos, s, dir));
    in_set != class.negated
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
