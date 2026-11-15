//! Named diagnostic channels, selected by `SLAG_TRACE`.
//!
//! A switch that used to be a boolean per call site (`JIT_DUMP_INLINE`, and the
//! one-off variables before it) is a channel here. `SLAG_TRACE=inline` turns one
//! on, `SLAG_TRACE=*` turns them all on, and the selection is resolved once, so a
//! disabled channel costs a load and a test with no formatting and no string
//! building. Emitting goes through [`diag!`], which checks the mask before it
//! evaluates its arguments.
//!
//! Adding a channel is one variant plus its name; its stream is separate from
//! every other, which is the point: a body's inliner decisions and its stack maps
//! are read at different times by different people.

use std::sync::OnceLock;

/// A diagnostic stream a build can emit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    /// The optimizing tier's inliner: the sites it found and why it refused.
    Inline,
    /// The live-value analysis at each call, and the values it named.
    Live,
    /// The user stack maps the JIT finalized for a body.
    StackMap,
}

impl Channel {
    const fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The name `SLAG_TRACE` selects this channel by.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Channel::Inline => "inline",
            Channel::Live => "live",
            Channel::StackMap => "stackmap",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "inline" => Some(Channel::Inline),
            "live" => Some(Channel::Live),
            "stackmap" => Some(Channel::StackMap),
            _ => None,
        }
    }
}

static MASK: OnceLock<u32> = OnceLock::new();

fn mask() -> u32 {
    *MASK.get_or_init(|| {
        let Ok(spec) = std::env::var("SLAG_TRACE") else {
            return 0;
        };
        let spec = spec.trim();
        if spec.is_empty() || spec == "0" {
            return 0;
        }
        if spec == "1" || spec == "*" || spec == "all" {
            return u32::MAX;
        }
        let mut mask = 0;
        for name in spec.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            match Channel::from_name(name) {
                Some(channel) => mask |= channel.bit(),
                // A typo silently emitting nothing is worse than one line here.
                None => eprintln!("diag: unknown channel `{name}` (SLAG_TRACE)"),
            }
        }
        mask
    })
}

/// Whether `channel` is selected. Cached: this is called on hot compile paths.
#[must_use]
pub fn enabled(channel: Channel) -> bool {
    mask() & channel.bit() != 0
}

/// Write one line to a channel's stream. Prefer [`diag!`], which skips the call
/// and its arguments entirely when the channel is off.
pub fn report(channel: Channel, args: std::fmt::Arguments<'_>) {
    eprintln!("{}: {args}", channel.name());
}

/// Emit to a diagnostic channel, evaluating the arguments only when it is on.
///
/// ```
/// # use runtime::diag;
/// diag!(Inline, "b{} spliced={}", 3, true);
/// ```
#[macro_export]
macro_rules! diag {
    ($channel:ident, $($arg:tt)*) => {
        if $crate::diag::enabled($crate::diag::Channel::$channel) {
            $crate::diag::report($crate::diag::Channel::$channel, format_args!($($arg)*));
        }
    };
}
