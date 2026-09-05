//! In-process `.wast` -> wast2json-format converter.
//!
//! Replaces the external `wast2json` binary: the maintained `wast` crate
//! parses the corpus's `.wast` (modern GC/rec syntax included), each module is
//! encoded in-process, and the runner's wast2json-shaped JSON command stream
//! is written next to the per-module `.wasm`/`.wat` files.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};
use wast::core::{
    AbstractHeapType, HeapType, NanPattern, V128Const, V128Pattern, WastArgCore, WastRetCore,
};
use wast::parser::{self, ParseBuffer};
use wast::token::{F32, F64, Index};
use wast::{QuoteWat, QuoteWatTest, Wast, WastDirective, WastExecute, WastInvoke};

/// Convert one `.wast` into the wast2json JSON at `out_json` (whose directory
/// also receives the per-module `.wasm`/`.wat` files).
pub fn convert_wast(wast_path: &Path, out_json: &Path) -> Result<(), String> {
    let source =
        fs::read_to_string(wast_path).map_err(|e| format!("read {}: {e}", wast_path.display()))?;
    let buffer = ParseBuffer::new(&source).map_err(|e| e.to_string())?;
    let mut wast: Wast = parser::parse(&buffer).map_err(|e| e.to_string())?;

    let dir = out_json
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let prefix = wast_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());

    let mut commands: Vec<Value> = Vec::new();
    let mut module_index = 0usize;

    let mut directives: Vec<WastDirective<'_>> = std::mem::take(&mut wast.directives);
    directives.reverse();
    while let Some(mut directive) = directives.pop() {
        // wabt reports a directive at its innermost subject's line: the module
        // for module-bearing directives, the invoke/get for action asserts.
        let span = module_span(&directive);
        let line = (span.linecol_in(&source).0 + 1) as u64;
        match &mut directive {
            WastDirective::Module(quote) => {
                emit_module(
                    &dir,
                    &prefix,
                    &mut module_index,
                    quote,
                    &mut commands,
                    line,
                    "module",
                    None,
                    false,
                )?;
            }
            WastDirective::ModuleDefinition(quote) => {
                emit_module(
                    &dir,
                    &prefix,
                    &mut module_index,
                    quote,
                    &mut commands,
                    line,
                    "module",
                    None,
                    true,
                )?;
            }
            WastDirective::ModuleInstance { .. } => {
                return Err(format!(
                    "{}:{}: module-linking `instance` directives are not supported by the converter",
                    wast_path.display(),
                    line
                ));
            }
            WastDirective::AssertMalformed {
                module, message, ..
            }
            | WastDirective::AssertMalformedCustom {
                module, message, ..
            } => {
                emit_module(
                    &dir,
                    &prefix,
                    &mut module_index,
                    module,
                    &mut commands,
                    line,
                    "assert_malformed",
                    Some(*message),
                    false,
                )?;
            }
            WastDirective::AssertInvalid {
                module, message, ..
            }
            | WastDirective::AssertInvalidCustom {
                module, message, ..
            } => {
                emit_module(
                    &dir,
                    &prefix,
                    &mut module_index,
                    module,
                    &mut commands,
                    line,
                    "assert_invalid",
                    Some(*message),
                    false,
                )?;
            }
            WastDirective::AssertUnlinkable {
                module: wat,
                message,
                ..
            } => {
                let bytes = wat.encode().map_err(|e| e.to_string())?;
                let file = format!("{prefix}.{module_index}.wasm");
                fs::write(dir.join(&file), &bytes).map_err(|e| format!("write {file}: {e}"))?;
                let mut pairs: Vec<(&str, Value)> = vec![
                    ("type", str("assert_unlinkable")),
                    ("line", line.into()),
                    ("filename", str(&file)),
                    ("module_type", str("binary")),
                ];
                pairs.push(("text", str(message)));
                commands.push(object(&pairs));
                module_index += 1;
            }
            WastDirective::Register { name, module, .. } => {
                commands.push(register_command(line, name, module));
            }
            WastDirective::Invoke(invoke) => {
                commands.push(action_command(
                    "action",
                    line,
                    invoke_json(invoke)?,
                    None,
                    None,
                ));
            }
            WastDirective::AssertTrap { exec, message, .. } => {
                let message = *message;
                if let WastExecute::Wat(wat) = exec {
                    emit_module_wat(
                        &dir,
                        &prefix,
                        &mut module_index,
                        wat,
                        &mut commands,
                        line,
                        "assert_uninstantiable",
                        message,
                    )?;
                } else {
                    commands.push(action_command(
                        "assert_trap",
                        line,
                        execute_json(exec)?,
                        Some(message),
                        None,
                    ));
                }
            }
            WastDirective::AssertReturn { exec, results, .. } => {
                if let WastExecute::Wat(_) = exec {
                    // A bare module under `assert_return` instantiates
                    // (wabt marks it with `definition`); the corpus only
                    // uses `(module definition ...)` for that.
                    return Err(format!(
                        "{}:{line}: module under assert_return is not supported by the converter",
                        wast_path.display()
                    ));
                }
                let expected: Vec<Value> =
                    results.iter().map(ret_json).collect::<Result<_, _>>()?;
                commands.push(action_command(
                    "assert_return",
                    line,
                    execute_json(exec)?,
                    None,
                    Some(expected),
                ));
            }
            WastDirective::AssertExhaustion { call, message, .. } => commands.push(action_command(
                "assert_exhaustion",
                line,
                invoke_json(call)?,
                Some(*message),
                None,
            )),
            WastDirective::AssertException { exec, .. } => {
                if let WastExecute::Wat(_) = exec {
                    return Err(format!(
                        "{}:{line}: module under assert_exception is not supported by the converter",
                        wast_path.display()
                    ));
                }
                commands.push(action_command(
                    "assert_exception",
                    line,
                    execute_json(exec)?,
                    None,
                    Some(Vec::new()),
                ));
            }
            WastDirective::Thread(_)
            | WastDirective::Wait { .. }
            | WastDirective::AssertSuspension { .. } => {
                return Err(format!(
                    "{}:{line}: thread/suspension directives are not supported by the converter",
                    wast_path.display()
                ));
            }
        }
    }

    let root = object(&[("commands", Value::Array(commands))]);
    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("serialize {}: {e}", out_json.display()))?;
    fs::write(out_json, json).map_err(|e| format!("write {}: {e}", out_json.display()))
}

/// The span whose line a directive is reported at: the module for
/// module-bearing directives and the invoke/get for action directives
/// (matching wabt's line numbers, which point at the innermost subject, not
/// the directive keyword).
fn module_span(directive: &WastDirective<'_>) -> wast::token::Span {
    use WastDirective::*;
    match directive {
        Module(quote)
        | ModuleDefinition(quote)
        | AssertMalformed { module: quote, .. }
        | AssertMalformedCustom { module: quote, .. }
        | AssertInvalid { module: quote, .. }
        | AssertInvalidCustom { module: quote, .. } => quote.span(),
        AssertUnlinkable { module, .. } => module.span(),
        AssertTrap {
            exec: WastExecute::Wat(wat),
            ..
        } => wat.span(),
        Invoke(invoke) => invoke.span,
        AssertExhaustion { call, .. } => call.span,
        AssertTrap {
            exec: WastExecute::Invoke(invoke),
            ..
        }
        | AssertReturn {
            exec: WastExecute::Invoke(invoke),
            ..
        } => invoke.span,
        AssertTrap {
            exec: WastExecute::Get { span, .. },
            ..
        }
        | AssertReturn {
            exec: WastExecute::Get { span, .. },
            ..
        }
        | AssertException {
            exec: WastExecute::Get { span, .. },
            ..
        } => *span,
        other => other.span(),
    }
}

/// Encode a directive's inline `Wat` module (e.g. under `assert_trap`, where
/// the trap happens at instantiation) to its `.wasm` file and record the
/// command entry.
#[allow(clippy::too_many_arguments)]
fn emit_module_wat(
    dir: &Path,
    prefix: &str,
    module_index: &mut usize,
    module: &mut wast::Wat<'_>,
    commands: &mut Vec<Value>,
    line: u64,
    kind: &str,
    message: &str,
) -> Result<(), String> {
    let bytes = module.encode().map_err(|e| e.to_string())?;
    let file = format!("{prefix}.{module_index}.wasm");
    fs::write(dir.join(&file), &bytes).map_err(|e| format!("write {file}: {e}"))?;
    let mut pairs: Vec<(&str, Value)> = vec![
        ("type", str(kind)),
        ("line", line.into()),
        ("filename", str(&file)),
        ("module_type", str("binary")),
    ];
    pairs.push(("text", str(message)));
    commands.push(object(&pairs));
    *module_index += 1;
    Ok(())
}

/// Encode one module-bearing directive to its `.wasm`/`.wat` file and record
/// the command entry.
#[allow(clippy::too_many_arguments)]
fn emit_module(
    dir: &Path,
    prefix: &str,
    module_index: &mut usize,
    quote: &mut QuoteWat<'_>,
    commands: &mut Vec<Value>,
    line: u64,
    kind: &str,
    message: Option<&str>,
    definition: bool,
) -> Result<(), String> {
    let name = quote.name().map(|id| format!("${}", id.name()));
    // A `(module quote ...)` directive's text is a *valid* module in
    // module position (wabt parses it back and encodes it to run), while a
    // quote under an assert keeps its raw text for the harness to judge.
    let (module_type, file) = if kind == "module" {
        let bytes = quote.encode().map_err(|e| e.to_string())?;
        let file = format!("{prefix}.{module_index}.wasm");
        fs::write(dir.join(&file), &bytes).map_err(|e| format!("write {file}: {e}"))?;
        ("binary", file)
    } else {
        match quote.to_test().map_err(|e| e.to_string())? {
            QuoteWatTest::Binary(bytes) => {
                let file = format!("{prefix}.{module_index}.wasm");
                fs::write(dir.join(&file), &bytes).map_err(|e| format!("write {file}: {e}"))?;
                ("binary", file)
            }
            QuoteWatTest::Text(text) => {
                let file = format!("{prefix}.{module_index}.wat");
                fs::write(dir.join(&file), &text).map_err(|e| format!("write {file}: {e}"))?;
                ("text", file)
            }
        }
    };
    let mut pairs: Vec<(&str, Value)> = vec![
        ("type", str(kind)),
        ("line", line.into()),
        ("filename", str(&file)),
    ];
    // wabt tags assertion modules with their source form; plain `module`
    // commands are binary by definition and carry no `module_type`.
    if kind != "module" {
        pairs.push(("module_type", str(module_type)));
    }
    if let Some(name) = name {
        pairs.push(("name", str(&name)));
    }
    if definition {
        pairs.push(("definition", str("true")));
    }
    if let Some(message) = message {
        pairs.push(("text", str(message)));
    }
    commands.push(object(&pairs));
    *module_index += 1;
    Ok(())
}

fn register_command(line: u64, name: &str, module: &Option<wast::token::Id<'_>>) -> Value {
    let mut pairs: Vec<(&str, Value)> = vec![
        ("type", str("register")),
        ("line", line.into()),
        ("as", str(name)),
    ];
    if let Some(id) = module {
        pairs.push(("name", str(&format!("${}", id.name()))));
    }
    object(&pairs)
}

/// A command that runs an action. `expected` carries wast2json's type-only
/// result list for `assert_trap`/`assert_exhaustion`/bare `action` (the
/// runner judges those by `text`, not `expected`) and the full value list for
/// `assert_return`.
fn action_command(
    kind: &str,
    line: u64,
    action: Value,
    text: Option<&str>,
    expected: Option<Vec<Value>>,
) -> Value {
    let mut pairs: Vec<(&str, Value)> = vec![
        ("type", str(kind)),
        ("line", line.into()),
        ("action", action),
    ];
    if let Some(text) = text {
        pairs.push(("text", str(text)));
    }
    if let Some(expected) = expected {
        pairs.push(("expected", Value::Array(expected)));
    }
    object(&pairs)
}

fn execute_json(exec: &WastExecute<'_>) -> Result<Value, String> {
    match exec {
        WastExecute::Invoke(invoke) => invoke_json(invoke),
        WastExecute::Get { module, global, .. } => {
            let mut pairs: Vec<(&str, Value)> = vec![
                ("type", str("get")),
                ("field", str(global)),
                (
                    "module",
                    module
                        .as_ref()
                        .map(|id| str(&format!("${}", id.name())))
                        .unwrap_or(Value::Null),
                ),
            ];
            pairs.swap(1, 2);
            Ok(object(&pairs))
        }
        WastExecute::Wat(_) => Err("module execution is not an action".into()),
    }
}

fn invoke_json(invoke: &WastInvoke<'_>) -> Result<Value, String> {
    let args: Vec<Value> = invoke.args.iter().map(arg_json).collect::<Result<_, _>>()?;
    Ok(object(&[
        ("type", str("invoke")),
        (
            "module",
            invoke
                .module
                .as_ref()
                .map(|id| str(&format!("${}", id.name())))
                .unwrap_or(Value::Null),
        ),
        ("field", str(invoke.name)),
        ("args", Value::Array(args)),
    ]))
}

fn arg_json(arg: &wast::WastArg<'_>) -> Result<Value, String> {
    let wast::WastArg::Core(arg) = arg else {
        unreachable!("component-model arguments are not built")
    };
    Ok(match arg {
        WastArgCore::I32(v) => scalar("i32", (*v as u32).to_string()),
        WastArgCore::I64(v) => scalar("i64", (*v as u64).to_string()),
        WastArgCore::F32(F32 { bits }) => scalar("f32", bits.to_string()),
        WastArgCore::F64(F64 { bits }) => scalar("f64", bits.to_string()),
        WastArgCore::V128(constant) => v128_const_json(constant),
        WastArgCore::RefNull(heap) => scalar(&heap_ref_type(heap), "null".to_string()),
        WastArgCore::RefExtern(value) => scalar("externref", value.to_string()),
        WastArgCore::RefHost(value) => scalar("externref", value.to_string()),
    })
}

fn ret_json(ret: &wast::WastRet<'_>) -> Result<Value, String> {
    let wast::WastRet::Core(inner) = ret else {
        unreachable!("component-model results are not built")
    };
    ret_core_json(inner)
}

fn ret_core_json(inner: &WastRetCore<'_>) -> Result<Value, String> {
    Ok(match inner {
        WastRetCore::I32(v) => scalar("i32", (*v as u32).to_string()),
        WastRetCore::I64(v) => scalar("i64", (*v as u64).to_string()),
        WastRetCore::F32(pattern) => float_json("f32", *pattern),
        WastRetCore::F64(pattern) => float_json64("f64", *pattern),
        WastRetCore::V128(pattern) => v128_pattern_json(pattern),
        WastRetCore::RefNull(heap) => scalar(
            &heap
                .as_ref()
                .map(heap_ref_type)
                .unwrap_or_else(|| "funcref".to_string()),
            "null".to_string(),
        ),
        WastRetCore::RefExtern(Some(value)) => scalar("externref", value.to_string()),
        WastRetCore::RefExtern(None) => scalar("externref", "any".to_string()),
        WastRetCore::RefHost(value) => scalar("externref", value.to_string()),
        WastRetCore::RefFunc(index) => {
            let value = match index {
                Some(Index::Num(n, _)) => n.to_string(),
                _ => "0".to_string(),
            };
            scalar("funcref", value)
        }
        WastRetCore::RefAny => scalar("anyref", "0".to_string()),
        WastRetCore::RefEq => scalar("eqref", "0".to_string()),
        WastRetCore::RefArray => scalar("arrayref", "0".to_string()),
        WastRetCore::RefStruct => scalar("structref", "0".to_string()),
        WastRetCore::RefI31 => scalar("i31ref", "0".to_string()),
        WastRetCore::RefI31Shared => scalar("i31ref_shared", "0".to_string()),
        WastRetCore::Either(cases) => {
            let options: Vec<Value> = cases.iter().map(ret_core_json).collect::<Result<_, _>>()?;
            Value::Object(Map::from_iter([(
                "either".to_string(),
                Value::Array(options),
            )]))
        }
    })
}

fn v128_const_json(constant: &V128Const) -> Value {
    use V128Const::*;
    let (lane_type, lanes): (&str, Vec<String>) = match constant {
        I8x16(l) => ("i8", l.iter().map(|v| (*v as u8).to_string()).collect()),
        I16x8(l) => ("i16", l.iter().map(|v| (*v as u16).to_string()).collect()),
        I32x4(l) => ("i32", l.iter().map(|v| (*v as u32).to_string()).collect()),
        I64x2(l) => ("i64", l.iter().map(|v| (*v as u64).to_string()).collect()),
        F32x4(l) => (
            "f32",
            l.iter().map(|F32 { bits }| bits.to_string()).collect(),
        ),
        F64x2(l) => (
            "f64",
            l.iter().map(|F64 { bits }| bits.to_string()).collect(),
        ),
    };
    v128_json(lane_type, lanes)
}

fn v128_pattern_json(pattern: &V128Pattern) -> Value {
    use V128Pattern::*;
    let (lane_type, lanes) = match pattern {
        I8x16(l) => ("i8", l.iter().map(|v| (*v as u8).to_string()).collect()),
        I16x8(l) => ("i16", l.iter().map(|v| (*v as u16).to_string()).collect()),
        I32x4(l) => ("i32", l.iter().map(|v| (*v as u32).to_string()).collect()),
        I64x2(l) => ("i64", l.iter().map(|v| (*v as u64).to_string()).collect()),
        F32x4(l) => (
            "f32",
            l.iter()
                .map(|lane| match lane {
                    NanPattern::CanonicalNan => "nan:canonical".to_string(),
                    NanPattern::ArithmeticNan => "nan:arithmetic".to_string(),
                    NanPattern::Value(F32 { bits }) => bits.to_string(),
                })
                .collect(),
        ),
        F64x2(l) => (
            "f64",
            l.iter()
                .map(|lane| match lane {
                    NanPattern::CanonicalNan => "nan:canonical".to_string(),
                    NanPattern::ArithmeticNan => "nan:arithmetic".to_string(),
                    NanPattern::Value(F64 { bits }) => bits.to_string(),
                })
                .collect(),
        ),
    };
    v128_json(lane_type, lanes)
}

fn v128_json(lane_type: &str, lanes: Vec<String>) -> Value {
    Value::Object(Map::from_iter([
        ("type".to_string(), "v128".into()),
        ("lane_type".to_string(), lane_type.into()),
        (
            "value".to_string(),
            Value::Array(lanes.into_iter().map(Value::String).collect()),
        ),
    ]))
}

fn float_json(ty: &str, pattern: NanPattern<F32>) -> Value {
    let value = match pattern {
        NanPattern::CanonicalNan => "nan:canonical".to_string(),
        NanPattern::ArithmeticNan => "nan:arithmetic".to_string(),
        NanPattern::Value(F32 { bits }) => bits.to_string(),
    };
    scalar(ty, value)
}

fn float_json64(ty: &str, pattern: NanPattern<F64>) -> Value {
    let value = match pattern {
        NanPattern::CanonicalNan => "nan:canonical".to_string(),
        NanPattern::ArithmeticNan => "nan:arithmetic".to_string(),
        NanPattern::Value(F64 { bits }) => bits.to_string(),
    };
    scalar(ty, value)
}

/// The JSON reference-type name of a nullable reference to `heap` (the
/// `funcref`-style abbreviation; typed references keep their `$name`).
fn heap_ref_type(heap: &HeapType<'_>) -> String {
    match heap {
        HeapType::Abstract { shared: false, ty } => match ty {
            AbstractHeapType::Func => "funcref".to_string(),
            AbstractHeapType::Extern => "externref".to_string(),
            AbstractHeapType::Exn => "exnref".to_string(),
            AbstractHeapType::Any => "anyref".to_string(),
            AbstractHeapType::Eq => "eqref".to_string(),
            AbstractHeapType::Struct => "structref".to_string(),
            AbstractHeapType::Array => "arrayref".to_string(),
            AbstractHeapType::I31 => "i31ref".to_string(),
            AbstractHeapType::NoFunc => "funcref".to_string(),
            AbstractHeapType::NoExtern => "externref".to_string(),
            AbstractHeapType::None => "nullref".to_string(),
            AbstractHeapType::NoExn => "exnref".to_string(),
            AbstractHeapType::Cont => "contref".to_string(),
            _ => "anyref".to_string(),
        },
        HeapType::Abstract { shared: true, ty } => format!(
            "shared {}",
            heap_ref_type(&HeapType::Abstract {
                shared: false,
                ty: *ty
            })
        ),
        HeapType::Concrete(index) | HeapType::Exact(index) => match index {
            Index::Num(n, _) => format!("ref {n}"),
            Index::Id(id) => format!("ref ${}", id.name()),
        },
    }
}

fn scalar(ty: &str, value: String) -> Value {
    object(&[("type", str(ty)), ("value", str(&value))])
}

fn str(s: &str) -> Value {
    Value::String(s.to_string())
}

fn object(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}
