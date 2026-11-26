//! Temporary measurement (deleted after): what a string literal's evaluation
//! costs with the agent-side cache, including the large and over-budget paths.

#![cfg(test)]

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const ROUNDS: usize = 11;

fn time<F: FnMut()>(iters: usize, mut body: F) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    start.elapsed().as_secs_f64() * 1e9 / iters as f64
}

fn median(mut rounds: Vec<f64>) -> f64 {
    rounds.sort_by(f64::total_cmp);
    rounds[ROUNDS / 2]
}

fn units(text: &str) -> Arc<[u16]> {
    Arc::from(text.encode_utf16().collect::<Vec<u16>>())
}

#[test]
fn literal_cache_shapes() {
    let agent = crate::agent::Agent::new();
    let one = units("a string literal in a hot function");
    let other = units("another string literal in the same program");
    // 100 KiB of text: fits the budget, would never sit in a small slot.
    let mid: Arc<[u16]> = Arc::from(std::iter::repeat_n(b'y' as u16, 50_000).collect::<Vec<u16>>());
    // Over the whole budget: never cached, but still shares its text.
    let big: Arc<[u16]> = Arc::from(
        std::iter::repeat_n(b'x' as u16, crate::expr::LITERAL_CACHE_BUDGET_BYTES / 2 + 1)
            .collect::<Vec<u16>>(),
    );

    let (mut one_rounds, mut two, mut mid_rounds, mut over) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        one_rounds.push(time(50_000, || {
            black_box(agent.literal_string_value(black_box(&one)));
        }));
        two.push(
            time(50_000, || {
                black_box(agent.literal_string_value(black_box(&one)));
                black_box(agent.literal_string_value(black_box(&other)));
            }) / 2.0,
        );
        mid_rounds.push(time(20_000, || {
            black_box(agent.literal_string_value(black_box(&mid)));
        }));
        over.push(time(20_000, || {
            black_box(agent.literal_string_value(black_box(&big)));
        }));
    }
    println!(
        "one literal, cached           {:6.1} ns",
        median(one_rounds)
    );
    println!("two literals alternating      {:6.1} ns each", median(two));
    println!(
        "100 KiB literal, cached       {:6.1} ns",
        median(mid_rounds)
    );
    println!("288 KiB literal, over budget  {:6.1} ns", median(over));
}
