//! The ICU the engine carries, as much of it as a host asks about (`v8::icu`).
//!
//! The crate we stand in for exposes the ICU data *its* build was linked with.
//! This engine carries its own ICU — the `Intl` built-ins and the data behind
//! them — and the one question the crate's surface asks of it is the locale
//! those built-ins resolve by default, which is the engine's own
//! [`default_locale`](runtime::builtins::intl::number_format::default_locale).
//! Answering that rather than a constant is what keeps a restored realm's locale
//! and the engine's `Intl` in step: two answers to "what locale is this" would be
//! one more than a host can reconcile.

/// The default locale's language tag (`v8::icu::get_language_tag`).
///
/// Owned because that is the shape the crate we stand in for hands back: the tag
/// is read once per runtime and kept by the host.
pub fn get_language_tag() -> String {
    runtime::builtins::intl::number_format::default_locale().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag is the engine's own default locale — the one its `Intl` resolves
    /// by — rather than a string this module happens to hold.
    #[test]
    fn the_language_tag_is_the_engines_default_locale() {
        assert_eq!(
            get_language_tag(),
            runtime::builtins::intl::number_format::default_locale()
        );
    }
}
