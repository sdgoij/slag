//! Which V8 flags this bridge recognizes, and what it does with each.
//!
//! A host hands its command line to V8 and gets back the flags V8 did not
//! understand (`v8::V8::SetFlagsFromCommandLine`). Deno turns a non-empty answer
//! into a fatal error, because a flag it asked for and V8 did not know is a
//! program that will not mean what the host thinks (`cli/util/v8.rs:33-44`).
//! This engine is not V8, so "understood" cannot mean "applied": it means the
//! bridge has a stated answer for the name, and the answer is a [`FlagTier`], not
//! a boolean. A name this table does not hold is returned to the host unchanged,
//! which is V8's own signal for a flag it has never heard of.
//!
//! Recognition is by name only: nothing here parses a value, so a recognized
//! flag with a malformed value is consumed where V8 would refuse it.

use std::cell::RefCell;

/// What recognizing a flag means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagTier {
    /// The flag names a setting that belongs to the *host*, not to the engine.
    /// `--stack-size` is the one entry: it is a JS stack limit in V8, and here
    /// the JS stack is the native thread stack, which the host sizes.
    HostOwned,
    /// Nothing in the engine corresponds to the flag, so its effect is absent.
    /// This is the answer §9 gives the `CreateParams` heap-geometry knobs and
    /// the inspector, for the same reason: an engine that claimed otherwise
    /// would be reporting a setting it does not have.
    Inert,
}

/// A flag this bridge consumed: its name as the host wrote it, and its tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecognizedFlag {
    pub(crate) name: String,
    pub(crate) tier: FlagTier,
}

/// The flags this bridge recognizes, in the normalized form [`tier_of`]
/// compares against (`-` written as `_`, and no `no_` negation prefix).
const RECOGNIZED: &[(&str, FlagTier)] = &[
    // V8's JS stack limit, in KB. The JS stack here is the thread's own stack.
    ("stack_size", FlagTier::HostOwned),
    // V8's inspector. This bridge refuses to create one, so live edit has
    // nothing to reach.
    ("inspector_live_edit", FlagTier::Inert),
    // V8's bound on what counts as a reasonable amount of external memory, which
    // decides when it collects. This arena keeps no such ledger.
    ("external_memory_max_reasonable_size", FlagTier::Inert),
    // A V8 old-generation heap size: the tier the CreateParams heap-geometry
    // knobs already take.
    ("max_old_space_size", FlagTier::Inert),
    // A host asking for V8's flag list. Recognized because a host checks for it
    // itself after this call (Deno exits 0 on it), and turning a help request
    // into an error exit is worse than printing nothing.
    ("help", FlagTier::Inert),
];

thread_local! {
    /// Every flag this thread has recognized, in arrival order.
    static RECOGNIZED_FLAGS: RefCell<Vec<RecognizedFlag>> = const { RefCell::new(Vec::new()) };
}

/// Remove the flags this bridge recognizes from `args`, leaving everything else.
///
/// V8's `remove_flags = true`: index 0 is the binary name and is always kept, so
/// a caller can skip it and read only the names that were not understood, which
/// is the shape `deno_core::v8_set_flags` and its callers depend on.
pub(crate) fn consume(args: Vec<String>) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len());
    for (index, arg) in args.into_iter().enumerate() {
        let recognized = if index == 0 { None } else { recognize(&arg) };
        match recognized {
            Some(flag) => record(flag),
            None => kept.push(arg),
        }
    }
    kept
}

/// The flag `arg` names, if this bridge recognizes it — with the name as the host
/// wrote it, dashes and any `=value` removed. `None` for an argument that is not
/// a flag, or that names one this bridge does not know.
fn recognize(arg: &str) -> Option<RecognizedFlag> {
    let name = flag_name(arg)?;
    let tier = tier_of(name)?;
    Some(RecognizedFlag {
        name: name.to_owned(),
        tier,
    })
}

/// The name an argument carries, as written. An argument is a flag when one or
/// two leading dashes are followed by a name; `-`, `--`, a bare word and an empty
/// name are not flags.
fn flag_name(arg: &str) -> Option<&str> {
    let body = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
    let name = body.split('=').next().unwrap_or_default();
    (!name.is_empty()).then_some(name)
}

/// The tier of a recognized flag name, if this bridge knows it.
///
/// The compare normalizes the spelling because V8 accepts both and one host
/// writes both: `deno_core`'s own flag string carries `--no-validate-asm` beside
/// `--turbo_fast_api_calls`.
fn tier_of(name: &str) -> Option<FlagTier> {
    let normalized = name.replace('-', "_");
    let positive = normalized.strip_prefix("no_").unwrap_or(&normalized);
    RECOGNIZED
        .iter()
        .find(|(flag, _)| *flag == positive)
        .map(|(_, tier)| *tier)
}

fn record(flag: RecognizedFlag) {
    RECOGNIZED_FLAGS.with(|slot| slot.borrow_mut().push(flag));
}

/// The flags this thread has recognized, in arrival order (this bridge's own
/// accessor, so the tiers in the table above are pinned by tests rather than only
/// stated in prose).
#[cfg(test)]
pub(crate) fn recognized_flags() -> Vec<RecognizedFlag> {
    RECOGNIZED_FLAGS.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recognized(name: &str, tier: FlagTier) -> RecognizedFlag {
        RecognizedFlag {
            name: name.to_owned(),
            tier,
        }
    }

    /// Deno's own defaults, through the function its check calls: the list
    /// `construct_v8_flags` builds is `[arg0, ...defaults, ...env, ...user]`, and
    /// Deno reports whatever survives `.skip(1)`. So consuming all three, and
    /// keeping the binary name at index 0, is the whole requirement.
    #[test]
    fn denos_default_flags_are_recognized_and_the_binary_name_is_kept() {
        let left = consume(vec![
            "UNUSED_BUT_NECESSARY_ARG0".to_string(),
            "--stack-size=1024".to_string(),
            "--inspector-live-edit".to_string(),
            "--external-memory-max-reasonable-size=0".to_string(),
        ]);

        assert_eq!(left, vec!["UNUSED_BUT_NECESSARY_ARG0".to_string()]);
        assert_eq!(
            recognized_flags(),
            vec![
                recognized("stack-size", FlagTier::HostOwned),
                recognized("inspector-live-edit", FlagTier::Inert),
                recognized("external-memory-max-reasonable-size", FlagTier::Inert),
            ]
        );
    }

    /// V8 starts its walk at index 1, so the binary name is never parsed even
    /// when it is spelled like a flag — the contract `deno_core::v8_set_flags`'s
    /// `.skip(1)` rests on.
    #[test]
    fn the_binary_name_is_never_parsed_even_when_it_looks_like_a_flag() {
        let left = consume(vec![
            "--stack-size=1024".to_string(),
            "--stack-size=2048".to_string(),
        ]);

        assert_eq!(left, vec!["--stack-size=1024".to_string()]);
        assert_eq!(
            recognized_flags(),
            vec![recognized("stack-size", FlagTier::HostOwned)]
        );
    }

    /// The `lsp` subcommand's extra default, which is a heap knob and so takes
    /// the tier the `CreateParams` knobs take.
    #[test]
    fn the_lsp_defaults_heap_size_is_recognized_and_inert() {
        let left = consume(vec![
            "deno".to_string(),
            "--stack-size=1024".to_string(),
            "--max-old-space-size=3072".to_string(),
        ]);

        assert_eq!(left, vec!["deno".to_string()]);
        assert_eq!(
            recognized_flags(),
            vec![
                recognized("stack-size", FlagTier::HostOwned),
                recognized("max-old-space-size", FlagTier::Inert),
            ]
        );
    }

    /// A name this bridge does not know comes back, which is V8's signal for a
    /// flag it has never heard of — and Deno's own `--allow-net` is such a name:
    /// it belongs to Deno's command line, not to V8's.
    #[test]
    fn an_unknown_flag_is_returned_to_the_host() {
        let left = consume(vec![
            "deno".to_string(),
            "--allow-net".to_string(),
            "run".to_string(),
            "--stack-size=2048".to_string(),
        ]);

        assert_eq!(left, vec!["deno", "--allow-net", "run"]);
        assert_eq!(
            recognized_flags(),
            vec![recognized("stack-size", FlagTier::HostOwned)]
        );
    }

    /// Both spellings V8 accepts, and the boolean negation, reach one entry:
    /// `deno_core`'s own flag string carries `--no-validate-asm` beside
    /// `--turbo_fast_api_calls`.
    #[test]
    fn the_spelling_and_the_negation_both_reach_the_entry() {
        let left = consume(vec![
            "deno".to_string(),
            "--no-inspector-live-edit".to_string(),
            "--stack_size=2048".to_string(),
        ]);

        assert_eq!(left, vec!["deno".to_string()]);
        assert_eq!(
            recognized_flags(),
            vec![
                recognized("no-inspector-live-edit", FlagTier::Inert),
                recognized("stack_size", FlagTier::HostOwned),
            ]
        );
    }

    /// Deno checks for help itself *after* this call and exits 0, so a help
    /// request that arrived unrecognized would become an error exit.
    #[test]
    fn a_help_request_is_recognized_in_both_spellings() {
        let left = consume(vec![
            "deno".to_string(),
            "--help".to_string(),
            "-help".to_string(),
        ]);

        assert_eq!(left, vec!["deno".to_string()]);
        assert_eq!(
            recognized_flags(),
            vec![
                recognized("help", FlagTier::Inert),
                recognized("help", FlagTier::Inert),
            ]
        );
    }

    /// An argument that is not a flag is not consumed, and neither is a flag
    /// *name* that arrived without its dashes: the dashes are what make an
    /// argument V8's to parse.
    #[test]
    fn a_non_flag_is_left_alone() {
        let left = consume(vec![
            "deno".to_string(),
            "--".to_string(),
            "-".to_string(),
            "console.log(1)".to_string(),
            "--=5".to_string(),
            "help".to_string(),
            "stack-size=1024".to_string(),
        ]);

        assert_eq!(
            left,
            vec![
                "deno",
                "--",
                "-",
                "console.log(1)",
                "--=5",
                "help",
                "stack-size=1024"
            ]
        );
        assert!(recognized_flags().is_empty());
    }
}
