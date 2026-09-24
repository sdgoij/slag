//! Scripts (`v8::Script`) and where they came from (`v8::ScriptOrigin`).

use std::rc::Rc;

use runtime::api;

use crate::data::{Data, Script, String, Value};
use crate::handle::{Local, LocalHandle, Payload};
use crate::position::Origin;
use crate::scope::PinScope;

/// Where a script came from (`v8::ScriptOrigin`).
///
/// Slag's parser takes source and nothing else, so these are the fields a host
/// sets, kept as the host handed them over. Two of them are read back: the
/// resource name and the two offsets are what an error's recorded position is
/// built from ([`position`](crate::position)), and the name is also the script's
/// own — carried into the engine so a dynamic import written in the script can
/// offer it as its referrer.
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
    ///
    /// `origin` is what the error's position is recorded from, and the name it
    /// carries is kept: this is the script's resource name, which the host's
    /// dynamic-import callback is handed as the referrer of an `import()`
    /// written in the script. An origin with no name, or an empty one, leaves
    /// the script unnamed, which is what such a script offers the host.
    pub fn compile<'s>(
        scope: &PinScope<'s, '_, ()>,
        source: Local<'_, String>,
        origin: Option<&ScriptOrigin>,
    ) -> Option<Local<'s, Script>> {
        let text = source.engine().as_string()?;
        let name = Origin::of(origin).name.filter(|name| !name.is_empty());
        match Self::parse(scope, &text, name) {
            Ok(script) => Some(script),
            Err(error) => {
                crate::throw_at(scope, &error, &text, &Origin::of(origin));
                None
            }
        }
    }

    /// Parse `text` in the scope's realm, handing back the error instead of
    /// reporting it (what both compile paths share).
    pub(crate) fn parse<'s>(
        scope: &PinScope<'s, '_, ()>,
        text: &str,
        name: Option<Rc<str>>,
    ) -> Result<Local<'s, Script>, crux::error::JsError> {
        let realm = crate::realm_of(scope);
        api::Script::compile(&realm, text)?;
        let (slot, generation) = crate::store::store(text.into(), name);
        Ok(Local::from_payload(Payload::Script { slot, generation }))
    }
}

impl<'s> LocalHandle<'s, Script> {
    /// Evaluate the script (`v8::Script::Run`).
    pub fn run<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Value>> {
        let realm = crate::realm_of(scope);
        let entry = self.script_entry();
        let name = crate::store::engine_name(entry.name.as_deref());
        match realm.try_eval_named(&entry.source, name) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Data, FixedArray, Promise};
    use crate::test_support::{eval, in_context};

    thread_local! {
        /// The referrer the bridge handed the host's import callback.
        static REFERRER: std::cell::RefCell<Option<std::string::String>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A host's dynamic-import callback that records the referrer and answers
    /// nothing, which leaves the engine's own resolution in place. The test only
    /// reads the first argument it is handed.
    fn record_referrer<'s, 'i>(
        scope: &mut PinScope<'s, 'i>,
        _options: Local<'s, Data>,
        resource_name: Local<'s, Value>,
        _specifier: Local<'s, String>,
        _attributes: Local<'s, FixedArray>,
    ) -> Option<Local<'s, Promise>> {
        REFERRER.with(|seen| {
            *seen.borrow_mut() = Some(resource_name.to_rust_string_lossy(scope));
        });
        None
    }

    /// The name a `ScriptOrigin` gives the script reaches the host: it is the
    /// referrer of an `import()` written in the script, which is what a host
    /// resolves a relative specifier against. A script compiled with no name (or
    /// an empty one) offers the empty name the crate sends for code without an
    /// origin name.
    #[test]
    fn the_scripts_origin_name_reaches_the_hosts_import_callback() {
        in_context!(scope, {
            scope
                .isolate_ptr()
                .set_host_import_module_dynamically_callback(record_referrer);
            for (name, expected) in [("file:///named.js", "file:///named.js"), ("", "")] {
                let resource_name: Local<Value> = Local::from_engine(api::Local::string(name));
                let origin = ScriptOrigin::new(
                    scope,
                    resource_name,
                    0,
                    0,
                    false,
                    -1,
                    None,
                    false,
                    false,
                    false,
                    None,
                );
                let code = String::new(
                    scope,
                    "(async () => { await import('./dep.js').catch(() => {}); })();",
                )
                .expect("string");
                let script = Script::compile(scope, code, Some(&origin)).expect("compile");
                assert!(script.run(scope).is_some(), "the script runs");
                REFERRER.with(|seen| {
                    assert_eq!(
                        seen.borrow().as_deref(),
                        Some(expected),
                        "the origin's name is the referrer"
                    );
                });
            }
        });
    }

    /// The store keeps what a `Global` has to own: a script read back in a later
    /// scope still knows its name, which is what makes the name outlive the
    /// handle scope it was compiled in.
    #[test]
    fn a_persistent_script_keeps_its_name() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        let script = {
            crate::scope!(let handle_scope, isolate);
            let context = crate::Context::new(handle_scope, Default::default());
            let scope = &mut crate::ContextScope::new(handle_scope, context);
            let resource_name: Local<Value> =
                Local::from_engine(api::Local::string("file:///kept.js"));
            let origin = ScriptOrigin::new(
                scope,
                resource_name,
                0,
                0,
                false,
                -1,
                None,
                false,
                false,
                false,
                None,
            );
            let code = String::new(scope, "1 + 1").expect("string");
            let script = Script::compile(scope, code, Some(&origin)).expect("compile");
            crate::Global::new(&scope.isolate_ptr(), script)
        };
        crate::scope!(let handle_scope, isolate);
        let context = crate::Context::new(handle_scope, Default::default());
        let scope = &mut crate::ContextScope::new(handle_scope, context);
        let reopened = script.open(scope);
        assert_eq!(
            reopened.script_entry().name.as_deref(),
            Some("file:///kept.js"),
            "the name survives the scope that compiled the script"
        );
        assert_eq!(
            eval(scope, "1 + 1"),
            Local::<Value>::from_engine(api::Local::number(2.0))
        );
    }
}
