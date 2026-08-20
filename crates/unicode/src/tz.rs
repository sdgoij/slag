//! IANA time-zone operations over the generated `tz_data` tables: offset at
//! an instant, the next/previous transition (GetNamedTimeZoneNextTransition /
//! GetNamedTimeZonePreviousTransition), identifier lookup (case-folded), and
//! the canonical primary identifier.

use crate::tz_data::{NAME_INDEX, TzTransition, TzZone, ZONES};

/// The nanoseconds per second.
const NS_PER_SEC: i128 = 1_000_000_000;

/// Resolve a time-zone identifier (ASCII-case-insensitive) to its zone
/// index. `None` when the identifier is not a known named zone.
pub fn resolve_zone(name: &str) -> Option<usize> {
    let folded = name.to_ascii_lowercase();
    NAME_INDEX
        .binary_search_by(|(n, _)| n.as_bytes().cmp(folded.as_bytes()))
        .ok()
        .map(|i| NAME_INDEX[i].1 as usize)
}

/// The zone record for an index.
pub fn zone(zone: usize) -> &'static TzZone {
    &ZONES[zone]
}

/// The canonical primary identifier of a zone (links resolve to their
/// primary).
pub fn primary_identifier(zone: usize) -> &'static str {
    ZONES[zone].primary
}

/// The offset (seconds east of UTC), DST flag, and abbreviation in effect
/// at `epoch_ns`. Instants past the last transition use the zone's terminal
/// offset (the POSIX-footer rules are baked into the generated tables).
pub fn offset_info_at(zone: usize, epoch_ns: i128) -> (i32, bool, &'static str) {
    let z = &ZONES[zone];
    let ts = z.transitions;
    let i = ts.partition_point(|t| t.at_secs as i128 * NS_PER_SEC <= epoch_ns);
    if i == 0 {
        (z.initial_offset, z.initial_dst, z.initial_abbr)
    } else if i >= ts.len() {
        let last = &ts[ts.len() - 1];
        (z.final_offset, false, last.abbr)
    } else {
        let t = &ts[i - 1];
        (t.offset_secs, t.dst, t.abbr)
    }
}

/// The next transition strictly after the instant that changes the offset
/// (GetNamedTimeZoneNextTransition: abbreviation-only / rule changes without
/// an offset transition are skipped — the corpus pins Europe/Paris's 1891
/// LMT->PMT switch, which keeps the same offset, is not a transition).
pub fn next_transition(zone: usize, epoch_ns: i128) -> Option<TzTransition> {
    let z = &ZONES[zone];
    let ts = z.transitions;
    let mut i = ts.partition_point(|t| t.at_secs as i128 * NS_PER_SEC <= epoch_ns);
    while i < ts.len() {
        let offset_before = if i == 0 {
            z.initial_offset
        } else {
            ts[i - 1].offset_secs
        };
        if ts[i].offset_secs != offset_before {
            return Some(ts[i]);
        }
        i += 1;
    }
    None
}

/// The latest transition at or before the instant that changes the offset
/// (GetNamedTimeZonePreviousTransition, with the same offset-preserving
/// filter).
pub fn previous_transition(zone: usize, epoch_ns: i128) -> Option<TzTransition> {
    let z = &ZONES[zone];
    let ts = z.transitions;
    let mut i = ts.partition_point(|t| t.at_secs as i128 * NS_PER_SEC <= epoch_ns);
    while i > 0 {
        let offset_before = if i - 1 == 0 {
            z.initial_offset
        } else {
            ts[i - 2].offset_secs
        };
        if ts[i - 1].offset_secs != offset_before {
            return Some(ts[i - 1]);
        }
        i -= 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> usize {
        resolve_zone(name).unwrap()
    }

    fn offset(name: &str, epoch_secs: i64) -> i32 {
        offset_info_at(zone(name), epoch_secs as i128 * NS_PER_SEC).0
    }

    #[test]
    fn new_york_transitions() {
        // The corpus (getTimeZoneTransition/specific-tzdb-values.js):
        // 2019-11-03T06:00Z fall-back and the 1883-11-18T17:00Z standard-time
        // introduction.
        assert_eq!(offset("America/New_York", 1_555_448_460), -4 * 3600); // 2019-04-16T21:01Z EDT
        assert_eq!(
            next_transition(zone("America/New_York"), 1_555_448_460 * 1_000_000_000)
                .unwrap()
                .at_secs,
            1_572_760_800
        );
        assert_eq!(
            next_transition(zone("America/New_York"), -5_364_662_400 * 1_000_000_000)
                .unwrap()
                .at_secs,
            -2_717_650_800
        );
        assert_eq!(offset("America/New_York", -2_717_650_801), -17_762); // pre-1883 LMT (-04:56:02)
        assert_eq!(offset("America/New_York", -2_717_650_800), -5 * 3600); // EST at the 1883 transition
    }

    #[test]
    fn london_transitions() {
        // The 2020-03-29T01:00Z spring-forward and the 1847-12-01T00:01:15Z
        // LMT -> GMT switch.
        assert_eq!(
            previous_transition(zone("Europe/London"), 1_591_909_260 * 1_000_000_000)
                .unwrap()
                .at_secs,
            1_585_443_600
        );
        assert_eq!(
            previous_transition(zone("Europe/London"), -3_849_984_000 * 1_000_000_000)
                .unwrap()
                .at_secs,
            -3_852_662_325
        );
        assert_eq!(offset("Europe/London", -3_852_662_326), -75); // pre-1847 LMT
    }

    #[test]
    fn sub_minute_and_historical_offsets() {
        // Vancouver's pre-1883 LMT (-08:12:28) and Monrovia's -00:44:30 MMT.
        assert_eq!(offset("America/Vancouver", -2_840_140_800), -29_548);
        assert_eq!(offset("Africa/Monrovia", -631_152_000), -2_670);
    }

    #[test]
    fn los_angeles_1965_transition() {
        // The corpus (offsetNanoseconds/...dst-transition.js): the 1965-04-25
        // CA spring-forward at exactly 09:00:00Z.
        let ns = -147_884_400i64 * 1_000_000_000;
        assert_eq!(
            offset_info_at(zone("America/Los_Angeles"), ns as i128).0,
            -7 * 3600
        );
        assert_eq!(
            offset_info_at(zone("America/Los_Angeles"), ns as i128 - 1).0,
            -8 * 3600
        );
        assert_eq!(
            offset_info_at(zone("America/Los_Angeles"), ns as i128 + 1).0,
            -7 * 3600
        );
    }

    #[test]
    fn apia_dateline_jump() {
        // Pacific/Apia: DST -10:00 until the 2011-12-30T10:00Z dateline jump
        // to +14:00 (DST), then +13:00 standard from 2012.
        assert_eq!(offset("Pacific/Apia", 1_322_496_000), -10 * 3600); // 2011-12-28T00:00Z (DST)
        assert_eq!(offset("Pacific/Apia", 1_325_289_600), 14 * 3600); // 2011-12-31T00:00Z (+14 DST)
        assert_eq!(offset("Pacific/Apia", 1_335_830_400), 13 * 3600); // 2012-05-01T00:00Z (+13 std)
    }

    #[test]
    fn links_and_case() {
        assert_eq!(primary_identifier(zone("Asia/Calcutta")), "Asia/Kolkata");
        assert_eq!(primary_identifier(zone("europe/kiev")), "Europe/Kyiv");
        assert_eq!(primary_identifier(zone("US/Eastern")), "America/New_York");
        assert!(resolve_zone("Etc/GMT+24").is_none());
        assert!(resolve_zone("Not/AZone").is_none());
    }
}
