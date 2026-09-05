//! Debug dumps shared by the CLI and wasm hosts: the token stream, the AST,
//! and the compiled step stream — mirrors of the CLI's `--dump-tokens`,
//! `--dump-ast`, and `--print-bytecode` flags, rendered into a String so any
//! host can display them.

use std::fmt::Write as _;

/// `--dump-tokens`: every token as `Debug`, one per line.
pub fn tokens(source: &str) -> Result<String, String> {
    let text = syntax::SourceText::from_utf8(source);
    let mut lexer = lexer::Lexer::new(&text, syntax::LexGoal::Div, true);
    let mut out = String::new();
    loop {
        let token = lexer
            .next_token()
            .map_err(|error| format!("tokenize: {error}"))?;
        writeln!(out, "{token:?}").ok();
        if token.kind == syntax::TokenKind::Eof {
            break;
        }
    }
    Ok(out)
}

/// `--dump-ast`: the parsed program as pretty `Debug`.
pub fn ast(source: &str) -> Result<String, String> {
    let program = parser::parse_script(source).map_err(|error| format!("parse: {error}"))?;
    Ok(format!("{program:#?}"))
}

/// `--print-bytecode`: the compiled step stream of a fast script.
pub fn bytecode(source: &str) -> Result<String, String> {
    let program = parser::parse_script(source).map_err(|error| format!("parse: {error}"))?;
    let strict = crate::script::script_is_strict(&crux::JsString::from_utf8(source), &program);
    let body = crate::ir::compile_statements(&program.body, strict, true)
        .map_err(|error| format!("compile: {error}"))?;
    Ok(crate::ir::debug_body_to_string(&body))
}
