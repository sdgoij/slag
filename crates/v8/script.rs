//! Scripts (`v8::Script`).

use runtime::api;

use crate::data::{Script, String, Value};
use crate::handle::{Local, Payload};
use crate::scope::PinScope;

/// Where a script came from (`v8::ScriptOrigin`).
///
/// Slag's parser takes source and nothing else, so this carries the fields a
/// host sets and the engine ignores them until it can do better.
#[derive(Debug, Default)]
pub struct ScriptOrigin {
    pub resource_name: Option<std::string::String>,
    pub source_map_url: Option<std::string::String>,
}

impl Script {
    /// Parse `source` in the scope's realm (`v8::Script::Compile`). A syntax
    /// error is reported as `None` with a pending exception, as there.
    pub fn compile<'s>(
        scope: &PinScope<'s, '_, ()>,
        source: Local<'_, String>,
        _origin: Option<&ScriptOrigin>,
    ) -> Option<Local<'s, Script>> {
        let text = source.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Script::compile(&realm, &text) {
            Ok(_) => Some(Local::from_payload(Payload::Script(text.into()))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

impl<'s> Local<'s, Script> {
    /// Evaluate the script (`v8::Script::Run`).
    pub fn run<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Value>> {
        let realm = crate::realm_of(scope);
        match realm.try_eval(self.script_source()) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}
