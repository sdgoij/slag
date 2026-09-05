//! The wasm conformance runner.
//!
//! Drives the pinned `waspec/test/core` corpus through the `crates/wasm`
//! engine. The corpus is `.wast` (text + assertions); the engine never
//! parses text, so `wast2json` (wabt; or the Rust wast2json-rs) turns each
//! file into one `.wasm` per module plus a `.json` of commands. See
//! `.notes/wasm-plan.md` for the cut plan.
//!
//! Command outcomes are classified by what the current engine can judge:
//! module decodes and `assert_malformed` are decidable now (Cut 1);
//! `assert_invalid` needs validation (Cut 2) and invocations/`assert_return`
//! need execution (Cut 3) — those count as *pending*, not pass or fail, so
//! the runner stays honest as cuts land.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;
use wasm::valid::Error as ValidError;
use wasm::{DecodeError, decode, validate};

const USAGE: &str = "\
wasmtest — WebAssembly conformance runner

usage:
  wasmtest check <file.wasm|dir>   decode modules and report per-file status
  wasmtest convert <file.wast>     compile a .wast with wast2json (wabt)
  wasmtest run <wast|json|dir>     convert (if needed) and run a suite
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print!("{USAGE}");
        return ExitCode::from(2);
    };
    match command.as_str() {
        "check" => {
            let Some(path) = args.next() else {
                eprintln!("wasmtest check: missing path\n\n{USAGE}");
                return ExitCode::from(2);
            };
            check(Path::new(&path))
        }
        "convert" => {
            let Some(path) = args.next() else {
                eprintln!("wasmtest convert: missing path\n\n{USAGE}");
                return ExitCode::from(2);
            };
            match convert_wast(Path::new(&path)) {
                Ok(json) => {
                    println!("wrote {}", json.display());
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            }
        }
        "run" => {
            let Some(path) = args.next() else {
                eprintln!("wasmtest run: missing path\n\n{USAGE}");
                return ExitCode::from(2);
            };
            run(Path::new(&path))
        }
        _ => {
            eprintln!("unknown command {command:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

// ---- check: decode a directory/file of .wasm ----

fn check(path: &Path) -> ExitCode {
    let mut files = Vec::new();
    collect_wasm(path, &mut files);
    if files.is_empty() {
        eprintln!("wasmtest check: no .wasm files at {path:?}");
        return ExitCode::from(1);
    }
    let total = files.len();
    let mut failed = 0usize;
    for file in files {
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("read  {}: {error}", file.display());
                failed += 1;
                continue;
            }
        };
        match decode(&bytes) {
            Ok(_) => println!("ok    {}", file.display()),
            Err(error) => {
                println!("FAIL  {}: {error}", file.display());
                failed += 1;
            }
        }
    }
    println!("\n{total} module(s), {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---- run: convert (if needed) and execute a suite's commands ----

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: usize,
    pending: usize,
}

fn run(path: &Path) -> ExitCode {
    let mut jsons = Vec::new();
    if path.extension().is_some_and(|ext| ext == "json") {
        jsons.push(path.to_path_buf());
    } else if path.extension().is_some_and(|ext| ext == "wast") {
        match convert_wast(path) {
            Ok(json) => jsons.push(json),
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        }
    } else if path.is_dir() {
        collect_json(path, &mut jsons);
    } else {
        eprintln!("wasmtest run: expected a .wast, a .json, or a directory of jsons");
        return ExitCode::from(2);
    }

    if jsons.is_empty() {
        eprintln!("wasmtest run: no suites found at {path:?}");
        return ExitCode::FAILURE;
    }

    let mut totals = Tally::default();
    for json in jsons {
        let tally = run_json(&json);
        let label = json.display();
        println!(
            "\n{label}: {} pass, {} fail, {} pending",
            tally.pass, tally.fail, tally.pending
        );
        totals.pass += tally.pass;
        totals.fail += tally.fail;
        totals.pending += tally.pending;
    }
    println!(
        "\ntotal: {} pass, {} fail, {} pending",
        totals.pass, totals.fail, totals.pending
    );
    if totals.fail == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_json(json_path: &Path) -> Tally {
    let mut tally = Tally::default();
    let dir = json_path.parent().unwrap_or_else(|| Path::new("."));
    let text = match fs::read_to_string(json_path) {
        Ok(text) => text,
        Err(error) => {
            tally.fail += 1;
            println!("read {}: {error}", json_path.display());
            return tally;
        }
    };
    let root: Value = match serde_json::from_str(&text) {
        Ok(root) => root,
        Err(error) => {
            tally.fail += 1;
            println!("parse {}: {error}", json_path.display());
            return tally;
        }
    };
    let Some(commands) = root.get("commands").and_then(Value::as_array) else {
        tally.fail += 1;
        println!("parse {}: no commands array", json_path.display());
        return tally;
    };

    for command in commands {
        let line = command.get("line").and_then(Value::as_u64).unwrap_or(0);
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let outcome = match kind {
            "module" => {
                let module = command.get("module").unwrap_or(command);
                match module_file(module, dir) {
                    Some((_, bytes)) => match decode(&bytes) {
                        Err(error) => Outcome::Fail(format!("module decode: {error}")),
                        Ok(module) => match validate(&module) {
                            Ok(_) => Outcome::Pass,
                            Err(error) => Outcome::Fail(format!("module invalid: {error}")),
                        },
                    },
                    None => Outcome::Pending("module without a file"),
                }
            }
            "assert_malformed" => {
                let module = command.get("module").unwrap_or(command);
                match module_file(module, dir) {
                    Some((_, bytes)) => match decode(&bytes) {
                        Err(DecodeError::Malformed(_)) => Outcome::Pass,
                        Err(DecodeError::Unsupported(_)) => Outcome::Fail(
                            "expected malformed, decoder hit unsupported feature".into(),
                        ),
                        Ok(_) => Outcome::Fail("expected malformed, module decoded".into()),
                    },
                    None => Outcome::Pending("assert_malformed without a module file"),
                }
            }
            "assert_invalid" => match module_file(command.get("module").unwrap_or(command), dir) {
                Some((_, bytes)) => match decode(&bytes) {
                    Err(DecodeError::Malformed(message)) => Outcome::Fail(format!(
                        "expected invalid, decoder says malformed: {message}"
                    )),
                    Err(DecodeError::Unsupported(_)) => {
                        Outcome::Fail("expected invalid, decoder hit unsupported feature".into())
                    }
                    Ok(module) => match validate(&module) {
                        Err(ValidError::Invalid(_)) => Outcome::Pass,
                        Ok(_) => Outcome::Fail("expected invalid, module validated".into()),
                    },
                },
                None => Outcome::Pending("assert_invalid without a module file"),
            },
            "assert_unlinkable" => Outcome::Pending("linking (Cut 4)"),
            "register"
            | "action"
            | "assert_return"
            | "assert_trap"
            | "assert_exhaustion"
            | "assert_uninstantiable" => Outcome::Pending("execution (Cut 3)"),
            other => Outcome::Fail(format!("unknown command type {other:?}")),
        };
        match outcome {
            Outcome::Pass => tally.pass += 1,
            Outcome::Pending(reason) => {
                tally.pending += 1;
                println!("pending {}:{} {kind}: {reason}", json_path.display(), line);
            }
            Outcome::Fail(reason) => {
                tally.fail += 1;
                println!("FAIL   {}:{} {kind}: {reason}", json_path.display(), line);
            }
        }
    }
    tally
}

enum Outcome {
    Pass,
    Pending(&'static str),
    Fail(String),
}

/// Resolve a command's module reference (either a top-level `filename` or a
/// nested `module.filename`) to its bytes.
fn module_file(command: &Value, dir: &Path) -> Option<(PathBuf, Vec<u8>)> {
    let filename = command.get("filename").and_then(Value::as_str)?;
    let path = dir.join(filename);
    fs::read(&path).ok().map(|bytes| (path, bytes))
}

// ---- conversion (wast2json-rs, wabt fallback) ----

const TOOL_HINT: &str = "\
Install wabt's wast2json (https://github.com/WebAssembly/wabt) and retry, or set
WAST2JSON to its path. The Rust wast2json-rs (cargo install wast2json-rs) is
also accepted but parses a smaller feature set.";

fn convert_wast(wast: &Path) -> Result<PathBuf, String> {
    let out_dir = Path::new("target").join("wastest");
    fs::create_dir_all(&out_dir)
        .map_err(|error| format!("cannot create {}: {error}", out_dir.display()))?;
    let json_path = out_dir.join(json_name(wast));
    for (tool, form) in converter_tools() {
        if let Some(result) = run_converter(&tool, form, wast, &json_path) {
            result?;
            return Ok(json_path);
        }
    }
    Err(format!("no wast2json converter available.\n{TOOL_HINT}"))
}

/// Whether a converter speaks wabt's CLI (feature flags, no `-c`) or the
/// Rust reimplementation's CLI (`-c` for compact JSON).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConverterForm {
    Wabt,
    Rust,
}

/// Converter candidates in order of preference: a wabt build named by
/// `WAST2JSON`, wabt's `wast2json` on PATH, the Rust `wast2json-rs` on PATH,
/// then this machine's wabt build as a last resort.
fn converter_tools() -> Vec<(String, ConverterForm)> {
    let mut tools = Vec::new();
    if let Ok(path) = std::env::var("WAST2JSON")
        && !path.is_empty()
    {
        tools.push((path, ConverterForm::Wabt));
    }
    tools.push((String::from("wast2json"), ConverterForm::Wabt));
    tools.push((String::from("wast2json-rs"), ConverterForm::Rust));
    let wabt_default = Path::new("C:\\Users\\T\\Desktop\\wabt\\build\\Release\\wast2json.exe");
    if wabt_default.exists() {
        tools.push((
            wabt_default.to_string_lossy().into_owned(),
            ConverterForm::Wabt,
        ));
    }
    tools
}

/// Try a converter; returns `None` when it is not runnable, `Some` with the
/// outcome otherwise.
fn run_converter(
    tool: &str,
    form: ConverterForm,
    wast: &Path,
    out_json: &Path,
) -> Option<Result<(), String>> {
    let Ok(probe) = Command::new(tool).arg("--version").output() else {
        return None;
    };
    println!(
        "using {tool} ({})",
        String::from_utf8_lossy(&probe.stdout).trim()
    );
    let mut command = Command::new(tool);
    match form {
        ConverterForm::Wabt => {
            // Enable the proposal features the corpus exercises but wabt
            // leaves off by default. Not `--enable-all`: compact-imports
            // rewrites the import section encoding of *every* module, and
            // the spec tests are written for the standard layout.
            for feature in ["--enable-function-references", "--enable-gc"] {
                command.arg(feature);
            }
            command.arg(wast).arg("-o").arg(out_json);
        }
        ConverterForm::Rust => {
            command.arg("-c").arg(wast).arg("-o").arg(out_json);
        }
    }
    match command.status() {
        Ok(status) if status.success() => Some(Ok(())),
        Ok(status) => Some(Err(format!("{tool} exited with {status}"))),
        Err(error) => Some(Err(format!("failed to run {tool}: {error}"))),
    }
}

fn json_name(wast: &Path) -> String {
    wast.file_name()
        .map(|name| format!("{}.json", name.to_string_lossy()))
        .unwrap_or_else(|| "out.json".to_string())
}

fn collect_wasm(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "wasm") {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_wasm(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "wasm") {
            files.push(path);
        }
    }
    files.sort();
    out.extend(files);
}

fn collect_json(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            files.push(path);
        }
    }
    files.sort();
    out.extend(files);
}
