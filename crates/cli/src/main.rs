//! `jsrt` command-line runner and REPL.
//!
//! Phase 0: argument handling and version reporting. Script evaluation and the
//! REPL arrive with the runtime (Phase 4+).

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
        Command::Run(path) => {
            eprintln!("jsrt: {path}: script execution not yet implemented (Phase 4+)");
            Err(1)
        }
        Command::Help => {
            eprintln!("jsrt: usage: jsrt [--version] <file.js>");
            Err(2)
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
}
