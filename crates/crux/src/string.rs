//! JavaScript strings: UTF-16 code-unit sequences (spec 6.1.4) and the string
//! interner backing `AtomId`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};

/// A JavaScript string: a sequence of UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsString {
    units: Box<[u16]>,
}

impl JsString {
    pub fn from_utf16(units: &[u16]) -> Self {
        Self {
            units: units.into(),
        }
    }

    pub fn from_utf8(text: &str) -> Self {
        Self {
            units: text.encode_utf16().collect(),
        }
    }

    /// The number of code units (spec 6.1.4.1 StringLength).
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn as_slice(&self) -> &[u16] {
        &self.units
    }

    pub fn code_unit(&self, index: usize) -> Option<u16> {
        self.units.get(index).copied()
    }

    /// spec 6.1.4.3 CodePointAt: `(code point, is unpaired surrogate, code units consumed)`.
    pub fn code_point_at(&self, index: usize) -> Option<(u32, bool, usize)> {
        let hi = *self.units.get(index)?;
        if (0xD800..=0xDBFF).contains(&hi) {
            if let Some(&lo) = self.units.get(index + 1)
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
        String::from_utf16_lossy(&self.units)
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

/// Interns `units`, returning a stable id for it.
pub fn intern(units: &[u16]) -> AtomId {
    with_interner(|i| i.intern(units))
}

pub fn intern_utf8(text: &str) -> AtomId {
    let units: Vec<u16> = text.encode_utf16().collect();
    intern(&units)
}

/// Returns the interned text for `id`.
pub fn lookup(id: AtomId) -> JsString {
    let units = with_interner(|i| i.lookup(id).to_vec());
    JsString::from_utf16(&units)
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
}
