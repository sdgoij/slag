//! `test262-sweep`: run test262 fixtures in small, parallel, timeout-guarded
//! batches (PLAN Phase 18 conformance tooling).
//!
//! A whole sweep is tens of thousands of fixtures, and a single hanging
//! fixture (usually an interpreter bug) can stall an in-process runner
//! forever — Rust threads cannot be terminated. This runner therefore
//! isolates each *batch* in a child process: the parent kills any batch that
//! exceeds its deadline, then re-runs the un-reported fixtures individually
//! to pinpoint the hang. Fixtures run through the same `harness::run_fixture`
//! path the vendored `cargo test -p test262` tests use.
//!
//! Usage:
//!   test262-sweep [area] [options]
//!
//! Area (default `all`):
//!   language | built-ins | annexB | all
//!
//! Options:
//!   --jobs N             concurrent batches (default: available parallelism)
//!   --batch N            fixtures per batch (default: 32)
//!   --timeout SECS       per-batch deadline (default: 30)
//!   --recheck-timeout S  per-fixture hang-recheck deadline (default: 5)
//!   --sample N           at most N fixtures per top-level directory
//!   --filter GLOB        only fixtures whose relative path matches (`*`, `?`)
//!   --json               emit a JSON report instead of the text report
//!   --worker             (internal) run the batch described on stdin

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use test262::harness::{Area, FixtureResult, collect_js_files, run_fixture};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--worker") {
        return worker_main();
    }
    match run_parent(&args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("test262-sweep: {message}");
            ExitCode::from(2)
        }
    }
}

// ---- shared protocol ----

/// The worker's area tokens and the report's area names.
fn area_label(area: Area) -> &'static str {
    match area {
        Area::Language => "language",
        Area::Builtins => "built-ins",
        Area::AnnexB => "annexB",
    }
}

fn parse_area(label: &str) -> Option<Area> {
    match label {
        "language" => Some(Area::Language),
        "built-ins" => Some(Area::Builtins),
        "annexB" => Some(Area::AnnexB),
        _ => None,
    }
}

/// A single fixture: which area root and the path relative to it.
#[derive(Debug, Clone)]
struct Fixture {
    area: Area,
    relative: String,
}

/// The outcome of one fixture.
#[derive(Debug, Clone)]
enum SweepResult {
    Pass,
    Skip(String),
    Fail(String),
    /// The batch process died while the fixture was running.
    Crash(String),
    /// The fixture exceeded its deadline twice (batch + individual recheck).
    Hang,
}

/// One worker output line: `STATUS\tpath\tdetail`.
fn parse_worker_line(line: &str) -> Option<(&str, &str, &str)> {
    let mut parts = line.splitn(3, '\t');
    let status = parts.next()?;
    let path = parts.next()?;
    let detail = parts.next().unwrap_or("");
    Some((status, path, detail))
}

/// Flatten newlines/tabs out of a worker detail so the protocol stays
/// line-oriented.
fn sanitize(detail: &str) -> String {
    detail
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

// ---- worker mode ----

fn worker_main() -> ExitCode {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((area_token, relative)) = line.split_once('\t') else {
            continue;
        };
        let Some(area) = parse_area(area_token) else {
            continue;
        };
        let (status, detail) = match run_fixture(area, relative) {
            FixtureResult::Pass => ("PASS", String::new()),
            FixtureResult::Skip(reason) => ("SKIP", reason),
            FixtureResult::Fail(reason) => ("FAIL", reason),
        };
        println!("{status}\t{relative}\t{}", sanitize(&detail));
        let _ = stdout.lock().flush();
    }
    ExitCode::SUCCESS
}

// ---- parent mode ----

struct Options {
    jobs: usize,
    batch: usize,
    timeout: Duration,
    recheck_timeout: Duration,
    sample: Option<usize>,
    filter: Option<String>,
    json: bool,
    areas: Vec<Area>,
}

const USAGE: &str = "\
usage: test262-sweep [area] [options]

area: language | built-ins | annexB | all (default: all)

options:
  --jobs N             concurrent batches (default: available parallelism)
  --batch N            fixtures per batch (default: 32)
  --timeout SECS       per-batch deadline (default: 30)
  --recheck-timeout S  per-fixture hang-recheck deadline (default: 5)
  --sample N           at most N fixtures per top-level directory
  --filter GLOB        only fixtures whose relative path matches (* and ?)
  --json               emit a JSON report instead of the text report
  --help, -h";

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        jobs: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
        batch: 32,
        timeout: Duration::from_secs(30),
        recheck_timeout: Duration::from_secs(5),
        sample: None,
        filter: None,
        json: false,
        areas: vec![Area::Language, Area::Builtins, Area::AnnexB],
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--jobs" => {
                index += 1;
                options.jobs = parse_usize(args, index, "--jobs")?;
            }
            "--batch" => {
                index += 1;
                options.batch = parse_usize(args, index, "--batch")?;
            }
            "--timeout" => {
                index += 1;
                options.timeout = Duration::from_secs(parse_u64(args, index, "--timeout")?);
            }
            "--recheck-timeout" => {
                index += 1;
                options.recheck_timeout =
                    Duration::from_secs(parse_u64(args, index, "--recheck-timeout")?);
            }
            "--sample" => {
                index += 1;
                options.sample = Some(parse_usize(args, index, "--sample")?);
            }
            "--filter" => {
                index += 1;
                options.filter = Some(
                    args.get(index)
                        .ok_or_else(|| "--filter needs a value".to_string())?
                        .clone(),
                );
            }
            "--json" => options.json = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "language" => options.areas = vec![Area::Language],
            "built-ins" => options.areas = vec![Area::Builtins],
            "annexB" => options.areas = vec![Area::AnnexB],
            "all" => options.areas = vec![Area::Language, Area::Builtins, Area::AnnexB],
            other => return Err(format!("unknown argument {other}")),
        }
        index += 1;
    }
    if options.jobs == 0 || options.batch == 0 {
        return Err("--jobs and --batch must be positive".into());
    }
    Ok(options)
}

fn parse_usize(args: &[String], index: usize, flag: &str) -> Result<usize, String> {
    let value = args
        .get(index)
        .ok_or_else(|| format!("{flag} needs a value"))?;
    value
        .parse()
        .map_err(|_| format!("{flag} expects a number, got {value}"))
}

fn parse_u64(args: &[String], index: usize, flag: &str) -> Result<u64, String> {
    let value = args
        .get(index)
        .ok_or_else(|| format!("{flag} needs a value"))?;
    value
        .parse()
        .map_err(|_| format!("{flag} expects a number, got {value}"))
}

/// Collect the fixtures for the selected areas, sorted, with the optional
/// sample cap per top-level directory and the path filter applied.
fn collect_fixtures(options: &Options) -> Result<Vec<Fixture>, String> {
    let mut fixtures = Vec::new();
    for &area in &options.areas {
        let mut files = Vec::new();
        collect_js_files(&area.root(), &mut files)
            .map_err(|error| format!("{}: {error}", area_label(area)))?;
        files.sort();
        let mut per_dir: BTreeMap<String, usize> = BTreeMap::new();
        for path in files {
            let relative = path
                .strip_prefix(area.root())
                .expect("collected under the area root")
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(filter) = &options.filter
                && !glob_match(filter, &relative)
            {
                continue;
            }
            let top = relative.split('/').next().unwrap_or("").to_string();
            if let Some(limit) = options.sample {
                let seen = per_dir.entry(top).or_default();
                if *seen >= limit {
                    continue;
                }
                *seen += 1;
            }
            fixtures.push(Fixture { area, relative });
        }
    }
    Ok(fixtures)
}

/// Minimal `*`/`?` glob match (fnmatch-style, with backtracking `*`).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_ti) = (usize::MAX, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = pi;
            star_ti = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

enum Msg {
    BatchDone(Vec<(String, SweepResult)>),
}

fn run_parent(args: &[String]) -> Result<u8, String> {
    let options = std::sync::Arc::new(parse_options(args)?);
    let fixtures = collect_fixtures(&options)?;
    if fixtures.is_empty() {
        return Err("no fixtures matched".into());
    }
    let batches: Vec<Vec<Fixture>> = fixtures.chunks(options.batch).map(|c| c.to_vec()).collect();
    eprintln!(
        "test262-sweep: {} fixtures, {} batches, {} jobs, {}s batch timeout",
        fixtures.len(),
        batches.len(),
        options.jobs,
        options.timeout.as_secs()
    );

    let (tx, rx) = mpsc::channel();
    let mut next = 0usize;
    let mut active = 0usize;
    let mut results: BTreeMap<String, SweepResult> = BTreeMap::new();
    let mut finished = 0usize;
    while next < batches.len() || active > 0 {
        while active < options.jobs && next < batches.len() {
            let batch = batches[next].clone();
            next += 1;
            let tx = tx.clone();
            let opts = options.clone();
            std::thread::spawn(move || run_batch(batch, &opts, tx));
            active += 1;
        }
        match rx.recv() {
            Ok(Msg::BatchDone(batch_results)) => {
                active -= 1;
                for (path, result) in batch_results {
                    results.insert(path, result);
                }
                finished += 1;
                if finished == batches.len() || finished.is_multiple_of(32) {
                    eprintln!("  {finished}/{} batches", batches.len());
                }
            }
            Err(_) => break,
        }
    }
    report(&options, &results, fixtures.len());
    let has_issues = results.values().any(|result| {
        matches!(
            result,
            SweepResult::Fail(_) | SweepResult::Crash(_) | SweepResult::Hang
        )
    });
    Ok(if has_issues { 1 } else { 0 })
}

/// Run one batch in a child process, kill it on the deadline, and re-check
/// any un-reported fixtures individually to pinpoint hangs.
fn run_batch(batch: Vec<Fixture>, options: &std::sync::Arc<Options>, tx: Sender<Msg>) {
    let results = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_batch_inner(&batch, options)
    })) {
        Ok(results) => results,
        Err(_) => batch
            .into_iter()
            .map(|fixture| {
                (
                    fixture.relative,
                    SweepResult::Crash("batch thread panicked".into()),
                )
            })
            .collect(),
    };
    let _ = tx.send(Msg::BatchDone(results));
}

fn run_batch_inner(batch: &[Fixture], options: &Options) -> Vec<(String, SweepResult)> {
    let mut child = spawn_worker(batch);
    let stdout = child.stdout.take().expect("worker stdout");
    let (line_tx, line_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });

    let timed_out = !wait_for_child(&mut child, options.timeout);
    // The child is dead or killed, so its stdout pipe is closed and the
    // reader thread has drained and ended.
    let _ = reader.join();
    let lines: Vec<String> = line_rx.try_iter().collect();

    let mut reported: BTreeMap<String, SweepResult> = BTreeMap::new();
    for line in lines {
        if let Some((status, path, detail)) = parse_worker_line(&line) {
            let result = match status {
                "PASS" => SweepResult::Pass,
                "SKIP" => SweepResult::Skip(detail.to_string()),
                "FAIL" => SweepResult::Fail(detail.to_string()),
                _ => continue,
            };
            reported.insert(path.to_string(), result);
        }
    }

    let mut results = Vec::with_capacity(batch.len());
    let mut hangs = Vec::new();
    for fixture in batch {
        if let Some(result) = reported.get(&fixture.relative) {
            results.push((fixture.relative.clone(), result.clone()));
        } else if timed_out {
            hangs.push(fixture.clone());
        } else {
            results.push((
                fixture.relative.clone(),
                SweepResult::Crash("batch process died mid-fixture".into()),
            ));
        }
    }
    for fixture in hangs {
        results.push((fixture.relative.clone(), run_single(&fixture, options)));
    }
    results
}

/// Re-run one fixture on its own with a short deadline; times out => Hang.
fn run_single(fixture: &Fixture, options: &Options) -> SweepResult {
    let mut child = spawn_worker(std::slice::from_ref(fixture));
    let stdout = child.stdout.take().expect("worker stdout");
    let reader = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        reader
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<String>>()
    });
    let timed_out = !wait_for_child(&mut child, options.recheck_timeout);
    let lines = reader.join().unwrap_or_default();
    if timed_out {
        return SweepResult::Hang;
    }
    match lines.first().and_then(|line| parse_worker_line(line)) {
        Some(("PASS", _, _)) => SweepResult::Pass,
        Some(("SKIP", _, detail)) => SweepResult::Skip(detail.to_string()),
        Some(("FAIL", _, detail)) => SweepResult::Fail(detail.to_string()),
        _ => SweepResult::Crash("fixture process died".into()),
    }
}

/// Wait for the child to exit, killing it once `deadline` passes. Returns
/// false when the deadline expired.
fn wait_for_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Spawn a worker child running exactly `batch`, described on its stdin.
fn spawn_worker(batch: &[Fixture]) -> Child {
    let mut input = String::new();
    for fixture in batch {
        input.push_str(area_label(fixture.area));
        input.push('\t');
        input.push_str(&fixture.relative);
        input.push('\n');
    }
    let mut command = Command::new(std::env::current_exe().expect("current exe"));
    command
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().expect("spawn sweep worker");
    let mut stdin = child.stdin.take().expect("worker stdin");
    stdin
        .write_all(input.as_bytes())
        .expect("write batch to worker");
    drop(stdin);
    child
}

// ---- reporting ----

struct Tally {
    pass: usize,
    skip: usize,
    fail: usize,
    crash: usize,
    hang: usize,
}

fn report(options: &Options, results: &BTreeMap<String, SweepResult>, total: usize) {
    let mut by_dir: BTreeMap<String, Tally> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut hangs = Vec::new();
    let mut skip_reasons: BTreeMap<String, usize> = BTreeMap::new();
    for (path, result) in results {
        let dir = path
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_else(|| ".".into());
        let tally = by_dir.entry(dir).or_insert(Tally {
            pass: 0,
            skip: 0,
            fail: 0,
            crash: 0,
            hang: 0,
        });
        match result {
            SweepResult::Pass => tally.pass += 1,
            SweepResult::Skip(reason) => {
                tally.skip += 1;
                *skip_reasons.entry(reason.clone()).or_default() += 1;
            }
            SweepResult::Fail(reason) => {
                tally.fail += 1;
                failures.push((path.clone(), reason.clone()));
            }
            SweepResult::Crash(reason) => {
                tally.crash += 1;
                failures.push((path.clone(), format!("CRASH: {reason}")));
            }
            SweepResult::Hang => {
                tally.hang += 1;
                hangs.push(path.clone());
            }
        }
    }
    if options.json {
        emit_json(total, results, &failures, &hangs);
        return;
    }
    let mut all = Tally {
        pass: 0,
        skip: 0,
        fail: 0,
        crash: 0,
        hang: 0,
    };
    for tally in by_dir.values() {
        all.pass += tally.pass;
        all.skip += tally.skip;
        all.fail += tally.fail;
        all.crash += tally.crash;
        all.hang += tally.hang;
    }
    let runnable = all.pass + all.fail;
    let runnable_pct = pct(runnable, total);
    let pass_pct = pct(all.pass, runnable);
    println!(
        "== {} pass, {} fail, {} skip, {} crash, {} hang of {} fixtures",
        all.pass, all.fail, all.skip, all.crash, all.hang, total
    );
    println!(
        "   runnable {runnable}/{total} ({runnable_pct:.1}%), pass rate of runnable {pass_pct:.1}%"
    );
    let mut dirs: Vec<(&String, &Tally)> = by_dir.iter().collect();
    dirs.sort_by(|a, b| b.1.fail.cmp(&a.1.fail).then(a.0.cmp(b.0)));
    println!("   worst directories by failing fixtures:");
    for (dir, tally) in dirs
        .iter()
        .filter(|(_, t)| t.fail + t.crash + t.hang > 0)
        .take(20)
    {
        println!(
            "     {dir}: {} pass, {} skip, {} fail, {} crash, {} hang",
            tally.pass, tally.skip, tally.fail, tally.crash, tally.hang
        );
    }
    for (path, reason) in failures.iter().take(15) {
        println!("   FAIL {path}: {reason}");
    }
    for (reason, count) in skip_reasons.iter().take(8) {
        println!("   skip x{count}: {reason}");
    }
    if !hangs.is_empty() {
        println!("   hangs (candidates for engine bugs):");
        for path in hangs.iter().take(20) {
            println!("     HANG {path}");
        }
    }
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn emit_json(
    total: usize,
    results: &BTreeMap<String, SweepResult>,
    failures: &[(String, String)],
    hangs: &[String],
) {
    let mut pass = 0usize;
    let mut skip = 0usize;
    let mut fail = 0usize;
    let mut crash = 0usize;
    let mut hang = 0usize;
    for result in results.values() {
        match result {
            SweepResult::Pass => pass += 1,
            SweepResult::Skip(_) => skip += 1,
            SweepResult::Fail(_) => fail += 1,
            SweepResult::Crash(_) => crash += 1,
            SweepResult::Hang => hang += 1,
        }
    }
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"total\": {total},\n  \"pass\": {pass},\n  \"fail\": {fail},\n  \"skip\": {skip},\n  \"crash\": {crash},\n  \"hang\": {hang},\n"
    ));
    out.push_str("  \"failures\": [\n");
    for (index, (path, reason)) in failures.iter().enumerate() {
        out.push_str(&format!(
            "    {{\"path\": \"{}\", \"reason\": \"{}\"}}{}\n",
            json_string(path),
            json_string(reason),
            if index + 1 == failures.len() { "" } else { "," }
        ));
    }
    out.push_str("  ],\n  \"hangs\": [\n");
    for (index, path) in hangs.iter().enumerate() {
        out.push_str(&format!(
            "    \"{}\"{}\n",
            json_string(path),
            if index + 1 == hangs.len() { "" } else { "," }
        ));
    }
    out.push_str("  ]\n}\n");
    print!("{out}");
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "a/b.js"));
        assert!(glob_match(
            "built-ins/String/*",
            "built-ins/String/prototype/slice.js"
        ));
        assert!(!glob_match(
            "built-ins/Number/*",
            "built-ins/String/prototype/slice.js"
        ));
        assert!(glob_match("statements/if/?", "statements/if/a"));
        assert!(glob_match("**", "x/y/z.js"));
    }

    #[test]
    fn worker_line_round_trip() {
        let line = "FAIL\tbuilt-ins/String/foo.js\tbad thing";
        let (status, path, detail) = parse_worker_line(line).unwrap();
        assert_eq!(status, "FAIL");
        assert_eq!(path, "built-ins/String/foo.js");
        assert_eq!(detail, "bad thing");
        assert_eq!(parse_worker_line("PASS\tx\t"), Some(("PASS", "x", "")));
    }

    #[test]
    fn sanitize_flattens_control_chars() {
        assert_eq!(sanitize("a\nb\tc"), "a b c");
    }
}
