//! Atoms: interned string keys (`JSAtom`, a `u32`). v1 keeps one thread-local
//! table shared by every runtime on the thread; atom ids are not scoped to a
//! single `JSRuntime` the way QuickJS's are.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char};

use crux::handle::Handle;
use crux::string::JsString;
use crux::value::{Value as CruxValue, ValueKind};

use crate::c::abi;
#[cfg(feature = "ng")]
use crate::c::cstring::release_cstring;
use crate::c::cstring::retain_cstring;
use crate::c::runtime::JSContext;

pub type JSAtom = u32;

struct AtomEntry {
    text: JsString,
    refs: u32,
}

thread_local! {
    static ATOMS: RefCell<HashMap<u32, AtomEntry>> = RefCell::new(HashMap::new());
    static ATOM_BY_TEXT: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    static NEXT_ATOM: Cell<u32> = const { Cell::new(1) };
}

/// The text an atom names, if the atom is live.
pub fn atom_text(atom: u32) -> Option<JsString> {
    ATOMS.with(|atoms| atoms.borrow().get(&atom).map(|entry| entry.text.clone()))
}

fn new_atom(text: JsString) -> u32 {
    let id = NEXT_ATOM.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    ATOMS.with(|atoms| {
        atoms.borrow_mut().insert(id, AtomEntry { text, refs: 1 });
    });
    id
}

/// Intern `text`, returning its atom (a new reference to an existing one).
pub(crate) fn intern(text: JsString) -> u32 {
    let key = text.to_string_lossy();
    if let Some(id) = ATOM_BY_TEXT.with(|by_text| by_text.borrow().get(&key).copied()) {
        dup_atom(id);
        return id;
    }
    let id = new_atom(text);
    ATOM_BY_TEXT.with(|by_text| by_text.borrow_mut().insert(key, id));
    id
}

fn dup_atom(atom: u32) {
    ATOMS.with(|atoms| {
        if let Some(entry) = atoms.borrow_mut().get_mut(&atom) {
            entry.refs += 1;
        }
    });
}

pub(crate) fn free_atom(atom: u32) {
    ATOMS.with(|atoms| {
        let mut atoms = atoms.borrow_mut();
        let Some(entry) = atoms.get(&atom) else {
            return;
        };
        if entry.refs > 1 {
            atoms.get_mut(&atom).unwrap().refs -= 1;
            return;
        }
        let text = entry.text.clone();
        atoms.remove(&atom);
        ATOM_BY_TEXT.with(|by_text| by_text.borrow_mut().remove(&text.to_string_lossy()));
    });
}

fn bytes_jsstring(str: *const c_char, len: usize) -> Option<JsString> {
    // SAFETY: the header requires `len` bytes readable from `str`.
    let bytes = unsafe { std::slice::from_raw_parts(str as *const u8, len) };
    let text = std::str::from_utf8(bytes).ok()?;
    Some(JsString::from_utf8(text))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewAtomLen(
    _ctx: *mut JSContext,
    str: *const c_char,
    len: usize,
) -> JSAtom {
    let Some(text) = bytes_jsstring(str, len) else {
        return 0;
    };
    intern(text)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewAtom(ctx: *mut JSContext, str: *const c_char) -> JSAtom {
    // SAFETY: the header requires a NUL-terminated string.
    let len = unsafe { CStr::from_ptr(str) }.to_bytes().len();
    // SAFETY: forwarding the C contract to `JS_NewAtomLen`.
    unsafe { JS_NewAtomLen(ctx, str, len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewAtomUInt32(_ctx: *mut JSContext, n: u32) -> JSAtom {
    intern(JsString::from_utf8(&n.to_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DupAtom(_ctx: *mut JSContext, atom: JSAtom) -> JSAtom {
    dup_atom(atom);
    atom
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeAtom(_ctx: *mut JSContext, atom: JSAtom) {
    free_atom(atom);
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DupAtomRT(
    _rt: *mut crate::c::runtime::JSRuntime,
    atom: JSAtom,
) -> JSAtom {
    dup_atom(atom);
    atom
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeAtomRT(_rt: *mut crate::c::runtime::JSRuntime, atom: JSAtom) {
    free_atom(atom);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AtomToValue(_ctx: *mut JSContext, atom: JSAtom) -> abi::JSValue {
    match atom_text(atom) {
        Some(text) => abi::value_to_jsvalue(CruxValue::String(Handle::new(text))),
        None => abi::undefined(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AtomToString(ctx: *mut JSContext, atom: JSAtom) -> abi::JSValue {
    // SAFETY: forwarding the C contract to `JS_AtomToValue`.
    unsafe { JS_AtomToValue(ctx, atom) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AtomToCStringLen(
    _ctx: *mut JSContext,
    plen: *mut usize,
    atom: JSAtom,
) -> *const c_char {
    let Some(text) = atom_text(atom) else {
        return std::ptr::null();
    };
    let text = text.to_string_lossy();
    if !plen.is_null() {
        // SAFETY: the caller passes a writable length slot or null.
        unsafe { *plen = text.len() };
    }
    retain_cstring(text.as_bytes())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AtomToCString(ctx: *mut JSContext, atom: JSAtom) -> *const c_char {
    // SAFETY: forwarding the C contract to `JS_AtomToCStringLen`.
    unsafe { JS_AtomToCStringLen(ctx, std::ptr::null_mut(), atom) }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeCStringRT(
    _rt: *mut crate::c::runtime::JSRuntime,
    ptr: *const c_char,
) {
    release_cstring(ptr);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ValueToAtom(_ctx: *mut JSContext, value: abi::JSValue) -> JSAtom {
    let Some(value) = abi::jsvalue_to_value(value) else {
        return 0;
    };
    match value.kind() {
        ValueKind::String(text) => intern(JsString::owned_of(&text)),
        _ => 0,
    }
}
