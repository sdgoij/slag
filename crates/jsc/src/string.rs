//! Strings (`JSStringRef`): UTF-16 code-unit storage with UTF-8 entry
//! points. `JsString` stores UTF-16, so `JSStringGetCharactersPtr` returns
//! the exact ECMAScript string contents (lone surrogates included).

use std::ffi::{CStr, c_char};
use std::ptr;
use std::slice;

use crux::string::JsString;

use crate::context::{release_string_ref, string_from_ref, string_ref};
use crate::{JSChar, JSStringRef};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringCreateWithCharacters(
    characters: *const JSChar,
    length: usize,
) -> JSStringRef {
    crate::guard(|| {
        if characters.is_null() && length > 0 {
            return ptr::null_mut();
        }
        let units: &[u16] = if length == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(characters, length) }
        };
        string_ref(&JsString::from_utf16(units))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringCreateWithUTF8CString(string: *const c_char) -> JSStringRef {
    crate::guard(|| {
        if string.is_null() {
            return ptr::null_mut();
        }
        let text = unsafe { CStr::from_ptr(string) }.to_string_lossy();
        string_ref(&JsString::from_utf8(&text))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringRetain(string: JSStringRef) -> JSStringRef {
    crate::guard(|| {
        if string_from_ref(string).is_some() {
            string
        } else {
            ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringRelease(string: JSStringRef) {
    crate::guard(|| release_string_ref(string));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringGetLength(string: JSStringRef) -> usize {
    crate::guard(|| {
        string_from_ref(string)
            .map(|string| string.len())
            .unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringGetCharactersPtr(string: JSStringRef) -> *const JSChar {
    crate::guard(|| {
        // The ref holds the JsString strongly; the slice points into its
        // stable heap allocation. The pointer stays valid while the ref is
        // retained (JSC's contract).
        ffi::with_string(string as usize as u64, |string| string.as_slice().as_ptr())
            .unwrap_or(ptr::null())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringGetMaximumUTF8CStringSize(string: JSStringRef) -> usize {
    crate::guard(|| {
        string_from_ref(string)
            // Each UTF-16 code unit needs at most 3 UTF-8 bytes (lone
            // surrogates encode as U+FFFD); plus the terminator.
            .map(|string| string.len() * 3 + 1)
            .unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringGetUTF8CString(
    string: JSStringRef,
    buffer: *mut c_char,
    buffer_size: usize,
) -> usize {
    crate::guard(|| {
        let Some(string) = string_from_ref(string) else {
            return 0;
        };
        let bytes = string.to_string_lossy().into_bytes();
        let needed = bytes.len() + 1;
        if buffer.is_null() || buffer_size < needed {
            return needed;
        }
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
            *buffer.add(bytes.len()) = 0;
        }
        needed
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringIsEqual(a: JSStringRef, b: JSStringRef) -> bool {
    crate::guard(|| match (string_from_ref(a), string_from_ref(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSStringIsEqualToUTF8CString(a: JSStringRef, b: *const c_char) -> bool {
    crate::guard(|| {
        if b.is_null() {
            return false;
        }
        match string_from_ref(a) {
            Some(a) => {
                let b = unsafe { CStr::from_ptr(b) }.to_string_lossy();
                a == JsString::from_utf8(&b)
            }
            None => false,
        }
    })
}
