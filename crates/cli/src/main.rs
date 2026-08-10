//! `jsrt` command-line runner and REPL.
//!
//! Phase 4: script evaluation via the runtime's execution model; the REPL
//! and `--dump-ast`/`--dump-tokens` arrive in later phases.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Version,
    Run(String),
    Help,
}

fn parse(args: &[String]) -> Command {
    match args {
        [flag] if flag.as_str() == "--version" || flag.as_str() == "-V" => Command::Version,
        [path] => Command::Run(path.clone()),
        _ => Command::Help,
    }
}

fn run(args: &[String]) -> Result<(), u8> {
    match parse(args) {
        Command::Version => {
            println!("jsrt {VERSION}");
            Ok(())
        }
        Command::Run(path) => run_file(&path),
        Command::Help => {
            eprintln!("jsrt: usage: jsrt [--version] <file.js>");
            Err(2)
        }
    }
}

/// Parse and evaluate a script file (spec 16.1: ParseScript +
/// ScriptEvaluation in a fresh realm).
fn run_file(path: &str) -> Result<(), u8> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        eprintln!("jsrt: {path}: {e}");
        1
    })?;
    match runtime::evaluate(&source) {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(error) => {
            eprintln!("jsrt: {error}");
            Err(1)
        }
    }
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
        assert_eq!(parse(&["main.js".into()]), Command::Run("main.js".into()));
    }

    #[test]
    fn missing_or_extra_args_are_help() {
        assert_eq!(parse(&[]), Command::Help);
        assert_eq!(parse(&["a.js".into(), "b.js".into()]), Command::Help);
    }

    #[test]
    fn single_non_flag_arg_is_a_script_path() {
        assert_eq!(parse(&["--bogus".into()]), Command::Run("--bogus".into()));
    }

    #[test]
    fn runs_a_simple_script_file() {
        let path = std::env::temp_dir().join(format!("jsrt_cli_test_{}.js", std::process::id()));
        std::fs::write(&path, "42;").unwrap();
        let result = run_file(path.to_str().unwrap());
        std::fs::remove_file(&path).ok();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn missing_script_file_reports_an_error() {
        let result = run_file("definitely-not-a-real-file.js");
        assert_eq!(result, Err(1));
    }
}
