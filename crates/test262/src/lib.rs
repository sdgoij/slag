//! test262 conformance harness and runner (pinned `tc39/test262` submodule).
//!
//! Phase 6 begins running a small `test/language/` subset covering the
//! statement/expression families the evaluator implements. The harness reads
//! each fixture's YAML frontmatter, applies the `flags:` strict/sloppy/raw
//! modes, checks `negative:` expectations at their declared phase, installs a
//! minimal native `assert` helper (user functions join Phase 7, so the helpers
//! are builtins), and reports pass/skip/fail.

#[cfg(test)]
mod harness {
    use std::path::{Path, PathBuf};

    use crux::convert::to_boolean;
    use crux::error::{ErrorKind, JsError};
    use crux::function::Function;
    use crux::object::JsObject;
    use crux::string::JsString;
    use crux::value::Value;
    use runtime::Agent;

    /// The `test/language/` fixtures this phase can execute, relative to the
    /// submodule root. Grows as later phases add feature coverage.
    const SUBSET: &[&str] = &[
        "statements/if/cptn-empty-statement.js",
        "statements/if/cptn-no-else-false.js",
        "statements/if/cptn-no-else-true-abrupt-empty.js",
        "statements/if/cptn-no-else-true-nrml.js",
        "statements/if/empty-statement.js",
        "statements/if/if-const-else-const.js",
        "statements/if/if-let-else-let.js",
        "statements/if/let-block-with-newline.js",
        "statements/if/let-identifier-with-newline.js",
        "statements/while/cptn-abrupt-empty.js",
        "statements/while/cptn-iter.js",
        "statements/while/cptn-no-iter.js",
        "statements/while/decl-const.js",
        "statements/while/decl-let.js",
        "statements/while/let-identifier-with-newline.js",
        "statements/while/S12.6.2_A15.js",
        "statements/while/S12.6.2_A4_T5.js",
        "statements/while/S12.6.2_A6_T1.js",
        "statements/function/cptn-decl.js",
        "statements/function/enable-strict-via-body.js",
        "statements/function/early-body-super-call.js",
        "statements/function/dflt-params-arg-val-undefined.js",
        "statements/function/dflt-params-arg-val-not-undefined.js",
        "statements/function/dflt-params-ref-prior.js",
        "statements/function/dflt-params-trailing-comma.js",
        "statements/function/dflt-params-duplicates.js",
        "statements/function/dflt-params-rest.js",
        "statements/function/rest-param-strict-body.js",
        "statements/function/rest-params-trailing-comma-early-error.js",
        "statements/function/params-dflt-args-unmapped.js",
        "expressions/conditional/in-condition.js",
    ];

    /// `crates/test262` sits one level below the repo root, where the
    /// `test262` submodule is pinned.
    fn language_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test262/test/language")
    }

    /// The frontmatter fields the runner needs.
    #[derive(Default)]
    struct Frontmatter {
        negative_phase: Option<String>,
        negative_type: Option<String>,
        flags: Vec<String>,
        includes: Vec<String>,
    }

    /// Split the `/*--- ... ---*/` frontmatter (which follows the copyright
    /// header) from the fixture body.
    fn parse_fixture(source: &str) -> Option<(Frontmatter, &str)> {
        let rest = source.split_once("/*---")?.1;
        let end = rest.find("---*/")?;
        let body = &rest[end + 5..];
        let mut fm = Frontmatter::default();
        let mut in_negative = false;
        for raw in rest[..end].lines() {
            let trimmed = raw.trim();
            if trimmed.starts_with("negative:") {
                in_negative = true;
                continue;
            }
            if in_negative {
                if raw.starts_with(' ') || raw.starts_with('\t') {
                    if let Some(value) = trimmed.strip_prefix("phase:") {
                        fm.negative_phase = Some(value.trim().to_string());
                    } else if let Some(value) = trimmed.strip_prefix("type:") {
                        fm.negative_type = Some(value.trim().to_string());
                    }
                    continue;
                }
                in_negative = false;
            }
            if let Some(value) = trimmed.strip_prefix("flags:") {
                fm.flags = list_items(value);
            } else if let Some(value) = trimmed.strip_prefix("includes:") {
                fm.includes = list_items(value);
            }
        }
        Some((fm, body))
    }

    /// `[a, b]`-style YAML list into items.
    fn list_items(s: &str) -> Vec<String> {
        s.trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|item| !item.is_empty())
            .collect()
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Mode {
        Sloppy,
        Strict,
    }

    fn modes(fm: &Frontmatter) -> Vec<Mode> {
        if fm.flags.iter().any(|f| f == "raw" || f == "noStrict") {
            vec![Mode::Sloppy]
        } else if fm.flags.iter().any(|f| f == "onlyStrict") {
            vec![Mode::Strict]
        } else {
            vec![Mode::Sloppy, Mode::Strict]
        }
    }

    fn expected_kind(fm: &Frontmatter) -> Result<ErrorKind, String> {
        let ty = fm
            .negative_type
            .as_deref()
            .ok_or_else(|| "negative test lacks a type".to_string())?;
        match ty {
            "SyntaxError" => Ok(ErrorKind::SyntaxError),
            "TypeError" => Ok(ErrorKind::TypeError),
            "ReferenceError" => Ok(ErrorKind::ReferenceError),
            "RangeError" => Ok(ErrorKind::RangeError),
            "EvalError" => Ok(ErrorKind::EvalError),
            "UriError" => Ok(ErrorKind::UriError),
            other => Err(format!("unsupported negative type {other}")),
        }
    }

    fn wrap(body: &str, mode: Mode) -> String {
        match mode {
            Mode::Strict => format!("'use strict';\n{body}"),
            Mode::Sloppy => body.to_string(),
        }
    }

    /// Run one fixture in one mode; `Err` carries a human-readable failure.
    fn run_one(body: &str, mode: Mode, fm: &Frontmatter) -> Result<(), String> {
        let wrapped = wrap(body, mode);
        if matches!(fm.negative_phase.as_deref(), Some("parse" | "early")) {
            let kind = expected_kind(fm)?;
            return match parser::parse_script(&wrapped) {
                Err(e) if e.kind == kind => Ok(()),
                Err(e) => Err(format!(
                    "parse {:?} != expected {kind:?}: {}",
                    e.kind, e.message
                )),
                Ok(_) => Err("expected a parse error but the script parsed".into()),
            };
        }
        if let Err(e) = parser::parse_script(&wrapped) {
            return Err(format!("parse error: {}", e.message));
        }
        let mut agent = Agent::new();
        agent
            .initialize_host_defined_realm()
            .map_err(|e| e.message)?;
        install_harness_globals(&agent)?;
        let result = agent.run_script(&wrapped);
        agent.run_jobs().map_err(|e| e.message)?;
        match (result, fm.negative_phase.as_deref()) {
            (Ok(_), None) => Ok(()),
            (Ok(_), Some("runtime")) => {
                Err("expected a runtime error but the script completed".into())
            }
            (Ok(_), Some(phase)) => Err(format!("unexpected negative phase {phase}")),
            (Err(e), Some("runtime")) => {
                let kind = expected_kind(fm)?;
                if e.kind == kind {
                    Ok(())
                } else {
                    Err(format!(
                        "runtime {:?} != expected {kind:?}: {}",
                        e.kind, e.message
                    ))
                }
            }
            (Err(e), None) => Err(format!("unexpected runtime error: {}", e.message)),
            (Err(_), Some(_)) => Err("unexpected error on a parse-phase negative test".into()),
        }
    }

    fn assertion_error(message: String) -> JsError {
        JsError::new(ErrorKind::TypeError, message)
    }

    fn arity_error(name: &str) -> JsError {
        assertion_error(format!("{name} called with the wrong number of arguments"))
    }

    /// Minimal native `assert` helper: the surface the vendored fixtures use.
    fn install_harness_globals(agent: &Agent) -> Result<(), String> {
        let global = agent
            .current_realm()
            .map_err(|e| e.message)?
            .global_object
            .clone();
        let assert_obj = JsObject::ordinary_object_create(None);

        let bare = Function::create_builtin(
            Some(JsString::from_utf8("assert")),
            1,
            Box::new(|_, args| {
                let Some(actual) = args.first() else {
                    return Err(arity_error("assert"));
                };
                if to_boolean(actual) {
                    Ok(Value::Undefined)
                } else {
                    Err(assertion_error("assertion failed".into()))
                }
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        let same_value = Function::create_builtin(
            Some(JsString::from_utf8("sameValue")),
            2,
            Box::new(|_, args| {
                let [actual, expected, ..] = args else {
                    return Err(arity_error("assert.sameValue"));
                };
                if crux::ops::same_value(actual, expected) {
                    Ok(Value::Undefined)
                } else {
                    Err(assertion_error(format!(
                        "Expected SameValue(«{actual}», «{expected}») to be true"
                    )))
                }
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        let not_same_value = Function::create_builtin(
            Some(JsString::from_utf8("notSameValue")),
            2,
            Box::new(|_, args| {
                let [actual, expected, ..] = args else {
                    return Err(arity_error("assert.notSameValue"));
                };
                if crux::ops::is_strictly_equal(actual, expected) {
                    Err(assertion_error(format!(
                        "Expected not SameValue(«{actual}», «{expected}»)"
                    )))
                } else {
                    Ok(Value::Undefined)
                }
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        let assert_true = Function::create_builtin(
            Some(JsString::from_utf8("true")),
            1,
            Box::new(|_, args| {
                let Some(actual) = args.first() else {
                    return Err(arity_error("assert.true"));
                };
                if to_boolean(actual) {
                    Ok(Value::Undefined)
                } else {
                    Err(assertion_error(format!("Expected «{actual}» to be true")))
                }
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        let assert_false = Function::create_builtin(
            Some(JsString::from_utf8("false")),
            1,
            Box::new(|_, args| {
                let Some(actual) = args.first() else {
                    return Err(arity_error("assert.false"));
                };
                if to_boolean(actual) {
                    Err(assertion_error(format!("Expected «{actual}» to be false")))
                } else {
                    Ok(Value::Undefined)
                }
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        for (name, func) in [
            ("assert", bare),
            ("sameValue", same_value),
            ("notSameValue", not_same_value),
            ("true", assert_true),
            ("false", assert_false),
        ] {
            assert_obj
                .create_data_property(&JsString::from_utf8(name), Value::Function(func))
                .map_err(|e| e.message)?;
        }

        let test262_error = Function::create_builtin(
            Some(JsString::from_utf8("Test262Error")),
            1,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(|_, _| {
                Ok(Value::Object(JsObject::ordinary_object_create(None)))
            })),
            None,
        )
        .map_err(|e| e.message)?;

        let dollar_error = Function::create_builtin(
            Some(JsString::from_utf8("$ERROR")),
            1,
            Box::new(|_, args| match args.first() {
                Some(value) => Err(assertion_error(format!("$ERROR: {value}"))),
                None => Err(assertion_error("$ERROR".into())),
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        global
            .create_data_property(&JsString::from_utf8("assert"), Value::Object(assert_obj))
            .map_err(|e| e.message)?;
        global
            .create_data_property(
                &JsString::from_utf8("Test262Error"),
                Value::Function(test262_error),
            )
            .map_err(|e| e.message)?;
        global
            .create_data_property(
                &JsString::from_utf8("$ERROR"),
                Value::Function(dollar_error),
            )
            .map_err(|e| e.message)?;
        Ok(())
    }

    enum FixtureResult {
        Pass,
        Skip(String),
        Fail(String),
    }

    /// Run a fixture file (both modes unless `flags:` says otherwise).
    fn run_fixture(relative: &str) -> FixtureResult {
        let path = language_dir().join(relative);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) => return FixtureResult::Fail(format!("{relative}: {e}")),
        };
        let (fm, body) = match parse_fixture(&source) {
            Some(pair) => pair,
            None => return FixtureResult::Fail(format!("{relative}: missing frontmatter")),
        };
        if fm.flags.iter().any(|f| f == "module") {
            return FixtureResult::Skip("module tests are Phase 7".into());
        }
        if fm.flags.iter().any(|f| f == "async") {
            return FixtureResult::Skip("async tests are Phase 7".into());
        }
        let unsupported: Vec<&str> = fm
            .includes
            .iter()
            .map(String::as_str)
            .filter(|include| *include != "assert.js")
            .collect();
        if !unsupported.is_empty() {
            return FixtureResult::Skip(format!(
                "unsupported includes: {}",
                unsupported.join(", ")
            ));
        }
        for mode in modes(&fm) {
            if let Err(e) = run_one(body, mode, &fm) {
                return FixtureResult::Fail(format!("{relative} ({mode:?}): {e}"));
            }
        }
        FixtureResult::Pass
    }

    #[test]
    fn phase_6_language_subset() {
        if !language_dir().exists() {
            eprintln!("test262 submodule not checked out; run `git submodule update --init`");
            return;
        }
        let mut passed = 0usize;
        let mut skipped = Vec::new();
        let mut failed = Vec::new();
        for relative in SUBSET {
            match run_fixture(relative) {
                FixtureResult::Pass => passed += 1,
                FixtureResult::Skip(reason) => skipped.push((relative, reason)),
                FixtureResult::Fail(reason) => failed.push((relative, reason)),
            }
        }
        eprintln!(
            "test262 subset: {passed}/{} passed, {} skipped",
            SUBSET.len(),
            skipped.len()
        );
        for (relative, reason) in &skipped {
            eprintln!("SKIP {relative}: {reason}");
        }
        for (relative, reason) in &failed {
            eprintln!("FAIL {relative}: {reason}");
        }
        assert!(
            failed.is_empty(),
            "{} test262 fixtures failed",
            failed.len()
        );
    }
}
