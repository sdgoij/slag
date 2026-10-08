//! Per-site feedback records for the optimizing tier (`.notes/optimizing-tier-plan.md`
//! stage O).
//!
//! A record is keyed by `(body, step)`: it lives on the
//! [`CompiledBody`](crate::ir::CompiledBody) at the step's index and is written
//! by the interpreter handlers as they run a site. The valve acts — a site that
//! overflows its stub budget turns generic and stops being recorded — and the
//! count probe ([`summary`]) reports how many sites did, which is the plan's
//! "retirements that would fire" signal. No transform consumes a record yet, so
//! a default build (`SLAG_FEEDBACK` unset) allocates nothing and behaves exactly
//! as before.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crux::Value;
use crux::heap::{GcAny, Trace};

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
    /// speculatable shape — a primitive or an exotic object). Returns what the
    /// observation did to the record; a site that is already generic is frozen
    /// (the valve's action: a generic site costs nothing more).
    pub fn observe(&mut self, map: u64) -> Observed {
        self.hits = self.hits.saturating_add(1);
        if self.generic {
            return Observed::Frozen;
        }
        if map == 0 {
            return Observed::Shapeless;
        }
        let len = self.len as usize;
        if self.maps[..len].contains(&map) {
            return Observed::Repeat;
        }
        if len < self.maps.len() {
            self.maps[len] = map;
            self.len += 1;
            return Observed::NewMap;
        }
        self.generic = true;
        Observed::Overflow
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

/// What one member-read observation did to its site's record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Observed {
    /// The receiver had no speculatable shape (a primitive or an exotic).
    Shapeless,
    /// The map was already in the log.
    Repeat,
    /// A new distinct map entered the log. A transition to the second map means
    /// a monomorphic guard built from the first would have to retire.
    NewMap,
    /// The log was full, so the site turned generic.
    Overflow,
    /// The site was already generic: the record was left untouched (frozen).
    Frozen,
}

/// A call site's record (stage I, `.notes/optimizing-tier-impl.md` I5a): the
/// distinct callee identities it has seen, in first-seen order (bounded by
/// [`MAX_OPTIMIZED_STUBS`]), and the valve state. The identity is the callee's
/// encoded `Value` bits — the same `u64` the compiled call path keys its leaf
/// record on — so a monomorphic site is exactly one a trial inline could guard.
#[derive(Clone, Copy, Debug)]
pub struct CallSite {
    callees: [u64; MAX_OPTIMIZED_STUBS],
    len: u8,
    generic: bool,
    hits: u64,
    /// The first callee's function id (`Function::id()`, never reused) and the
    /// callee itself as a GC `Value`. The id is what a splice resolves (through
    /// the agent's `ecma_functions`), and the `Value` is what `Op::GuardCallee`
    /// compares at run time **and holds alive**: a box is recyclable once its
    /// function is collected, and compiled code cannot clear a guard the way the
    /// leaf caches clear a record, so the retained `Value` is what keeps the
    /// guard exact (I5c-2c-ii). `undefined` when the first callee had no
    /// compiled body (a builtin).
    first_id: u64,
    first_callee: Value,
}

impl Default for CallSite {
    fn default() -> Self {
        CallSite {
            callees: [0; MAX_OPTIMIZED_STUBS],
            len: 0,
            generic: false,
            hits: 0,
            first_id: 0,
            first_callee: Value::Undefined,
        }
    }
}

impl Trace for CallSite {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.first_callee.trace(visit);
    }
}

impl CallSite {
    /// Record a call whose callee's code identity is `identity` (the shared
    /// `CompiledBody` pointer), function id `function_id`, and value `callee`
    /// (`0` identity = a callee with none: a non-function). Returns what the
    /// observation did to the record; a site that is already generic is frozen.
    pub fn observe(&mut self, identity: u64, function_id: u64, callee: Value) -> CallObserved {
        self.hits = self.hits.saturating_add(1);
        if self.generic {
            return CallObserved::Frozen;
        }
        if identity == 0 {
            return CallObserved::Shapeless;
        }
        let len = self.len as usize;
        if self.callees[..len].contains(&identity) {
            return CallObserved::Repeat;
        }
        if len < self.callees.len() {
            self.callees[len] = identity;
            self.len += 1;
            if self.len == 1 {
                self.first_id = function_id;
                self.first_callee = callee;
                return CallObserved::First;
            }
            return CallObserved::NewCallee;
        }
        self.generic = true;
        CallObserved::Overflow
    }

    /// The first callee's function id (`0` when it had no compiled body). The
    /// handle a splice resolves at compile time through the agent.
    #[must_use]
    pub fn first_callee_id(&self) -> u64 {
        self.first_id
    }

    /// The first callee's encoded `Value` bits (the `Op::GuardCallee` expected
    /// constant); `0` when there was no compiled-body callee.
    #[must_use]
    pub fn first_callee_box(&self) -> u64 {
        if self.first_id == 0 {
            0
        } else {
            self.first_callee.bits()
        }
    }

    /// The distinct callees this site has served.
    #[must_use]
    pub fn distinct_callees(&self) -> usize {
        self.len as usize
    }

    /// The calls this site has served.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// The valve state implied by the callees seen so far.
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
}

/// What one call observation did to its site's record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallObserved {
    /// The callee had no identity (a non-function or an unresolved one).
    Shapeless,
    /// The identity was already in the log.
    Repeat,
    /// The first callee this site has seen (the site is new).
    First,
    /// A second-or-later distinct callee: the site went polymorphic.
    NewCallee,
    /// The log was full, so the site turned generic.
    Overflow,
    /// The site was already generic: the record was left untouched (frozen).
    Frozen,
}

/// One step's record. The enum is the seam for the site classes the later
/// increments add.
#[derive(Clone, Copy, Debug, Default)]
pub enum SiteRecord {
    #[default]
    Empty,
    MemberRead(MemberReadSite),
    Call(CallSite),
}

impl Trace for SiteRecord {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // Only a call site retains a GC value (its callee); a member-read site's
        // log is plain map ids.
        if let SiteRecord::Call(site) = self {
            site.trace(visit);
        }
    }
}

/// The per-body feedback store: one [`SiteRecord`] per step, allocated lazily
/// and only when the probe is enabled.
#[derive(Clone, Debug, Default)]
pub struct Feedback {
    sites: Box<[SiteRecord]>,
}

impl Trace for Feedback {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for site in &self.sites {
            site.trace(visit);
        }
    }
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
                    _ => None,
                }
            }
            SiteRecord::MemberRead(site) => Some(site),
            _ => None,
        }
    }

    /// The call record at step `ip`, initialized on first use.
    pub fn call_site(&mut self, ip: usize) -> Option<&mut CallSite> {
        match self.sites.get_mut(ip)? {
            record @ SiteRecord::Empty => {
                *record = SiteRecord::Call(CallSite::default());
                match record {
                    SiteRecord::Call(site) => Some(site),
                    _ => None,
                }
            }
            SiteRecord::Call(site) => Some(site),
            _ => None,
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

/// The count probe of stage O (`.notes/optimizing-tier-plan.md` §6): how many
/// records were written, how many sites went polymorphic ("retirements that
/// would fire" for a monomorphic guard built on the first shape), and how many
/// overflowed their stub budget and turned generic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Records actually written (a frozen or shapeless read writes none).
    pub writes: usize,
    /// Sites that saw a second distinct receiver map.
    pub polymorphic: usize,
    /// Sites that overflowed [`MAX_OPTIMIZED_STUBS`] and turned generic.
    pub generic: usize,
}

static WRITES: AtomicUsize = AtomicUsize::new(0);
static POLYMORPHIC: AtomicUsize = AtomicUsize::new(0);
static GENERIC: AtomicUsize = AtomicUsize::new(0);

/// Count one written record.
pub fn record_write() {
    WRITES.fetch_add(1, Ordering::Relaxed);
}

/// Count a site that went from monomorphic to polymorphic.
pub fn record_polymorphic() {
    POLYMORPHIC.fetch_add(1, Ordering::Relaxed);
}

/// Count a site that overflowed its stub budget and turned generic.
pub fn record_generic() {
    GENERIC.fetch_add(1, Ordering::Relaxed);
}

/// The probe's running totals.
#[must_use]
pub fn summary() -> Summary {
    Summary {
        writes: WRITES.load(Ordering::Relaxed),
        polymorphic: POLYMORPHIC.load(Ordering::Relaxed),
        generic: GENERIC.load(Ordering::Relaxed),
    }
}

/// The number of feedback records written since process start.
#[must_use]
pub fn writes() -> usize {
    summary().writes
}

/// The provisional callee-size budget for the I5a probe
/// (`.notes/optimizing-tier-impl.md`): a monomorphic site whose callee's
/// compiled body is at most this many steps counts as inlinable. The printed
/// size histogram is what fixes the real number for I5c.
pub const CALLEE_BUDGET_STEPS: u32 = 128;

/// The number of step-count buckets in the callee-size histogram.
pub const SIZE_BUCKETS: usize = 5;

/// The bucket a callee's compiled-body step count falls in.
fn size_bucket(steps: u32) -> usize {
    match steps {
        0..=16 => 0,
        17..=64 => 1,
        65..=256 => 2,
        257..=1024 => 3,
        _ => 4,
    }
}

/// Classify one call observation for the I5a probe. `distinct` is the site's
/// distinct-callee count *after* the observation; `certified`/`steps` describe
/// the callee's compiled body (when it has one). The deciding number is
/// `inlinable_hits / hits`: the share of call traffic at a monomorphic site
/// whose callee is a certified body under the budget — the traffic a trial
/// inline would cover before any retirement.
pub fn record_call(observed: CallObserved, distinct: usize, certified: bool, steps: u32) {
    if observed == CallObserved::Frozen {
        return;
    }
    CALL_HITS.fetch_add(1, Ordering::Relaxed);
    if observed == CallObserved::First {
        CALL_SITES.fetch_add(1, Ordering::Relaxed);
    }
    if matches!(observed, CallObserved::First | CallObserved::NewCallee) {
        CALL_SIZE_HIST[size_bucket(steps)].fetch_add(1, Ordering::Relaxed);
    }
    if observed == CallObserved::Overflow {
        CALL_GENERIC_HITS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match distinct {
        1 => {
            CALL_MONO_HITS.fetch_add(1, Ordering::Relaxed);
            if certified && steps <= CALLEE_BUDGET_STEPS {
                CALL_INLINABLE_HITS.fetch_add(1, Ordering::Relaxed);
            }
        }
        2..=MAX_OPTIMIZED_STUBS => {
            CALL_POLY_HITS.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// The I5a call-site probe's running totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallSummary {
    /// Call-site records created (a new site's first callee).
    pub sites: usize,
    /// Every call observation (excluding a frozen site's later calls).
    pub hits: u64,
    /// Observations at a site serving exactly one callee so far.
    pub mono_hits: u64,
    /// Observations at a site serving two or more.
    pub poly_hits: u64,
    /// Observations at a site that overflowed its stub budget.
    pub generic_hits: u64,
    /// Observations at a monomorphic site whose callee is a certified body
    /// under [`CALLEE_BUDGET_STEPS`].
    pub inlinable_hits: u64,
    /// The callee-body step-count histogram (see [`SIZE_BUCKETS`]).
    pub size_hist: [u64; SIZE_BUCKETS],
}

static CALL_SITES: AtomicUsize = AtomicUsize::new(0);
static CALL_HITS: AtomicU64 = AtomicU64::new(0);
static CALL_MONO_HITS: AtomicU64 = AtomicU64::new(0);
static CALL_POLY_HITS: AtomicU64 = AtomicU64::new(0);
static CALL_GENERIC_HITS: AtomicU64 = AtomicU64::new(0);
static CALL_INLINABLE_HITS: AtomicU64 = AtomicU64::new(0);
static CALL_SIZE_HIST: [AtomicU64; SIZE_BUCKETS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The I5a probe's running totals.
#[must_use]
pub fn call_summary() -> CallSummary {
    CallSummary {
        sites: CALL_SITES.load(Ordering::Relaxed),
        hits: CALL_HITS.load(Ordering::Relaxed),
        mono_hits: CALL_MONO_HITS.load(Ordering::Relaxed),
        poly_hits: CALL_POLY_HITS.load(Ordering::Relaxed),
        generic_hits: CALL_GENERIC_HITS.load(Ordering::Relaxed),
        inlinable_hits: CALL_INLINABLE_HITS.load(Ordering::Relaxed),
        size_hist: CALL_SIZE_HIST.each_ref().map(|c| c.load(Ordering::Relaxed)),
    }
}

/// Print the I5a call-site summary (the corpus runner's closing dump). A no-op
/// unless the probe is enabled; one `call\t<key>\t<value>` line each.
pub fn dump_call_summary() {
    if !enabled() {
        return;
    }
    let s = call_summary();
    println!("call\tsites\t{}", s.sites);
    println!("call\thits\t{}", s.hits);
    println!("call\tmono_hits\t{}", s.mono_hits);
    println!("call\tpoly_hits\t{}", s.poly_hits);
    println!("call\tgeneric_hits\t{}", s.generic_hits);
    println!("call\tinlinable_hits\t{}", s.inlinable_hits);
    for (i, count) in s.size_hist.iter().enumerate() {
        println!("call\tsize_bucket_{i}\t{count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe's on/off flag is process-global, so the tests that flip it must
    /// not interleave (one would turn it off inside the other's script).
    fn probe_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn a_site_is_specialized_then_megamorphic_then_generic() {
        let mut site = MemberReadSite::default();
        assert_eq!(site.observe(7), Observed::NewMap);
        assert_eq!(site.state(), IcState::Specialized);
        assert_eq!(site.observe(7), Observed::Repeat);
        assert_eq!(site.distinct_maps(), 1);
        assert_eq!(site.observe(9), Observed::NewMap);
        assert_eq!(site.state(), IcState::Megamorphic);
        // Fill the remaining stubs (two are already in the log).
        for map in 10..(10 + MAX_OPTIMIZED_STUBS as u64 - 2) {
            let _ = site.observe(map);
        }
        assert_eq!(site.distinct_maps(), MAX_OPTIMIZED_STUBS);
        // One more distinct shape overflows the stub budget: generic, the log
        // stops growing, and later reads are frozen.
        assert_eq!(site.observe(1000), Observed::Overflow);
        assert_eq!(site.state(), IcState::Generic);
        assert_eq!(site.distinct_maps(), MAX_OPTIMIZED_STUBS);
        assert_eq!(site.observe(7), Observed::Frozen);
    }

    #[test]
    fn a_shapeless_receiver_does_not_pollute_the_log() {
        let mut site = MemberReadSite::default();
        assert_eq!(site.observe(0), Observed::Shapeless);
        assert_eq!(site.observe(0), Observed::Shapeless);
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
    fn the_probe_classifies_sites() {
        // One `f` body reads `o.a` on two distinct shapes across calls, so its
        // site goes polymorphic (the retire-fires signal) and records writes.
        let _guard = probe_guard();
        force_enabled(true);
        let before = summary();
        let mut agent = crate::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let _ = agent
            .run_script(
                "function f(o, n) { var s = 0; var i = n; \
                   do { s += o.a; i = i - 1; } while (i > 0); return s; } \
                 var p = { a: 1 }; var q = { a: 2, b: 3 }; \
                 f(p, 3); f(q, 3); f(p, 3);",
            )
            .expect("runs");
        let after = summary();
        force_enabled(false);
        assert!(after.writes > before.writes, "records written");
        assert!(
            after.polymorphic > before.polymorphic,
            "a site went polymorphic"
        );
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
        let _guard = probe_guard();
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

    #[test]
    fn a_call_site_is_monomorphic_then_polymorphic_then_generic() {
        let mut site = CallSite::default();
        let callee = Value::Number(1.0);
        assert_eq!(site.observe(0x1000, 7, callee), CallObserved::First);
        assert_eq!(site.observe(0x1000, 7, callee), CallObserved::Repeat);
        assert_eq!(site.distinct_callees(), 1);
        assert_eq!(site.state(), IcState::Specialized);
        assert_eq!(
            site.observe(0x2000, 0, Value::Undefined),
            CallObserved::NewCallee
        );
        assert_eq!(site.state(), IcState::Megamorphic);
        for id in 0x3000..(0x3000 + MAX_OPTIMIZED_STUBS as u64 - 2) {
            let _ = site.observe(id, 0, Value::Undefined);
        }
        assert_eq!(site.distinct_callees(), MAX_OPTIMIZED_STUBS);
        // One more distinct callee overflows the stub budget.
        assert_eq!(
            site.observe(0x9999, 0, Value::Undefined),
            CallObserved::Overflow
        );
        assert_eq!(site.state(), IcState::Generic);
        assert_eq!(
            site.observe(0x1000, 0, Value::Undefined),
            CallObserved::Frozen
        );
    }

    #[test]
    fn a_call_site_remembers_its_first_callee() {
        // I5c-2a/I5c-2c-ii: the first callee's id is what a splice resolves and
        // its `Value` is what `Op::GuardCallee` compares and what keeps the
        // callee (and its box) alive. They are the retire premise, so a later
        // distinct body must not overwrite them.
        let mut site = CallSite::default();
        let callee = Value::Number(2.5);
        assert_eq!(site.observe(0x1000, 7, callee), CallObserved::First);
        assert_eq!(site.first_callee_id(), 7);
        assert_eq!(site.first_callee_box(), callee.bits());
        assert_eq!(
            site.observe(0x2000, 9, Value::Number(9.0)),
            CallObserved::NewCallee
        );
        assert_eq!(site.first_callee_id(), 7);
        assert_eq!(site.first_callee_box(), callee.bits());
    }

    #[test]
    fn a_shapeless_callee_does_not_pollute_the_call_log() {
        let mut site = CallSite::default();
        assert_eq!(
            site.observe(0, 0, Value::Undefined),
            CallObserved::Shapeless
        );
        assert_eq!(site.distinct_callees(), 0);
        assert_eq!(site.hits(), 1);
    }

    #[test]
    fn the_probe_counts_calls() {
        // The interpreter wires the call steps to `record_call`; with the
        // probe on, a script that calls a function in a loop must record both
        // the site and its observations, and a monomorphic site must dominate.
        let _guard = probe_guard();
        force_enabled(true);
        let before = call_summary();
        let mut agent = crate::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let _ = agent
            .run_script(
                "function f(x) { return x + 1; }\n\
                 var s = 0;\n\
                 for (var i = 0; i < 50; i = i + 1) { s = f(s); }\n\
                 s;",
            )
            .expect("runs");
        let after = call_summary();
        force_enabled(false);
        assert!(after.sites > before.sites, "call sites recorded");
        assert!(after.hits > before.hits, "call observations recorded");
        assert!(
            after.mono_hits > before.mono_hits,
            "the loop's call site is monomorphic"
        );
    }

    #[test]
    fn records_key_by_the_executing_step_not_the_next() {
        // The run loop increments `self.ip` before dispatching a step, so a probe
        // that recorded `self.ip` files a record one step too high. The lift's
        // `Op::Call` imm (and the inline pass's site map) index by the true step,
        // so a record must land on the step itself. Regression for the off-by-one
        // that kept I5c splices from firing: without it, every record sat at
        // `step + 1`.
        let _guard = probe_guard();
        force_enabled(true);
        let mut agent = crate::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let _ = agent
            .run_script(
                "function f(x) { return x + 1; }\n\
                 function g(o, n) { var s = 0; for (var i = 0; i < n; i = i + 1) { s = s + o.m(i); } return s; }\n\
                 g({ m: f }, 50);",
            )
            .expect("runs");
        force_enabled(false);
        let mut calls = 0;
        let mut reads = 0;
        for data in agent.ecma_functions.values() {
            let Some(body) = data.ir.as_ref() else {
                continue;
            };
            let store = body.feedback.borrow();
            let Some(store) = store.as_ref() else {
                continue;
            };
            for (ip, step) in body.steps.iter().enumerate() {
                let keyed_call = matches!(store.site(ip), Some(SiteRecord::Call(_)));
                let keyed_read = matches!(store.site(ip), Some(SiteRecord::MemberRead(_)));
                if records_a_call(step) || keyed_call {
                    assert!(
                        records_a_call(step) && keyed_call,
                        "a call at {ip} keys at {ip}"
                    );
                    calls += 1;
                }
                if matches!(step, crate::ir::Step::GetMemberName { .. }) || keyed_read {
                    assert!(
                        matches!(step, crate::ir::Step::GetMemberName { .. }) && keyed_read,
                        "a member read at {ip} keys at {ip}"
                    );
                    reads += 1;
                }
            }
        }
        assert!(calls > 0, "the script exercised a call step");
        assert!(reads > 0, "the script exercised a member-read step");
    }

    /// Whether a step is one the interpreter wires to `record_call` (the six
    /// call shapes that publish a site).
    fn records_a_call(step: &crate::ir::Step) -> bool {
        use crate::ir::Step;
        matches!(
            step,
            Step::Call { .. }
                | Step::CallFast { .. }
                | Step::CallFastGlobal { .. }
                | Step::CallFastSlot { .. }
                | Step::CallFastSlotStore { .. }
                | Step::CallFastGlobalStore { .. }
        )
    }

    #[test]
    fn a_call_store_materializes_a_call_site_on_first_use() {
        let mut feedback = Feedback::new(3);
        assert!(feedback.call_site(1).is_some());
        assert!(matches!(feedback.site(1), Some(SiteRecord::Call(_))));
        // A step past the body is simply absent.
        assert!(feedback.call_site(9).is_none());
    }
}
