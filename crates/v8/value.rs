//! The `Value` surface: predicates and conversions (`v8::Value`).

use runtime::api;

use crate::data::{String, Value};
use crate::handle::Local;
use crate::scope::PinScope;

impl<'s> Local<'s, Value> {
    pub fn is_undefined(&self) -> bool {
        self.engine().is_undefined()
    }

    pub fn is_null(&self) -> bool {
        self.engine().is_null()
    }

    pub fn is_null_or_undefined(&self) -> bool {
        self.is_null() || self.is_undefined()
    }

    pub fn is_true(&self) -> bool {
        self.engine().as_boolean() == Some(true)
    }

    pub fn is_false(&self) -> bool {
        self.engine().as_boolean() == Some(false)
    }

    pub fn is_boolean(&self) -> bool {
        self.engine().is_boolean()
    }

    pub fn is_number(&self) -> bool {
        self.engine().is_number()
    }

    pub fn is_string(&self) -> bool {
        self.engine().is_string()
    }

    pub fn is_symbol(&self) -> bool {
        self.engine().is_symbol()
    }

    /// A string or a symbol, which is what `v8::Name` names.
    pub fn is_name(&self) -> bool {
        self.is_string() || self.is_symbol()
    }

    pub fn is_big_int(&self) -> bool {
        self.engine().is_bigint()
    }

    pub fn is_object(&self) -> bool {
        self.engine().is_object()
    }

    pub fn is_function(&self) -> bool {
        self.engine().is_function()
    }

    pub fn is_constructor(&self) -> bool {
        self.engine().is_constructor()
    }

    /// Strict equality (spec 7.2.13). Engine values compare structurally, which
    /// for `Rc`-backed handles means reference identity for objects.
    pub fn strict_equals(&self, other: &Local<'s, Value>) -> bool {
        self.engine() == other.engine()
    }

    pub fn same_value(&self, other: &Local<'s, Value>) -> bool {
        self.strict_equals(other)
    }

    /// ToString (spec 7.1.17). A failing conversion leaves a pending exception.
    pub fn to_string<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, String>> {
        let value = self.engine().clone();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::context::to_string(agent, value.value())) {
            Ok(text) => Some(Local::from_engine(api::Local::from(
                crux::value::Value::String(crux::handle::Handle::new(text)),
            ))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// `v8::Value::ToRustStringLossy`: the string rendering, or an empty string
    /// when the conversion throws.
    pub fn to_rust_string_lossy(&self, scope: &PinScope<'_, '_>) -> std::string::String {
        self.to_string(scope)
            .map_or_else(std::string::String::new, |text| {
                text.to_rust_string_lossy(scope)
            })
    }
}
