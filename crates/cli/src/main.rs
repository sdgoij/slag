//! `slag` command-line runner and REPL (PLAN Phase 18 CLI polish).
//!
//! Usage: `slag [options] [file.js [args...]]`
//! - With a file: parse and evaluate it, exposing `process.argv` to the
//!   script; extra arguments are script arguments.
//! - Without a file: start the REPL (multi-line input; `.exit` or Ctrl-D
//!   quits).
//!
//! Flags: `--version`/`-V`, `--help`/`-h`, `--dump-ast`, `--dump-tokens`,
//! `--bench` (run the micro-benchmark suite), `--jit-bench` (run the
//! JIT-vs-interpreter comparison suite), `--print-bytecode` (print the
//! compiled step stream), `--jit` (install the Cranelift JIT hook on every
//! context; on by default for script runs and the REPL, `--no-jit` disables
//! it, and `--bench` always measures the interpreter), and the
//! accepted-no-op knobs `--stack-size N`,
//! `--max-old-space N`, `--harmony-*`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use runtime::embed::Context;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// CLI knobs. The performance flags are accepted for CLI compatibility; the
/// GC knobs (`--gc-stress` is live since GC-1 slice 3; `--max-old-space` is
/// still a no-op) are documented in `docs/gc-plan.md`.
#[derive(Debug, Default, Clone, PartialEq)]
struct Options {
    dump_ast: bool,
    dump_tokens: bool,
    bench: bool,
    jit_bench: bool,
    print_bytecode: bool,
    gc_stress: bool,
    jit: bool,
    no_jit: bool,
    stack_size: Option<u64>,
    max_old_space: Option<u64>,
}

#[derive(Debug, PartialEq)]
enum Command {
    Version,
    Help,
    Run {
        file: String,
        args: Vec<String>,
        options: Options,
    },
    Repl(Options),
}

fn parse(args: &[String]) -> Command {
    let mut options = Options::default();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--version" | "-V" => return Command::Version,
            "--help" | "-h" => return Command::Help,
            "--dump-ast" => options.dump_ast = true,
            "--dump-tokens" => options.dump_tokens = true,
            "--bench" => options.bench = true,
            "--jit-bench" => options.jit_bench = true,
            "--print-bytecode" => options.print_bytecode = true,
            "--jit" => options.jit = true,
            "--no-jit" => options.no_jit = true,
            "--stack-size" => {
                index += 1;
                options.stack_size = args.get(index).and_then(|value| value.parse().ok());
            }
            "--max-old-space" => {
                index += 1;
                options.max_old_space = args.get(index).and_then(|value| value.parse().ok());
            }
            "--gc-stress" => options.gc_stress = true,
            flag if flag.starts_with("--harmony") => {}
            flag if flag.starts_with('-') && flag != "-" => {
                eprintln!("slag: unknown flag {flag}");
                return Command::Help;
            }
            _ => positional.push(arg.clone()),
        }
        index += 1;
    }
    let mut positional = positional.into_iter();
    match positional.next() {
        Some(file) => Command::Run {
            file,
            args: positional.collect(),
            options,
        },
        None => Command::Repl(options),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(parse(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

fn run(command: Command) -> Result<(), u8> {
    match command {
        Command::Version => {
            println!("slag {VERSION}");
            Ok(())
        }
        Command::Help => {
            eprintln!("usage: slag [options] [file.js [args...]]");
            eprintln!("options:");
            eprintln!("  --version, -V");
            eprintln!("  --help, -h");
            eprintln!("  --dump-ast, --dump-tokens");
            eprintln!("  --bench");
            eprintln!("  --jit-bench               JIT vs interpreter comparison");
            eprintln!("  --jit                     install the Cranelift JIT hook (default)");
            eprintln!("  --no-jit                  run without the JIT hook");
            eprintln!("  --print-bytecode");
            eprintln!("  --stack-size N            (no-op)");
            eprintln!("  --max-old-space N         (no-op)");
            eprintln!("  --gc-stress               collect at every safe point");
            eprintln!("  --harmony-*               (no-op)");
            Err(2)
        }
        Command::Run {
            file,
            args,
            options,
        } => run_file(&file, &args, &options),
        Command::Repl(options) => {
            if options.jit_bench {
                return run_jit_benchmarks();
            }
            if options.bench {
                let mut context = Context::new().map_err(report)?;
                context.set_gc_stress(options.gc_stress);
                if options.jit {
                    jit::install(context.agent_mut()).map_err(report)?;
                }
                return run_benchmarks(&mut context);
            }
            repl(&options)
        }
    }
}

fn report(error: impl std::fmt::Display) -> u8 {
    eprintln!("slag: {error}");
    1
}

/// Parse and evaluate a script file (spec 16.1: ParseScript +
/// ScriptEvaluation in a fresh realm), exposing `process.argv`.
fn run_file(file: &str, args: &[String], options: &Options) -> Result<(), u8> {
    let source = std::fs::read_to_string(file).map_err(|e| {
        eprintln!("slag: {file}: {e}");
        1
    })?;
    run_file_inner(file, args, options, &source)
}

fn run_file_inner(file: &str, args: &[String], options: &Options, source: &str) -> Result<(), u8> {
    if options.dump_tokens {
        dump_tokens(source)?;
    }
    if options.dump_ast {
        dump_ast(source)?;
    }
    if options.print_bytecode {
        dump_bytecode(source)?;
    }
    let mut context = Context::new().map_err(report)?;
    context.set_gc_stress(options.gc_stress);
    if options.jit || !options.no_jit {
        jit::install(context.agent_mut()).map_err(report)?;
    }
    context.install_fs().map_err(report)?;
    if !args.is_empty() {
        let mut argv = vec![
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "slag".to_string()),
            file.to_string(),
        ];
        argv.extend(args.iter().cloned());
        context.install_process_argv(&argv).map_err(report)?;
    }
    match context.eval(source) {
        // A script file's completion value is not printed (matching node et
        // al.); the REPL prints its own results.
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("slag: {error}");
            Err(1)
        }
    }
}

/// Lex `source` and print every token.
fn dump_tokens(source: &str) -> Result<(), u8> {
    let text = syntax::SourceText::from_utf8(source);
    let mut lexer = lexer::Lexer::new(&text, syntax::LexGoal::Div, true);
    loop {
        match lexer.next_token() {
            Ok(token) => {
                println!("{token:?}");
                if token.kind == syntax::TokenKind::Eof {
                    break;
                }
            }
            Err(error) => {
                eprintln!("slag: tokenize: {error}");
                return Err(1);
            }
        }
    }
    Ok(())
}

/// Parse `source` and print the AST.
fn dump_ast(source: &str) -> Result<(), u8> {
    match parser::parse_script(source) {
        Ok(program) => {
            println!("{program:#?}");
            Ok(())
        }
        Err(error) => {
            eprintln!("slag: parse: {error}");
            Err(1)
        }
    }
}

/// Parse `source` as a fast script, compile it, and print the bytecode
/// (`--print-bytecode`; the `docs/bytecode-plan.md` Cut 5 debugging tool).
fn dump_bytecode(source: &str) -> Result<(), u8> {
    match parser::parse_script(source) {
        Ok(program) => {
            let strict =
                runtime::script::script_is_strict(&crux::JsString::from_utf8(source), &program);
            match runtime::ir::compile_statements(&program.body, strict, true) {
                Ok(body) => {
                    runtime::ir::debug_print_body(&body);
                    Ok(())
                }
                Err(error) => {
                    eprintln!("slag: compile: {error}");
                    Err(1)
                }
            }
        }
        Err(error) => {
            eprintln!("slag: parse: {error}");
            Err(1)
        }
    }
}

/// The REPL: read lines, continue while the input is syntactically
/// incomplete, evaluate complete inputs, and print the completion value.
fn repl(options: &Options) -> Result<(), u8> {
    let mut context = Context::new().map_err(report)?;
    context.set_gc_stress(options.gc_stress);
    if options.jit || !options.no_jit {
        jit::install(context.agent_mut()).map_err(report)?;
    }
    println!("slag {VERSION} REPL (type .exit or Ctrl-D to quit)");
    let stdin = io::stdin();
    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "> " } else { "... " };
        print!("{prompt}");
        io::stdout().flush().map_err(|e| {
            eprintln!("slag: {e}");
            1
        })?;
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("slag: {error}");
                return Err(1);
            }
        }
        if buffer.is_empty() && line.trim() == ".exit" {
            break;
        }
        buffer.push_str(&line);
        if !input_complete(&buffer) {
            continue;
        }
        let source = std::mem::take(&mut buffer);
        match context.eval(&source) {
            Ok(value) => {
                if value.type_name() != "undefined" {
                    println!("{value}");
                }
            }
            Err(error) => eprintln!("{error}"),
        }
    }
    Ok(())
}

/// Whether `source` looks like a complete program: no unclosed strings,
/// comments, templates, or bracket groups. Drives the REPL's `...` prompt.
fn input_complete(source: &str) -> bool {
    let chars: Vec<char> = source.chars().collect();
    let mut depth = 0i32;
    let mut in_string = None;
    let mut in_template = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if line_comment {
            if c == '\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if c == '*' && chars.get(index + 1) == Some(&'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(quote) = in_string {
            if c == '\\' {
                index += 2;
            } else {
                if c == quote {
                    in_string = None;
                }
                index += 1;
            }
            continue;
        }
        match c {
            '/' if chars.get(index + 1) == Some(&'/') => {
                line_comment = true;
                index += 2;
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                block_comment = true;
                index += 2;
            }
            '\'' | '"' => {
                in_string = Some(c);
                index += 1;
            }
            '`' => {
                in_template = !in_template;
                index += 1;
            }
            '(' | '[' | '{' => {
                depth += 1;
                index += 1;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    depth <= 0 && in_string.is_none() && !in_template && !block_comment
}

/// Run the micro-benchmark suite: each snippet is evaluated once to warm up
/// (interning, hooks), then timed. The sources use `var` (not `let`) so a
/// second evaluation in the same realm is legal and the timed run measures
/// the real loop rather than a re-declaration error. The timings are only
/// comparable within a build profile; see `docs/perf.md` for the benchmark
/// gates.
///
/// The last three rows exercise the Cut 3 continuation certification
/// shapes the first five predate: a closure reading an enclosing body's
/// captured binding (the context-chain slices), closures over a `for`
/// `let` head (the per-iteration machinery), and a constructor reading
/// `this` (the this slots + construct fast path). The `for (let i ...)`
/// head lives inside a function so re-evaluation stays legal.
fn run_benchmarks(context: &mut Context) -> Result<(), u8> {
    let benchmarks: &[(&str, &str)] = &[
        (
            "arithmetic",
            "var n = 0; for (var i = 0; i < 1_000_000; i++) { n += i * 2; } n",
        ),
        (
            "bare loop",
            "var n = 0; for (var i = 0; i < 1_000_000; i++) { n += 1; } n",
        ),
        (
            "indexed store",
            "var a = []; var l = 0; for (var i = 0; i < 1_000_000; i++) { a[l++] = i; } l",
        ),
        (
            "property access",
            "var o = { a: 1, b: 2 }; var n = 0; for (var i = 0; i < 1_000_000; i++) { n += o.a + o.b; } n",
        ),
        (
            "string concat",
            "var s = ''; for (var i = 0; i < 100_000; i++) { s += 'x'; } s.length",
        ),
        (
            "array iteration",
            "var a = [1,2,3,4,5,6,7,8,9,10]; var n = 0; for (var i = 0; i < 100_000; i++) { for (var v of a) { n += v; } } n",
        ),
        (
            "function calls",
            "function f(x) { return x + 1; } var n = 0; for (var i = 0; i < 1_000_000; i++) { n = f(n); } n",
        ),
        (
            "closure capture",
            "function make(base) { var x = base; return (y) => x + y; } var f = make(2); var n = 0; for (var i = 0; i < 1_000_000; i++) { n = f(n); } n",
        ),
        (
            "per-iteration",
            "function makeFns() { var fns = []; for (let i = 0; i < 16; i++) { fns.push(() => i); } return fns; } var fns = makeFns(); var n = 0; for (var j = 0; j < 100_000; j++) { n += fns[j & 15](); } n",
        ),
        (
            "construct churn",
            "function C(x) { this.x = x; } var n = 0; for (var i = 0; i < 100_000; i++) { var o = new C(i); n += o.x; } n",
        ),
        (
            "buildString shape",
            "var a = []; var l = 0; var c = 0; for (var i = 0; i < 3_000_000; i++) { a[l++] = i; if (l === 10000) { c++; a.length = l = 0; } } c",
        ),
        (
            "buildString full",
            "function buildString() { var lone = [0x2D, 0x58A, 0x5BE, 0x1400, 0x1806, 0x2053, 0x207B, 0x208B, 0x2212, 0x2E17, 0x2E1A, 0x2E40, 0x2E5D, 0x301C, 0x3030, 0x30A0, 0xFE58, 0xFE63, 0xFF0D, 0x10D6E, 0x10EAD]; var ranges = [[0xDC00, 0xDFFF], [0x0, 0x2C], [0x2E, 0x589], [0x58B, 0x5BD], [0x5BF, 0x13FF], [0x1401, 0x1805], [0x1807, 0x200F], [0x2016, 0x2052], [0x2054, 0x207A], [0x207C, 0x208A], [0x208C, 0x2211], [0x2213, 0x2E16], [0x2E18, 0x2E19], [0x2E1B, 0x2E39], [0x2E3C, 0x2E3F], [0x2E41, 0x2E5C], [0x2E5E, 0x301B], [0x301D, 0x302F], [0x3031, 0x309F], [0x30A1, 0xDBFF], [0xE000, 0xFE30], [0xFE33, 0xFE57], [0xFE59, 0xFE62], [0xFE64, 0xFF0C], [0xFF0E, 0x10D6D], [0x10D6F, 0x10EAC], [0x10EAE, 0x10FFFF]]; var CHUNK = 10000; var result = String.fromCodePoint.apply(null, lone); for (var i = 0; i < ranges.length; i++) { var start = ranges[i][0]; var end = ranges[i][1]; var codePoints = []; for (var length = 0, codePoint = start; codePoint <= end; codePoint++) { codePoints[length++] = codePoint; if (length === CHUNK) { result += String.fromCodePoint.apply(null, codePoints); codePoints.length = length = 0; } } result += String.fromCodePoint.apply(null, codePoints); } return result; } var s = buildString(); s.length",
        ),
    ];
    println!("slag {VERSION} micro-benchmarks");
    for (name, source) in benchmarks {
        let _ = context.eval(source);
        let start = Instant::now();
        let timed_ok = context.eval(source).is_ok();
        let elapsed = start.elapsed();
        println!("{name:18} {elapsed:?} ok={timed_ok}");
    }
    Ok(())
}

/// Run the JIT-vs-interpreter comparison suite: each snippet is a certified
/// leaf callee whose body is inside the JIT's supported subset (the
/// counter-loop / arithmetic / member shapes the JIT lowers), so the JIT
/// column actually executes machine code. Each snippet runs in two fresh
/// contexts — one with the JIT hook installed, one without — each warmed by
/// one eval and timed on a second. The printed ratio is jit/interpreter
/// (below 1 means the JIT is faster; ~1 means the body did not JIT). Note
/// that re-evaluating the source re-parses the function into a fresh body,
/// so the JIT column's timed run includes one Cranelift compile (~1ms for
/// these tiny bodies) — the loop timings are therefore pessimistic. The
/// completion values are compared as a differential check that the machine
/// code agrees with the interpreter.
fn run_jit_benchmarks() -> Result<(), u8> {
    let benchmarks: &[(&str, &str)] = &[
        (
            "arithmetic",
            "function bench() { var n = 0; for (var i = 0; i < 1_000_000; i++) { n += i * 2; } return n; } bench();",
        ),
        (
            "property read",
            "function bench(o) { var n = 0; for (var i = 0; i < 1_000_000; i++) { n += o.a + o.b; } return n; } bench({ a: 1, b: 2 });",
        ),
        (
            "string concat",
            "function bench(x) { var s = x; for (var i = 0; i < 100_000; i++) { s += x; } return s.length; } bench('x');",
        ),
        (
            "function calls",
            "function bench(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.f(i); } return s; }\n\
             bench({ f: function (x) { return x + 1; } }, 100_000);",
        ),
        (
            "global read",
            "var g = 1; function bench(n) { var s = 0; for (var i = 0; i < n; i++) { s += g; } return s; } bench(1_000_000);",
        ),
        (
            "compound assign",
            "function bench(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x += 1; s += o.x; } return s; }\n\
             bench({ x: 0 }, 100_000);",
        ),
        (
            "buildString shape",
            "function bench() { var a = []; var l = 0; var c = 0; for (var i = 0; i < 3_000_000; i++) { a[l++] = i; if (l === 10000) { c++; a.length = l = 0; } } return c; } bench();",
        ),
        (
            "buildString full",
            "function bench() { var lone = [0x2D, 0x58A, 0x5BE, 0x1400, 0x1806, 0x2053, 0x207B, 0x208B, 0x2212, 0x2E17, 0x2E1A, 0x2E40, 0x2E5D, 0x301C, 0x3030, 0x30A0, 0xFE58, 0xFE63, 0xFF0D, 0x10D6E, 0x10EAD]; var ranges = [[0xDC00, 0xDFFF], [0x0, 0x2C], [0x2E, 0x589], [0x58B, 0x5BD], [0x5BF, 0x13FF], [0x1401, 0x1805], [0x1807, 0x200F], [0x2016, 0x2052], [0x2054, 0x207A], [0x207C, 0x208A], [0x208C, 0x2211], [0x2213, 0x2E16], [0x2E18, 0x2E19], [0x2E1B, 0x2E39], [0x2E3C, 0x2E3F], [0x2E41, 0x2E5C], [0x2E5E, 0x301B], [0x301D, 0x302F], [0x3031, 0x309F], [0x30A1, 0xDBFF], [0xE000, 0xFE30], [0xFE33, 0xFE57], [0xFE59, 0xFE62], [0xFE64, 0xFF0C], [0xFF0E, 0x10D6D], [0x10D6F, 0x10EAC], [0x10EAE, 0x10FFFF]]; var CHUNK = 10000; var result = String.fromCodePoint.apply(null, lone); for (var i = 0; i < ranges.length; i++) { var start = ranges[i][0]; var end = ranges[i][1]; var codePoints = []; for (var length = 0, codePoint = start; codePoint <= end; codePoint++) { codePoints[length++] = codePoint; if (length === CHUNK) { result += String.fromCodePoint.apply(null, codePoints); codePoints.length = length = 0; } } result += String.fromCodePoint.apply(null, codePoints); } return result.length; } bench();",
        ),
        (
            "typed-array write",
            "function bench(ta) { for (var k = 0; k < ta.length; k++) { ta[k] = k & 255; } return ta.length; } bench(new Uint8Array(800000));",
        ),
        (
            "typed-array length",
            "function bench(ta) { var s = 0; for (var k = 0; k < ta.length; k++) { s += ta.length; } return s; } bench(new Uint8Array(800000));",
        ),
        (
            "vector leaf call",
            "function bench(f) { var s = 0; for (var i = 0; i < 200000; i++) { s += f(i, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17); } return s; } bench(function (a, b, c, d, e, g, h, k, l, m, n, o, p, q, r, t, u) { return a + 1; });",
        ),
        (
            "apply leaf call",
            "function bench(f) { var s = 0; var arr = [1, 2, 3, 4, 5, 6, 7, 8, 9]; for (var i = 0; i < 200000; i++) { s += f.apply(null, arr); } return s; } bench(function (a, b, c, d, e, g, h, k, l) { return a + 1; });",
        ),
    ];
    println!("slag {VERSION} JIT vs interpreter micro-benchmarks");
    println!("(ratio < 1 means the JIT is faster; ~1 means the body did not JIT)");
    for (name, source) in benchmarks {
        let (interp, interp_result) = bench_once(source, false)?;
        let (jit, jit_result) = bench_once(source, true)?;
        let ratio = jit.as_secs_f64() / interp.as_secs_f64();
        let agrees = interp_result == jit_result;
        if agrees {
            println!("{name:18} interp {interp:12?}  jit {jit:12?}  ratio {ratio:5.2}  result-ok");
        } else {
            println!(
                "{name:18} interp {interp:12?}  jit {jit:12?}  ratio {ratio:5.2}  MISMATCH interp={interp_result:?} jit={jit_result:?}"
            );
        }
    }
    Ok(())
}

/// Time one evaluation of `source` in a fresh context (warm-up eval, then a
/// timed eval), installing the JIT hook when `jit` is set. Returns the
/// elapsed time and the completion value's number (the suite's snippets all
/// complete with a Number).
fn bench_once(source: &str, jit: bool) -> Result<(std::time::Duration, Option<f64>), u8> {
    let mut context = Context::new().map_err(report)?;
    if jit {
        jit::install(context.agent_mut()).map_err(report)?;
    }
    context.eval(source).map_err(report)?;
    let start = Instant::now();
    let value = context.eval(source).map_err(report)?;
    Ok((start.elapsed(), value.as_number()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_flags() {
        assert_eq!(parse(&["--version".into()]), Command::Version);
        assert_eq!(parse(&["-V".into()]), Command::Version);
    }

    #[test]
    fn parses_script_path() {
        assert_eq!(
            parse(&["main.js".into()]),
            Command::Run {
                file: "main.js".into(),
                args: vec![],
                options: Options::default(),
            }
        );
    }

    #[test]
    fn no_args_starts_the_repl() {
        assert_eq!(parse(&[]), Command::Repl(Options::default()));
    }

    #[test]
    fn extra_args_become_script_arguments() {
        assert_eq!(
            parse(&["a.js".into(), "b.js".into(), "c".into()]),
            Command::Run {
                file: "a.js".into(),
                args: vec!["b.js".into(), "c".into()],
                options: Options::default(),
            }
        );
    }

    #[test]
    fn dump_flags_are_recognized() {
        let options = Options {
            dump_ast: true,
            dump_tokens: true,
            ..Options::default()
        };
        assert_eq!(
            parse(&["--dump-ast".into(), "--dump-tokens".into(), "a.js".into()]),
            Command::Run {
                file: "a.js".into(),
                args: vec![],
                options,
            }
        );
    }

    #[test]
    fn jit_flags_are_recognized() {
        let options = Options {
            jit: true,
            ..Options::default()
        };
        assert_eq!(parse(&["--jit".into()]), Command::Repl(options.clone()));
        let options = Options {
            no_jit: true,
            ..Options::default()
        };
        assert_eq!(parse(&["--no-jit".into()]), Command::Repl(options.clone()));
        let options = Options {
            jit_bench: true,
            ..Options::default()
        };
        assert_eq!(parse(&["--jit-bench".into()]), Command::Repl(options));
    }

    #[test]
    fn unknown_flags_report_help() {
        assert_eq!(parse(&["--bogus".into()]), Command::Help);
    }

    #[test]
    fn harmony_flags_are_accepted_noops() {
        assert_eq!(
            parse(&["--harmony-something".into()]),
            Command::Repl(Options::default())
        );
    }

    #[test]
    fn gc_stress_flag_is_an_accepted_noop() {
        // The flag parses and sets the knob; GC-1 slice 3 made the stress
        // mode live (collect at every safe point).
        assert_eq!(
            parse(&["--gc-stress".into()]),
            Command::Repl(Options {
                gc_stress: true,
                ..Options::default()
            })
        );
    }

    #[test]
    fn runs_a_simple_script_file() {
        let path = std::env::temp_dir().join(format!("slag_cli_test_{}.js", std::process::id()));
        std::fs::write(&path, "42;").unwrap();
        let result = run_file(path.to_str().unwrap(), &[], &Options::default());
        std::fs::remove_file(&path).ok();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn missing_script_file_reports_an_error() {
        let result = run_file("definitely-not-a-real-file.js", &[], &Options::default());
        assert_eq!(result, Err(1));
    }

    #[test]
    fn dumps_tokens_and_ast() {
        assert_eq!(dump_tokens("let x = 1;"), Ok(()));
        assert_eq!(dump_ast("let x = 1;"), Ok(()));
    }

    #[test]
    fn input_completeness_detection() {
        assert!(input_complete("1 + 2"));
        assert!(input_complete("'it\\'s ok'"));
        assert!(!input_complete("function f() {"));
        assert!(!input_complete("const s = 'unterminated"));
        assert!(!input_complete("/* dangling"));
        assert!(input_complete("/* closed */ 1"));
        assert!(input_complete("`template`"));
        assert!(!input_complete("`template"));
    }
}
