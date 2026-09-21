//! Compiling scripts and functions (`v8::script_compiler`).
//!
//! # What compiling means here
//!
//! The crate we stand in for compiles to bytecode and can hand that bytecode
//! back for reuse, which is what most of this module's vocabulary is about.
//! Slag parses source and evaluates it in one pass, so:
//!
//! - a [`Source`] carries the text and what came with it, and `compile` parses
//!   it into the script the engine will evaluate;
//! - `compile_function` wraps the body in a function expression with the
//!   parameters it was given and evaluates *that*, which yields the same
//!   function;
//! - a code cache round-trips — it is stored, handed back, and readable — but
//!   nothing consumes or produces one, so [`CachedData::rejected`] answers
//!   `true` and a host that trusts it will re-produce its cache. That is the
//!   safe direction: it costs a re-parse, where believing a cache the engine
//!   cannot use would cost correctness.

use std::cell::Cell;
use std::ops::Deref;

use runtime::api;

use crate::data::{Function, Module, Object, Script, String};
use crate::handle::Local;
use crate::scope::PinScope;
use crate::script::ScriptOrigin;
use crate::support::UniqueRef;

/// Source code to compile (`v8::script_compiler::Source`).
///
/// The crate we stand in for keeps the string handle and lets C++ hold the rest;
/// here the text is what the engine parses, so that is what this carries.
#[derive(Debug)]
pub struct Source {
    text: std::string::String,
    cached_data: Option<CachedData<'static>>,
}

impl Source {
    /// A source to compile (`v8::ScriptCompiler::Source::New`).
    ///
    /// The origin is carried by the host's own [`ScriptOrigin`]; nothing reads
    /// it back yet, so it is taken for the shape's sake and dropped.
    pub fn new(source_string: Local<'_, String>, _origin: Option<&ScriptOrigin<'_>>) -> Self {
        Self {
            text: text_of(&source_string),
            cached_data: None,
        }
    }

    /// A source with the data a previous compile produced
    /// (`v8::ScriptCompiler::Source::New` with a cache).
    pub fn new_with_cached_data(
        source_string: Local<'_, String>,
        _origin: Option<&ScriptOrigin<'_>>,
        cached_data: UniqueRef<CachedData<'_>>,
    ) -> Self {
        // SAFETY: the host's `&'a [u8]` outlives the source it is compiled by —
        // that is the promise `CachedData::new` takes — and the borrow is
        // laundered here exactly as the crate we stand in for launders it into
        // a raw pointer.
        let cached_data = unsafe {
            std::mem::transmute::<CachedData<'_>, CachedData<'static>>(cached_data.into_inner())
        };
        Self {
            text: text_of(&source_string),
            cached_data: Some(cached_data),
        }
    }

    /// The data this source was given, if any
    /// (`v8::ScriptCompiler::Source::GetCachedData`).
    pub fn get_cached_data(&self) -> Option<&CachedData<'_>> {
        self.cached_data.as_ref()
    }
}

/// The text a string handle holds.
///
/// Read as code units and rendered lossily: source text that is not valid
/// UTF-16 is a parse error either way, and this keeps the reading scope-free,
/// which is what `Source::new` is.
fn text_of(string: &Local<'_, String>) -> std::string::String {
    std::string::String::from_utf16_lossy(&string.to_utf16())
}

/// Data a host can cache and hand back
/// (`v8::script_compiler::CachedData`).
#[derive(Debug)]
pub struct CachedData<'a> {
    data: &'a [u8],
    rejected: Cell<bool>,
}

impl<'a> CachedData<'a> {
    /// Wrap bytes an earlier compile produced
    /// (`v8::ScriptCompiler::CachedData::new`).
    pub fn new(data: &'a [u8]) -> UniqueRef<Self> {
        UniqueRef::new(Self {
            data,
            rejected: Cell::new(true),
        })
    }

    /// Whether the engine refused this data
    /// (`v8::ScriptCompiler::CachedData::rejected`).
    ///
    /// Nothing here consumes it, so it is refused from the start; see the
    /// module documentation.
    pub fn rejected(&self) -> bool {
        self.rejected.get()
    }
}

impl Deref for CachedData<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.data
    }
}

/// What a compile may do (`v8::ScriptCompiler::CompileOptions`).
///
/// A set of flags there too, and the ones that ask for code-cache interaction
/// have nothing to interact with here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompileOptions(u32);

// The names are the crate we stand in for's, which get them from its bitflags
// macro; a host writes `CompileOptions::ConsumeCodeCache`.
#[allow(non_upper_case_globals)]
impl CompileOptions {
    pub const NoCompileOptions: Self = Self(0);
    pub const ConsumeCodeCache: Self = Self(1 << 0);
    pub const EagerCompile: Self = Self(1 << 1);
    pub const ProduceCompileHints: Self = Self(1 << 2);
    pub const ConsumeCompileHints: Self = Self(1 << 3);
    pub const FollowCompileHintsMagicComment: Self = Self(1 << 4);
    pub const FollowCompileHintsPerFunctionMagicComment: Self = Self(1 << 5);
}

/// Why no code cache was requested or produced
/// (`v8::ScriptCompiler::NoCacheReason`).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum NoCacheReason {
    NoReason = 0,
    BecauseCachingDisabled,
    BecauseNoResource,
    BecauseInlineScript,
    BecauseModule,
    BecauseStreamingSource,
    BecauseInspector,
    BecauseScriptTooSmall,
    BecauseCacheTooCold,
    BecauseV8Extension,
    BecauseExtensionModule,
    BecausePacScript,
    BecauseInDocumentWrite,
    BecauseResourceWithNoCacheHandler,
    BecauseDeferredProduceCodeCache,
}

/// Compile a script (`v8::ScriptCompiler::Compile`).
pub fn compile<'s>(
    scope: &PinScope<'s, '_, ()>,
    source: &mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> Option<Local<'s, Script>> {
    match Script::parse(scope, &source.text) {
        Ok(script) => Some(script),
        Err(error) => {
            crate::throw(scope, &error);
            None
        }
    }
}

/// Compile a module (`v8::ScriptCompiler::CompileModule`).
pub fn compile_module<'s>(
    scope: &PinScope<'s, '_>,
    source: &mut Source,
) -> Option<Local<'s, Module>> {
    compile_module2(
        scope,
        source,
        CompileOptions::NoCompileOptions,
        NoCacheReason::NoReason,
    )
}

/// Compile a module with explicit options
/// (`v8::ScriptCompiler::CompileModule`).
///
/// The engine classifies a module by its specifier and import attributes, and
/// both reach it later — the host resolves each request during instantiation,
/// and a JSON, text or bytes module is a synthetic one the host builds itself.
/// So a compile here is JavaScript source, with no specifier to classify by.
pub fn compile_module2<'s>(
    scope: &PinScope<'s, '_>,
    source: &mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> Option<Local<'s, Module>> {
    let realm = crate::realm_of(scope);
    match api::Module::compile(&realm, "", &source.text) {
        Ok(module) => Some(Local::from_module(module)),
        Err(error) => {
            crate::throw(scope, &error);
            None
        }
    }
}

/// Compile a function body with the given parameters
/// (`v8::ScriptCompiler::CompileFunction`).
///
/// The engine parses source as a whole program, so the body is wrapped in a
/// function expression with the same parameters and the function is what that
/// expression evaluates to.
pub fn compile_function<'s>(
    scope: &PinScope<'s, '_, ()>,
    source: &mut Source,
    arguments: &[Local<'_, String>],
    _context_extensions: &[Local<'_, Object>],
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> Option<Local<'s, Function>> {
    let parameters = arguments
        .iter()
        .map(|name| to_rust_string_lossy(name))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("(function ({parameters}) {{\n{}\n}})", source.text);
    let realm = crate::realm_of(scope);
    let value = match realm.try_eval(&text) {
        Ok(value) => value,
        Err(error) => {
            crate::throw(scope, &error);
            return None;
        }
    };
    let function: Local<'_, Function> = Local::from_engine(value);
    if !function.is_function() {
        return None;
    }
    Some(function)
}

/// The tag a host stores beside a code cache to know whether it is still valid
/// (`v8::ScriptCompiler::CachedDataVersionTag`).
///
/// One tag, not V8's version: nothing here produces or reads the data, so a
/// host that compares tags against this one will re-produce its cache, which is
/// what the module documentation describes.
pub fn cached_data_version_tag() -> u32 {
    /// This bridge's tag. A host that stores it beside a cache and compares it
    /// will discard caches from a different build, which is the direction the
    /// module documentation describes.
    const TAG: u32 = 1;
    TAG
}

fn to_rust_string_lossy(string: &Local<'_, String>) -> std::string::String {
    std::string::String::from_utf16_lossy(&string.to_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Value};
    use crate::test_support::{bind, eval_number, in_context};

    /// Compiling through this module is the same script the other path makes.
    #[test]
    fn a_compiled_script_runs_in_the_scope() {
        in_context!(scope, {
            let text = String::new(scope, "1 + 2").expect("string");
            let mut source = Source::new(text, None);
            let script = compile(
                scope,
                &mut source,
                CompileOptions::NoCompileOptions,
                NoCacheReason::NoReason,
            )
            .expect("compile");
            let value = script.run(scope).expect("run");
            assert_eq!(
                Local::<Number>::try_from(value).expect("number").value(),
                3.0
            );
        });
    }

    /// A function body becomes a function with the parameters it was given.
    #[test]
    fn a_compiled_function_body_becomes_a_function() {
        in_context!(scope, {
            let text = String::new(scope, "return a + b;").expect("string");
            let a = String::new(scope, "a").expect("string");
            let b = String::new(scope, "b").expect("string");
            let mut source = Source::new(text, None);
            let function = compile_function(
                scope,
                &mut source,
                &[a, b],
                &[],
                CompileOptions::NoCompileOptions,
                NoCacheReason::NoReason,
            )
            .expect("compile");
            bind(scope, "add", function.cast::<Value>());
            assert_eq!(eval_number(scope, "add(2, 3)"), 5.0);
        });
    }

    /// A code cache round-trips through a source, and is refused — which is what
    /// tells a host to produce a fresh one.
    #[test]
    fn a_code_cache_is_carried_and_refused() {
        let bytes = [1u8, 2, 3];
        let cache = CachedData::new(&bytes);
        assert!(cache.rejected());
        assert_eq!(&**cache, &bytes[..]);

        in_context!(scope, {
            let text = String::new(scope, "1").expect("string");
            let mut source = Source::new_with_cached_data(text, None, cache);
            let carried = source.get_cached_data().expect("cached data");
            assert!(carried.rejected());
            assert_eq!(&**carried, &bytes[..]);
            assert!(
                compile(
                    scope,
                    &mut source,
                    CompileOptions::ConsumeCodeCache,
                    NoCacheReason::NoReason,
                )
                .is_some()
            );
        });
    }
}
