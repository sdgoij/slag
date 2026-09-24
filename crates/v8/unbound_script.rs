//! Context-unbound scripts (`v8::UnboundScript`, `v8::UnboundModuleScript`) and
//! the code cache a host reads them for.
//!
//! # What "unbound" costs here
//!
//! In the crate we stand in for, a `Script` is compiled bytecode bound to the
//! context it was compiled in, and an `UnboundScript` is that bytecode without
//! the binding — the form a host caches and binds later. Here a script *is* its
//! source text ([`crate::script`]) and a module *is* a record, and neither names
//! a context: every script handle is already unbound, and
//! `bind_to_current_context` is the identity. The tags exist because a host's
//! code names them.
//!
//! # What the code cache is, and is not
//!
//! The engine keeps no serialized compiled form — it parses and evaluates in one
//! pass — so there is nothing a real code cache could carry. It also cannot
//! answer `None`, which is the crate's answer for a script that cannot be
//! serialized: `deno_core`'s module map asks for a cache on *every* module load
//! and turns that `None` into a failed load
//! (`libs/core/modules/map.rs:900`; the CLI enables caching by default). So what
//! it answers instead is the code's own source text.
//!
//! That is a real answer to the question a host asks — "give me something I can
//! store and hand back" — and the next compile *checks* it rather than believing
//! it: the data is consumed when its bytes are the text being compiled and
//! refused when they are not ([`CachedData::rejected`]), so a cache is never
//! trusted about code it does not spell out. The cost is stated here rather
//! than hidden: a host's cache database holds a copy of the source, and the
//! round trip saves nothing. A cache that saves something means serializing the
//! engine's compiled program, which `.notes/embedding.md` §10 lists as
//! engine-side build-order work.

use runtime::api;

use crate::data::{Module, Script, UnboundModuleScript, UnboundScript, Value};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;
use crate::script_compiler::CachedData;
use crate::support::UniqueRef;

impl<'s> LocalHandle<'s, UnboundScript> {
    /// The script bound to the current context
    /// (v8::UnboundScript::BindToCurrentContext).
    ///
    /// The identity here, and honestly so: nothing an unbound script holds names
    /// a context, so the script that comes back is the one that went in, and
    /// `run` on it evaluates in the scope's realm either way.
    pub fn bind_to_current_context<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, Script> {
        Local::from_payload(*self.payload())
    }

    /// A code cache for this script
    /// (v8::UnboundScript::CreateCodeCache).
    ///
    /// The script's own source text; see the module documentation.
    pub fn create_code_cache(&self) -> Option<UniqueRef<CachedData<'static>>> {
        self.payload()
            .as_script_source()
            .map(|source| CachedData::owned(source.as_bytes().to_vec()))
    }

    /// The source map URL this script's source declares
    /// (v8::UnboundScript::GetSourceMappingURL), or `undefined` when it declares
    /// none.
    pub fn get_source_mapping_url<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, Value> {
        source_mapping_url_value(self.payload().as_script_source().as_deref())
    }
}

impl<'s> LocalHandle<'s, UnboundModuleScript> {
    /// A code cache for this module's script
    /// (v8::UnboundModuleScript::CreateCodeCache).
    ///
    /// The module's own source text; see the module documentation.
    pub fn create_code_cache(&self) -> Option<UniqueRef<CachedData<'static>>> {
        self.payload()
            .as_module()
            .source_text()
            .map(String::into_bytes)
            .map(CachedData::owned)
    }

    /// The source map URL this module's source declares
    /// (v8::UnboundModuleScript::GetSourceMappingURL), or `undefined`.
    pub fn get_source_mapping_url<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, Value> {
        let source = self.payload().as_module().source_text();
        source_mapping_url_value(source.as_deref())
    }
}

impl<'s> LocalHandle<'s, Script> {
    /// This script as a context-unbound one
    /// (v8::Script::GetUnboundScript).
    pub fn get_unbound_script<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, UnboundScript> {
        Local::from_payload(*self.payload())
    }
}

impl<'s> LocalHandle<'s, Module> {
    /// This module's context-unbound script
    /// (v8::Module::GetUnboundModuleScript).
    ///
    /// The crate we stand in for `DCHECK`s that the module is not synthetic —
    /// a synthetic module has no script — and an abort with the reason is this
    /// crate's shape for the caller's error.
    pub fn get_unbound_module_script<'a>(
        &self,
        _scope: &PinScope<'a, '_>,
    ) -> Local<'a, UnboundModuleScript> {
        assert!(
            self.payload().as_module().source_text().is_some(),
            "bridge: a synthetic module has no unbound script"
        );
        Local::from_payload(*self.payload())
    }
}

/// The value a source map URL is reported as: a string, or `undefined` when the
/// source declares none.
fn source_mapping_url_value<'a>(source: Option<&str>) -> Local<'a, Value> {
    match source.and_then(source_mapping_url) {
        Some(url) => Local::from_engine(api::Local::string(url)),
        None => Local::from_engine(api::Local::undefined()),
    }
}

/// The source map URL a source declares, if it declares one.
///
/// V8's own rule (`Scanner::TryToParseMagicComment`,
/// `v8/src/parsing/scanner.cc:280`) reads the magic comment
/// `//[#@]\s*sourceMappingURL\s*=\s*<url>` as real syntax, keeps the last one the
/// scanner sees, and requires it to be a comment. The bridge has no lexer — the
/// engine's parser keeps its comments to itself — so it reads the source as text,
/// with one restriction that makes a false positive very unlikely: the comment
/// must be on a line of its own. Every emitter of source maps writes it that way,
/// as the last line, and a string or template literal holding the comment's text
/// would have to fill a whole line to be mistaken for one.
/// `.notes/embedding.md` §9 states the divergence.
fn source_mapping_url(source: &str) -> Option<String> {
    let mut found = None;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("//#")
            .or_else(|| trimmed.strip_prefix("//@"))
        else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix("sourceMappingURL") else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('=') else {
            continue;
        };
        // V8 takes the value up to the first whitespace and ignores the rest,
        // which is where a trailing comment about the map would sit.
        let url = value.split_whitespace().next().unwrap_or("");
        if !url.is_empty() {
            found = Some(url.to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Function, String as JsString};
    use crate::test_support::{eval, in_context};

    const MODULE_SOURCE: &str = "export const answer = 42;\n//# sourceMappingURL=answer.js.map\n";

    fn compile_script<'s>(scope: &PinScope<'s, '_, ()>, source: &str) -> Local<'s, Script> {
        let code = JsString::new(scope, source).expect("string");
        Script::compile(scope, code, None).expect("compile")
    }

    fn compile_module<'s>(
        scope: &PinScope<'s, '_>,
        specifier: &str,
        source: &str,
    ) -> Local<'s, Module> {
        let realm = crate::realm_of(scope);
        Local::from_module(api::Module::compile(&realm, specifier, source).expect("compile"))
    }

    /// An unbound script is the script itself here, so the cache a host stores is
    /// the source text — which the next compile of that source consumes, so a
    /// host that hands it back is not told to produce a fresh one — and binding
    /// it is the identity.
    #[test]
    fn a_scripts_cache_is_its_source_and_a_compile_consumes_it() {
        in_context!(scope, {
            let script = compile_script(scope, "1 + 1");
            let unbound = script.get_unbound_script(scope);

            let cache = unbound.create_code_cache().expect("a cache");
            assert_eq!(&**cache, b"1 + 1");
            assert!(
                !cache.rejected(),
                "a cache is not refused before a compile has seen it"
            );

            let bound = unbound.bind_to_current_context(scope);
            assert_eq!(
                bound.run(scope).expect("run").to_rust_string_lossy(scope),
                "2"
            );
        });
    }

    /// A module's unbound script answers its source, and the source map URL that
    /// source declares — the magic comment, which is how a host finds a module's
    /// map.
    #[test]
    fn a_modules_unbound_script_carries_its_source_and_map_url() {
        in_context!(scope, {
            let module = compile_module(scope, "answer.js", MODULE_SOURCE);
            let unbound = module.get_unbound_module_script(scope);

            let cache = unbound.create_code_cache().expect("a cache");
            assert_eq!(&**cache, MODULE_SOURCE.as_bytes());
            assert_eq!(
                unbound
                    .get_source_mapping_url(scope)
                    .to_rust_string_lossy(scope),
                "answer.js.map"
            );
        });
    }

    /// The magic comment is read the way V8 reads it: the last one in the source
    /// wins, `//@` is the other spelling, and a source with none — or one whose
    /// text is inside a line of code — answers `undefined`.
    #[test]
    fn the_source_map_url_is_the_last_magic_comment_on_its_own_line() {
        assert_eq!(
            source_mapping_url("1;\n//# sourceMappingURL=a.map\n"),
            Some("a.map".to_owned())
        );
        assert_eq!(
            source_mapping_url("1;\n//# sourceMappingURL=a.map\n2;\n//@ sourceMappingURL=b.map\n"),
            Some("b.map".to_owned())
        );
        assert_eq!(
            source_mapping_url("1;\n//# sourceMappingURL=a.map trailing words\n"),
            Some("a.map".to_owned())
        );
        assert_eq!(source_mapping_url("1;\n"), None);
        assert_eq!(
            source_mapping_url("const s = \"//# sourceMappingURL=lie.map\";\n"),
            None,
            "a comment's text inside a statement is not the comment"
        );

        in_context!(scope, {
            let script = compile_script(scope, "1;");
            let unbound = script.get_unbound_script(scope);
            assert!(unbound.get_source_mapping_url(scope).is_undefined());
        });
    }

    /// The cache a *function* answers is the function's own definition text — the
    /// text the record keeps for `Function.prototype.toString` — and a builtin,
    /// which has no text of its own, says so instead of inventing one.
    #[test]
    fn a_functions_cache_is_its_definition_text() {
        in_context!(scope, {
            let function =
                Local::<Function>::try_from(eval(scope, "(function add(a, b) { return a + b; })"))
                    .expect("a function");
            let cache = function.create_code_cache().expect("a cache");
            assert_eq!(
                std::str::from_utf8(&cache).expect("utf-8"),
                "function add(a, b) { return a + b; }"
            );

            let builtin = Local::<Function>::try_from(eval(scope, "Array.prototype.map"))
                .expect("a function");
            assert!(builtin.create_code_cache().is_none());
        });
    }

    /// A module with no map declares no map, and the answer is `undefined` rather
    /// than a value borrowed from somewhere else.
    #[test]
    fn a_module_without_a_map_answers_undefined() {
        in_context!(scope, {
            let module = compile_module(scope, "plain.js", "export const a = 1;\n");
            let unbound = module.get_unbound_module_script(scope);
            assert!(unbound.get_source_mapping_url(scope).is_undefined());
            assert!(
                unbound.create_code_cache().is_some(),
                "a module with no map still has a cache to hand out"
            );
        });
    }

    /// A synthetic module has no script, and asking for one is the caller's
    /// mistake — reported, not guessed at.
    #[test]
    #[should_panic(expected = "a synthetic module has no unbound script")]
    fn a_synthetic_module_has_no_unbound_script() {
        fn no_steps(
            _context: api::Context,
            _module: api::Module,
        ) -> Result<api::Local, crux::error::JsError> {
            Ok(api::Local::undefined())
        }

        in_context!(scope, {
            let realm = crate::realm_of(scope);
            let name = JsString::new(scope, "synthetic").expect("string");
            let module = Local::<Module>::from_module(
                api::Module::create_synthetic_module(&realm, &name.into_engine(), &[], no_steps)
                    .expect("create"),
            );
            module.get_unbound_module_script(scope);
        });
    }
}
