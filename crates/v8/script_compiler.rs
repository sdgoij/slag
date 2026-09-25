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
//! - a code cache round-trips — it is stored, handed back, and readable — and
//!   [`create_code_cache`](crate::UnboundScript::create_code_cache) produces
//!   one, whose bytes are the code's own source text. A compile given one
//!   *checks* it rather than believing it: the data is consumed
//!   ([`CachedData::rejected`] answers `false`) when its bytes are the text
//!   being compiled, and refused when they are not — which is V8's own shape,
//!   the compile refusing data it cannot use. So a host's cache is a copy of
//!   the source, the round trip saves nothing, and no cache is ever trusted
//!   about code it does not spell out. What a real one would save means
//!   serializing the engine's compiled program, which `.notes/embedding.md`
//!   §10 lists as engine-side build-order work.

use std::cell::Cell;
use std::ops::Deref;

use runtime::api;

use crate::data::{Function, Module, Object, Script, String, UnboundScript};
use crate::handle::Local;
use crate::position::Origin;
use crate::scope::PinScope;
use crate::script::ScriptOrigin;
use crate::support::UniqueRef;

/// Source code to compile (`v8::script_compiler::Source`).
///
/// The crate we stand in for keeps the string handle and lets C++ hold the rest;
/// here the text is what the engine parses, so that is what this carries. The
/// origin's name and offsets are kept beside it, because a failed compile
/// records the error's position from them ([`crate::position`]).
#[derive(Debug)]
pub struct Source {
    text: std::string::String,
    origin: Origin,
    cached_data: Option<CachedData<'static>>,
}

impl Source {
    /// A source to compile (`v8::ScriptCompiler::Source::New`).
    ///
    /// The origin is the host's own [`ScriptOrigin`], of which the name and the
    /// two offsets are carried here: the engine's parser takes source and no
    /// origin, so the bridge is the only place they can be read from when a
    /// compile fails.
    pub fn new(source_string: Local<'_, String>, origin: Option<&ScriptOrigin<'_>>) -> Self {
        Self {
            text: text_of(&source_string),
            origin: Origin::of(origin),
            cached_data: None,
        }
    }

    /// A source with the data a previous compile produced
    /// (`v8::ScriptCompiler::Source::New` with a cache).
    pub fn new_with_cached_data(
        source_string: Local<'_, String>,
        origin: Option<&ScriptOrigin<'_>>,
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
            origin: Origin::of(origin),
            cached_data: Some(cached_data),
        }
    }

    /// The data this source was given, if any
    /// (`v8::ScriptCompiler::Source::GetCachedData`).
    pub fn get_cached_data(&self) -> Option<&CachedData<'_>> {
        self.cached_data.as_ref()
    }

    /// Whether `text` — the text this source is being compiled *as* — is what
    /// the data it was given was made from, recorded on the data the way V8's
    /// compile records a refusal (`CachedData::rejected_`).
    ///
    /// The two compiles that take the text as they find it pass their own;
    /// [`compile_function`] wraps its text first and passes the wrapped form,
    /// because that is what the function it produces is made of, which is also
    /// what [`Function::create_code_cache`](crate::Function::create_code_cache)
    /// reads back.
    fn consume_cached_data(&self, text: &[u8]) {
        if let Some(cached_data) = &self.cached_data {
            cached_data.record_consumption(text);
        }
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
///
/// Both forms exist because the crate we stand in for has them: bytes a host
/// hands in are borrowed for the compile ([`new`](Self::new)), while the data
/// [`create_code_cache`](crate::UnboundScript::create_code_cache) answers with
/// belongs to the caller and outlives the call it was made in.
#[derive(Debug)]
pub struct CachedData<'a> {
    data: CacheBytes<'a>,
    rejected: Cell<bool>,
}

#[derive(Debug)]
enum CacheBytes<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> CachedData<'a> {
    /// Wrap bytes an earlier compile produced
    /// (`v8::ScriptCompiler::CachedData::new`).
    pub fn new(data: &'a [u8]) -> UniqueRef<Self> {
        UniqueRef::new(Self {
            data: CacheBytes::Borrowed(data),
            rejected: Cell::new(false),
        })
    }

    /// Data the bridge produced, owned by the value rather than borrowed from a
    /// caller — what a `create_code_cache` answer is.
    ///
    /// See the type's own documentation for what those bytes are.
    pub(crate) fn owned(data: Vec<u8>) -> UniqueRef<Self> {
        UniqueRef::new(Self {
            data: CacheBytes::Owned(data),
            rejected: Cell::new(false),
        })
    }

    /// The bytes, borrowed or owned.
    fn bytes(&self) -> &[u8] {
        match &self.data {
            CacheBytes::Borrowed(data) => data,
            CacheBytes::Owned(data) => data,
        }
    }

    /// Whether the compile refused this data
    /// (`v8::ScriptCompiler::CachedData::rejected`).
    ///
    /// V8's flag is unset until a compile sets it (`cached-data.h`:
    /// `bool rejected_ = false;`), and so is this one. What refusing means here
    /// is stated by [`Source::consume_cached_data`]: the bytes are not the text
    /// being compiled. So a host that hands back the cache for the code it is
    /// compiling is told the data was used, and one that hands back a cache for
    /// other code is told it was refused — where a cache is refused, the host
    /// re-produces it, and where it is consumed, V8 leaves the host's copy
    /// alone for the same reason.
    pub fn rejected(&self) -> bool {
        self.rejected.get()
    }

    /// Record whether `text` is what this data was made from — the compile's
    /// answer, which V8 writes the same way (`CachedData::rejected_`).
    pub(crate) fn record_consumption(&self, text: &[u8]) {
        self.rejected.set(self.bytes() != text);
    }
}

impl Deref for CachedData<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes()
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
    source.consume_cached_data(source.text.as_bytes());
    match Script::parse(scope, &source.text, source.origin.name.clone()) {
        Ok(script) => Some(script),
        Err(error) => {
            crate::throw_at(scope, &error, &source.text, &source.origin);
            None
        }
    }
}

/// Compile a script without binding it to a context
/// (`v8::ScriptCompiler::CompileUnboundScript`).
///
/// The re-tagged answer to [`compile`], and honestly so: a script here *is* its
/// source text and names no context, so every script handle is already unbound
/// and the tag is the only difference — see [`crate::unbound_script`], which
/// says the same about `bind_to_current_context`.
pub fn compile_unbound_script<'s>(
    scope: &PinScope<'s, '_, ()>,
    source: &mut Source,
    options: CompileOptions,
    no_cache_reason: NoCacheReason,
) -> Option<Local<'s, UnboundScript>> {
    compile(scope, source, options, no_cache_reason)
        .map(|script| Local::from_payload(*script.payload()))
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
    let name = source.origin.name.clone();
    source.consume_cached_data(source.text.as_bytes());
    match api::Module::compile_with_name(&realm, "", name.as_deref(), &source.text) {
        Ok(module) => Some(Local::from_module(module)),
        Err(error) => {
            crate::throw_at(scope, &error, &source.text, &source.origin);
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
///
/// Nothing records a position for a failure here, unlike the two compiles
/// above: the span the engine reports indexes the *wrapped* text, so the line
/// it names is one the host never wrote.
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
    source.consume_cached_data(text.as_bytes());
    let realm = crate::realm_of(scope);
    let value = match realm.try_eval_named(
        &text,
        crate::store::engine_name(source.origin.name.as_deref()),
    ) {
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
/// One tag, not V8's version: what this bridge's data carries is the source
/// text, which does not depend on the engine build, so a host that compares
/// this tag against its own gets a stable answer. The tag a stale cache
/// answers to is `rejected`, which is about the text.
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

    /// The unbound compile is what `vm.rs` reaches for, and it is the bound one's
    /// answer re-tagged: the same script, which here names no context either way.
    /// What the test pins is the whole host-visible contract around it — the code
    /// cache a host stores is the source it handed over, the cache is consumed by
    /// the next compile of that source, and the script a later
    /// `bind_to_current_context` produces runs.
    #[test]
    fn an_unbound_compile_answers_a_cache_and_a_runnable_script() {
        in_context!(scope, {
            let text = String::new(scope, "6 * 7").expect("string");
            let mut source = Source::new(text, None);
            let unbound = compile_unbound_script(
                scope,
                &mut source,
                CompileOptions::NoCompileOptions,
                NoCacheReason::NoReason,
            )
            .expect("compile");

            let cache = unbound.create_code_cache().expect("a cache");
            assert_eq!(&**cache, b"6 * 7");

            let bound = unbound.bind_to_current_context(scope);
            assert_eq!(
                bound.run(scope).expect("run").to_rust_string_lossy(scope),
                "42"
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

    /// A cache round-trips through a source, and the compile is what has an
    /// opinion about it: data that is the text being compiled is consumed, and
    /// data made from other code is refused — which is what tells a host to
    /// produce a fresh one.
    #[test]
    fn a_matching_cache_is_consumed_and_other_code_is_refused() {
        let bytes = [1u8, 2, 3];
        in_context!(scope, {
            let text = String::new(scope, "1 + 2").expect("string");
            let cache = CachedData::new(&bytes);
            assert!(
                !cache.rejected(),
                "a cache is not refused before a compile has seen it"
            );
            let mut source = Source::new_with_cached_data(text, None, cache);
            let carried = source.get_cached_data().expect("cached data");
            assert_eq!(carried.to_vec(), bytes);
            assert!(
                compile(
                    scope,
                    &mut source,
                    CompileOptions::ConsumeCodeCache,
                    NoCacheReason::NoReason,
                )
                .is_some()
            );
            assert!(
                source.get_cached_data().expect("cached data").rejected(),
                "data that is not the text compiled is refused"
            );
        });

        in_context!(scope, {
            let text = String::new(scope, "1 + 2").expect("string");
            let cache = CachedData::new(b"1 + 2");
            let mut source = Source::new_with_cached_data(text, None, cache);
            assert!(
                compile(
                    scope,
                    &mut source,
                    CompileOptions::ConsumeCodeCache,
                    NoCacheReason::NoReason,
                )
                .is_some()
            );
            assert!(
                !source.get_cached_data().expect("cached data").rejected(),
                "the compile consumed data that is the text it compiled"
            );
        });
    }

    /// The round trip a host makes: the cache `create_code_cache` answers is
    /// consumed by the next compile of the same code, for a script and for a
    /// module alike — the bytes have to be what the compile compares.
    #[test]
    fn a_code_cache_round_trips_through_the_next_compile() {
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
            let cache = script
                .get_unbound_script(scope)
                .create_code_cache()
                .expect("cache");
            let bytes = cache.to_vec();

            let text = String::new(scope, "1 + 2").expect("string");
            let mut source = Source::new_with_cached_data(text, None, CachedData::new(&bytes));
            let script = compile(
                scope,
                &mut source,
                CompileOptions::ConsumeCodeCache,
                NoCacheReason::NoReason,
            )
            .expect("compile");
            assert!(
                !source.get_cached_data().expect("cached data").rejected(),
                "the cache a script produced is consumed by the next compile"
            );
            assert_eq!(
                Local::<Number>::try_from(script.run(scope).expect("run"))
                    .expect("number")
                    .value(),
                3.0
            );
        });

        in_context!(scope, {
            let text = String::new(scope, "export const x = 1;").expect("string");
            let mut source = Source::new(text, None);
            let module = compile_module(scope, &mut source).expect("compile");
            let cache = module
                .get_unbound_module_script(scope)
                .create_code_cache()
                .expect("cache");
            let bytes = cache.to_vec();

            let text = String::new(scope, "export const x = 1;").expect("string");
            let mut source = Source::new_with_cached_data(text, None, CachedData::new(&bytes));
            let module = compile_module2(
                scope,
                &mut source,
                CompileOptions::ConsumeCodeCache,
                NoCacheReason::NoReason,
            );
            assert!(module.is_some());
            assert!(
                !source.get_cached_data().expect("cached data").rejected(),
                "the cache a module produced is consumed by the next compile"
            );
        });
    }
}
