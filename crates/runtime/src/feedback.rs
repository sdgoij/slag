//! Per-site feedback records for the optimizing tier (`.notes/optimizing-tier-plan.md`
//! stage O).
//!
//! A record is keyed by `(body, step)`: it lives on the
//! [`CompiledBody`](crate::ir::CompiledBody) at the step's index and is written
//! by the interpreter handlers as they run a site. Nothing consumes the records
//! yet — this increment is the substrate and the count probe (`SLAG_FEEDBACK`),
//! so a default build allocates nothing and behaves exactly as before.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The `ICState` valve (`optimizing-tier-plan.md` §3.2): how many distinct
/// shapes a site may specialize to before it turns generic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IcState {
    /// One shape seen (or none): a monomorphic guarded fast path pays.
    Specialized,
    /// Several shapes seen: a polymorphic stub still pays, but is bounded.
    Megamorphic,
    /// Too many shapes seen: a guard can no longer pay, so the site is generic.
    Generic,
}

/// `MaxOptimizedStubs` (`.notes/optimizing-tier-plan.md` §3.2): the number of
/// distinct shapes a site may accumulate before it turns megamorphic.
pub const MAX_OPTIMIZED_STUBS: usize = 6;

/// The adaptation budget of §3.2 (`maxFailures = 5 + 40 * stubs`): how many
/// failed specializations a site may see before it stops trying.
#[must_use]
pub const fn max_failures(stubs: usize) -> u32 {
    5 + 40 * stubs as u32
}

/// A member-read site's record: the receiver maps it has served, in first-seen
/// order (bounded by [`MAX_OPTIMIZED_STUBS`]), and the resulting valve state.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemberReadSite {
    maps: [u64; MAX_OPTIMIZED_STUBS],
    len: u8,
    generic: bool,
    hits: u64,
    failures: u32,
}

impl MemberReadSite {
    /// Record a read whose receiver had map id `map` (`0` = a receiver with no
    /// speculatable shape — a primitive or an exotic object). Returns the valve
    /// state after observing it.
    pub fn observe(&mut self, map: u64) -> IcState {
        self.hits = self.hits.saturating_add(1);
        if map != 0 {
            let len = self.len as usize;
            if self.maps[..len].contains(&map) {
                return self.state();
            }
            if len < self.maps.len() {
                self.maps[len] = map;
                self.len += 1;
            } else {
                self.generic = true;
            }
        }
        self.state()
    }

    /// A failed specialization (a guard that did not hold at run time).
    pub fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    /// The valve state implied by the shapes seen so far.
    #[must_use]
    pub fn state(&self) -> IcState {
        if self.generic {
            IcState::Generic
        } else if self.len <= 1 {
            IcState::Specialized
        } else {
            IcState::Megamorphic
        }
    }

    /// The distinct receiver maps this site has served.
    #[must_use]
    pub fn distinct_maps(&self) -> usize {
        self.len as usize
    }

    /// The reads this site has served.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Whether the site has spent its adaptation budget (a later increment
    /// reads this to stop re-specializing).
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.failures > max_failures(self.distinct_maps())
    }
}

/// One step's record. A single variant now; the enum is the seam for the other
/// site classes (global, call, arithmetic) the later increments add.
#[derive(Clone, Copy, Debug, Default)]
pub enum SiteRecord {
    #[default]
    Empty,
    MemberRead(MemberReadSite),
}

/// The per-body feedback store: one [`SiteRecord`] per step, allocated lazily
/// and only when the probe is enabled.
#[derive(Clone, Debug, Default)]
pub struct Feedback {
    sites: Box<[SiteRecord]>,
}

impl Feedback {
    /// A store with one empty record per step of the body.
    #[must_use]
    pub fn new(steps: usize) -> Self {
        Feedback {
            sites: vec![SiteRecord::Empty; steps].into_boxed_slice(),
        }
    }

    /// The member-read record at step `ip`, initialized on first use.
    pub fn member_read(&mut self, ip: usize) -> Option<&mut MemberReadSite> {
        match self.sites.get_mut(ip)? {
            record @ SiteRecord::Empty => {
                *record = SiteRecord::MemberRead(MemberReadSite::default());
                match record {
                    SiteRecord::MemberRead(site) => Some(site),
                    SiteRecord::Empty => None,
                }
            }
            SiteRecord::MemberRead(site) => Some(site),
        }
    }

    /// The record at step `ip`, whatever its class.
    #[must_use]
    pub fn site(&self, ip: usize) -> Option<&SiteRecord> {
        self.sites.get(ip)
    }
}

/// Whether the feedback probe is active (`SLAG_FEEDBACK`). Resolved once and
/// cached, so a hot site pays one predictable load.
#[must_use]
pub fn enabled() -> bool {
    RESOLVE.call_once(|| {
        let on = std::env::var("SLAG_FEEDBACK").is_ok_and(|v| v != "0");
        ENABLED.store(on, Ordering::Relaxed);
    });
    ENABLED.load(Ordering::Relaxed)
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static RESOLVE: std::sync::Once = std::sync::Once::new();

/// Force the probe on/off, bypassing the environment (tests only). Marks the
/// resolve as done so a later [`enabled`] does not override the forced value.
#[cfg(test)]
pub(crate) fn force_enabled(on: bool) {
    RESOLVE.call_once(|| {});
    ENABLED.store(on, Ordering::Relaxed);
}

/// The number of feedback records written since process start (the stage-O
/// count probe).
pub static WRITES: AtomicUsize = AtomicUsize::new(0);

/// Count one written record.
pub fn record_write() {
    WRITES.fetch_add(1, Ordering::Relaxed);
}

/// The count `record_write` has accumulated.
#[must_use]
pub fn writes() -> usize {
    WRITES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_is_specialized_then_megamorphic_then_generic() {
        let mut site = MemberReadSite::default();
        assert_eq!(site.observe(7), IcState::Specialized);
        assert_eq!(site.observe(7), IcState::Specialized);
        assert_eq!(site.distinct_maps(), 1);
        assert_eq!(site.observe(9), IcState::Megamorphic);
        // The first `MAX_OPTIMIZED_STUBS` distinct maps stay megamorphic.
        for map in 10..(10 + MAX_OPTIMIZED_STUBS as u64) {
            let _ = site.observe(map);
        }
        assert_eq!(site.distinct_maps(), MAX_OPTIMIZED_STUBS);
        // One more distinct shape overflows the stub budget: generic, and the
        // log stops growing.
        assert_eq!(site.observe(1000), IcState::Generic);
        assert_eq!(site.distinct_maps(), MAX_OPTIMIZED_STUBS);
        assert_eq!(site.observe(7), IcState::Generic);
    }

    #[test]
    fn a_shapeless_receiver_does_not_pollute_the_log() {
        let mut site = MemberReadSite::default();
        assert_eq!(site.observe(0), IcState::Specialized);
        assert_eq!(site.observe(0), IcState::Specialized);
        assert_eq!(site.distinct_maps(), 0);
        assert_eq!(site.hits(), 2);
    }

    #[test]
    fn the_budget_scales_with_the_stub_count() {
        let mut site = MemberReadSite::default();
        let _ = site.observe(1);
        let _ = site.observe(2);
        assert!(!site.over_budget());
        for _ in 0..=max_failures(2) {
            site.fail();
        }
        assert!(site.over_budget());
    }

    #[test]
    fn a_store_materializes_a_site_on_first_use() {
        let mut feedback = Feedback::new(3);
        assert!(feedback.site(0).is_some());
        assert!(matches!(feedback.site(0), Some(SiteRecord::Empty)));
        assert!(feedback.member_read(2).is_some());
        assert!(matches!(feedback.site(2), Some(SiteRecord::MemberRead(_))));
        // A step index past the body is simply absent.
        assert!(feedback.member_read(9).is_none());
    }

    #[test]
    fn the_probe_counts_member_reads() {
        // The interpreter wires `Step::GetMemberName` to `record_member_read`;
        // with the probe on, a script that reads members must record them.
        force_enabled(true);
        let before = writes();
        let mut agent = crate::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let _ = agent
            .run_script("var o = { a: 1, b: 2, c: 3 }; o.a; o.b; o.c; o.a; o.b; o.c; o.a;")
            .expect("runs");
        let wrote = writes() - before;
        force_enabled(false);
        assert!(wrote > 0, "the probe recorded {wrote} member reads");
    }
}
