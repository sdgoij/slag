//! The WPT-lite JS-API harness (Cut 10 wave 5): run the `waspec/test/js-api`
//! testharness-style fixtures against the Slag embed.
//!
//! The fixtures are WPT `.any.js` files that call `test`/`promise_test` plus
//! the `assert_*` family and load sibling helpers through `// META: script=`
//! comments. Instead of vendoring WPT's 5000-line `testharness.js` (which
//! overflows the engine's parser), a compact shim implements exactly the
//! surface the corpus uses and records results into `globalThis.__WPT`, which
//! the runner reads back as JSON after the job queues drain.
//!
//! Each fixture runs in a fresh [`Context`]; helper files referenced by its
//! META comments are evaluated first. Files that register no tests (helper
//! scripts) are reported as `skip`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use runtime::embed::Context;

/// The testharness subset the js-api corpus uses: sync/async/promise test
/// registration plus the assertion family, recording into `globalThis.__WPT`.
const SHIM: &str = r#"
(function () {
  var state = { total: 0, passed: 0, failed: 0, messages: [] };
  globalThis.__WPT = state;

  function fail(description) { throw new Error(description || "assertion failed"); }

  function assert_true(actual, description) {
    if (!actual) fail(description || ("expected true, got " + String(actual)));
  }
  function assert_false(actual, description) {
    if (actual) fail(description || ("expected false, got " + String(actual)));
  }
  function assert_equals(actual, expected, description) {
    if (!(actual === expected)) {
      fail((description ? description + ": " : "") + "expected " + String(expected) + " but got " + String(actual));
    }
  }
  function assert_not_equals(actual, unexpected, description) {
    if (actual === unexpected) {
      fail((description ? description + ": " : "") + "expected a value different from " + String(unexpected));
    }
  }
  function assert_unreached(description) {
    fail(description || "unreachable code was reached");
  }
  // WPT's `assert_array_equals` compares array-likes (real arrays and typed
  // arrays alike): both sides must have a numeric `length`, and elements are
  // compared recursively when the expected element is itself array-like.
  function array_like(x) {
    return x !== null && typeof x === "object" && typeof x.length === "number";
  }
  function assert_array_equals(actual, expected, description) {
    if (!array_like(actual)) {
      fail((description ? description + ": " : "") + "expected an array-like, got " + Object.prototype.toString.call(actual));
    }
    if (!array_like(expected)) {
      fail((description ? description + ": " : "") + "expected an array-like");
    }
    assert_equals(actual.length, expected.length, description ? description + ": length" : "array length");
    for (var index = 0; index < expected.length; index++) {
      var expected_element = expected[index];
      if (array_like(expected_element)) {
        assert_array_equals(actual[index], expected_element, (description ? description + ": " : "") + "element " + index);
      } else {
        assert_equals(actual[index], expected_element, (description ? description + ": " : "") + "element " + index);
      }
    }
  }
  function assert_class_string(object, class_string, description) {
    assert_equals(Object.prototype.toString.call(object), "[object " + class_string + "]", description || "class string");
  }
  function assert_own_property(object, property_name, description) {
    assert_true(Object.prototype.hasOwnProperty.call(object, property_name),
                (description ? description + ": " : "") + "expected own property " + String(property_name));
  }
  function assert_not_own_property(object, property_name, description) {
    assert_false(Object.prototype.hasOwnProperty.call(object, property_name),
                 (description ? description + ": " : "") + "expected not to be an own property " + String(property_name));
  }
  function assert_inherits(object, property_name, description) {
    assert_false(Object.prototype.hasOwnProperty.call(object, property_name),
                 (description ? description + ": " : "") + String(property_name) + " should be inherited, not own");
    assert_true(property_name in Object(object),
                (description ? description + ": " : "") + "expected inherited property " + String(property_name));
  }

  function thrown_name(thrown) {
    if (thrown === null || thrown === undefined) return String(thrown);
    if (typeof thrown === "object" || typeof thrown === "function") {
      return (thrown.constructor && thrown.constructor.name) || thrown.name || Object.prototype.toString.call(thrown);
    }
    return String(thrown);
  }

  // WPT's `format_value` (a string/symbol-safe description for messages).
  function format_value(value) {
    if (value === null) return "null";
    if (value === undefined) return "undefined";
    if (typeof value === "string") return '"' + value + '"';
    if (typeof value === "symbol") return String(value);
    if (typeof value === "function") return "function " + (value.name || "anonymous");
    try { return String(value); } catch (error) { return Object.prototype.toString.call(value); }
  }

  function throws_match(expected, thrown) {
    if (thrown === expected) return true;
    if (typeof expected === "function" && thrown !== null && (typeof thrown === "object" || typeof thrown === "function")) {
      // Native error classes from another realm still share a prototype
      // chain, so walk the chain (a bare `instanceof` is not enough).
      var proto = Object.getPrototypeOf(thrown);
      while (proto !== null) {
        if (proto === expected.prototype) return true;
        proto = Object.getPrototypeOf(proto);
      }
      try { return thrown instanceof expected; } catch (_) { return false; }
    }
    return false;
  }

  function assert_throws_js(constructor, func, description) {
    if (typeof func !== "function") fail("assert_throws_js requires a function to run");
    var threw = null;
    try { func(); } catch (error) { threw = error; }
    if (threw === null) {
      fail((description ? description + ": " : "") + "expected " + (constructor && constructor.name) + " to be thrown, but nothing was thrown");
    }
    if (!throws_match(constructor, threw)) {
      fail((description ? description + ": " : "") + "expected " + (constructor && constructor.name) + " but got " + thrown_name(threw));
    }
  }
  function assert_throws_exactly(exception, func, description) {
    if (typeof func !== "function") fail("assert_throws_exactly requires a function to run");
    var threw = null;
    try { func(); } catch (error) { threw = error; }
    if (threw === null) {
      fail((description ? description + ": " : "") + "expected the exact exception to be thrown, but nothing was");
    }
    if (threw !== exception) {
      fail((description ? description + ": " : "") + "expected the exact exception but got " + thrown_name(threw));
    }
  }

  function record(name, ok, message) {
    state.total++;
    if (ok) { state.passed++; return; }
    state.failed++;
    if (message) state.messages.push(name + ": " + message);
  }

  function message_of(error) { return (error && error.message) || String(error); }

  // The WPT Test object passed to `test`/`promise_test` callbacks: the
  // corpus uses `unreached_func` (a function that fails the test when
  // invoked, e.g. as a `valueOf` that must not run) and `add_cleanup`
  // (restorations run when the test finishes). `step`/`step_func`/`done`
  // round out the surface for parity.
  function make_test_record(name) {
    var done = false;
    var cleanups = [];
    var t = {
      name: name,
      _cleanups: cleanups,
      unreached_func: function (description) {
        return t.step_func(function () { assert_unreached(description); });
      },
      step_func: function (fn) {
        return function () { return fn.apply(this, arguments); };
      },
      step: function (fn) {
        try { fn.call(t); } catch (error) { t._fail(error); }
      },
      add_cleanup: function (fn) { cleanups.push(fn); },
      done: function () { done = true; },
      _fail: function (error) { throw error; },
    };
    return t;
  }

  function run_cleanups(t) {
    var cleanups = t._cleanups || [];
    for (var index = cleanups.length - 1; index >= 0; index--) cleanups[index]();
  }

  globalThis.test = function (fn, name) {
    var t = make_test_record(name);
    var ok = true;
    var message = null;
    try { fn(t); } catch (error) { ok = false; message = message_of(error); }
    try { run_cleanups(t); } catch (error) {
      if (ok) { ok = false; message = message_of(error); }
    }
    record(name, ok, message);
  };

  globalThis.promise_test = function (fn, name) {
    var t = make_test_record(name);
    var promise;
    try {
      promise = fn(t);
    } catch (error) {
      record(name, false, message_of(error));
      return;
    }
    if (!promise || typeof promise.then !== "function") {
      record(name, true);
      return;
    }
    promise.then(function () {
      try { run_cleanups(t); record(name, true); } catch (error) {
        record(name, false, message_of(error));
      }
    }, function (error) {
      try { run_cleanups(t); } catch (cleanup_error) {}
      record(name, false, message_of(error));
    });
  };

  globalThis.async_test = function (name) {
    var done = false;
    var cleanups = [];
    function fail(error) {
      if (done) return;
      done = true;
      record(name, false, message_of(error));
    }
    return {
      name: name,
      step: function (fn) {
        if (done) return;
        try { fn(); } catch (error) { fail(error); }
      },
      step_func: function (fn) {
        var t = this;
        return function () { try { return fn.apply(this, arguments); } catch (error) { fail(error); } };
      },
      unreached_func: function (description) {
        return function () { fail(new Error(description || "unreachable code was reached")); };
      },
      add_cleanup: function (fn) { cleanups.push(fn); },
      done: function () {
        if (done) return;
        done = true;
        try { run_cleanups({ _cleanups: cleanups }); record(name, true); } catch (error) {
          record(name, false, message_of(error));
        }
      },
    };
  };

  globalThis.setup = function (func_or_properties) {
    if (typeof func_or_properties === "function") {
      try { func_or_properties(); } catch (error) {
        record("setup", false, (error && error.message) || String(error));
      }
    }
  };

  globalThis.done = function () {};

  // `promise_rejects_js`: a promise (from `Promise.resolve`) that fulfills
  // when `promise` rejects with `constructor`, else rejects.
  globalThis.promise_rejects_js = function (_test, constructor, promise, description) {
    return Promise.resolve(promise).then(
      function () {
        throw new Error((description ? description + ": " : "") + "expected " + (constructor && constructor.name) + " rejection, but the promise fulfilled");
      },
      function (error) {
        if (!throws_match(constructor, error)) {
          throw new Error((description ? description + ": " : "") + "expected " + (constructor && constructor.name) + " rejection, but got " + thrown_name(error));
        }
      }
    );
  };

  globalThis.format_value = format_value;

  // The assertion family (also used by the sibling `assertions.js` helpers).
  globalThis.assert_true = assert_true;
  globalThis.assert_false = assert_false;
  globalThis.assert_equals = assert_equals;
  globalThis.assert_not_equals = assert_not_equals;
  globalThis.assert_unreached = assert_unreached;
  globalThis.assert_not_reached = assert_unreached;
  globalThis.assert_array_equals = assert_array_equals;
  globalThis.assert_class_string = assert_class_string;
  globalThis.assert_own_property = assert_own_property;
  globalThis.assert_not_own_property = assert_not_own_property;
  globalThis.assert_inherits = assert_inherits;
  globalThis.assert_throws_js = assert_throws_js;
  globalThis.assert_throws_exactly = assert_throws_exactly;
  // Legacy `assert_throws(errorCtor, fn)` form.
  globalThis.assert_throws = assert_throws_js;
})();
"#;

/// The result of running one fixture.
pub struct FixtureReport {
    /// `None` when the file registered no tests (a helper script).
    pub counts: Option<(usize, usize, usize)>,
    /// A top-level load/parse failure (`Some(message)`) makes the file a
    /// failure even when no test registered.
    pub harness_error: Option<String>,
    /// The individual failing-test messages (for triage).
    pub failure_messages: Vec<String>,
}

fn collect_js(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "js") {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_js(&child, out);
        } else if child.extension().is_some_and(|ext| ext == "js") {
            out.push(child);
        }
    }
}

/// The sibling helper scripts a fixture loads through its `// META: script=`
/// comments. Absolute `/wasm/jsapi/...` paths resolve against the suite root;
/// relative paths resolve against the fixture's directory.
fn meta_scripts(source: &str, file_dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("//") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(meta) = rest.strip_prefix("META:") else {
            continue;
        };
        let Some(script) = meta.trim().strip_prefix("script=") else {
            continue;
        };
        let target = if let Some(relative) = script.strip_prefix("/wasm/jsapi/") {
            root.join(relative)
        } else {
            file_dir.join(script)
        };
        scripts.push(target);
    }
    scripts
}

/// Fixture-source patches for defects in the pinned spec snapshot (the
/// submodule is read-only). Each entry prepends helper text to the fixture
/// whose file name matches, restoring what the fixture's own `META: script=`
/// list assumes. `grow-memory64.any.js` uses `nulls(n)` but only loads
/// `assertions.js` + the builder; the helper lives in `grow.any.js`, and the
/// commit that split the memory64 file (`2929f4497`, "Split memory64 JS API
/// tests into separate files") never moved it.
const FIXTURE_PATCHES: &[(&str, &str)] = &[(
    "grow-memory64.any.js",
    "function nulls(n) { return Array(n).fill(null); }\n",
)];

/// Run one fixture on a dedicated thread with the engine's deep-recursion
/// stack budget. The debug interpreter grows ~160 KB of native stack per JS
/// call level (see the `run_deep` helper in `crates/runtime/src/eval.rs`), so
/// a fixture whose helpers nest past a handful of frames overflows the
/// process main thread's default 1 MiB stack and would kill the whole sweep.
/// [`Context`] cannot cross threads, so the whole eval plus the JSON
/// read-back happens on that thread and only the plain-data report crosses
/// `join()`. A fixture that still exhausts the budget panics on its own
/// thread and surfaces as an `Err` for that file instead of aborting the run.
fn run_fixture(path: &Path, root: &Path) -> Result<FixtureReport, String> {
    let original =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let source = FIXTURE_PATCHES
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, patch)| format!("{patch}{original}"))
        .unwrap_or(original);
    let file = path.to_path_buf();
    let file_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("jsapi-fixture".into())
        .stack_size(64 << 20)
        .spawn(move || run_fixture_on(&file_dir, &root, &source))
        .map_err(|error| format!("spawn fixture thread: {error}"))?
        .join()
        .map_err(|_| format!("{}: fixture thread panicked", file.display()))?
}

/// The per-fixture body, executed on the big-stack thread created by
/// [`run_fixture`].
fn run_fixture_on(file_dir: &Path, root: &Path, source: &str) -> Result<FixtureReport, String> {
    let mut context = Context::new().map_err(|error| format!("context: {error}"))?;

    let eval = |context: &mut Context, name: &str, text: &str| -> Result<(), String> {
        if std::env::var_os("WASM_JSAPI_TRACE").is_some() {
            eprintln!("trace: evaluating {name} ({} bytes)", text.len());
        }
        context
            .eval(text)
            .map(|_| ())
            .map_err(|error| format!("{name}: {error}"))
    };
    eval(&mut context, "testharness shim", SHIM)?;

    for include in meta_scripts(source, file_dir, root) {
        let include_source = fs::read_to_string(&include)
            .map_err(|error| format!("read {}: {error}", include.display()))?;
        eval(
            &mut context,
            &include.display().to_string(),
            &include_source,
        )?;
    }

    if let Err(error) = eval(&mut context, "fixture", source) {
        return Ok(FixtureReport {
            counts: None,
            harness_error: Some(error),
            failure_messages: Vec::new(),
        });
    }

    // Read the recorded results back (the `Context::eval` drains the job
    // queues, so promise tests have settled by now).
    let json = context
        .eval("JSON.stringify(globalThis.__WPT)")
        .map_err(|error| format!("reading results: {error}"))?
        .as_string()
        .ok_or_else(|| "results were not a string".to_string())?;
    let parsed: serde_json::Value =
        serde_json::from_str(&json).map_err(|error| format!("bad results json: {error}"))?;
    let failed = parsed["failed"].as_u64().unwrap_or(0) as usize;
    let passed = parsed["passed"].as_u64().unwrap_or(0) as usize;
    let total = parsed["total"].as_u64().unwrap_or(0) as usize;
    let messages: Vec<String> = parsed["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let counts = if total == 0 {
        None
    } else {
        Some((total, passed, failed))
    };
    Ok(FixtureReport {
        counts,
        harness_error: None,
        failure_messages: messages,
    })
}

/// The suite root the `/wasm/jsapi/` META prefix maps to: the directory of
/// the first path's tree whose name is `js-api`, or the path's own parent for
/// a standalone file outside the suite.
fn suite_root(paths: &[PathBuf]) -> PathBuf {
    let candidate = paths.first().and_then(|path| path.canonicalize().ok());
    let mut current = if candidate.as_deref().is_some_and(Path::is_file) {
        candidate
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    } else {
        candidate
    };
    while let Some(dir) = current {
        if dir.file_name().is_some_and(|name| name == "js-api")
            && dir.join("wasm-module-builder.js").is_file()
        {
            return dir;
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    paths
        .first()
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from("waspec/test/js-api"))
}

/// Run the JS-API fixtures under each path (a file or a directory tree).
/// Prints a per-file line like the core runner and returns a nonzero exit
/// code when any fixture reports a failing test.
pub fn run(paths: &[PathBuf]) -> ExitCode {
    let root = suite_root(paths);
    let mut files = Vec::new();
    for path in paths {
        collect_js(path, &mut files);
    }
    if files.is_empty() {
        eprintln!("wasmtest jsapi: no .js files found");
        return ExitCode::from(2);
    }
    let mut failures = 0usize;
    let mut skips = 0usize;
    let mut pass_files = 0usize;
    let mut fail_files = 0usize;
    let mut total_pass = 0usize;
    let mut total_fail = 0usize;
    let exclusions = crate::load_exclusions();
    for file in &files {
        if let Some(reason) = crate::excluded(file, &exclusions) {
            skips += 1;
            println!("{}: excluded: {reason}", file.display());
            continue;
        }
        match run_fixture(file, &root) {
            Ok(report) => {
                if let Some((total, passed, failed)) = report.counts {
                    if failed > 0 {
                        failures += 1;
                        fail_files += 1;
                    } else {
                        pass_files += 1;
                    }
                    total_pass += passed;
                    total_fail += failed;
                    println!(
                        "{}: {passed} pass, {failed} fail (of {total})",
                        file.display()
                    );
                    if std::env::var_os("WASM_JSAPI_VERBOSE").is_some() {
                        for message in &report.failure_messages {
                            eprintln!("    {message}");
                        }
                    }
                } else if report.harness_error.is_some() {
                    failures += 1;
                    fail_files += 1;
                    total_fail += 1;
                    eprintln!(
                        "{}: harness error: {}",
                        file.display(),
                        report.harness_error.as_deref().unwrap_or("")
                    );
                } else {
                    skips += 1;
                    println!("{}: skipped (no tests)", file.display());
                }
            }
            Err(error) => {
                failures += 1;
                fail_files += 1;
                eprintln!("{}: {error}", file.display());
            }
        }
    }
    println!(
        "js-api total: {} fixture-files ({} pass, {} fail, {} skipped), {total_pass} tests pass, {total_fail} fail",
        pass_files + fail_files + skips,
        pass_files,
        fail_files,
        skips,
    );
    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
