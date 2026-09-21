//! Scripts (`v8::Script`) and where they came from (`v8::ScriptOrigin`).

use runtime::api;

use crate::data::{Data, Script, String, Value};
use crate::handle::{Local, Payload};
use crate::scope::PinScope;

/// Where a script came from (`v8::ScriptOrigin`).
///
/// Slag's parser takes source and nothing else, so these are the fields a host
/// sets, kept as the host handed them over. Nothing reads them yet: they are
/// what the engine would need to give a script a name in a stack trace, and
/// what makes a host's call sites resolve until it can.
#[derive(Debug, Clone, Copy)]
pub struct ScriptOrigin<'s> {
    /// The script's resource name — what a stack frame would call it.
    pub resource_name: Local<'s, Value>,
    pub resource_line_offset: i32,
    pub resource_column_offset: i32,
    pub resource_is_shared_cross_origin: bool,
    pub script_id: i32,
    pub source_map_url: Option<Local<'s, Value>>,
    pub resource_is_opaque: bool,
    pub is_wasm: bool,
    pub is_module: bool,
    pub host_defined_options: Option<Local<'s, Data>>,
}

impl<'s> ScriptOrigin<'s> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _scope: &PinScope<'s, '_>,
        resource_name: Local<'s, Value>,
        resource_line_offset: i32,
        resource_column_offset: i32,
        resource_is_shared_cross_origin: bool,
        script_id: i32,
        source_map_url: Option<Local<'s, Value>>,
        resource_is_opaque: bool,
        is_wasm: bool,
        is_module: bool,
        host_defined_options: Option<Local<'s, Data>>,
    ) -> Self {
        Self {
            resource_name,
            resource_line_offset,
            resource_column_offset,
            resource_is_shared_cross_origin,
            script_id,
            source_map_url,
            resource_is_opaque,
            is_wasm,
            is_module,
            host_defined_options,
        }
    }
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
        match Self::parse(scope, &text) {
            Ok(script) => Some(script),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Parse `text` in the scope's realm, handing back the error instead of
    /// reporting it (what both compile paths share).
    pub(crate) fn parse<'s>(
        scope: &PinScope<'s, '_, ()>,
        text: &str,
    ) -> Result<Local<'s, Script>, crux::error::JsError> {
        let realm = crate::realm_of(scope);
        api::Script::compile(&realm, text)?;
        let (slot, generation) = crate::store::store(text.into());
        Ok(Local::from_payload(Payload::Script { slot, generation }))
    }
}

impl<'s> Local<'s, Script> {
    /// Evaluate the script (`v8::Script::Run`).
    pub fn run<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Value>> {
        let realm = crate::realm_of(scope);
        match realm.try_eval(&self.script_source()) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}
