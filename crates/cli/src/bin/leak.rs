//! Leak-detection harness for the GC milestone (docs/gc-plan.md, GC-0).
//!
//! Runs a workload in-process and samples the process working set, so the
//! Rc-model baseline can be measured before the arena + collector lands and
//! re-checked after each GC cut:
//!
//! ```sh
//! cargo run --release -p cli --bin leak cycle            # self-cycle
//! cargo run --release -p cli --bin leak chain            # acyclic chain
//! cargo run --release -p cli --bin leak cycle 1_000_000  # custom iterations
//! ```
//!
//! Under the current `Rc` ownership model the `cycle` workload must show
//! unbounded working-set growth (self-cycles never drop) while `chain` stays
//! flat (acyclic structures are freed). After GC-1 the collector must bound
//! both; the sweep gate for the milestone is a flat `cycle` line.

use runtime::embed::Context;

const WORKLOADS: &[(&str, &str)] = &[
    // A self-referential object: under Rc the cycle keeps the refcount above
    // zero forever, so every iteration leaks one object graph.
    ("cycle", "var o = {}; o.self = o;"),
    // A shallow acyclic linked list: dropping `head` frees the whole chain.
    (
        "chain",
        "var head = null; for (var i = 0; i < 8; i++) { head = { next: head }; }",
    ),
];

const ITERATIONS: usize = 200_000;
const SAMPLE_EVERY: usize = 20_000;

/// Current process working set in kilobytes. Windows reads it from
/// `tasklist` (std-only); elsewhere from `/proc/self/status`.
fn working_set_kb() -> Option<u64> {
    #[cfg(windows)]
    {
        let pid = std::process::id().to_string();
        let filter = format!("PID eq {pid}");
        let output = std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH", "/FI", &filter])
            .output()
            .ok()?;
        let line = String::from_utf8_lossy(&output.stdout);
        // Last CSV field is the memory usage, e.g. `"12,345 K"`.
        let (_, field) = line.rsplit_once("\",\"")?;
        let digits: String = field.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }
    #[cfg(not(windows))]
    {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = text.lines().find(|line| line.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }
}

fn main() {
    let name = match std::env::args().nth(1) {
        Some(name) => name,
        None => {
            eprintln!("usage: leak <cycle|chain> [iterations]");
            std::process::exit(2);
        }
    };
    let source = WORKLOADS
        .iter()
        .find(|(workload, _)| *workload == name)
        .map(|(_, source)| *source)
        .unwrap_or_else(|| {
            eprintln!("unknown workload {name:?}; expected cycle or chain");
            std::process::exit(2);
        });
    let iterations: usize = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(ITERATIONS);

    let mut context = Context::new().expect("failed to create context");
    // Warm up (interning, host hooks) before the first sample.
    context.eval(source).expect("warm-up eval failed");

    println!("workload={name} iterations={iterations}");
    println!("  {iterations:>10} iterations   RSS (KB)");
    let start_kb = working_set_kb().unwrap_or(0);
    println!("  {0:>10} {start_kb:>12}", 0usize);
    for iteration in 1..=iterations {
        context.eval(source).expect("workload eval failed");
        if iteration % SAMPLE_EVERY == 0 {
            let kb = working_set_kb().unwrap_or(0);
            println!("  {iteration:>10} {kb:>12}");
        }
    }
    let end_kb = working_set_kb().unwrap_or(0);
    println!(
        "growth: {end_kb} - {start_kb} = {} KB",
        end_kb.saturating_sub(start_kb)
    );
}
