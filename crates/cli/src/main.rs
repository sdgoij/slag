//! `slag` command-line runner and REPL (PLAN Phase 18 CLI polish).
//!
//! Usage: `slag [options] [file.js [args...]]`
//! - With a file: parse and evaluate it, exposing `process.argv` to the
//!   script; extra arguments are script arguments.
//! - Without a file: start the REPL (multi-line input; `.exit` or Ctrl-D
//!   quits).
//!
//! Flags: `--version`/`-V`, `--help`/`-h`, `--dump-ast`, `--dump-tokens`,
//! `--bench` (run the micro-benchmark suite), and the accepted-no-op knobs
//! `--print-bytecode`, `--stack-size N`, `--max-old-space N`, `--harmony-*`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use runtime::embed::Context;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// CLI knobs. The performance flags are accepted for CLI compatibility; the
/// tree-walker interpreter has no bytecode or GC knobs yet (see
/// `docs/perf.md`), so they are no-ops.
#[derive(Debug, Default, Clone, PartialEq)]
struct Options {
    dump_ast: bool,
    dump_tokens: bool,
    bench: bool,
    print_bytecode: bool,
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
            "--print-bytecode" => options.print_bytecode = true,
            "--stack-size" => {
                index += 1;
                options.stack_size = args.get(index).and_then(|value| value.parse().ok());
            }
            "--max-old-space" => {
                index += 1;
                options.max_old_space = args.get(index).and_then(|value| value.parse().ok());
            }
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
            eprintln!("  --print-bytecode          (no-op)");
            eprintln!("  --stack-size N            (no-op)");
            eprintln!("  --max-old-space N         (no-op)");
            eprintln!("  --harmony-*               (no-op)");
            Err(2)
        }
        Command::Run {
            file,
            args,
            options,
        } => run_file(&file, &args, &options),
        Command::Repl(options) => {
            if options.bench {
                let mut context = Context::new().map_err(report)?;
                return run_benchmarks(&mut context);
            }
            repl()
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
    if options.dump_tokens {
        dump_tokens(&source)?;
    }
    if options.dump_ast {
        dump_ast(&source)?;
    }
    let mut context = Context::new().map_err(report)?;
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
    match context.eval(&source) {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
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

/// The REPL: read lines, continue while the input is syntactically
/// incomplete, evaluate complete inputs, and print the completion value.
fn repl() -> Result<(), u8> {
    let mut context = Context::new().map_err(report)?;
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
/// (interning, hooks), then timed. The timings are only comparable within a
/// build profile; see `docs/perf.md` for the benchmark gates.
fn run_benchmarks(context: &mut Context) -> Result<(), u8> {
    let benchmarks: &[(&str, &str)] = &[
        (
            "arithmetic",
            "let n = 0; for (let i = 0; i < 1_000_000; i++) { n += i * 2; } n",
        ),
        (
            "property access",
            "const o = { a: 1, b: 2 }; let n = 0; for (let i = 0; i < 1_000_000; i++) { n += o.a + o.b; } n",
        ),
        (
            "string concat",
            "let s = ''; for (let i = 0; i < 100_000; i++) { s += 'x'; } s.length",
        ),
        (
            "array iteration",
            "const a = [1,2,3,4,5,6,7,8,9,10]; let n = 0; for (let i = 0; i < 100_000; i++) { for (const v of a) { n += v; } } n",
        ),
        (
            "function calls",
            "function f(x) { return x + 1; } let n = 0; for (let i = 0; i < 1_000_000; i++) { n = f(n); } n",
        ),
    ];
    println!("slag {VERSION} micro-benchmarks");
    for (name, source) in benchmarks {
        let _ = context.eval(source);
        let start = Instant::now();
        let _ = context.eval(source);
        let elapsed = start.elapsed();
        println!("{name:18} {elapsed:?}");
    }
    Ok(())
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
