//! Slow-path entry points the JIT bakes into compiled machine code.
//!
//! The JIT inlines the number fast paths; anything it cannot handle calls
//! one of these `extern "C"` functions (their addresses are baked into the
//! code at compile time, so the table does not need to stay alive). The
//! runtime integration fills in real helpers that route to the interpreter's
//! machinery (`apply_binary`, `get_member_name`, `update_value`, the TDZ
//! ReferenceError, `to_boolean`). The scaffold's tests provide test doubles.
//!
//! Every helper takes `vm` (the opaque pointer the caller passed to the JIT
//! entry point) so the runtime implementation can reach the `Vm`/`Agent`.
//! `op`/`inc`/`name` arguments are the raw discriminants of
//! [`syntax::ast::BinaryOp`]/[`syntax::ast::UpdateOp`] / the `AtomId`, passed
//! as `u64`.

use std::os::raw::c_void;

use crux::Value;

/// Identifies one slow-path helper; used to look the entry point up and to
/// name it in bail diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    BinarySlow,
    RelationalSlow,
    UpdateValueSlow,
    ToBooleanSlow,
    TdzError,
    GetMemberName,
    GetMemberComputed,
    SetMemberName,
    SetMemberComputed,
}

impl Helper {
    pub fn name(self) -> &'static str {
        match self {
            Helper::BinarySlow => "binary_slow",
            Helper::RelationalSlow => "relational_slow",
            Helper::UpdateValueSlow => "update_value_slow",
            Helper::ToBooleanSlow => "to_boolean_slow",
            Helper::TdzError => "tdz_error",
            Helper::GetMemberName => "get_member_name",
            Helper::GetMemberComputed => "get_member_computed",
            Helper::SetMemberName => "set_member_name",
            Helper::SetMemberComputed => "set_member_computed",
        }
    }
}

/// The slow-path helper table (see the module docs).
///
/// `#[repr(C)]` so the offsets are stable for future code that reads the
/// table from the compiled code side; the scaffold bakes the addresses in at
/// compile time instead.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitHelpers {
    /// Full binary-operator semantics (`apply_binary`): `op` is a
    /// `BinaryOp` discriminant. Returns the result value.
    pub binary_slow: Option<extern "C" fn(vm: *mut c_void, op: u64, a: u64, b: u64) -> u64>,
    /// JS relational semantics for a loop test on a non-Number: `op` is a
    /// `BinaryOp` discriminant; returns 1 when the test holds, else 0.
    pub relational_slow: Option<extern "C" fn(vm: *mut c_void, op: u64, a: u64, b: u64) -> u64>,
    /// The general `++`/`--` machinery on a non-Number: `inc` is an
    /// `UpdateOp` discriminant; returns the NEW value.
    pub update_value_slow: Option<extern "C" fn(vm: *mut c_void, inc: u64, value: u64) -> u64>,
    /// Full JS `ToBoolean` for a heap value (empty-string, object, ...):
    /// returns 1 when truthy, else 0.
    pub to_boolean_slow: Option<extern "C" fn(vm: *mut c_void, value: u64) -> u64>,
    /// Throws the TDZ ReferenceError. Never returns normally (the JIT emits
    /// `unreachable` after the call).
    pub tdz_error: Option<extern "C" fn(vm: *mut c_void) -> u64>,
    /// `Get(o, name)`: `name` is an `AtomId`; returns the value.
    pub get_member_name: Option<extern "C" fn(vm: *mut c_void, object: u64, name: u64) -> u64>,
    /// `Get(o, key)` with a computed key value.
    pub get_member_computed: Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64) -> u64>,
    /// `Set(o, name, v)` (plain assignment); returns the stored value.
    pub set_member_name:
        Option<extern "C" fn(vm: *mut c_void, object: u64, name: u64, value: u64) -> u64>,
    /// `Set(o, key, v)` with a computed key; returns the stored value.
    pub set_member_computed:
        Option<extern "C" fn(vm: *mut c_void, object: u64, key: u64, value: u64) -> u64>,
}

impl JitHelpers {
    /// An empty table: any body that needs a slow path bails.
    pub fn none() -> Self {
        Self {
            binary_slow: None,
            relational_slow: None,
            update_value_slow: None,
            to_boolean_slow: None,
            tdz_error: None,
            get_member_name: None,
            get_member_computed: None,
            set_member_name: None,
            set_member_computed: None,
        }
    }

    /// The address of a helper, when present.
    pub fn get(&self, helper: Helper) -> Option<u64> {
        match helper {
            Helper::BinarySlow => self.binary_slow.map(|f| f as usize as u64),
            Helper::RelationalSlow => self.relational_slow.map(|f| f as usize as u64),
            Helper::UpdateValueSlow => self.update_value_slow.map(|f| f as usize as u64),
            Helper::ToBooleanSlow => self.to_boolean_slow.map(|f| f as usize as u64),
            Helper::TdzError => self.tdz_error.map(|f| f as usize as u64),
            Helper::GetMemberName => self.get_member_name.map(|f| f as usize as u64),
            Helper::GetMemberComputed => self.get_member_computed.map(|f| f as usize as u64),
            Helper::SetMemberName => self.set_member_name.map(|f| f as usize as u64),
            Helper::SetMemberComputed => self.set_member_computed.map(|f| f as usize as u64),
        }
    }
}

// ---------------------------------------------------------------------------
// Test doubles. The scaffold tests never hit these except to prove the call
// ABI works end to end; each returns a fixed marker value. They are `extern
// "C"` because the compiled code calls them through the platform ABI.
// ---------------------------------------------------------------------------

/// Returns `42` — proves `binary_slow` was called with the right ABI.
pub extern "C" fn test_binary_slow(_vm: *mut c_void, _op: u64, _a: u64, _b: u64) -> u64 {
    Value::Number(42.0).bits()
}

pub extern "C" fn test_relational_slow(_vm: *mut c_void, _op: u64, _a: u64, _b: u64) -> u64 {
    1
}

pub extern "C" fn test_update_value_slow(_vm: *mut c_void, _inc: u64, _value: u64) -> u64 {
    Value::Number(7.0).bits()
}

pub extern "C" fn test_to_boolean_slow(_vm: *mut c_void, _value: u64) -> u64 {
    1
}

pub extern "C" fn test_tdz_error(_vm: *mut c_void) -> u64 {
    panic!("the TDZ error slow path ran in a test (a lexical slot was read before init)")
}

pub extern "C" fn test_get_member_name(_vm: *mut c_void, _object: u64, _name: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_get_member_computed(_vm: *mut c_void, _object: u64, _key: u64) -> u64 {
    Value::Undefined.bits()
}

pub extern "C" fn test_set_member_name(
    _vm: *mut c_void,
    _object: u64,
    _name: u64,
    value: u64,
) -> u64 {
    value
}

pub extern "C" fn test_set_member_computed(
    _vm: *mut c_void,
    _object: u64,
    _key: u64,
    value: u64,
) -> u64 {
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_table_has_no_helpers() {
        let none = JitHelpers::none();
        for h in [
            Helper::BinarySlow,
            Helper::RelationalSlow,
            Helper::UpdateValueSlow,
            Helper::ToBooleanSlow,
            Helper::TdzError,
            Helper::GetMemberName,
            Helper::GetMemberComputed,
            Helper::SetMemberName,
            Helper::SetMemberComputed,
        ] {
            assert!(none.get(h).is_none(), "{} should be None", h.name());
        }
    }

    #[test]
    fn helper_names_are_stable() {
        assert_eq!(Helper::BinarySlow.name(), "binary_slow");
        assert_eq!(Helper::TdzError.name(), "tdz_error");
    }

    #[test]
    fn test_double_proves_the_c_abi() {
        // The test doubles are real extern "C" fn pointers, so a call through
        // them is exactly what the compiled code emits.
        let f = test_binary_slow as extern "C" fn(*mut c_void, u64, u64, u64) -> u64;
        let bits = f(std::ptr::null_mut(), 0, 1, 2);
        assert_eq!(bits, Value::Number(42.0).bits());
    }
}
