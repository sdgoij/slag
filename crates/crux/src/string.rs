//! JavaScript strings: UTF-16 code-unit sequences (spec 6.1.4) and the string
//! interner backing `AtomId`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::handle::Handle;

/// A JavaScript string: a sequence of UTF-16 code units (spec 6.1.4).
///
/// A string is either a single contiguous buffer or a rope — a binary tree of
/// concatenation nodes. `concat` appends in O(1) once the accumulated string
/// is large enough that repeated copying would dominate, and the contiguous
/// form is materialized lazily on first access (and cached, since strings are
/// immutable). `len` is O(1) for both forms.
///
/// The rope node IS the box the value points at (`Handle<JsString>`), so an
/// append is a single allocation; the children are the operands' own handles
/// (Rc bumps, no copies). That makes ropes `!Send`, which is fine: the one
/// `Send` consumer — the well-known-symbol table — keeps its symbols
/// thread-locally instead. Flat buffers are `Arc`-shared, so flat clones are
/// O(1). The rope tracks a tree depth and folds an over-deep left side into a
/// single shared flat; both the fold and the final drop are amortized across
/// the cap appends, so arbitrarily long append chains stay linear.
pub enum JsString {
    Flat(Arc<[u16]>),
    Rope {
        left: Option<Handle<JsString>>,
        right: Option<Handle<JsString>>,
        len: usize,
        /// The rope's tree depth (0 for a flat leaf): concat folds the left
        /// side into one shared flat once it would exceed `ROPE_MAX_DEPTH`.
        /// Bounding the depth keeps the final drop (and any single flatten) at
        /// O(cap) nodes. u32 is far beyond any real append chain.
        depth: u32,
        /// The materialized contiguous form, computed on first access; immutable
        /// once set (strings are immutable), which is what makes `as_slice`
        /// return a stable reference. (Rope clones get a fresh cache — a rope
        /// flattened through two handles flattens twice.)
        flat: OnceLock<Arc<[u16]>>,
    },
}

/// Concatenations whose total length is at or below this stay flat: the rope
/// machinery (a node allocation plus a later flatten) only pays off once the
/// string is large enough that repeated copies would dominate.
const CONCAT_FLAT_THRESHOLD: usize = 16;

/// The deepest a rope may grow before concat folds the accumulated left side
/// into a single shared flat. The fold copies the accumulated string, so the
/// total is ~n²/(2·cap) units — at cap 16384 that is ~305k units (0.6 MB) for
/// a 100k-unit append chain and ~30M units (60 MB) for a 1M-unit chain,
/// versus a cap-sized final tree of ~cap nodes to drop either way. (The fold
/// shares the materialized flat via the Arc — O(1) after the one copy.)
const ROPE_MAX_DEPTH: usize = 16384;

impl JsString {
    pub fn from_utf16(units: &[u16]) -> Self {
        JsString::Flat(units.into())
    }

    pub fn from_utf8(text: &str) -> Self {
        JsString::Flat(text.encode_utf16().collect())
    }

    /// Concatenate without copying when the result is large enough: a rope
    /// node that IS the result box — one allocation per append. Small results
    /// (and empty operands) stay flat or reuse an operand. The operands' own
    /// boxes become the node's children (refcount bumps, no copies).
    pub fn concat(left: &Handle<JsString>, right: &Handle<JsString>) -> Handle<JsString> {
        if right.is_empty() {
            return left.clone();
        }
        if left.is_empty() {
            return right.clone();
        }
        let len = left.len() + right.len();
        if len <= CONCAT_FLAT_THRESHOLD {
            let mut units = Vec::with_capacity(len);
            units.extend_from_slice(left.as_slice());
            units.extend_from_slice(right.as_slice());
            return Handle::new(JsString::Flat(units.into()));
        }
        // Fold an over-deep left side into one shared flat so the tree stays
        // shallow (see ROPE_MAX_DEPTH): the materialization copy is amortized
        // across the cap appends, and the Arc share makes the fold itself
        // O(1). Appends between folds are a single node allocation.
        let left = if left.depth() >= ROPE_MAX_DEPTH as u32 {
            Handle::new(JsString::Flat(left.flat_arc()))
        } else {
            left.clone()
        };
        let depth = 1 + left.depth().max(right.depth());
        Handle::new(JsString::Rope {
            left: Some(left),
            right: Some(right.clone()),
            len,
            depth,
            flat: OnceLock::new(),
        })
    }

    /// The tree depth: 0 for a flat string, else the node's cached depth.
    fn depth(&self) -> u32 {
        match self {
            JsString::Flat(_) => 0,
            JsString::Rope { depth, .. } => *depth,
        }
    }

    /// The materialized contiguous form as a shared buffer, materializing the
    /// rope first if needed. The Arc share lets `concat`'s depth fold reuse
    /// the buffer instead of copying it a second time.
    fn flat_arc(&self) -> Arc<[u16]> {
        match self {
            JsString::Flat(arc) => arc.clone(),
            JsString::Rope { flat, .. } => flat.get_or_init(|| self.flatten()).clone(),
        }
    }

    /// The number of code units (spec 6.1.4.1 StringLength).
    pub fn len(&self) -> usize {
        match self {
            JsString::Flat(units) => units.len(),
            JsString::Rope { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u16] {
        match self {
            JsString::Flat(units) => units,
            JsString::Rope { flat, .. } => flat.get_or_init(|| self.flatten()).as_ref(),
        }
    }

    pub fn code_unit(&self, index: usize) -> Option<u16> {
        self.as_slice().get(index).copied()
    }

    /// spec 6.1.4.3 CodePointAt: `(code point, is unpaired surrogate, code units consumed)`.
    pub fn code_point_at(&self, index: usize) -> Option<(u32, bool, usize)> {
        let units = self.as_slice();
        let hi = *units.get(index)?;
        if (0xD800..=0xDBFF).contains(&hi) {
            if let Some(&lo) = units.get(index + 1)
                && (0xDC00..=0xDFFF).contains(&lo)
            {
                let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                return Some((cp, false, 2));
            }
            Some((hi as u32, true, 1))
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            Some((hi as u32, true, 1))
        } else {
            Some((hi as u32, false, 1))
        }
    }

    /// Iterate the code points (spec 6.1.4 StringToCodePoints), preserving
    /// lone surrogates as their own code point.
    pub fn code_points(&self) -> impl Iterator<Item = u32> + '_ {
        CodePointIter { s: self, pos: 0 }
    }

    pub fn to_code_points(&self) -> Vec<u32> {
        self.code_points().collect()
    }

    /// Lossy UTF-8 rendering for diagnostics; lone surrogates become U+FFFD.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.as_slice())
    }

    /// The concatenation of the leaves, in order. Iterative (an explicit
    /// stack) so a deep left-leaning chain of small appends cannot overflow
    /// the call stack.
    fn flatten(&self) -> Arc<[u16]> {
        let mut units = Vec::with_capacity(self.len());
        // Pop the left side first, so the leaves emit in order. Children are
        // always `Some` until the node is being dropped; a missing side is
        // skipped defensively.
        let mut stack = vec![self];
        while let Some(string) = stack.pop() {
            match string {
                JsString::Flat(flat) => units.extend_from_slice(flat),
                JsString::Rope { left, right, .. } => {
                    if let Some(right) = right.as_deref() {
                        stack.push(right);
                    }
                    if let Some(left) = left.as_deref() {
                        stack.push(left);
                    }
                }
            }
        }
        // Vec → Arc reuses the buffer (exact capacity), no second copy.
        Arc::from(units)
    }
}

impl Drop for JsString {
    fn drop(&mut self) {
        // A long append chain is hundreds of thousands of nodes deep; dropping
        // the children recursively would overflow the stack. Unwind
        // iteratively: `take` the children out (unique ownership — the box's
        // own field drop then finds `None`), unwrap any we uniquely own, and
        // keep going (shared children just decrement).
        let mut work: Vec<Handle<JsString>> = Vec::new();
        if let JsString::Rope { left, right, .. } = self {
            if let Some(left) = left.take() {
                work.push(left);
            }
            if let Some(right) = right.take() {
                work.push(right);
            }
        } else {
            return;
        }
        while let Some(handle) = work.pop() {
            match Handle::try_unwrap(handle) {
                Ok(mut js) => {
                    if let JsString::Rope { left, right, .. } = &mut js {
                        if let Some(left) = left.take() {
                            work.push(left);
                        }
                        if let Some(right) = right.take() {
                            work.push(right);
                        }
                    }
                }
                Err(handle) => drop(handle),
            }
        }
    }
}

impl Clone for JsString {
    fn clone(&self) -> Self {
        // Flat strings share their buffer (Arc bump, O(1)); rope clones share
        // their children (Rc bumps) and get a fresh flat cache — strings are
        // immutable, so sharing is never observable.
        match self {
            JsString::Flat(units) => JsString::Flat(units.clone()),
            JsString::Rope {
                left,
                right,
                len,
                depth,
                ..
            } => JsString::Rope {
                left: left.clone(),
                right: right.clone(),
                len: *len,
                depth: *depth,
                flat: OnceLock::new(),
            },
        }
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for JsString {}

impl std::hash::Hash for JsString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for JsString {
    /// Diagnostics show the text (not the unit numbers or the rope shape);
    /// the rope derive would recurse the tree and overflow on deep chains.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

struct CodePointIter<'a> {
    s: &'a JsString,
    pos: usize,
}

impl Iterator for CodePointIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let (cp, _, count) = self.s.code_point_at(self.pos)?;
        self.pos += count;
        Some(cp)
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

/// spec 6.1.4.5 CodePointsToString.
pub fn code_points_to_string(code_points: &[u32]) -> Result<JsString, crate::JsError> {
    let mut units = Vec::with_capacity(code_points.len());
    for &cp in code_points {
        if cp <= 0xFFFF {
            units.push(cp as u16);
        } else if cp <= 0x10FFFF {
            let x = cp - 0x10000;
            units.push(0xD800 + (x >> 10) as u16);
            units.push(0xDC00 + (x & 0x3FF) as u16);
        } else {
            return Err(crate::JsError::new(
                crate::ErrorKind::RangeError,
                "Invalid code point".into(),
            ));
        }
    }
    Ok(JsString::from_utf16(&units))
}

/// An interned string identifier; property keys use these for O(1) equality.
pub type AtomId = u32;

#[derive(Default)]
struct Interner {
    map: HashMap<Box<[u16]>, AtomId>,
    atoms: Vec<Box<[u16]>>,
}

impl Interner {
    fn intern(&mut self, units: &[u16]) -> AtomId {
        if let Some(&id) = self.map.get(units) {
            return id;
        }
        let id = self.atoms.len() as AtomId;
        let key: Box<[u16]> = units.into();
        self.map.insert(key.clone(), id);
        self.atoms.push(key);
        id
    }

    fn lookup(&self, id: AtomId) -> &[u16] {
        &self.atoms[id as usize]
    }
}

static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();

fn with_interner<R>(f: impl FnOnce(&mut Interner) -> R) -> R {
    let mut guard = INTERNER
        .get_or_init(|| Mutex::new(Interner::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// The identifier hot path converts `AtomId`→`JsString` (lookup) and
/// `JsString`→`AtomId` (intern) several times per identifier read. Both take
/// the global interner lock and re-hash/copy the units every call; the
/// interpreter does this for the same handful of names in a loop, so a small
/// per-thread memo turns those into linear scans over a few cached entries.
/// The memo is a pure cache of the append-only interner: entries can never go
/// stale, so eviction is just a size cap.
const MEMO_CAP: usize = 64;

thread_local! {
    static LOOKUP_MEMO: std::cell::RefCell<Vec<(AtomId, JsString)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static INTERN_MEMO: std::cell::RefCell<Vec<(Box<[u16]>, AtomId)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Interns `units`, returning a stable id for it.
pub fn intern(units: &[u16]) -> AtomId {
    INTERN_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((_, id)) = memo.iter().rev().find(|(key, _)| key.as_ref() == units) {
            return *id;
        }
        let id = with_interner(|i| i.intern(units));
        memo.push((units.into(), id));
        if memo.len() > MEMO_CAP {
            memo.clear();
        }
        id
    })
}

pub fn intern_utf8(text: &str) -> AtomId {
    let units: Vec<u16> = text.encode_utf16().collect();
    intern(&units)
}

/// Returns the interned text for `id`.
pub fn lookup(id: AtomId) -> JsString {
    LOOKUP_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((_, text)) = memo.iter().rev().find(|(cached, _)| *cached == id) {
            return text.clone();
        }
        let text = {
            let units = with_interner(|i| i.lookup(id).to_vec());
            JsString::from_utf16(&units)
        };
        memo.push((id, text.clone()));
        if memo.len() > MEMO_CAP {
            memo.clear();
        }
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_utf8_and_utf16_agree() {
        let a = JsString::from_utf8("hello");
        let b = JsString::from_utf16(&[
            b'h' as u16,
            b'e' as u16,
            b'l' as u16,
            b'l' as u16,
            b'o' as u16,
        ]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
        assert!(!a.is_empty());
        assert!(JsString::from_utf8("").is_empty());
    }

    #[test]
    fn code_unit_indexing() {
        let s = JsString::from_utf8("abc");
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(3), None);
    }

    #[test]
    fn code_point_at_bmp() {
        let s = JsString::from_utf8("A");
        assert_eq!(s.code_point_at(0), Some((0x41, false, 1)));
        assert_eq!(s.code_point_at(1), None);
    }

    #[test]
    fn code_point_at_surrogate_pair() {
        // U+1F600 as a surrogate pair.
        let s = JsString::from_utf16(&[0xD83D, 0xDE00]);
        assert_eq!(s.code_point_at(0), Some((0x1F600, false, 2)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn code_point_at_lone_surrogates() {
        let hi = JsString::from_utf16(&[0xD83D]);
        assert_eq!(hi.code_point_at(0), Some((0xD83D, true, 1)));
        let lo = JsString::from_utf16(&[0xDE00]);
        assert_eq!(lo.code_point_at(0), Some((0xDE00, true, 1)));
    }

    #[test]
    fn code_points_iterate_including_surrogates() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD83D, 0xDE00, 0xD800]);
        assert_eq!(s.to_code_points(), vec![0x61, 0x1F600, 0xD800]);
    }

    #[test]
    fn code_points_to_string_round_trips() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD83D, 0xDE00]);
        let cps = s.to_code_points();
        let back = code_points_to_string(&cps).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn code_points_to_string_rejects_out_of_range() {
        let err = code_points_to_string(&[0x110000]).unwrap_err();
        assert_eq!(err.kind, crate::ErrorKind::RangeError);
    }

    #[test]
    fn to_string_lossy_replaces_lone_surrogates() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD800]);
        assert_eq!(s.to_string_lossy(), "a\u{FFFD}");
        assert_eq!(s.to_string(), "a\u{FFFD}");
    }

    #[test]
    fn interning_is_stable_and_identity_based() {
        let a = intern_utf8("length");
        let b = intern_utf8("length");
        let c = intern_utf8("width");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(lookup(a).to_string_lossy(), "length");
        assert_eq!(lookup(c).to_string_lossy(), "width");
    }

    #[test]
    fn interning_preserves_utf16_content() {
        let s = JsString::from_utf16(&[0xD83D, 0xDE00]);
        let id = intern(s.as_slice());
        assert_eq!(lookup(id), s);
    }

    #[test]
    fn concat_small_stays_flat_and_equals_the_units() {
        let a = Handle::new(JsString::from_utf8("ab"));
        let b = Handle::new(JsString::from_utf8("cd"));
        let s = JsString::concat(&a, &b);
        assert!(matches!(*s, JsString::Flat(_)));
        assert_eq!(s.len(), 4);
        assert_eq!(
            s.as_slice(),
            &[b'a' as u16, b'b' as u16, b'c' as u16, b'd' as u16]
        );
    }

    #[test]
    fn concat_large_builds_a_rope_with_correct_content() {
        // Cross the flat threshold by accumulating a long string.
        let mut s = Handle::new(JsString::from_utf8(""));
        let leaf = Handle::new(JsString::from_utf8("x"));
        for _ in 0..64 {
            s = JsString::concat(&s, &leaf);
        }
        assert!(matches!(*s, JsString::Rope { .. }));
        assert_eq!(s.len(), 64);
        let units = s.as_slice();
        assert_eq!(units.len(), 64);
        assert!(units.iter().all(|&u| u == b'x' as u16));
        // The materialized form is cached: a second view is the same buffer.
        let again = s.as_slice();
        assert_eq!(units.as_ptr(), again.as_ptr());
    }

    #[test]
    fn rope_indexing_and_code_points() {
        let mut s = Handle::new(JsString::from_utf8("a"));
        let x = Handle::new(JsString::from_utf8("x"));
        for _ in 0..20 {
            s = JsString::concat(&s, &x);
        }
        let emoji = Handle::new(JsString::from_utf16(&[0xD83D, 0xDE00]));
        s = JsString::concat(&s, &emoji);
        assert_eq!(s.len(), 23); // 21 units + the surrogate pair
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(20), Some(b'x' as u16));
        assert_eq!(s.code_point_at(21), Some((0x1F600, false, 2)));
    }

    #[test]
    fn rope_equality_and_clone_correctness() {
        let mut left = Handle::new(JsString::from_utf8(""));
        let chunk = Handle::new(JsString::from_utf8("ab"));
        for _ in 0..32 {
            left = JsString::concat(&left, &chunk);
        }
        let mut right = Handle::new(JsString::from_utf8(""));
        for _ in 0..32 {
            right = JsString::concat(&right, &chunk);
        }
        assert_eq!(left.len(), 64);
        assert_eq!(left, right);
        assert_eq!(left.to_code_points(), right.to_code_points());
        // Clones share the rope's children (not its flat cache — each box has
        // its own), and materialize the same content.
        let cloned = left.clone();
        assert_eq!(cloned.as_slice(), left.as_slice());
        assert_eq!(cloned.as_slice().len(), left.as_slice().len());
    }

    #[test]
    fn concat_with_empty_operand_reuses_the_other_side() {
        let s = Handle::new(JsString::from_utf8("hello"));
        let empty = Handle::new(JsString::from_utf8(""));
        assert_eq!(JsString::concat(&s, &empty).as_slice(), s.as_slice());
        assert_eq!(JsString::concat(&empty, &s).as_slice(), s.as_slice());
        assert_eq!(JsString::concat(&empty, &empty).len(), 0);
    }

    #[test]
    fn deep_append_chain_drops_and_flattens_iteratively() {
        // Left-leaning appends: the depth cap folds the tree every
        // ROPE_MAX_DEPTH appends, so it stays shallow — the fold's
        // materialization and the final drop exercise the iterative paths.
        let mut s = Handle::new(JsString::from_utf8(""));
        let leaf = Handle::new(JsString::from_utf8("x"));
        for _ in 0..200_000 {
            s = JsString::concat(&s, &leaf);
        }
        assert_eq!(s.len(), 200_000);
        let units = s.as_slice();
        assert_eq!(units.len(), 200_000);
        assert!(units.iter().all(|&u| u == b'x' as u16));

        // Right-leaning prepends: the cap only inspects the left side, so the
        // depth grows unbounded — drop and flatten must still be iterative.
        let mut p = Handle::new(JsString::from_utf8(""));
        for _ in 0..200_000 {
            p = JsString::concat(&leaf, &p);
        }
        assert_eq!(p.len(), 200_000);
        let units = p.as_slice();
        assert_eq!(units.len(), 200_000);
        assert!(units.iter().all(|&u| u == b'x' as u16));
    }
}
