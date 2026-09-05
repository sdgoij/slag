//! The wasm conformance runner.
//!
//! Drives the pinned `waspec/test/core` corpus through the `crates/wasm`
//! engine. The corpus is `.wast` (text + assertions); the engine never
//! parses text, so `wast2json` (wabt; or the Rust wast2json-rs) turns each
//! file into one `.wasm` per module plus a `.json` of commands. See
//! `.notes/wasm-plan.md` for the cut plan.
//!
//! Command outcomes are classified by what the current engine can judge:
//! decoding/validation/`assert_invalid` are Cut 1-2; instantiating modules
//! and running `assert_return`/`assert_trap`/`assert_exhaustion` are Cut 3+.
//! Anything behind a later cut counts as *pending*, not pass or fail, so the
//! runner stays honest as cuts land.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;
use wasm::Value as WasmValue;
use wasm::types::{FuncType, GlobalType, Limits, MemType, RefType, TableType, ValType};
use wasm::valid::Error as ValidError;
use wasm::values::FuncAddr;
use wasm::values::RefValue;
use wasm::{
    DecodeError, ExecFail, ExternVal, InstantiateError, Module, Store, Trap, decode, validate,
};

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

    // Instance state across a file's commands: the store of live instances,
    // an import registry (spectest plus every `register`-named module), the
    // most recent module id, and module ids given a name by `register`.
    let mut store = Store::new();
    let mut registry = spectest_exports(&mut store);
    let mut last: Option<usize> = None;
    let mut named: HashMap<String, usize> = HashMap::new();
    // The latest module command's outcome: whether `last` is its instance, or
    // the reason it could not be created (so a following `register` names an
    // *unavailable* module instead of a stale one).
    let mut last_defined_ok = false;
    let mut module_pending: Option<&'static str> = None;
    // Module names whose instance could not be created (Cut N): imports from
    // them are pending, not unknown-import failures.
    let mut unavailable: HashMap<String, &'static str> = HashMap::new();

    for command in commands {
        let line = command.get("line").and_then(Value::as_u64).unwrap_or(0);
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let outcome = match kind {
            "module" => {
                let module = command.get("module").unwrap_or(command);
                let definition_only = command.get("definition").and_then(Value::as_str);
                match module_source(module, dir) {
                    ModuleSource::Binary(bytes) => {
                        if definition_only == Some("true") {
                            // `(module definition ...)`: decode and validate
                            // only — never instantiate (its tables/memories
                            // may be huge).
                            match decode(&bytes) {
                                Err(error) => Outcome::Fail(format!("module decode: {error}")),
                                Ok(module) => match validate(&module) {
                                    Err(error) => Outcome::Fail(format!("module invalid: {error}")),
                                    Ok(_) => Outcome::Pass,
                                },
                            }
                        } else {
                            match judge_module(
                                &mut store,
                                &registry,
                                &unavailable,
                                &bytes,
                                ModuleExpect::Instantiate,
                            ) {
                                Verdict::Id(id) => {
                                    last = Some(id);
                                    last_defined_ok = true;
                                    module_pending = None;
                                    if let Some(label) = command.get("name").and_then(Value::as_str)
                                    {
                                        named.insert(label.to_string(), id);
                                    }
                                    Outcome::Pass
                                }
                                Verdict::Pass => {
                                    last_defined_ok = true;
                                    module_pending = None;
                                    Outcome::Pass
                                }
                                Verdict::Pending(reason) => {
                                    last_defined_ok = false;
                                    module_pending = Some(reason);
                                    Outcome::Pending(reason)
                                }
                                Verdict::Fail(message) => {
                                    last_defined_ok = false;
                                    module_pending = None;
                                    Outcome::Fail(message)
                                }
                            }
                        }
                    }
                    ModuleSource::Text => {
                        last_defined_ok = false;
                        module_pending = None;
                        Outcome::Pending("quote text module")
                    }
                    ModuleSource::Missing => {
                        last_defined_ok = false;
                        module_pending = None;
                        Outcome::Pending("module without a file")
                    }
                }
            }
            "assert_malformed" => {
                let module = command.get("module").unwrap_or(command);
                match module_source(module, dir) {
                    ModuleSource::Binary(bytes) => match decode(&bytes) {
                        Err(DecodeError::Malformed(_)) => Outcome::Pass,
                        Err(DecodeError::Unsupported(_)) => Outcome::Fail(
                            "expected malformed, decoder hit unsupported feature".into(),
                        ),
                        Ok(_) => Outcome::Fail("expected malformed, module decoded".into()),
                    },
                    ModuleSource::Text => Outcome::Pending("quote text module"),
                    ModuleSource::Missing => {
                        Outcome::Pending("assert_malformed without a module file")
                    }
                }
            }
            "assert_invalid" => {
                match module_source(command.get("module").unwrap_or(command), dir) {
                    ModuleSource::Binary(bytes) => match decode(&bytes) {
                        Err(DecodeError::Malformed(message)) => Outcome::Fail(format!(
                            "expected invalid, decoder says malformed: {message}"
                        )),
                        Err(DecodeError::Unsupported(_)) => Outcome::Fail(
                            "expected invalid, decoder hit unsupported feature".into(),
                        ),
                        Ok(module) => match validate(&module) {
                            Err(ValidError::Invalid(_)) => Outcome::Pass,
                            Ok(_) => Outcome::Fail("expected invalid, module validated".into()),
                        },
                    },
                    ModuleSource::Text => Outcome::Pending("quote text module"),
                    ModuleSource::Missing => {
                        Outcome::Pending("assert_invalid without a module file")
                    }
                }
            }
            "register" => {
                // The most recent module becomes addressable by name; its
                // exports become importable under that name.
                let name = command.get("as").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    Outcome::Fail("register without a name".into())
                } else if last_defined_ok {
                    if let Some(id) = last {
                        named.insert(name.to_string(), id);
                        if let Some(label) = command.get("name").and_then(Value::as_str) {
                            named.insert(label.to_string(), id);
                        }
                        for (field, value) in store.exports(id) {
                            registry.insert((name.to_string(), field), value);
                        }
                    }
                    Outcome::Pass
                } else if let Some(reason) = module_pending {
                    unavailable.insert(name.to_string(), reason);
                    Outcome::Pending("module unavailable")
                } else {
                    Outcome::Pending("no module to register")
                }
            }
            "assert_unlinkable" => {
                match module_source(command.get("module").unwrap_or(command), dir) {
                    ModuleSource::Binary(bytes) => judge_module(
                        &mut store,
                        &registry,
                        &unavailable,
                        &bytes,
                        ModuleExpect::Unlinkable,
                    )
                    .into_outcome(),
                    ModuleSource::Text => Outcome::Pending("quote text module"),
                    ModuleSource::Missing => {
                        Outcome::Pending("assert_unlinkable without a module file")
                    }
                }
            }
            "assert_uninstantiable" => {
                match module_source(command.get("module").unwrap_or(command), dir) {
                    ModuleSource::Binary(bytes) => judge_module(
                        &mut store,
                        &registry,
                        &unavailable,
                        &bytes,
                        ModuleExpect::Uninstantiable,
                    )
                    .into_outcome(),
                    ModuleSource::Text => Outcome::Pending("quote text module"),
                    ModuleSource::Missing => {
                        Outcome::Pending("assert_uninstantiable without a module file")
                    }
                }
            }
            "action" | "assert_return" | "assert_trap" | "assert_exhaustion"
            | "assert_exception" => {
                if kind == "assert_trap" && command.get("action").is_none() {
                    // `(assert_trap (module ...))`: instantiation must trap.
                    let text = command.get("text").and_then(Value::as_str).unwrap_or("");
                    match module_source(command.get("module").unwrap_or(command), dir) {
                        ModuleSource::Binary(bytes) => judge_module(
                            &mut store,
                            &registry,
                            &unavailable,
                            &bytes,
                            ModuleExpect::InstantiationTrap(text),
                        )
                        .into_outcome(),
                        ModuleSource::Text => Outcome::Pending("quote text module"),
                        ModuleSource::Missing => {
                            Outcome::Pending("assert_trap module without a file")
                        }
                    }
                } else {
                    let action = command.get("action").unwrap_or(command);
                    let instance = if let Some(name) = action.get("module").and_then(Value::as_str)
                    {
                        named.get(name).copied()
                    } else {
                        last
                    };
                    match instance {
                        None => Outcome::Pending("no module to act on"),
                        Some(instance) => {
                            let result = run_action(&mut store, instance, action);
                            match kind {
                                "action" => match result {
                                    Ok(_) => Outcome::Pass,
                                    Err(ActOutcome::Trap(_)) => {
                                        Outcome::Fail("action trapped".into())
                                    }
                                    Err(ActOutcome::Unsupported(reason)) => {
                                        Outcome::Pending(reason)
                                    }
                                    Err(ActOutcome::Exception) => {
                                        Outcome::Fail("action threw an exception".into())
                                    }
                                },
                                "assert_return" => match result {
                                    Ok(values) => match command
                                        .get("expected")
                                        .and_then(Value::as_array)
                                    {
                                        Some(expected) => {
                                            if expected.len() != values.len()
                                                || expected.iter().zip(&values).any(|(e, a)| {
                                                    !matches_expected(e, *a, instance)
                                                })
                                            {
                                                Outcome::Fail("results differ from expected".into())
                                            } else {
                                                Outcome::Pass
                                            }
                                        }
                                        None => {
                                            Outcome::Fail("assert_return without expected".into())
                                        }
                                    },
                                    Err(ActOutcome::Trap(_)) => {
                                        Outcome::Fail("expected return, trapped".into())
                                    }
                                    Err(ActOutcome::Unsupported(reason)) => {
                                        Outcome::Pending(reason)
                                    }
                                    Err(ActOutcome::Exception) => {
                                        Outcome::Fail("expected return, exception".into())
                                    }
                                },
                                "assert_trap" => match result {
                                    Err(ActOutcome::Trap(trap)) => {
                                        let expected = command
                                            .get("text")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if trap_text_matches(trap, expected) {
                                            Outcome::Pass
                                        } else {
                                            Outcome::Fail(format!(
                                                "trapped with {:?}, expected {expected:?}",
                                                trap
                                            ))
                                        }
                                    }
                                    Ok(_) => Outcome::Fail("expected trap, returned".into()),
                                    Err(ActOutcome::Unsupported(reason)) => {
                                        Outcome::Pending(reason)
                                    }
                                    Err(ActOutcome::Exception) => {
                                        Outcome::Fail("expected trap, exception".into())
                                    }
                                },
                                "assert_exhaustion" => match result {
                                    Err(ActOutcome::Trap(Trap::CallStackExhausted)) => {
                                        Outcome::Pass
                                    }
                                    Err(ActOutcome::Trap(_)) => {
                                        Outcome::Fail("expected exhaustion, other trap".into())
                                    }
                                    Ok(_) => Outcome::Fail("expected exhaustion, returned".into()),
                                    Err(ActOutcome::Unsupported(reason)) => {
                                        Outcome::Pending(reason)
                                    }
                                    Err(ActOutcome::Exception) => {
                                        Outcome::Fail("expected exhaustion, exception".into())
                                    }
                                },
                                "assert_exception" => match result {
                                    Err(ActOutcome::Exception) => Outcome::Pass,
                                    Err(ActOutcome::Trap(_)) => {
                                        Outcome::Fail("expected exception, trapped".into())
                                    }
                                    Err(ActOutcome::Unsupported(reason)) => {
                                        Outcome::Pending(reason)
                                    }
                                    Ok(_) => Outcome::Fail("expected exception, returned".into()),
                                },
                                _ => Outcome::Pending("execution (Cut 3)"),
                            }
                        }
                    }
                }
            }
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

enum ActOutcome {
    Trap(Trap),
    /// An uncaught wasm exception (`assert_exception`).
    Exception,
    Unsupported(&'static str),
}

impl From<ExecFail> for ActOutcome {
    fn from(fail: ExecFail) -> Self {
        match fail {
            ExecFail::Trap(trap) => ActOutcome::Trap(trap),
            ExecFail::Exception(_) => ActOutcome::Exception,
            ExecFail::Unsupported(reason) => ActOutcome::Unsupported(reason),
        }
    }
}

/// What a module-bearing command expects from instantiation.
enum ModuleExpect<'a> {
    /// A plain `module` command: instantiation must succeed.
    Instantiate,
    /// `assert_unlinkable`: instantiation must fail at import resolution.
    Unlinkable,
    /// `assert_uninstantiable`: instantiation must trap.
    Uninstantiable,
    /// `(assert_trap (module ...))`: instantiation must trap with `text`.
    InstantiationTrap(&'a str),
}

/// A judged module command.
enum Verdict {
    Id(usize),
    Pass,
    Pending(&'static str),
    Fail(String),
}

impl Verdict {
    fn into_outcome(self) -> Outcome {
        match self {
            Verdict::Id(_) | Verdict::Pass => Outcome::Pass,
            Verdict::Pending(reason) => Outcome::Pending(reason),
            Verdict::Fail(message) => Outcome::Fail(message),
        }
    }
}

/// Decode, validate, and instantiate a module through the registry, judging
/// the outcome against what the command expects.
fn judge_module(
    store: &mut Store,
    registry: &HashMap<(String, String), ExternVal>,
    unavailable: &HashMap<String, &'static str>,
    bytes: &[u8],
    expect: ModuleExpect<'_>,
) -> Verdict {
    let module: Module = match decode(bytes) {
        Err(error) => return Verdict::Fail(format!("module decode: {error}")),
        Ok(module) => module,
    };
    if let Err(error) = validate(&module) {
        return Verdict::Fail(format!("module invalid: {error}"));
    }
    let result = {
        let mut resolve = |m: &str, n: &str| {
            registry
                .get(&(m.to_string(), n.to_string()))
                .copied()
                .or_else(|| {
                    unavailable
                        .get(m)
                        .map(|&reason| ExternVal::Unsupported(reason))
                })
        };
        store.instantiate(&module, &mut resolve)
    };
    match expect {
        ModuleExpect::Instantiate => match result {
            Ok(id) => Verdict::Id(id),
            Err(InstantiateError::Unsupported(reason)) => Verdict::Pending(reason),
            Err(error) => Verdict::Fail(format!("module did not instantiate: {error}")),
        },
        ModuleExpect::Unlinkable => match result {
            Err(InstantiateError::Unlinkable(_)) => Verdict::Pass,
            Err(InstantiateError::Unsupported(reason)) => Verdict::Pending(reason),
            Err(error) => Verdict::Fail(format!("expected unlinkable, got {error}")),
            Ok(_) => Verdict::Fail("expected unlinkable, module instantiated".into()),
        },
        ModuleExpect::Uninstantiable => match result {
            Err(InstantiateError::Trap(_)) => Verdict::Pass,
            Err(InstantiateError::Unsupported(reason)) => Verdict::Pending(reason),
            Err(error) => Verdict::Fail(format!("expected trap, got {error}")),
            Ok(_) => Verdict::Fail("expected trap, module instantiated".into()),
        },
        ModuleExpect::InstantiationTrap(text) => match result {
            Err(InstantiateError::Trap(trap)) if trap_text_matches(trap, text) => Verdict::Pass,
            Err(InstantiateError::Trap(trap)) => {
                Verdict::Fail(format!("trapped with {trap:?}, expected {text:?}"))
            }
            Err(InstantiateError::Unsupported(reason)) => Verdict::Pending(reason),
            Err(error) => Verdict::Fail(format!("expected trap, got {error}")),
            Ok(_) => Verdict::Fail("expected trap, module instantiated".into()),
        },
    }
}

/// Run an `invoke` or `get` action against an instance.
fn run_action(
    store: &mut Store,
    instance: usize,
    action: &Value,
) -> Result<Vec<WasmValue>, ActOutcome> {
    let kind = action.get("type").and_then(Value::as_str).unwrap_or("");
    let field = action.get("field").and_then(Value::as_str).unwrap_or("");
    match kind {
        "invoke" => {
            let index = store
                .exported_func(instance, field)
                .ok_or(ActOutcome::Unsupported("unknown function export"))?;
            let mut args = Vec::new();
            if let Some(arg_values) = action.get("args").and_then(Value::as_array) {
                for arg in arg_values {
                    args.push(
                        parse_const(arg).ok_or(ActOutcome::Unsupported("unparseable argument"))?,
                    );
                }
            }
            store
                .invoke(instance, index, &args)
                .map_err(ActOutcome::from)
        }
        "get" => {
            let value = store
                .export_global_value(instance, field)
                .ok_or(ActOutcome::Unsupported("unknown global export"))?;
            Ok(vec![value])
        }
        _ => Err(ActOutcome::Unsupported("non-invoke action")),
    }
}

/// Parse a JSON const (or expected) entry `{"type": ..., "value": ...}` into
/// a runtime value. NaN patterns are only legal in expectations.
fn parse_const(entry: &serde_json::Value) -> Option<WasmValue> {
    let ty = entry.get("type").and_then(Value::as_str)?;
    let value = entry.get("value").and_then(Value::as_str)?;
    if value.starts_with("nan:") {
        return None;
    }
    match ty {
        "funcref" => {
            if value == "null" {
                Some(WasmValue::Ref(RefValue::Null))
            } else {
                None
            }
        }
        "externref" => {
            let reference = if value == "null" {
                RefValue::Null
            } else {
                RefValue::Extern(value.parse::<u64>().ok()? as u32)
            };
            Some(WasmValue::Ref(reference))
        }
        _ => {
            let bits = value.parse::<u64>().ok()?;
            Some(match ty {
                "i32" => WasmValue::I32(bits as i32),
                "i64" => WasmValue::I64(bits as i64),
                "f32" => WasmValue::F32(bits as u32),
                "f64" => WasmValue::F64(bits),
                _ => return None,
            })
        }
    }
}

/// Whether an actual result value satisfies an expected entry (which may be
/// a NaN pattern).
fn matches_expected(expected: &serde_json::Value, actual: WasmValue, module: usize) -> bool {
    let ty = expected.get("type").and_then(Value::as_str);
    let value = expected.get("value").and_then(Value::as_str);
    let (ty, value) = match (ty, value) {
        (Some(ty), Some(value)) => (ty, value),
        _ => return false,
    };
    if ty == "funcref" {
        // wast2json encodes the `(ref.func)` non-null pattern as value "0":
        // any non-null function reference.
        if value == "0" {
            return matches!(actual, WasmValue::Ref(RefValue::Func(_)));
        }
        if let Ok(index) = value.parse::<u64>() {
            let expected = FuncAddr {
                instance: module,
                index: index as usize,
            };
            return actual == WasmValue::Ref(RefValue::Func(expected));
        }
        return value == "null" && matches!(actual, WasmValue::Ref(RefValue::Null));
    }
    if ty == "externref" {
        return parse_const(expected) == Some(actual)
            || (value == "null" && matches!(actual, WasmValue::Ref(RefValue::Null)));
    }
    let bits = match actual {
        WasmValue::I32(v) => v as u32 as u64,
        WasmValue::I64(v) => v as u64,
        WasmValue::F32(v) => v as u64,
        WasmValue::F64(v) => v,
        WasmValue::Ref(_) => return false,
    };
    match value {
        "nan:canonical" => match ty {
            "f32" => bits & 0x7fff_ffff == 0x7fc0_0000,
            "f64" => bits & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
            _ => false,
        },
        "nan:arithmetic" => match ty {
            "f32" => bits & 0x7f80_0000 == 0x7f80_0000 && bits & 0x007f_ffff >= 0x0040_0000,
            "f64" => {
                bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000
                    && bits & 0x000f_ffff_ffff_ffff >= 0x0008_0000_0000_0000
            }
            _ => false,
        },
        _ => parse_const(expected) == Some(actual),
    }
}

/// The spec's trap message for each trap kind, for `assert_trap` text checks.
fn trap_text(trap: Trap) -> &'static str {
    match trap {
        Trap::Unreachable => "unreachable",
        Trap::IntegerDivideByZero => "integer divide by zero",
        Trap::IntegerOverflow => "integer overflow",
        Trap::InvalidConversionToInteger => "invalid conversion to integer",
        Trap::OutOfBoundsMemoryAccess => "out of bounds memory access",
        Trap::OutOfBoundsTableAccess => "out of bounds table access",
        Trap::IndirectCallTypeMismatch => "indirect call type mismatch",
        Trap::UndefinedElement => "undefined element",
        Trap::CallStackExhausted => "call stack exhausted",
        Trap::UninitializedElement => "uninitialized element",
        Trap::NullReference => "null reference",
        Trap::NullFunctionReference => "null function reference",
        Trap::NullExceptionReference => "null exception reference",
        Trap::UnsupportedImport => "unsupported import",
        Trap::UnknownFunction => "unknown function",
    }
}

/// Compare a trap against an expected `assert_trap` text. The spec reports
/// `uninitialized element <index>` (and `undefined element <index>`) with the
/// offending table index; the engine's trap only records the kind, so accept
/// the suffixed form for those kinds. `bulk.wast` expects
/// `"uninitialized element 2"`, the single corpus occurrence of a suffix.
fn trap_text_matches(trap: Trap, expected: &str) -> bool {
    if trap_text(trap) == expected {
        return true;
    }
    let index_suffix = match trap {
        Trap::UninitializedElement => "uninitialized element ",
        Trap::UndefinedElement => "undefined element ",
        _ => return false,
    };
    expected
        .strip_prefix(index_suffix)
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// The spec's predefined `spectest` host module: no-op `print*` functions,
/// `global_i32/i64` = 666 and `global_f32/f64` = 666.6, a 1..2-page memory,
/// and a 10..20-slot funcref table. Returns the (module, field) -> value map
/// used by import resolution.
fn spectest_exports(store: &mut Store) -> HashMap<(String, String), ExternVal> {
    let mut exports = HashMap::new();
    let mut host = |store: &mut Store, name: &str, params: Vec<ValType>| {
        let ty = FuncType {
            params,
            results: Vec::new(),
        };
        let id = store.host_func(ty);
        exports.insert(
            ("spectest".to_string(), name.to_string()),
            ExternVal::HostFunc(id),
        );
    };
    host(store, "print", vec![]);
    host(store, "print_i32", vec![ValType::I32]);
    host(store, "print_i64", vec![ValType::I64]);
    host(store, "print_f32", vec![ValType::F32]);
    host(store, "print_f64", vec![ValType::F64]);
    host(store, "print_i32_f32", vec![ValType::I32, ValType::F32]);
    host(store, "print_f64_f64", vec![ValType::F64, ValType::F64]);

    for (name, ty, value) in [
        ("global_i32", ValType::I32, WasmValue::I32(666)),
        ("global_i64", ValType::I64, WasmValue::I64(666)),
        (
            "global_f32",
            ValType::F32,
            WasmValue::F32(666.6f32.to_bits()),
        ),
        (
            "global_f64",
            ValType::F64,
            WasmValue::F64(666.6f64.to_bits()),
        ),
    ] {
        let cell = store.global(
            GlobalType {
                value: ty,
                mutable: false,
            },
            value,
        );
        exports.insert(
            ("spectest".to_string(), name.to_string()),
            ExternVal::Global(cell),
        );
    }

    if let Ok(cell) = store.memory(MemType {
        limits: Limits::new(1, Some(2)),
        memory64: false,
    }) {
        exports.insert(
            ("spectest".to_string(), "memory".to_string()),
            ExternVal::Memory(cell),
        );
    }
    if let Ok(cell) = store.table(
        TableType {
            element: RefType::FUNC,
            limits: Limits::new(10, Some(20)),
        },
        RefValue::Null,
    ) {
        exports.insert(
            ("spectest".to_string(), "table".to_string()),
            ExternVal::Table(cell),
        );
    }
    exports
}

/// A module command's file, classified by what the runner can do with it:
/// binary `.wasm` (decodable) versus quote `.wat` text, which needs a text
/// parser the engine does not have (reported pending at the call sites).
enum ModuleSource {
    Binary(Vec<u8>),
    Text,
    Missing,
}

fn module_source(command: &Value, dir: &Path) -> ModuleSource {
    match module_file(command, dir) {
        Some((path, _bytes)) if path.extension().is_some_and(|ext| ext == "wat") => {
            ModuleSource::Text
        }
        Some((_, bytes)) => ModuleSource::Binary(bytes),
        None => ModuleSource::Missing,
    }
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
