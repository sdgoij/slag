//! Manual performance harness for the RegExp engine. Not part of the normal
//! test run: `cargo test -p regexp --release --test perf -- --ignored --nocapture`.
//!
//! Criterion would need a network fetch, so this uses a plain wall-clock loop.

use regexp::{Flags, compile};
use std::time::Instant;

fn bench(name: &str, iters: u32, mut f: impl FnMut()) {
    let warmup = (iters / 20).max(1);
    for _ in 0..warmup {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let total = start.elapsed();
    let per = total / iters;
    println!(
        "{name:<32} {iters:>8} iters  {total:>12.3?} total  {:>10.1} ns/iter",
        per.as_nanos() as f64 / 1.0,
    );
    println!();
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn re(pattern: &str, flags: &str) -> regexp::Regex {
    compile(&utf16(pattern), Flags::parse(&utf16(flags)).unwrap()).unwrap()
}

/// The runtime's search path (RegExpBuiltinExec): the engine's leftmost
/// search with its leading-char prefilter.
fn runtime_search(re: &regexp::Regex, input: &[u16]) -> Option<(usize, usize)> {
    match re.search_at(input, 0) {
        Some((index, m)) => Some((index, m[0]?.1)),
        None => None,
    }
}

#[test]
#[ignore]
fn perf() {
    // Search-loop cost over a long input.
    {
        let input: Vec<u16> = vec![b'x' as u16; 100_000];
        let re = re("abc", "");
        bench("literal /abc/ no match 100k", 20, || {
            assert!(runtime_search(&re, &input).is_none());
        });
    }
    {
        let mut input: Vec<u16> = vec![b'x' as u16; 100_000];
        input.extend(utf16("abc"));
        let re = re("abc", "");
        bench("literal /abc/ match at end", 20, || {
            assert!(runtime_search(&re, &input).is_some());
        });
    }
    {
        let input: Vec<u16> = vec![b'x' as u16; 100_000];
        let re = re("(foo|bar|baz|qux|quux|corge|grault|garply|waldo|fred)", "");
        bench("alternation no match 100k", 20, || {
            assert!(runtime_search(&re, &input).is_none());
        });
    }
    {
        // The pattern matches empty at every position: worst case for the
        // search loop's per-attempt overhead (N matcher attempts, O(N); the
        // engine reuses its capture buffer across attempts).
        let input: Vec<u16> = vec![b'x' as u16; 200_000];
        let re = re("a*", "");
        bench("empty-match /a*/ every pos 200k", 3, || {
            let mut index = 0;
            let mut count = 0u32;
            while let Some((i, m)) = re.search_at(&input, index) {
                let _ = m;
                count += 1;
                if i >= input.len() {
                    break;
                }
                index = re.advance_string_index(&input, i);
            }
            assert_eq!(count, input.len() as u32 + 1);
        });
    }

    // Atom-level matching cost.
    {
        let input: Vec<u16> = vec![b'a' as u16; 200_000];
        let re = re("a+", "");
        bench("greedy repeat /a+/ on 200k", 3, || {
            let m = re.exec(&input, 0).unwrap();
            assert_eq!(m[0], Some((0, input.len())));
        });
    }
    {
        let input: Vec<u16> = vec![b'a' as u16; 10_000];
        let re = re("(a+)", "");
        bench("greedy capture /(a+)/ on 10k", 20, || {
            let m = re.exec(&input, 0).unwrap();
            assert_eq!(m[1], Some((0, input.len())));
        });
    }
    {
        let input: Vec<u16> = vec![b'a' as u16; 100_000];
        let re = re("a*$", "");
        bench("greedy repeat /a*$/ on 100k", 3, || {
            assert!(re.exec(&input, 0).is_some());
        });
    }
    {
        let input: Vec<u16> = vec![b'a' as u16; 50_000];
        let re = re("[a-zA-Z_0-9]+", "");
        bench("class repeat /[a-zA-Z_0-9]+/ 50k", 5, || {
            assert!(re.exec(&input, 0).is_some());
        });
    }

    // Feature-specific cost.
    {
        let mut input: Vec<u16> = vec![b'x' as u16; 50_000];
        input.extend(utf16("ABCDEFGHIJ"));
        let re = re("abcdefghij", "i");
        bench("ignore-case /abcdefghij/i match at end", 20, || {
            assert!(runtime_search(&re, &input).is_some());
        });
    }
    {
        let input: Vec<u16> = vec![b'A' as u16; 20_000];
        let re = re("\\p{L}+", "u");
        bench("unicode /\\p{L}+/u on 20k", 3, || {
            assert!(re.exec(&input, 0).is_some());
        });
    }
    {
        let mut input: Vec<u16> = vec![b'a' as u16; 10_000];
        input.push(b'b' as u16);
        input.extend(utf16("aaaa"));
        let re = re("(a+)b\\1", "");
        bench("backref /(a+)b\\1/ on 10k", 10, || {
            assert!(re.exec(&input, 0).is_some());
        });
    }
    {
        // Pathological: exponential backtracking is inherent to the
        // backtracking design (V8/PCRE blow up here too). Documented for
        // reference; not expected to be fast.
        let mut input: Vec<u16> = vec![b'a' as u16; 20];
        input.push(b'c' as u16);
        let re = re("a*a*a*a*b", "");
        bench("pathological /a*a*a*a*b/ on 20", 3, || {
            let _ = re.exec(&input, 0);
        });
    }

    // Predicate-class prefilter (R3): `\d` now contributes its leading-char
    // set, so non-digit positions are skipped.
    {
        let mut input: Vec<u16> = vec![b'x' as u16; 100_000];
        input.extend(utf16("123456"));
        let re = re("\\d{6}", "");
        bench("digit /\\d{6}/ match at end", 20, || {
            assert!(runtime_search(&re, &input).is_some());
        });
    }

    // Linear-sequence capture repeat (R4).
    {
        let input: Vec<u16> = "ab".repeat(100_000).encode_utf16().collect();
        let re = re("(ab)+", "");
        bench("linear repeat /(ab)+/ on 200k", 3, || {
            let m = re.exec(&input, 0).unwrap();
            assert_eq!(m[1], Some((input.len() - 2, input.len())));
        });
    }

    // R5.2 failure memo: catastrophic nested repeats are polynomial now.
    {
        for n in [25, 50, 100] {
            let input: Vec<u16> = "a".repeat(n).encode_utf16().collect();
            let re = re("(a+)+b", "");
            bench("memoized /(a+)+b/ no match", 3, || {
                assert!(re.exec(&input, 0).is_none());
            });
        }
        let input: Vec<u16> = "a".repeat(100).encode_utf16().collect();
        let re2 = re("(?:a*)*b", "");
        bench("memoized /(?:a*)*b/ no match 100", 3, || {
            assert!(re2.exec(&input, 0).is_none());
        });
        // A `{2,}`-or-tighter ancestor disables the exhausted memo (the
        // continuation depends on its iteration count); correctness holds,
        // but the input here is small on purpose.
        let input: Vec<u16> = "a".repeat(12).encode_utf16().collect();
        let re3 = re("(a+){3,}b", "");
        bench("ungated /(a+){3,}b/ no match 12", 3, || {
            assert!(re3.exec(&input, 0).is_none());
        });
    }

    // Compile cost.
    {
        let pattern = utf16("(?<year>\\d{4})-(?<month>\\d{2})-(?<day>\\d{2})");
        let flags = Flags::parse(&utf16("u")).unwrap();
        bench("compile date regex 10k", 10_000, || {
            compile(&pattern, flags).unwrap();
        });
    }
}
