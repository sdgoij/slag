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

    /// Defines `assert.throws` (the real test262 assert.js checks
    /// `thrown.constructor === expectedErrorConstructor`); run before every
    /// fixture because the native closures cannot invoke `func`.
    const ASSERT_THROWS_PRELUDE: &str = r#"
assert.throws = function (expectedErrorConstructor, func) {
  if (typeof func !== "function") {
    throw new Test262Error("assert.throws requires two arguments: the error constructor and a function to run");
  }
  try {
    func();
  } catch (thrown) {
    if (typeof thrown !== "object" || thrown === null) {
      throw new Test262Error("Thrown value was not an object!");
    }
    // The harness Test262Error instances are plain objects without a
    // prototype, so `thrown.constructor` is undefined; treat that as
    // matching only when the expected constructor is Test262Error.
    var actualName = (thrown.constructor && thrown.constructor.name) || (thrown.name) || "unknown";
    if (thrown.constructor !== expectedErrorConstructor && !(thrown.constructor === undefined && expectedErrorConstructor.name === "Test262Error")) {
      throw new Test262Error("Expected a " + expectedErrorConstructor.name + " but got a " + actualName);
    }
    return;
  }
  throw new Test262Error("Expected a " + expectedErrorConstructor.name + " to be thrown but no exception was thrown at all");
};
"#;

    /// The harness-global helpers beyond `assert.throws` that the vendored
    /// fixtures rely on, defined in JS because they need user-level calls
    /// (property access, Array.isArray, eval) the native closures cannot make:
    /// `assert.compareArray` (real assert.js), the `$262` host object with
    /// `detachArrayBuffer` (detaches through `ArrayBuffer.prototype.transfer`)
    /// and `evalScript`, the `$DETACHBUFFER` helper of `detachArrayBuffer.js`,
    /// and a throwable `Test262Error` (the harness include files call it
    /// without `new`, e.g. `throw Test262Error("...")` in testTypedArray.js).
    /// The `verifyProperty`-family comes from the real propertyHelper.js when
    /// included.
    const HARNESS_PRELUDE: &str = r#"
Test262Error = function (message) {
  var err = { name: "Test262Error", message: message };
  err.constructor = Test262Error;
  return err;
};
assert.compareArray = function (actual, expected) {
  if (actual === expected) return;
  if (actual.length !== expected.length) {
    throw new Test262Error("Expected arrays to have the same length: " + actual.length + " !== " + expected.length);
  }
  for (var i = 0; i < actual.length; i++) {
    var a = actual[i];
    var b = expected[i];
    var same = (a === b) || (typeof a === "number" && typeof b === "number" && Number.isNaN(a) && Number.isNaN(b));
    if (!same) {
      throw new Test262Error("Expected arrays to contain the same values at index " + i);
    }
  }
};

$262 = {};
$262.global = globalThis;
$262.detachArrayBuffer = function (buffer) {
  if (typeof buffer !== "object" || buffer === null || typeof buffer.transfer !== "function") {
    throw new Test262Error("No method available to detach an ArrayBuffer");
  }
  buffer.transfer();
};
$262.evalScript = function (code) {
  return eval(code);
};

function $DETACHBUFFER(buffer) {
  $262.detachArrayBuffer(buffer);
}

function verifyProperty(obj, name, desc, options) {
  assert(arguments.length > 2, "verifyProperty should receive at least 3 arguments");
  var label = (options && options.label) || (typeof name === "symbol" ? name.toString() : String(name));
  var original = Object.getOwnPropertyDescriptor(obj, name);
  if (desc === undefined) {
    assert.sameValue(original, undefined, label + " descriptor should be undefined");
    return;
  }
  if (original === undefined) {
    throw new Test262Error(label + " property should exist");
  }
  if ("value" in desc) assert.sameValue(original.value, desc.value, label + " value");
  if ("writable" in desc) assert.sameValue(original.writable, desc.writable, label + " writable");
  if ("enumerable" in desc) assert.sameValue(original.enumerable, desc.enumerable, label + " enumerable");
  if ("configurable" in desc) assert.sameValue(original.configurable, desc.configurable, label + " configurable");
  if ("get" in desc) assert.sameValue(original.get, desc.get, label + " get");
  if ("set" in desc) assert.sameValue(original.set, desc.set, label + " set");
}
function propLabel(name) {
  return typeof name === "symbol" ? name.toString() : String(name);
}
function verifyNotEnumerable(obj, name) {
  assert.sameValue(Object.getOwnPropertyDescriptor(obj, name).enumerable, false, propLabel(name) + " should not be enumerable");
}
function verifyEnumerable(obj, name) {
  assert.sameValue(Object.getOwnPropertyDescriptor(obj, name).enumerable, true, propLabel(name) + " should be enumerable");
}
function verifyNotConfigurable(obj, name) {
  assert.sameValue(Object.getOwnPropertyDescriptor(obj, name).configurable, false, propLabel(name) + " should not be configurable");
}
function verifyConfigurable(obj, name) {
  assert.sameValue(Object.getOwnPropertyDescriptor(obj, name).configurable, true, propLabel(name) + " should be configurable");
}
function verifyWritable(obj, name, options) {
  var desc = Object.getOwnPropertyDescriptor(obj, name);
  if (desc && (desc.get || desc.set)) {
    throw new Test262Error("Expected " + propLabel(name) + " to be writable, but it is an accessor");
  }
  var expected = options && options.writable !== undefined ? options.writable : true;
  assert.sameValue(desc.writable, expected, propLabel(name) + " writable");
}
function verifyNotWritable(obj, name, options) {
  var desc = Object.getOwnPropertyDescriptor(obj, name);
  // Accessors are never writable.
  if (desc && (desc.get || desc.set)) return;
  var expected = options && options.writable !== undefined ? options.writable : false;
  assert.sameValue(desc.writable, expected, propLabel(name) + " writable");
}
function verifyEqualTo(obj, name, value) {
  assert.sameValue(obj[name], value, propLabel(name) + " value");
}
function verifyCallableProperty(obj, name, functionName, functionLength, desc, options) {
  var label = (options && options.label) || (typeof name === "symbol" ? name.toString() : String(name));
  var value = obj && obj[name];
  assert.sameValue(typeof value, "function", label + " should be a function");
  if (desc === undefined) {
    desc = { writable: true, enumerable: false, configurable: true, value: value };
  } else if (!("value" in desc) && !("get" in desc)) {
    desc.value = value;
  }
  verifyProperty(obj, name, desc, options);
  if (functionName === undefined) {
    functionName = typeof name === "symbol" ? "[" + name.description + "]" : name;
  }
  assert.sameValue(value.name, functionName, label + " name");
  if (functionLength !== undefined) {
    assert.sameValue(value.length, functionLength, label + " length");
  }
}
var verifyPrimordialCallableProperty = verifyCallableProperty;
function verifyAccessorProperty(obj, name, desc, options) {
  var label = (options && options.label) || (typeof name === "symbol" ? name.toString() : String(name));
  var original = Object.getOwnPropertyDescriptor(obj, name);
  if ("get" in desc) assert.sameValue(original.get, desc.get, label + " get");
  if ("set" in desc) assert.sameValue(original.set, desc.set, label + " set");
  if ("enumerable" in desc) assert.sameValue(original.enumerable, desc.enumerable, label + " enumerable");
  if ("configurable" in desc) assert.sameValue(original.configurable, desc.configurable, label + " configurable");
}
var verifyPrimordialAccessorProperty = verifyAccessorProperty;
"#;

    /// Where a fixture lives under the pinned submodule.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Area {
        Language,
        Builtins,
    }

    impl Area {
        fn root(self) -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(match self {
                Area::Language => "../../test262/test/language",
                Area::Builtins => "../../test262/test/built-ins",
            })
        }
    }

    /// One `#[test]` per fixture, so failures and skips are attributed to the
    /// file, tests run in parallel, and `cargo test -p test262 <substring>`
    /// filters to a single fixture. The ident is the path with `/`, `-`, and
    /// `.` folded to `_`; path and ident are paired by hand so the names stay
    /// readable.
    macro_rules! test262_fixture {
        ($name:ident, $path:literal) => {
            #[test]
            fn $name() {
                assert_fixture(Area::Language, $path);
            }
        };
    }

    /// Same, but rooted at `test/built-ins` (Phase 8+). Idents keep the
    /// upstream file case (`S15.6.4.2_A1_T1`), which is not snake_case.
    macro_rules! test262_builtin_fixture {
        ($name:ident, $path:literal) => {
            #[test]
            #[allow(non_snake_case)]
            fn $name() {
                assert_fixture(Area::Builtins, $path);
            }
        };
    }

    test262_fixture!(
        statements_if_cptn_empty_statement,
        "statements/if/cptn-empty-statement.js"
    );
    test262_fixture!(
        statements_if_cptn_no_else_false,
        "statements/if/cptn-no-else-false.js"
    );
    test262_fixture!(
        statements_if_cptn_no_else_true_abrupt_empty,
        "statements/if/cptn-no-else-true-abrupt-empty.js"
    );
    test262_fixture!(
        statements_if_cptn_no_else_true_nrml,
        "statements/if/cptn-no-else-true-nrml.js"
    );
    test262_fixture!(
        statements_if_empty_statement,
        "statements/if/empty-statement.js"
    );
    test262_fixture!(
        statements_if_if_const_else_const,
        "statements/if/if-const-else-const.js"
    );
    test262_fixture!(
        statements_if_if_let_else_let,
        "statements/if/if-let-else-let.js"
    );
    test262_fixture!(
        statements_if_let_block_with_newline,
        "statements/if/let-block-with-newline.js"
    );
    test262_fixture!(
        statements_if_let_identifier_with_newline,
        "statements/if/let-identifier-with-newline.js"
    );
    test262_fixture!(
        statements_while_cptn_abrupt_empty,
        "statements/while/cptn-abrupt-empty.js"
    );
    test262_fixture!(statements_while_cptn_iter, "statements/while/cptn-iter.js");
    test262_fixture!(
        statements_while_cptn_no_iter,
        "statements/while/cptn-no-iter.js"
    );
    test262_fixture!(
        statements_while_decl_const,
        "statements/while/decl-const.js"
    );
    test262_fixture!(statements_while_decl_let, "statements/while/decl-let.js");
    test262_fixture!(
        statements_while_let_identifier_with_newline,
        "statements/while/let-identifier-with-newline.js"
    );
    test262_fixture!(
        statements_while_s12_6_2_a15,
        "statements/while/S12.6.2_A15.js"
    );
    test262_fixture!(
        statements_while_s12_6_2_a4_t5,
        "statements/while/S12.6.2_A4_T5.js"
    );
    test262_fixture!(
        statements_while_s12_6_2_a6_t1,
        "statements/while/S12.6.2_A6_T1.js"
    );
    test262_fixture!(
        statements_function_cptn_decl,
        "statements/function/cptn-decl.js"
    );
    test262_fixture!(
        statements_function_enable_strict_via_body,
        "statements/function/enable-strict-via-body.js"
    );
    test262_fixture!(
        statements_function_early_body_super_call,
        "statements/function/early-body-super-call.js"
    );
    test262_fixture!(
        statements_function_dflt_params_arg_val_undefined,
        "statements/function/dflt-params-arg-val-undefined.js"
    );
    test262_fixture!(
        statements_function_dflt_params_arg_val_not_undefined,
        "statements/function/dflt-params-arg-val-not-undefined.js"
    );
    test262_fixture!(
        statements_function_dflt_params_ref_prior,
        "statements/function/dflt-params-ref-prior.js"
    );
    test262_fixture!(
        statements_function_dflt_params_trailing_comma,
        "statements/function/dflt-params-trailing-comma.js"
    );
    test262_fixture!(
        statements_function_dflt_params_duplicates,
        "statements/function/dflt-params-duplicates.js"
    );
    test262_fixture!(
        statements_function_dflt_params_rest,
        "statements/function/dflt-params-rest.js"
    );
    test262_fixture!(
        statements_function_rest_param_strict_body,
        "statements/function/rest-param-strict-body.js"
    );
    test262_fixture!(
        statements_function_rest_params_trailing_comma_early_error,
        "statements/function/rest-params-trailing-comma-early-error.js"
    );
    test262_fixture!(
        statements_function_params_dflt_args_unmapped,
        "statements/function/params-dflt-args-unmapped.js"
    );
    test262_fixture!(
        statements_function_arguments_with_arguments_fn,
        "statements/function/arguments-with-arguments-fn.js"
    );
    test262_fixture!(
        statements_function_arguments_with_arguments_lex,
        "statements/function/arguments-with-arguments-lex.js"
    );
    test262_fixture!(
        expressions_object_method_definition_meth_dflt_params_ref_prior,
        "expressions/object/method-definition/meth-dflt-params-ref-prior.js"
    );
    test262_fixture!(
        expressions_object_method_definition_meth_params_trailing_comma_single,
        "expressions/object/method-definition/meth-params-trailing-comma-single.js"
    );
    test262_fixture!(
        statements_class_method_dflt_params_ref_prior,
        "statements/class/method/dflt-params-ref-prior.js"
    );
    test262_fixture!(
        statements_class_method_dflt_params_arg_val_undefined,
        "statements/class/method/dflt-params-arg-val-undefined.js"
    );
    test262_fixture!(
        statements_class_method_dflt_params_trailing_comma,
        "statements/class/method/dflt-params-trailing-comma.js"
    );
    test262_fixture!(
        statements_class_method_params_trailing_comma_single,
        "statements/class/method/params-trailing-comma-single.js"
    );
    test262_fixture!(
        statements_class_private_static_getter_non_static_setter_early_error,
        "statements/class/private-static-getter-non-static-setter-early-error.js"
    );
    test262_fixture!(
        statements_class_private_static_setter_non_static_getter_early_error,
        "statements/class/private-static-setter-non-static-getter-early-error.js"
    );
    test262_fixture!(
        statements_class_private_non_static_getter_static_setter_early_error,
        "statements/class/private-non-static-getter-static-setter-early-error.js"
    );
    test262_fixture!(
        statements_class_private_non_static_setter_static_getter_early_error,
        "statements/class/private-non-static-setter-static-getter-early-error.js"
    );
    test262_fixture!(
        statements_class_static_init_scope_private,
        "statements/class/static-init-scope-private.js"
    );
    test262_fixture!(
        expressions_conditional_in_condition,
        "expressions/conditional/in-condition.js"
    );

    // Phase 8 built-ins: every fixture below passes in both modes with the
    // Phase 8 global surface (the list was produced by the scanner, so it is
    // data, not aspiration).
    test262_builtin_fixture!(global_10_2_1_1_3_4_22, "global/10.2.1.1.3-4-22.js");
    test262_builtin_fixture!(global_S10_2_3_A1_1_T1, "global/S10.2.3_A1.1_T1.js");
    test262_builtin_fixture!(global_S10_2_3_A1_1_T2, "global/S10.2.3_A1.1_T2.js");
    test262_builtin_fixture!(global_S10_2_3_A1_2_T1, "global/S10.2.3_A1.2_T1.js");
    test262_builtin_fixture!(global_S10_2_3_A1_2_T2, "global/S10.2.3_A1.2_T2.js");
    test262_builtin_fixture!(global_S10_2_3_A1_3_T1, "global/S10.2.3_A1.3_T1.js");
    test262_builtin_fixture!(global_S10_2_3_A1_3_T2, "global/S10.2.3_A1.3_T2.js");
    test262_builtin_fixture!(global_S10_2_3_A2_1_T1, "global/S10.2.3_A2.1_T1.js");
    test262_builtin_fixture!(global_S10_2_3_A2_1_T2, "global/S10.2.3_A2.1_T2.js");
    test262_builtin_fixture!(global_S10_2_3_A2_1_T3, "global/S10.2.3_A2.1_T3.js");
    test262_builtin_fixture!(global_S10_2_3_A2_1_T4, "global/S10.2.3_A2.1_T4.js");
    test262_builtin_fixture!(global_S10_2_3_A2_3_T1, "global/S10.2.3_A2.3_T1.js");
    test262_builtin_fixture!(global_S10_2_3_A2_3_T2, "global/S10.2.3_A2.3_T2.js");
    test262_builtin_fixture!(global_S10_2_3_A2_3_T3, "global/S10.2.3_A2.3_T3.js");
    test262_builtin_fixture!(global_S10_2_3_A2_3_T4, "global/S10.2.3_A2.3_T4.js");
    test262_builtin_fixture!(undefined_15_1_1_3_0, "undefined/15.1.1.3-0.js");
    test262_builtin_fixture!(undefined_15_1_1_3_1, "undefined/15.1.1.3-1.js");
    test262_builtin_fixture!(undefined_15_1_1_3_3, "undefined/15.1.1.3-3.js");
    test262_builtin_fixture!(undefined_S15_1_1_3_A1, "undefined/S15.1.1.3_A1.js");
    test262_builtin_fixture!(undefined_S15_1_1_3_A3_T2, "undefined/S15.1.1.3_A3_T2.js");
    test262_builtin_fixture!(undefined_S15_1_1_3_A4, "undefined/S15.1.1.3_A4.js");
    test262_builtin_fixture!(NaN_15_1_1_1_0, "NaN/15.1.1.1-0.js");
    test262_builtin_fixture!(NaN_S15_1_1_1_A2_T2, "NaN/S15.1.1.1_A2_T2.js");
    test262_builtin_fixture!(NaN_S15_1_1_1_A3_T2, "NaN/S15.1.1.1_A3_T2.js");
    test262_builtin_fixture!(NaN_S15_1_1_1_A4, "NaN/S15.1.1.1_A4.js");
    test262_builtin_fixture!(Infinity_15_1_1_2_0, "Infinity/15.1.1.2-0.js");
    test262_builtin_fixture!(Infinity_S15_1_1_2_A2_T2, "Infinity/S15.1.1.2_A2_T2.js");
    test262_builtin_fixture!(Infinity_S15_1_1_2_A3_T2, "Infinity/S15.1.1.2_A3_T2.js");
    test262_builtin_fixture!(Infinity_S15_1_1_2_A4, "Infinity/S15.1.1.2_A4.js");
    test262_builtin_fixture!(eval_length_enumerable, "eval/length-enumerable.js");
    test262_builtin_fixture!(
        eval_length_non_configurable,
        "eval/length-non-configurable.js"
    );
    test262_builtin_fixture!(eval_length_value, "eval/length-value.js");
    test262_builtin_fixture!(eval_no_construct, "eval/no-construct.js");
    test262_builtin_fixture!(eval_no_proto, "eval/no-proto.js");
    test262_builtin_fixture!(
        decodeURI_S15_1_3_1_A1_1_T1,
        "decodeURI/S15.1.3.1_A1.1_T1.js"
    );
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A3_T1, "decodeURI/S15.1.3.1_A3_T1.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A3_T2, "decodeURI/S15.1.3.1_A3_T2.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A3_T3, "decodeURI/S15.1.3.1_A3_T3.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A4_T1, "decodeURI/S15.1.3.1_A4_T1.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A4_T2, "decodeURI/S15.1.3.1_A4_T2.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A4_T3, "decodeURI/S15.1.3.1_A4_T3.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A4_T4, "decodeURI/S15.1.3.1_A4_T4.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A5_4, "decodeURI/S15.1.3.1_A5.4.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A5_6, "decodeURI/S15.1.3.1_A5.6.js");
    test262_builtin_fixture!(decodeURI_S15_1_3_1_A5_7, "decodeURI/S15.1.3.1_A5.7.js");
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A1_1_T1,
        "decodeURIComponent/S15.1.3.2_A1.1_T1.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A3_T1,
        "decodeURIComponent/S15.1.3.2_A3_T1.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A3_T2,
        "decodeURIComponent/S15.1.3.2_A3_T2.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A3_T3,
        "decodeURIComponent/S15.1.3.2_A3_T3.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A4_T1,
        "decodeURIComponent/S15.1.3.2_A4_T1.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A4_T2,
        "decodeURIComponent/S15.1.3.2_A4_T2.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A4_T3,
        "decodeURIComponent/S15.1.3.2_A4_T3.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A4_T4,
        "decodeURIComponent/S15.1.3.2_A4_T4.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A5_4,
        "decodeURIComponent/S15.1.3.2_A5.4.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A5_6,
        "decodeURIComponent/S15.1.3.2_A5.6.js"
    );
    test262_builtin_fixture!(
        decodeURIComponent_S15_1_3_2_A5_7,
        "decodeURIComponent/S15.1.3.2_A5.7.js"
    );
    test262_builtin_fixture!(
        encodeURI_S15_1_3_3_A3_1_T1,
        "encodeURI/S15.1.3.3_A3.1_T1.js"
    );
    test262_builtin_fixture!(
        encodeURI_S15_1_3_3_A3_2_T1,
        "encodeURI/S15.1.3.3_A3.2_T1.js"
    );
    test262_builtin_fixture!(
        encodeURI_S15_1_3_3_A3_2_T2,
        "encodeURI/S15.1.3.3_A3.2_T2.js"
    );
    test262_builtin_fixture!(
        encodeURI_S15_1_3_3_A3_2_T3,
        "encodeURI/S15.1.3.3_A3.2_T3.js"
    );
    test262_builtin_fixture!(
        encodeURI_S15_1_3_3_A3_3_T1,
        "encodeURI/S15.1.3.3_A3.3_T1.js"
    );
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A4_T1, "encodeURI/S15.1.3.3_A4_T1.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A4_T2, "encodeURI/S15.1.3.3_A4_T2.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A4_T3, "encodeURI/S15.1.3.3_A4_T3.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A4_T4, "encodeURI/S15.1.3.3_A4_T4.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A5_4, "encodeURI/S15.1.3.3_A5.4.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A5_6, "encodeURI/S15.1.3.3_A5.6.js");
    test262_builtin_fixture!(encodeURI_S15_1_3_3_A5_7, "encodeURI/S15.1.3.3_A5.7.js");
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A3_1_T1,
        "encodeURIComponent/S15.1.3.4_A3.1_T1.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A3_2_T1,
        "encodeURIComponent/S15.1.3.4_A3.2_T1.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A3_2_T2,
        "encodeURIComponent/S15.1.3.4_A3.2_T2.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A3_2_T3,
        "encodeURIComponent/S15.1.3.4_A3.2_T3.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A3_3_T1,
        "encodeURIComponent/S15.1.3.4_A3.3_T1.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A4_T1,
        "encodeURIComponent/S15.1.3.4_A4_T1.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A4_T2,
        "encodeURIComponent/S15.1.3.4_A4_T2.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A4_T3,
        "encodeURIComponent/S15.1.3.4_A4_T3.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A4_T4,
        "encodeURIComponent/S15.1.3.4_A4_T4.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A5_4,
        "encodeURIComponent/S15.1.3.4_A5.4.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A5_6,
        "encodeURIComponent/S15.1.3.4_A5.6.js"
    );
    test262_builtin_fixture!(
        encodeURIComponent_S15_1_3_4_A5_7,
        "encodeURIComponent/S15.1.3.4_A5.7.js"
    );
    test262_builtin_fixture!(
        isFinite_return_false_on_nan_or_infinities,
        "isFinite/return-false-on-nan-or-infinities.js"
    );
    test262_builtin_fixture!(isFinite_S15_1_2_5_A2_6, "isFinite/S15.1.2.5_A2.6.js");
    test262_builtin_fixture!(isNaN_S15_1_2_4_A2_6, "isNaN/S15.1.2.4_A2.6.js");
    test262_builtin_fixture!(parseFloat_15_1_2_3_2_1, "parseFloat/15.1.2.3-2-1.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A1_T1, "parseFloat/S15.1.2.3_A1_T1.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A1_T3, "parseFloat/S15.1.2.3_A1_T3.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T1, "parseFloat/S15.1.2.3_A2_T1.js");
    test262_builtin_fixture!(
        parseFloat_S15_1_2_3_A2_T10,
        "parseFloat/S15.1.2.3_A2_T10.js"
    );
    test262_builtin_fixture!(
        parseFloat_S15_1_2_3_A2_T10_U180E,
        "parseFloat/S15.1.2.3_A2_T10_U180E.js"
    );
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T2, "parseFloat/S15.1.2.3_A2_T2.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T3, "parseFloat/S15.1.2.3_A2_T3.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T4, "parseFloat/S15.1.2.3_A2_T4.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T5, "parseFloat/S15.1.2.3_A2_T5.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T6, "parseFloat/S15.1.2.3_A2_T6.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T7, "parseFloat/S15.1.2.3_A2_T7.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T8, "parseFloat/S15.1.2.3_A2_T8.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A2_T9, "parseFloat/S15.1.2.3_A2_T9.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A3_T1, "parseFloat/S15.1.2.3_A3_T1.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A3_T2, "parseFloat/S15.1.2.3_A3_T2.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A3_T3, "parseFloat/S15.1.2.3_A3_T3.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T1, "parseFloat/S15.1.2.3_A4_T1.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T2, "parseFloat/S15.1.2.3_A4_T2.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T3, "parseFloat/S15.1.2.3_A4_T3.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T5, "parseFloat/S15.1.2.3_A4_T5.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T6, "parseFloat/S15.1.2.3_A4_T6.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A4_T7, "parseFloat/S15.1.2.3_A4_T7.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A5_T2, "parseFloat/S15.1.2.3_A5_T2.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A5_T3, "parseFloat/S15.1.2.3_A5_T3.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A5_T4, "parseFloat/S15.1.2.3_A5_T4.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A7_6, "parseFloat/S15.1.2.3_A7.6.js");
    test262_builtin_fixture!(parseFloat_S15_1_2_3_A7_7, "parseFloat/S15.1.2.3_A7.7.js");
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dd_dot_dd_ep_sign_minus_dd_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-dd-dot-dd-ep-sign-minus-dd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dd_dot_dd_ep_sign_minus_dds_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-dd-dot-dd-ep-sign-minus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dd_dot_dd_ep_sign_plus_dd_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-dd-dot-dd-ep-sign-plus-dd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dd_dot_dd_ep_sign_plus_dds_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-dd-dot-dd-ep-sign-plus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dd_nsl_dd_one_of,
        "parseFloat/tonumber-numeric-separator-literal-dd-nsl-dd-one-of.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dds_dot_dd_nsl_dd_ep_dd,
        "parseFloat/tonumber-numeric-separator-literal-dds-dot-dd-nsl-dd-ep-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dds_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dot_dd_nsl_dd_ep,
        "parseFloat/tonumber-numeric-separator-literal-dot-dd-nsl-dd-ep.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dot_dd_nsl_dds_ep,
        "parseFloat/tonumber-numeric-separator-literal-dot-dd-nsl-dds-ep.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dot_dds_nsl_dd_ep,
        "parseFloat/tonumber-numeric-separator-literal-dot-dds-nsl-dd-ep.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_dot_dds_nsl_dds_ep,
        "parseFloat/tonumber-numeric-separator-literal-dot-dds-nsl-dds-ep.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_nzd_nsl_dd_one_of,
        "parseFloat/tonumber-numeric-separator-literal-nzd-nsl-dd-one-of.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_nzd_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-nzd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_nzd_nsl_dds,
        "parseFloat/tonumber-numeric-separator-literal-nzd-nsl-dds.js"
    );
    test262_builtin_fixture!(
        parseFloat_tonumber_numeric_separator_literal_sign_plus_dds_nsl_dd,
        "parseFloat/tonumber-numeric-separator-literal-sign-plus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(parseInt_15_1_2_2_2_1, "parseInt/15.1.2.2-2-1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A1_T1, "parseInt/S15.1.2.2_A1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A1_T3, "parseInt/S15.1.2.2_A1_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T1, "parseInt/S15.1.2.2_A2_T1.js");
    test262_builtin_fixture!(
        parseInt_S15_1_2_2_A2_T10_U180E,
        "parseInt/S15.1.2.2_A2_T10_U180E.js"
    );
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T2, "parseInt/S15.1.2.2_A2_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T3, "parseInt/S15.1.2.2_A2_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T4, "parseInt/S15.1.2.2_A2_T4.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T5, "parseInt/S15.1.2.2_A2_T5.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T6, "parseInt/S15.1.2.2_A2_T6.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T7, "parseInt/S15.1.2.2_A2_T7.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T8, "parseInt/S15.1.2.2_A2_T8.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A2_T9, "parseInt/S15.1.2.2_A2_T9.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A3_1_T1, "parseInt/S15.1.2.2_A3.1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A3_1_T2, "parseInt/S15.1.2.2_A3.1_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A3_1_T3, "parseInt/S15.1.2.2_A3.1_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A3_2_T2, "parseInt/S15.1.2.2_A3.2_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A4_1_T1, "parseInt/S15.1.2.2_A4.1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A4_1_T2, "parseInt/S15.1.2.2_A4.1_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A4_2_T1, "parseInt/S15.1.2.2_A4.2_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A4_2_T2, "parseInt/S15.1.2.2_A4.2_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A5_1_T1, "parseInt/S15.1.2.2_A5.1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A5_2_T1, "parseInt/S15.1.2.2_A5.2_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A5_2_T2, "parseInt/S15.1.2.2_A5.2_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A6_1_T1, "parseInt/S15.1.2.2_A6.1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A6_1_T2, "parseInt/S15.1.2.2_A6.1_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A6_1_T3, "parseInt/S15.1.2.2_A6.1_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A6_1_T4, "parseInt/S15.1.2.2_A6.1_T4.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A6_1_T5, "parseInt/S15.1.2.2_A6.1_T5.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_1_T1, "parseInt/S15.1.2.2_A7.1_T1.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_1_T2, "parseInt/S15.1.2.2_A7.1_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_2_T2, "parseInt/S15.1.2.2_A7.2_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_2_T3, "parseInt/S15.1.2.2_A7.2_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_3_T2, "parseInt/S15.1.2.2_A7.3_T2.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A7_3_T3, "parseInt/S15.1.2.2_A7.3_T3.js");
    test262_builtin_fixture!(parseInt_S15_1_2_2_A9_6, "parseInt/S15.1.2.2_A9.6.js");
    test262_builtin_fixture!(Boolean_S15_6_1_1_A1_T2, "Boolean/S15.6.1.1_A1_T2.js");
    test262_builtin_fixture!(Boolean_S15_6_1_1_A1_T3, "Boolean/S15.6.1.1_A1_T3.js");
    test262_builtin_fixture!(Boolean_S15_6_1_1_A1_T4, "Boolean/S15.6.1.1_A1_T4.js");
    test262_builtin_fixture!(Boolean_S15_6_1_1_A1_T5, "Boolean/S15.6.1.1_A1_T5.js");
    test262_builtin_fixture!(Boolean_S15_6_1_1_A2, "Boolean/S15.6.1.1_A2.js");
    test262_builtin_fixture!(Boolean_S15_6_2_1_A1, "Boolean/S15.6.2.1_A1.js");
    test262_builtin_fixture!(Boolean_S15_6_2_1_A3, "Boolean/S15.6.2.1_A3.js");
    test262_builtin_fixture!(Boolean_S15_6_2_1_A4, "Boolean/S15.6.2.1_A4.js");
    test262_builtin_fixture!(Boolean_S9_2_A1_T1, "Boolean/S9.2_A1_T1.js");
    test262_builtin_fixture!(Boolean_S9_2_A2_T1, "Boolean/S9.2_A2_T1.js");
    test262_builtin_fixture!(Boolean_S9_2_A3_T1, "Boolean/S9.2_A3_T1.js");
    test262_builtin_fixture!(Boolean_S9_2_A5_T1, "Boolean/S9.2_A5_T1.js");
    test262_builtin_fixture!(Boolean_S9_2_A5_T3, "Boolean/S9.2_A5_T3.js");
    test262_builtin_fixture!(Boolean_symbol_coercion, "Boolean/symbol-coercion.js");
    test262_builtin_fixture!(
        Symbol_auto_boxing_non_strict,
        "Symbol/auto-boxing-non-strict.js"
    );
    test262_builtin_fixture!(Symbol_constructor, "Symbol/constructor.js");
    test262_builtin_fixture!(Symbol_uniqueness, "Symbol/uniqueness.js");
    test262_builtin_fixture!(Error_length, "Error/length.js");
    test262_builtin_fixture!(Error_name, "Error/name.js");
    test262_builtin_fixture!(Error_tostring_1, "Error/tostring-1.js");
    test262_builtin_fixture!(Error_tostring_2, "Error/tostring-2.js");
    test262_builtin_fixture!(
        AggregateError_message_undefined_no_prop,
        "AggregateError/message-undefined-no-prop.js"
    );
    test262_builtin_fixture!(
        AggregateError_newtarget_proto,
        "AggregateError/newtarget-proto.js"
    );
    test262_builtin_fixture!(Function_15_3_2_1_11_1, "Function/15.3.2.1-11-1.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_2_s, "Function/15.3.2.1-11-2-s.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_3, "Function/15.3.2.1-11-3.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_4_s, "Function/15.3.2.1-11-4-s.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_5, "Function/15.3.2.1-11-5.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_6_s, "Function/15.3.2.1-11-6-s.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_7_s, "Function/15.3.2.1-11-7-s.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_8_s, "Function/15.3.2.1-11-8-s.js");
    test262_builtin_fixture!(Function_15_3_2_1_11_9_s, "Function/15.3.2.1-11-9-s.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_12gs, "Function/15.3.5.4_2-12gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_14gs, "Function/15.3.5.4_2-14gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_75gs, "Function/15.3.5.4_2-75gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_76gs, "Function/15.3.5.4_2-76gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_77gs, "Function/15.3.5.4_2-77gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_78gs, "Function/15.3.5.4_2-78gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_79gs, "Function/15.3.5.4_2-79gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_80gs, "Function/15.3.5.4_2-80gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_81gs, "Function/15.3.5.4_2-81gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_82gs, "Function/15.3.5.4_2-82gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_83gs, "Function/15.3.5.4_2-83gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_84gs, "Function/15.3.5.4_2-84gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_85gs, "Function/15.3.5.4_2-85gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_86gs, "Function/15.3.5.4_2-86gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_87gs, "Function/15.3.5.4_2-87gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_88gs, "Function/15.3.5.4_2-88gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_89gs, "Function/15.3.5.4_2-89gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_90gs, "Function/15.3.5.4_2-90gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_91gs, "Function/15.3.5.4_2-91gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_92gs, "Function/15.3.5.4_2-92gs.js");
    test262_builtin_fixture!(Function_15_3_5_4_2_93gs, "Function/15.3.5.4_2-93gs.js");
    test262_builtin_fixture!(Function_S10_1_1_A1_T3, "Function/S10.1.1_A1_T3.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A1_T10, "Function/S15.3.2.1_A1_T10.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A1_T11, "Function/S15.3.2.1_A1_T11.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A1_T12, "Function/S15.3.2.1_A1_T12.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A1_T3, "Function/S15.3.2.1_A1_T3.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A1_T4, "Function/S15.3.2.1_A1_T4.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T11, "Function/S15.3.2.1_A3_T11.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T12, "Function/S15.3.2.1_A3_T12.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T13, "Function/S15.3.2.1_A3_T13.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T14, "Function/S15.3.2.1_A3_T14.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T15, "Function/S15.3.2.1_A3_T15.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T4, "Function/S15.3.2.1_A3_T4.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T5, "Function/S15.3.2.1_A3_T5.js");
    test262_builtin_fixture!(Function_S15_3_2_1_A3_T8, "Function/S15.3.2.1_A3_T8.js");
    test262_builtin_fixture!(Function_S15_3_2_A1, "Function/S15.3.2_A1.js");
    test262_builtin_fixture!(Function_S15_3_3_A2_T2, "Function/S15.3.3_A2_T2.js");
    test262_builtin_fixture!(Function_S15_3_5_A1_T1, "Function/S15.3.5_A1_T1.js");
    test262_builtin_fixture!(Function_S15_3_5_A1_T2, "Function/S15.3.5_A1_T2.js");
    test262_builtin_fixture!(Function_S15_3_5_A2_T1, "Function/S15.3.5_A2_T1.js");
    test262_builtin_fixture!(Function_S15_3_5_A2_T2, "Function/S15.3.5_A2_T2.js");
    test262_builtin_fixture!(Function_S15_3_5_A3_T1, "Function/S15.3.5_A3_T1.js");
    test262_builtin_fixture!(Function_S15_3_5_A3_T2, "Function/S15.3.5_A3_T2.js");
    test262_builtin_fixture!(Function_S15_3_A1, "Function/S15.3_A1.js");
    test262_builtin_fixture!(Function_S15_3_A3_T1, "Function/S15.3_A3_T1.js");
    test262_builtin_fixture!(Function_S15_3_A3_T2, "Function/S15.3_A3_T2.js");
    test262_builtin_fixture!(Function_S15_3_A3_T4, "Function/S15.3_A3_T4.js");
    test262_builtin_fixture!(Function_S15_3_A3_T5, "Function/S15.3_A3_T5.js");
    test262_builtin_fixture!(Function_S15_3_A3_T6, "Function/S15.3_A3_T6.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A1_T1, "Object/S15.2.1.1_A1_T1.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A1_T2, "Object/S15.2.1.1_A1_T2.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A1_T3, "Object/S15.2.1.1_A1_T3.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A1_T4, "Object/S15.2.1.1_A1_T4.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A1_T5, "Object/S15.2.1.1_A1_T5.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A2_T11, "Object/S15.2.1.1_A2_T11.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A2_T8, "Object/S15.2.1.1_A2_T8.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A2_T9, "Object/S15.2.1.1_A2_T9.js");
    test262_builtin_fixture!(Object_S15_2_1_1_A3_T2, "Object/S15.2.1.1_A3_T2.js");
    test262_builtin_fixture!(Object_S15_2_2_1_A2_T1, "Object/S15.2.2.1_A2_T1.js");
    test262_builtin_fixture!(Object_S15_2_2_1_A2_T2, "Object/S15.2.2.1_A2_T2.js");
    test262_builtin_fixture!(Object_S15_2_2_1_A2_T6, "Object/S15.2.2.1_A2_T6.js");
    test262_builtin_fixture!(Object_S15_2_2_1_A2_T7, "Object/S15.2.2.1_A2_T7.js");
    test262_builtin_fixture!(Object_S15_2_2_1_A6_T2, "Object/S15.2.2.1_A6_T2.js");
    test262_builtin_fixture!(Object_S15_2_A1, "Object/S15.2_A1.js");
    test262_builtin_fixture!(Object_S9_9_A3, "Object/S9.9_A3.js");
    test262_builtin_fixture!(Object_S9_9_A6, "Object/S9.9_A6.js");
    test262_builtin_fixture!(
        Object_symbol_object_returns_fresh_symbol,
        "Object/symbol_object-returns-fresh-symbol.js"
    );
    test262_builtin_fixture!(Math_proto, "Math/proto.js");
    test262_builtin_fixture!(Number_15_7_4_1, "Number/15.7.4-1.js");
    test262_builtin_fixture!(Number_S15_7_1_1_A2, "Number/S15.7.1.1_A2.js");
    test262_builtin_fixture!(Number_S15_7_2_1_A1, "Number/S15.7.2.1_A1.js");
    test262_builtin_fixture!(Number_S15_7_2_1_A3, "Number/S15.7.2.1_A3.js");
    test262_builtin_fixture!(Number_S15_7_2_1_A4, "Number/S15.7.2.1_A4.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T01, "Number/S15.7.5_A1_T01.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T02, "Number/S15.7.5_A1_T02.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T03, "Number/S15.7.5_A1_T03.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T04, "Number/S15.7.5_A1_T04.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T05, "Number/S15.7.5_A1_T05.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T06, "Number/S15.7.5_A1_T06.js");
    test262_builtin_fixture!(Number_S15_7_5_A1_T07, "Number/S15.7.5_A1_T07.js");
    test262_builtin_fixture!(Number_S8_12_8_A4, "Number/S8.12.8_A4.js");
    test262_builtin_fixture!(Number_S9_3_1_A1, "Number/S9.3.1_A1.js");
    test262_builtin_fixture!(Number_S9_3_1_A10, "Number/S9.3.1_A10.js");
    test262_builtin_fixture!(Number_S9_3_1_A11, "Number/S9.3.1_A11.js");
    test262_builtin_fixture!(Number_S9_3_1_A12, "Number/S9.3.1_A12.js");
    test262_builtin_fixture!(Number_S9_3_1_A13, "Number/S9.3.1_A13.js");
    test262_builtin_fixture!(Number_S9_3_1_A14, "Number/S9.3.1_A14.js");
    test262_builtin_fixture!(Number_S9_3_1_A15, "Number/S9.3.1_A15.js");
    test262_builtin_fixture!(Number_S9_3_1_A16, "Number/S9.3.1_A16.js");
    test262_builtin_fixture!(Number_S9_3_1_A17, "Number/S9.3.1_A17.js");
    test262_builtin_fixture!(Number_S9_3_1_A18, "Number/S9.3.1_A18.js");
    test262_builtin_fixture!(Number_S9_3_1_A19, "Number/S9.3.1_A19.js");
    test262_builtin_fixture!(Number_S9_3_1_A2, "Number/S9.3.1_A2.js");
    test262_builtin_fixture!(Number_S9_3_1_A20, "Number/S9.3.1_A20.js");
    test262_builtin_fixture!(Number_S9_3_1_A21, "Number/S9.3.1_A21.js");
    test262_builtin_fixture!(Number_S9_3_1_A22, "Number/S9.3.1_A22.js");
    test262_builtin_fixture!(Number_S9_3_1_A23, "Number/S9.3.1_A23.js");
    test262_builtin_fixture!(Number_S9_3_1_A24, "Number/S9.3.1_A24.js");
    test262_builtin_fixture!(Number_S9_3_1_A25, "Number/S9.3.1_A25.js");
    test262_builtin_fixture!(Number_S9_3_1_A26, "Number/S9.3.1_A26.js");
    test262_builtin_fixture!(Number_S9_3_1_A27, "Number/S9.3.1_A27.js");
    test262_builtin_fixture!(Number_S9_3_1_A28, "Number/S9.3.1_A28.js");
    test262_builtin_fixture!(Number_S9_3_1_A29, "Number/S9.3.1_A29.js");
    test262_builtin_fixture!(Number_S9_3_1_A2_U180E, "Number/S9.3.1_A2_U180E.js");
    test262_builtin_fixture!(Number_S9_3_1_A30, "Number/S9.3.1_A30.js");
    test262_builtin_fixture!(Number_S9_3_1_A31, "Number/S9.3.1_A31.js");
    test262_builtin_fixture!(Number_S9_3_1_A32, "Number/S9.3.1_A32.js");
    test262_builtin_fixture!(Number_S9_3_1_A3_T1, "Number/S9.3.1_A3_T1.js");
    test262_builtin_fixture!(Number_S9_3_1_A3_T1_U180E, "Number/S9.3.1_A3_T1_U180E.js");
    test262_builtin_fixture!(Number_S9_3_1_A4_T1, "Number/S9.3.1_A4_T1.js");
    test262_builtin_fixture!(Number_S9_3_1_A5_T1, "Number/S9.3.1_A5_T1.js");
    test262_builtin_fixture!(Number_S9_3_1_A5_T2, "Number/S9.3.1_A5_T2.js");
    test262_builtin_fixture!(Number_S9_3_1_A6_T1, "Number/S9.3.1_A6_T1.js");
    test262_builtin_fixture!(Number_S9_3_1_A7, "Number/S9.3.1_A7.js");
    test262_builtin_fixture!(Number_S9_3_1_A8, "Number/S9.3.1_A8.js");
    test262_builtin_fixture!(Number_S9_3_1_A9, "Number/S9.3.1_A9.js");
    test262_builtin_fixture!(Number_S9_3_A1_T1, "Number/S9.3_A1_T1.js");
    test262_builtin_fixture!(Number_S9_3_A2_T1, "Number/S9.3_A2_T1.js");
    test262_builtin_fixture!(Number_S9_3_A3_T1, "Number/S9.3_A3_T1.js");
    test262_builtin_fixture!(Number_S9_3_A4_1_T1, "Number/S9.3_A4.1_T1.js");
    test262_builtin_fixture!(Number_S9_3_A4_2_T1, "Number/S9.3_A4.2_T1.js");
    test262_builtin_fixture!(
        Number_string_binary_literal_invalid,
        "Number/string-binary-literal-invalid.js"
    );
    test262_builtin_fixture!(
        Number_string_hex_literal_invalid,
        "Number/string-hex-literal-invalid.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_bil_bd_nsl_bd,
        "Number/string-numeric-separator-literal-bil-bd-nsl-bd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_bil_bd_nsl_bds,
        "Number/string-numeric-separator-literal-bil-bd-nsl-bds.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_bil_bds_nsl_bd,
        "Number/string-numeric-separator-literal-bil-bds-nsl-bd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_bil_bds_nsl_bds,
        "Number/string-numeric-separator-literal-bil-bds-nsl-bds.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dd_dot_dd_ep_sign_minus_dd_nsl_dd,
        "Number/string-numeric-separator-literal-dd-dot-dd-ep-sign-minus-dd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dd_dot_dd_ep_sign_minus_dds_nsl_dd,
        "Number/string-numeric-separator-literal-dd-dot-dd-ep-sign-minus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dd_dot_dd_ep_sign_plus_dd_nsl_dd,
        "Number/string-numeric-separator-literal-dd-dot-dd-ep-sign-plus-dd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dd_dot_dd_ep_sign_plus_dds_nsl_dd,
        "Number/string-numeric-separator-literal-dd-dot-dd-ep-sign-plus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dd_nsl_dd_one_of,
        "Number/string-numeric-separator-literal-dd-nsl-dd-one-of.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dds_dot_dd_nsl_dd_ep_dd,
        "Number/string-numeric-separator-literal-dds-dot-dd-nsl-dd-ep-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dds_nsl_dd,
        "Number/string-numeric-separator-literal-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dot_dd_nsl_dd_ep,
        "Number/string-numeric-separator-literal-dot-dd-nsl-dd-ep.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dot_dd_nsl_dds_ep,
        "Number/string-numeric-separator-literal-dot-dd-nsl-dds-ep.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dot_dds_nsl_dd_ep,
        "Number/string-numeric-separator-literal-dot-dds-nsl-dd-ep.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_dot_dds_nsl_dds_ep,
        "Number/string-numeric-separator-literal-dot-dds-nsl-dds-ep.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_hil_hd_nsl_hd,
        "Number/string-numeric-separator-literal-hil-hd-nsl-hd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_hil_hd_nsl_hds,
        "Number/string-numeric-separator-literal-hil-hd-nsl-hds.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_hil_hds_nsl_hd,
        "Number/string-numeric-separator-literal-hil-hds-nsl-hd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_hil_hds_nsl_hds,
        "Number/string-numeric-separator-literal-hil-hds-nsl-hds.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_hil_od_nsl_od_one_of,
        "Number/string-numeric-separator-literal-hil-od-nsl-od-one-of.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_nzd_nsl_dd_one_of,
        "Number/string-numeric-separator-literal-nzd-nsl-dd-one-of.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_nzd_nsl_dd,
        "Number/string-numeric-separator-literal-nzd-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_nzd_nsl_dds,
        "Number/string-numeric-separator-literal-nzd-nsl-dds.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_oil_od_nsl_od_one_of,
        "Number/string-numeric-separator-literal-oil-od-nsl-od-one-of.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_oil_od_nsl_od,
        "Number/string-numeric-separator-literal-oil-od-nsl-od.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_oil_od_nsl_ods,
        "Number/string-numeric-separator-literal-oil-od-nsl-ods.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_oil_ods_nsl_od,
        "Number/string-numeric-separator-literal-oil-ods-nsl-od.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_oil_ods_nsl_ods,
        "Number/string-numeric-separator-literal-oil-ods-nsl-ods.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_sign_minus_dds_nsl_dd,
        "Number/string-numeric-separator-literal-sign-minus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_numeric_separator_literal_sign_plus_dds_nsl_dd,
        "Number/string-numeric-separator-literal-sign-plus-dds-nsl-dd.js"
    );
    test262_builtin_fixture!(
        Number_string_octal_literal_invald,
        "Number/string-octal-literal-invald.js"
    );
    test262_builtin_fixture!(
        BigInt_constructor_empty_string,
        "BigInt/constructor-empty-string.js"
    );
    test262_builtin_fixture!(
        BigInt_constructor_from_decimal_string,
        "BigInt/constructor-from-decimal-string.js"
    );
    test262_builtin_fixture!(
        BigInt_constructor_from_hex_string,
        "BigInt/constructor-from-hex-string.js"
    );
    test262_builtin_fixture!(
        BigInt_constructor_from_octal_string,
        "BigInt/constructor-from-octal-string.js"
    );
    test262_builtin_fixture!(
        BigInt_constructor_trailing_leading_spaces,
        "BigInt/constructor-trailing-leading-spaces.js"
    );
    test262_builtin_fixture!(Date_S15_9_2_1_A1, "Date/S15.9.2.1_A1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T1, "Date/S15.9.3.1_A1_T1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T2, "Date/S15.9.3.1_A1_T2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T3, "Date/S15.9.3.1_A1_T3.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T4, "Date/S15.9.3.1_A1_T4.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T5, "Date/S15.9.3.1_A1_T5.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A1_T6, "Date/S15.9.3.1_A1_T6.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T1_1, "Date/S15.9.3.1_A3_T1.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T1_2, "Date/S15.9.3.1_A3_T1.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T2_1, "Date/S15.9.3.1_A3_T2.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T2_2, "Date/S15.9.3.1_A3_T2.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T3_1, "Date/S15.9.3.1_A3_T3.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T3_2, "Date/S15.9.3.1_A3_T3.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T4_1, "Date/S15.9.3.1_A3_T4.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T4_2, "Date/S15.9.3.1_A3_T4.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T5_1, "Date/S15.9.3.1_A3_T5.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T5_2, "Date/S15.9.3.1_A3_T5.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T6_1, "Date/S15.9.3.1_A3_T6.1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A3_T6_2, "Date/S15.9.3.1_A3_T6.2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A6_T1, "Date/S15.9.3.1_A6_T1.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A6_T2, "Date/S15.9.3.1_A6_T2.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A6_T3, "Date/S15.9.3.1_A6_T3.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A6_T4, "Date/S15.9.3.1_A6_T4.js");
    test262_builtin_fixture!(Date_S15_9_3_1_A6_T5, "Date/S15.9.3.1_A6_T5.js");
    test262_builtin_fixture!(String_15_5_5_5_2_1_1, "String/15.5.5.5.2-1-1.js");
    test262_builtin_fixture!(String_15_5_5_5_2_1_2, "String/15.5.5.5.2-1-2.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_1, "String/15.5.5.5.2-3-1.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_2, "String/15.5.5.5.2-3-2.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_3, "String/15.5.5.5.2-3-3.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_4, "String/15.5.5.5.2-3-4.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_5, "String/15.5.5.5.2-3-5.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_6, "String/15.5.5.5.2-3-6.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_7, "String/15.5.5.5.2-3-7.js");
    test262_builtin_fixture!(String_15_5_5_5_2_3_8, "String/15.5.5.5.2-3-8.js");
    test262_builtin_fixture!(String_15_5_5_5_2_7_1, "String/15.5.5.5.2-7-1.js");
    test262_builtin_fixture!(String_15_5_5_5_2_7_2, "String/15.5.5.5.2-7-2.js");
    test262_builtin_fixture!(String_15_5_5_5_2_7_3, "String/15.5.5.5.2-7-3.js");
    test262_builtin_fixture!(String_15_5_5_5_2_7_4, "String/15.5.5.5.2-7-4.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T1, "String/S15.5.1.1_A1_T1.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T10, "String/S15.5.1.1_A1_T10.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T11, "String/S15.5.1.1_A1_T11.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T12, "String/S15.5.1.1_A1_T12.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T13, "String/S15.5.1.1_A1_T13.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T14, "String/S15.5.1.1_A1_T14.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T15, "String/S15.5.1.1_A1_T15.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T16, "String/S15.5.1.1_A1_T16.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T17, "String/S15.5.1.1_A1_T17.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T18, "String/S15.5.1.1_A1_T18.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T2, "String/S15.5.1.1_A1_T2.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T3, "String/S15.5.1.1_A1_T3.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T4, "String/S15.5.1.1_A1_T4.js");
    test262_builtin_fixture!(String_S15_5_1_1_A1_T5, "String/S15.5.1.1_A1_T5.js");
    test262_builtin_fixture!(String_S15_5_1_1_A2_T1, "String/S15.5.1.1_A2_T1.js");
    test262_builtin_fixture!(String_S15_5_2_1_A2_T1, "String/S15.5.2.1_A2_T1.js");
    test262_builtin_fixture!(String_S15_5_2_1_A2_T2, "String/S15.5.2.1_A2_T2.js");
    test262_builtin_fixture!(String_S15_5_2_1_A3, "String/S15.5.2.1_A3.js");
    test262_builtin_fixture!(String_S15_5_3_A1, "String/S15.5.3_A1.js");
    test262_builtin_fixture!(String_S15_5_3_A2_T2, "String/S15.5.3_A2_T2.js");
    test262_builtin_fixture!(String_S15_5_5_1_A1, "String/S15.5.5.1_A1.js");
    test262_builtin_fixture!(String_S15_5_5_1_A2, "String/S15.5.5.1_A2.js");
    test262_builtin_fixture!(String_S15_5_5_1_A4_T1, "String/S15.5.5.1_A4_T1.js");
    test262_builtin_fixture!(String_S15_5_5_A1_T1, "String/S15.5.5_A1_T1.js");
    test262_builtin_fixture!(String_S15_5_5_A1_T2, "String/S15.5.5_A1_T2.js");
    test262_builtin_fixture!(String_S15_5_5_A2_T1, "String/S15.5.5_A2_T1.js");
    test262_builtin_fixture!(String_S15_5_5_A2_T2, "String/S15.5.5_A2_T2.js");
    test262_builtin_fixture!(String_S8_12_8_A1, "String/S8.12.8_A1.js");
    test262_builtin_fixture!(String_S9_8_1_A1, "String/S9.8.1_A1.js");
    test262_builtin_fixture!(String_S9_8_1_A10, "String/S9.8.1_A10.js");
    test262_builtin_fixture!(String_S9_8_1_A2, "String/S9.8.1_A2.js");
    test262_builtin_fixture!(String_S9_8_1_A3, "String/S9.8.1_A3.js");
    test262_builtin_fixture!(String_S9_8_1_A4, "String/S9.8.1_A4.js");
    test262_builtin_fixture!(String_S9_8_1_A6, "String/S9.8.1_A6.js");
    test262_builtin_fixture!(String_S9_8_1_A7, "String/S9.8.1_A7.js");
    test262_builtin_fixture!(String_S9_8_1_A8, "String/S9.8.1_A8.js");
    test262_builtin_fixture!(String_S9_8_1_A9_T1, "String/S9.8.1_A9_T1.js");
    test262_builtin_fixture!(String_S9_8_1_A9_T2, "String/S9.8.1_A9_T2.js");
    test262_builtin_fixture!(String_S9_8_A1_T1, "String/S9.8_A1_T1.js");
    test262_builtin_fixture!(String_S9_8_A2_T1, "String/S9.8_A2_T1.js");
    test262_builtin_fixture!(String_S9_8_A3_T1, "String/S9.8_A3_T1.js");
    test262_builtin_fixture!(String_S9_8_A4_T1, "String/S9.8_A4_T1.js");

    // Phase 11 RegExp surface (the list was produced by the scanner, so it
    // is data, not aspiration).

    // Phase 12 Array surface (the list was produced by the scanner, so it
    // is data, not aspiration).
    test262_builtin_fixture!(Array_15_4_5_1, "Array/15.4.5-1.js");
    test262_builtin_fixture!(Array_15_4_5_1_5_1, "Array/15.4.5.1-5-1.js");
    test262_builtin_fixture!(Array_15_4_5_1_5_2, "Array/15.4.5.1-5-2.js");
    test262_builtin_fixture!(Array_constructor, "Array/constructor.js");
    test262_builtin_fixture!(
        Array_property_cast_boolean_primitive,
        "Array/property-cast-boolean-primitive.js"
    );
    test262_builtin_fixture!(
        Array_property_cast_nan_infinity,
        "Array/property-cast-nan-infinity.js"
    );
    test262_builtin_fixture!(Array_property_cast_number, "Array/property-cast-number.js");
    test262_builtin_fixture!(Array_S15_4_1_A1_1_T1, "Array/S15.4.1_A1.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_1_A1_1_T2, "Array/S15.4.1_A1.1_T2.js");
    test262_builtin_fixture!(Array_S15_4_1_A1_1_T3, "Array/S15.4.1_A1.1_T3.js");
    test262_builtin_fixture!(Array_S15_4_1_A1_2_T1, "Array/S15.4.1_A1.2_T1.js");
    test262_builtin_fixture!(Array_S15_4_1_A1_3_T1, "Array/S15.4.1_A1.3_T1.js");
    test262_builtin_fixture!(Array_S15_4_1_A2_1_T1, "Array/S15.4.1_A2.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_1_A2_2_T1, "Array/S15.4.1_A2.2_T1.js");
    test262_builtin_fixture!(Array_S15_4_1_A3_1_T1, "Array/S15.4.1_A3.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A1_1_T1, "Array/S15.4.2.1_A1.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A1_1_T2, "Array/S15.4.2.1_A1.1_T2.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A1_1_T3, "Array/S15.4.2.1_A1.1_T3.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A1_2_T1, "Array/S15.4.2.1_A1.2_T1.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A1_3_T1, "Array/S15.4.2.1_A1.3_T1.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A2_1_T1, "Array/S15.4.2.1_A2.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_2_1_A2_2_T1, "Array/S15.4.2.1_A2.2_T1.js");
    test262_builtin_fixture!(Array_S15_4_3_A1_1_T1, "Array/S15.4.3_A1.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_3_A1_1_T2, "Array/S15.4.3_A1.1_T2.js");
    test262_builtin_fixture!(Array_S15_4_3_A1_1_T3, "Array/S15.4.3_A1.1_T3.js");
    test262_builtin_fixture!(Array_S15_4_5_1_A1_2_T2, "Array/S15.4.5.1_A1.2_T2.js");
    test262_builtin_fixture!(Array_S15_4_5_1_A2_1_T1, "Array/S15.4.5.1_A2.1_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_1_A2_2_T1, "Array/S15.4.5.1_A2.2_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_1_A2_3_T1, "Array/S15.4.5.1_A2.3_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_2_A1_T1, "Array/S15.4.5.2_A1_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_2_A1_T2, "Array/S15.4.5.2_A1_T2.js");
    test262_builtin_fixture!(Array_S15_4_5_2_A2_T1, "Array/S15.4.5.2_A2_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_2_A3_T1, "Array/S15.4.5.2_A3_T1.js");
    test262_builtin_fixture!(Array_S15_4_5_2_A3_T3, "Array/S15.4.5.2_A3_T3.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T10, "Array/S15.4_A1.1_T10.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T4, "Array/S15.4_A1.1_T4.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T5, "Array/S15.4_A1.1_T5.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T6, "Array/S15.4_A1.1_T6.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T7, "Array/S15.4_A1.1_T7.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T8, "Array/S15.4_A1.1_T8.js");
    test262_builtin_fixture!(Array_S15_4_A1_1_T9, "Array/S15.4_A1.1_T9.js");
    test262_builtin_fixture!(RegExp_15_10_4_1_4, "RegExp/15.10.4.1-4.js");
    test262_builtin_fixture!(
        RegExp_call_with_non_regexp_same_constructor,
        "RegExp/call_with_non_regexp_same_constructor.js"
    );
    test262_builtin_fixture!(
        RegExp_call_with_regexp_not_same_constructor,
        "RegExp/call_with_regexp_not_same_constructor.js"
    );
    test262_builtin_fixture!(
        RegExp_character_class_escape_non_whitespace_u180e,
        "RegExp/character-class-escape-non-whitespace-u180e.js"
    );
    test262_builtin_fixture!(
        RegExp_from_regexp_like_flag_override,
        "RegExp/from-regexp-like-flag-override.js"
    );
    test262_builtin_fixture!(
        RegExp_from_regexp_like_short_circuit,
        "RegExp/from-regexp-like-short-circuit.js"
    );
    test262_builtin_fixture!(RegExp_from_regexp_like, "RegExp/from-regexp-like.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A2_1_T1, "RegExp/S15.10.2.10_A2.1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A2_1_T2, "RegExp/S15.10.2.10_A2.1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A3_1_T1, "RegExp/S15.10.2.10_A3.1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A3_1_T2, "RegExp/S15.10.2.10_A3.1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A4_1_T1, "RegExp/S15.10.2.10_A4.1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A4_1_T2, "RegExp/S15.10.2.10_A4.1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A4_1_T3, "RegExp/S15.10.2.10_A4.1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_10_A5_1_T1, "RegExp/S15.10.2.10_A5.1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_11_A1_T1, "RegExp/S15.10.2.11_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_11_A1_T4, "RegExp/S15.10.2.11_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_11_A1_T6, "RegExp/S15.10.2.11_A1_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_11_A1_T8, "RegExp/S15.10.2.11_A1_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_11_A1_T9, "RegExp/S15.10.2.11_A1_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_12_A3_T5, "RegExp/S15.10.2.12_A3_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_12_A4_T5, "RegExp/S15.10.2.12_A4_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T10, "RegExp/S15.10.2.13_A1_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T11, "RegExp/S15.10.2.13_A1_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T12, "RegExp/S15.10.2.13_A1_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T13, "RegExp/S15.10.2.13_A1_T13.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T14, "RegExp/S15.10.2.13_A1_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T3, "RegExp/S15.10.2.13_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T4, "RegExp/S15.10.2.13_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T5, "RegExp/S15.10.2.13_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T6, "RegExp/S15.10.2.13_A1_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T8, "RegExp/S15.10.2.13_A1_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A1_T9, "RegExp/S15.10.2.13_A1_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A2_T3, "RegExp/S15.10.2.13_A2_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A2_T4, "RegExp/S15.10.2.13_A2_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A2_T5, "RegExp/S15.10.2.13_A2_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A2_T7, "RegExp/S15.10.2.13_A2_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A3_T1, "RegExp/S15.10.2.13_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A3_T2, "RegExp/S15.10.2.13_A3_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A3_T3, "RegExp/S15.10.2.13_A3_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_13_A3_T4, "RegExp/S15.10.2.13_A3_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T1, "RegExp/S15.10.2.3_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T10, "RegExp/S15.10.2.3_A1_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T11, "RegExp/S15.10.2.3_A1_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T12, "RegExp/S15.10.2.3_A1_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T13, "RegExp/S15.10.2.3_A1_T13.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T14, "RegExp/S15.10.2.3_A1_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T15, "RegExp/S15.10.2.3_A1_T15.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T16, "RegExp/S15.10.2.3_A1_T16.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T17, "RegExp/S15.10.2.3_A1_T17.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T2, "RegExp/S15.10.2.3_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T3, "RegExp/S15.10.2.3_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T4, "RegExp/S15.10.2.3_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T6, "RegExp/S15.10.2.3_A1_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T8, "RegExp/S15.10.2.3_A1_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_3_A1_T9, "RegExp/S15.10.2.3_A1_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_5_A1_T1, "RegExp/S15.10.2.5_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_5_A1_T2, "RegExp/S15.10.2.5_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_5_A1_T3, "RegExp/S15.10.2.5_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_5_A1_T5, "RegExp/S15.10.2.5_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A1_T2, "RegExp/S15.10.2.6_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A1_T3, "RegExp/S15.10.2.6_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A1_T4, "RegExp/S15.10.2.6_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A1_T5, "RegExp/S15.10.2.6_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T10, "RegExp/S15.10.2.6_A2_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T2, "RegExp/S15.10.2.6_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T3, "RegExp/S15.10.2.6_A2_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T4, "RegExp/S15.10.2.6_A2_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T5, "RegExp/S15.10.2.6_A2_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T6, "RegExp/S15.10.2.6_A2_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A2_T9, "RegExp/S15.10.2.6_A2_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T1, "RegExp/S15.10.2.6_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T10, "RegExp/S15.10.2.6_A3_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T11, "RegExp/S15.10.2.6_A3_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T12, "RegExp/S15.10.2.6_A3_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T14, "RegExp/S15.10.2.6_A3_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T2, "RegExp/S15.10.2.6_A3_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T4, "RegExp/S15.10.2.6_A3_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T6, "RegExp/S15.10.2.6_A3_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T7, "RegExp/S15.10.2.6_A3_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A3_T8, "RegExp/S15.10.2.6_A3_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T1, "RegExp/S15.10.2.6_A4_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T2, "RegExp/S15.10.2.6_A4_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T3, "RegExp/S15.10.2.6_A4_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T4, "RegExp/S15.10.2.6_A4_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T5, "RegExp/S15.10.2.6_A4_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T6, "RegExp/S15.10.2.6_A4_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T7, "RegExp/S15.10.2.6_A4_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A4_T8, "RegExp/S15.10.2.6_A4_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A5_T1, "RegExp/S15.10.2.6_A5_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A5_T2, "RegExp/S15.10.2.6_A5_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A6_T1, "RegExp/S15.10.2.6_A6_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A6_T2, "RegExp/S15.10.2.6_A6_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A6_T3, "RegExp/S15.10.2.6_A6_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_6_A6_T4, "RegExp/S15.10.2.6_A6_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T1, "RegExp/S15.10.2.7_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T10, "RegExp/S15.10.2.7_A1_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T11, "RegExp/S15.10.2.7_A1_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T12, "RegExp/S15.10.2.7_A1_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T3, "RegExp/S15.10.2.7_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T4, "RegExp/S15.10.2.7_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T5, "RegExp/S15.10.2.7_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T6, "RegExp/S15.10.2.7_A1_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T7, "RegExp/S15.10.2.7_A1_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A1_T8, "RegExp/S15.10.2.7_A1_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A2_T1, "RegExp/S15.10.2.7_A2_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A2_T2, "RegExp/S15.10.2.7_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A2_T3, "RegExp/S15.10.2.7_A2_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T1, "RegExp/S15.10.2.7_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T11, "RegExp/S15.10.2.7_A3_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T12, "RegExp/S15.10.2.7_A3_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T13, "RegExp/S15.10.2.7_A3_T13.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T14, "RegExp/S15.10.2.7_A3_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T2, "RegExp/S15.10.2.7_A3_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T5, "RegExp/S15.10.2.7_A3_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T6, "RegExp/S15.10.2.7_A3_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T7, "RegExp/S15.10.2.7_A3_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T8, "RegExp/S15.10.2.7_A3_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A3_T9, "RegExp/S15.10.2.7_A3_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T1, "RegExp/S15.10.2.7_A4_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T10, "RegExp/S15.10.2.7_A4_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T11, "RegExp/S15.10.2.7_A4_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T12, "RegExp/S15.10.2.7_A4_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T13, "RegExp/S15.10.2.7_A4_T13.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T14, "RegExp/S15.10.2.7_A4_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T15, "RegExp/S15.10.2.7_A4_T15.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T16, "RegExp/S15.10.2.7_A4_T16.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T17, "RegExp/S15.10.2.7_A4_T17.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T18, "RegExp/S15.10.2.7_A4_T18.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T19, "RegExp/S15.10.2.7_A4_T19.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T2, "RegExp/S15.10.2.7_A4_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T20, "RegExp/S15.10.2.7_A4_T20.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T3, "RegExp/S15.10.2.7_A4_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T4, "RegExp/S15.10.2.7_A4_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T5, "RegExp/S15.10.2.7_A4_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T6, "RegExp/S15.10.2.7_A4_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T7, "RegExp/S15.10.2.7_A4_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A4_T9, "RegExp/S15.10.2.7_A4_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T1, "RegExp/S15.10.2.7_A5_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T10, "RegExp/S15.10.2.7_A5_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T11, "RegExp/S15.10.2.7_A5_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T12, "RegExp/S15.10.2.7_A5_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T2, "RegExp/S15.10.2.7_A5_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T4, "RegExp/S15.10.2.7_A5_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T5, "RegExp/S15.10.2.7_A5_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T6, "RegExp/S15.10.2.7_A5_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T7, "RegExp/S15.10.2.7_A5_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T8, "RegExp/S15.10.2.7_A5_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A5_T9, "RegExp/S15.10.2.7_A5_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A6_T1, "RegExp/S15.10.2.7_A6_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A6_T3, "RegExp/S15.10.2.7_A6_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A6_T4, "RegExp/S15.10.2.7_A6_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A6_T5, "RegExp/S15.10.2.7_A6_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_7_A6_T6, "RegExp/S15.10.2.7_A6_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A1_T1, "RegExp/S15.10.2.8_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A1_T2, "RegExp/S15.10.2.8_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A1_T3, "RegExp/S15.10.2.8_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A1_T4, "RegExp/S15.10.2.8_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T10, "RegExp/S15.10.2.8_A2_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T11, "RegExp/S15.10.2.8_A2_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T2, "RegExp/S15.10.2.8_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T5, "RegExp/S15.10.2.8_A2_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T6, "RegExp/S15.10.2.8_A2_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T7, "RegExp/S15.10.2.8_A2_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A2_T9, "RegExp/S15.10.2.8_A2_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T1, "RegExp/S15.10.2.8_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T10, "RegExp/S15.10.2.8_A3_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T11, "RegExp/S15.10.2.8_A3_T11.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T12, "RegExp/S15.10.2.8_A3_T12.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T13, "RegExp/S15.10.2.8_A3_T13.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T14, "RegExp/S15.10.2.8_A3_T14.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T16, "RegExp/S15.10.2.8_A3_T16.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T18, "RegExp/S15.10.2.8_A3_T18.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T19, "RegExp/S15.10.2.8_A3_T19.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T2, "RegExp/S15.10.2.8_A3_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T20, "RegExp/S15.10.2.8_A3_T20.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T21, "RegExp/S15.10.2.8_A3_T21.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T22, "RegExp/S15.10.2.8_A3_T22.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T23, "RegExp/S15.10.2.8_A3_T23.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T24, "RegExp/S15.10.2.8_A3_T24.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T25, "RegExp/S15.10.2.8_A3_T25.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T26, "RegExp/S15.10.2.8_A3_T26.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T27, "RegExp/S15.10.2.8_A3_T27.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T28, "RegExp/S15.10.2.8_A3_T28.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T29, "RegExp/S15.10.2.8_A3_T29.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T30, "RegExp/S15.10.2.8_A3_T30.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T31, "RegExp/S15.10.2.8_A3_T31.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T32, "RegExp/S15.10.2.8_A3_T32.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T33, "RegExp/S15.10.2.8_A3_T33.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T4, "RegExp/S15.10.2.8_A3_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T5, "RegExp/S15.10.2.8_A3_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T6, "RegExp/S15.10.2.8_A3_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T7, "RegExp/S15.10.2.8_A3_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T8, "RegExp/S15.10.2.8_A3_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A3_T9, "RegExp/S15.10.2.8_A3_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T1, "RegExp/S15.10.2.8_A4_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T2, "RegExp/S15.10.2.8_A4_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T3, "RegExp/S15.10.2.8_A4_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T4, "RegExp/S15.10.2.8_A4_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T5, "RegExp/S15.10.2.8_A4_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T6, "RegExp/S15.10.2.8_A4_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T7, "RegExp/S15.10.2.8_A4_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T8, "RegExp/S15.10.2.8_A4_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A4_T9, "RegExp/S15.10.2.8_A4_T9.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A5_T1, "RegExp/S15.10.2.8_A5_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_8_A5_T2, "RegExp/S15.10.2.8_A5_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_9_A1_T1, "RegExp/S15.10.2.9_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_2_9_A1_T2, "RegExp/S15.10.2.9_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_2_9_A1_T3, "RegExp/S15.10.2.9_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_2_9_A1_T5, "RegExp/S15.10.2.9_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_2_A1_T1, "RegExp/S15.10.2_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A1_T1, "RegExp/S15.10.3.1_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A1_T2, "RegExp/S15.10.3.1_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A1_T3, "RegExp/S15.10.3.1_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A1_T4, "RegExp/S15.10.3.1_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A1_T5, "RegExp/S15.10.3.1_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A2_T1, "RegExp/S15.10.3.1_A2_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A2_T2, "RegExp/S15.10.3.1_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_3_1_A3_T1, "RegExp/S15.10.3.1_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A1_T1, "RegExp/S15.10.4.1_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A1_T2, "RegExp/S15.10.4.1_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A1_T3, "RegExp/S15.10.4.1_A1_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A1_T4, "RegExp/S15.10.4.1_A1_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A1_T5, "RegExp/S15.10.4.1_A1_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A3_T1, "RegExp/S15.10.4.1_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A3_T2, "RegExp/S15.10.4.1_A3_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A3_T3, "RegExp/S15.10.4.1_A3_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A3_T4, "RegExp/S15.10.4.1_A3_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A3_T5, "RegExp/S15.10.4.1_A3_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A4_T1, "RegExp/S15.10.4.1_A4_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A4_T2, "RegExp/S15.10.4.1_A4_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A4_T3, "RegExp/S15.10.4.1_A4_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A4_T4, "RegExp/S15.10.4.1_A4_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A4_T5, "RegExp/S15.10.4.1_A4_T5.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T1, "RegExp/S15.10.4.1_A5_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T3, "RegExp/S15.10.4.1_A5_T3.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T4, "RegExp/S15.10.4.1_A5_T4.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T6, "RegExp/S15.10.4.1_A5_T6.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T7, "RegExp/S15.10.4.1_A5_T7.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A5_T8, "RegExp/S15.10.4.1_A5_T8.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A6_T1, "RegExp/S15.10.4.1_A6_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A7_T1, "RegExp/S15.10.4.1_A7_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A7_T2, "RegExp/S15.10.4.1_A7_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A8_T1, "RegExp/S15.10.4.1_A8_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_4_1_A8_T10, "RegExp/S15.10.4.1_A8_T10.js");
    test262_builtin_fixture!(RegExp_S15_10_5_A1, "RegExp/S15.10.5_A1.js");
    test262_builtin_fixture!(RegExp_S15_10_5_A2_T2, "RegExp/S15.10.5_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A1_T1, "RegExp/S15.10.7_A1_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A1_T2, "RegExp/S15.10.7_A1_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A2_T1, "RegExp/S15.10.7_A2_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A2_T2, "RegExp/S15.10.7_A2_T2.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A3_T1, "RegExp/S15.10.7_A3_T1.js");
    test262_builtin_fixture!(RegExp_S15_10_7_A3_T2, "RegExp/S15.10.7_A3_T2.js");
    test262_builtin_fixture!(RegExp_u180e, "RegExp/u180e.js");
    test262_builtin_fixture!(RegExp_valid_flags_y, "RegExp/valid-flags-y.js");

    // Phase 12 Uint8Array hex/base64 surface (the list was produced by the
    // scanner; the remaining fixtures need assert.compareArray/propertyHelper
    // or detached-buffer support).
    test262_builtin_fixture!(
        Uint8Array_fromBase64_ignores_receiver,
        "Uint8Array/fromBase64/ignores-receiver.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromBase64_illegal_characters,
        "Uint8Array/fromBase64/illegal-characters.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromBase64_string_coercion,
        "Uint8Array/fromBase64/string-coercion.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromHex_ignores_receiver,
        "Uint8Array/fromHex/ignores-receiver.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromHex_illegal_characters,
        "Uint8Array/fromHex/illegal-characters.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromHex_odd_length_input,
        "Uint8Array/fromHex/odd-length-input.js"
    );
    test262_builtin_fixture!(
        Uint8Array_fromHex_string_coercion,
        "Uint8Array/fromHex/string-coercion.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromBase64_illegal_characters,
        "Uint8Array/prototype/setFromBase64/illegal-characters.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromBase64_string_coercion,
        "Uint8Array/prototype/setFromBase64/string-coercion.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromBase64_trailing_garbage_empty,
        "Uint8Array/prototype/setFromBase64/trailing-garbage-empty.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromBase64_trailing_garbage,
        "Uint8Array/prototype/setFromBase64/trailing-garbage.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromHex_illegal_characters,
        "Uint8Array/prototype/setFromHex/illegal-characters.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromHex_string_coercion,
        "Uint8Array/prototype/setFromHex/string-coercion.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_setFromHex_throws_when_string_length_is_odd,
        "Uint8Array/prototype/setFromHex/throws-when-string-length-is-odd.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_toBase64_alphabet,
        "Uint8Array/prototype/toBase64/alphabet.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_toBase64_omit_padding,
        "Uint8Array/prototype/toBase64/omit-padding.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_toBase64_option_coercion,
        "Uint8Array/prototype/toBase64/option-coercion.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_toBase64_results,
        "Uint8Array/prototype/toBase64/results.js"
    );
    test262_builtin_fixture!(
        Uint8Array_prototype_toHex_results,
        "Uint8Array/prototype/toHex/results.js"
    );

    // Phase 13 keyed-collection surface (Map/Set/WeakMap/WeakSet; the list was
    // produced by the scanner, so it is data, not aspiration).
    test262_builtin_fixture!(
        Map_bigint_number_same_value,
        "Map/bigint-number-same-value.js"
    );
    test262_builtin_fixture!(Map_constructor, "Map/constructor.js");
    test262_builtin_fixture!(
        Map_does_not_throw_when_set_is_not_callable,
        "Map/does-not-throw-when-set-is-not-callable.js"
    );
    test262_builtin_fixture!(Map_get_set_method_failure, "Map/get-set-method-failure.js");
    test262_builtin_fixture!(Map_groupBy_callback_arg, "Map/groupBy/callback-arg.js");
    test262_builtin_fixture!(
        Map_groupBy_callback_throws,
        "Map/groupBy/callback-throws.js"
    );
    test262_builtin_fixture!(Map_groupBy_emptyList, "Map/groupBy/emptyList.js");
    test262_builtin_fixture!(Map_groupBy_evenOdd, "Map/groupBy/evenOdd.js");
    test262_builtin_fixture!(Map_groupBy_groupLength, "Map/groupBy/groupLength.js");
    test262_builtin_fixture!(
        Map_groupBy_invalid_callback,
        "Map/groupBy/invalid-callback.js"
    );
    test262_builtin_fixture!(
        Map_groupBy_invalid_iterable,
        "Map/groupBy/invalid-iterable.js"
    );
    test262_builtin_fixture!(
        Map_groupBy_iterator_next_throws,
        "Map/groupBy/iterator-next-throws.js"
    );
    test262_builtin_fixture!(Map_groupBy_length, "Map/groupBy/length.js");
    test262_builtin_fixture!(Map_groupBy_map_instance, "Map/groupBy/map-instance.js");
    test262_builtin_fixture!(Map_groupBy_name, "Map/groupBy/name.js");
    test262_builtin_fixture!(Map_groupBy_negativeZero, "Map/groupBy/negativeZero.js");
    test262_builtin_fixture!(Map_groupBy_string, "Map/groupBy/string.js");
    test262_builtin_fixture!(Map_groupBy_toPropertyKey, "Map/groupBy/toPropertyKey.js");
    test262_builtin_fixture!(Map_is_a_constructor, "Map/is-a-constructor.js");
    test262_builtin_fixture!(Map_iterable_calls_set, "Map/iterable-calls-set.js");
    test262_builtin_fixture!(
        Map_iterator_close_after_set_failure,
        "Map/iterator-close-after-set-failure.js"
    );
    test262_builtin_fixture!(
        Map_iterator_close_failure_after_set_failure,
        "Map/iterator-close-failure-after-set-failure.js"
    );
    test262_builtin_fixture!(
        Map_iterator_is_undefined_throws,
        "Map/iterator-is-undefined-throws.js"
    );
    test262_builtin_fixture!(
        Map_iterator_item_first_entry_returns_abrupt,
        "Map/iterator-item-first-entry-returns-abrupt.js"
    );
    test262_builtin_fixture!(
        Map_iterator_item_second_entry_returns_abrupt,
        "Map/iterator-item-second-entry-returns-abrupt.js"
    );
    test262_builtin_fixture!(
        Map_iterator_items_are_not_object_close_iterator,
        "Map/iterator-items-are-not-object-close-iterator.js"
    );
    test262_builtin_fixture!(
        Map_iterator_items_are_not_object,
        "Map/iterator-items-are-not-object.js"
    );
    test262_builtin_fixture!(Map_iterator_next_failure, "Map/iterator-next-failure.js");
    test262_builtin_fixture!(Map_iterator_value_failure, "Map/iterator-value-failure.js");
    test262_builtin_fixture!(Map_length, "Map/length.js");
    test262_builtin_fixture!(
        Map_map_iterable_empty_does_not_call_set,
        "Map/map-iterable-empty-does-not-call-set.js"
    );
    test262_builtin_fixture!(
        Map_map_iterable_throws_when_set_is_not_callable,
        "Map/map-iterable-throws-when-set-is-not-callable.js"
    );
    test262_builtin_fixture!(Map_map_iterable, "Map/map-iterable.js");
    test262_builtin_fixture!(
        Map_map_no_iterable_does_not_call_set,
        "Map/map-no-iterable-does-not-call-set.js"
    );
    test262_builtin_fixture!(Map_map_no_iterable, "Map/map-no-iterable.js");
    test262_builtin_fixture!(Map_map, "Map/map.js");
    test262_builtin_fixture!(Map_name, "Map/name.js");
    test262_builtin_fixture!(Map_newtarget, "Map/newtarget.js");
    test262_builtin_fixture!(
        Map_properties_of_map_instances,
        "Map/properties-of-map-instances.js"
    );
    test262_builtin_fixture!(
        Map_properties_of_the_map_prototype_object,
        "Map/properties-of-the-map-prototype-object.js"
    );
    test262_builtin_fixture!(
        Map_prototype_clear_clear_map,
        "Map/prototype/clear/clear-map.js"
    );
    test262_builtin_fixture!(Map_prototype_clear_clear, "Map/prototype/clear/clear.js");
    test262_builtin_fixture!(
        Map_prototype_clear_context_is_not_map_object,
        "Map/prototype/clear/context-is-not-map-object.js"
    );
    test262_builtin_fixture!(
        Map_prototype_clear_context_is_not_object,
        "Map/prototype/clear/context-is-not-object.js"
    );
    test262_builtin_fixture!(
        Map_prototype_clear_context_is_set_object_throws,
        "Map/prototype/clear/context-is-set-object-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_clear_context_is_weakmap_object_throws,
        "Map/prototype/clear/context-is-weakmap-object-throws.js"
    );
    test262_builtin_fixture!(Map_prototype_clear_length, "Map/prototype/clear/length.js");
    test262_builtin_fixture!(
        Map_prototype_clear_map_data_list_is_preserved,
        "Map/prototype/clear/map-data-list-is-preserved.js"
    );
    test262_builtin_fixture!(Map_prototype_clear_name, "Map/prototype/clear/name.js");
    test262_builtin_fixture!(
        Map_prototype_clear_not_a_constructor,
        "Map/prototype/clear/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_clear_returns_undefined,
        "Map/prototype/clear/returns-undefined.js"
    );
    test262_builtin_fixture!(Map_prototype_constructor, "Map/prototype/constructor.js");
    test262_builtin_fixture!(
        Map_prototype_delete_context_is_not_map_object,
        "Map/prototype/delete/context-is-not-map-object.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_context_is_not_object,
        "Map/prototype/delete/context-is-not-object.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_context_is_set_object_throws,
        "Map/prototype/delete/context-is-set-object-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_context_is_weakmap_object_throws,
        "Map/prototype/delete/context-is-weakmap-object-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_delete,
        "Map/prototype/delete/delete.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_does_not_break_iterators,
        "Map/prototype/delete/does-not-break-iterators.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_length,
        "Map/prototype/delete/length.js"
    );
    test262_builtin_fixture!(Map_prototype_delete_name, "Map/prototype/delete/name.js");
    test262_builtin_fixture!(
        Map_prototype_delete_not_a_constructor,
        "Map/prototype/delete/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_returns_false,
        "Map/prototype/delete/returns-false.js"
    );
    test262_builtin_fixture!(
        Map_prototype_delete_returns_true_for_deleted_entry,
        "Map/prototype/delete/returns-true-for-deleted-entry.js"
    );
    test262_builtin_fixture!(Map_prototype_descriptor, "Map/prototype/descriptor.js");
    test262_builtin_fixture!(
        Map_prototype_entries_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/entries/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/entries/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_does_not_have_mapdata_internal_slot,
        "Map/prototype/entries/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_entries,
        "Map/prototype/entries/entries.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_length,
        "Map/prototype/entries/length.js"
    );
    test262_builtin_fixture!(Map_prototype_entries_name, "Map/prototype/entries/name.js");
    test262_builtin_fixture!(
        Map_prototype_entries_not_a_constructor,
        "Map/prototype/entries/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_returns_iterator_empty,
        "Map/prototype/entries/returns-iterator-empty.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_returns_iterator,
        "Map/prototype/entries/returns-iterator.js"
    );
    test262_builtin_fixture!(
        Map_prototype_entries_this_not_object_throw,
        "Map/prototype/entries/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_callback_parameters,
        "Map/prototype/forEach/callback-parameters.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_callback_result_is_abrupt,
        "Map/prototype/forEach/callback-result-is-abrupt.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_callback_this_non_strict,
        "Map/prototype/forEach/callback-this-non-strict.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_callback_this_strict,
        "Map/prototype/forEach/callback-this-strict.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_deleted_values_during_foreach,
        "Map/prototype/forEach/deleted-values-during-foreach.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/forEach/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/forEach/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_does_not_have_mapdata_internal_slot,
        "Map/prototype/forEach/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_first_argument_is_not_callable,
        "Map/prototype/forEach/first-argument-is-not-callable.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_forEach,
        "Map/prototype/forEach/forEach.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_iterates_in_key_insertion_order,
        "Map/prototype/forEach/iterates-in-key-insertion-order.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_iterates_values_added_after_foreach_begins,
        "Map/prototype/forEach/iterates-values-added-after-foreach-begins.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_iterates_values_deleted_then_readded,
        "Map/prototype/forEach/iterates-values-deleted-then-readded.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_length,
        "Map/prototype/forEach/length.js"
    );
    test262_builtin_fixture!(Map_prototype_forEach_name, "Map/prototype/forEach/name.js");
    test262_builtin_fixture!(
        Map_prototype_forEach_not_a_constructor,
        "Map/prototype/forEach/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_return_undefined,
        "Map/prototype/forEach/return-undefined.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_second_parameter_as_callback_context,
        "Map/prototype/forEach/second-parameter-as-callback-context.js"
    );
    test262_builtin_fixture!(
        Map_prototype_forEach_this_not_object_throw,
        "Map/prototype/forEach/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/get/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/get/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_does_not_have_mapdata_internal_slot,
        "Map/prototype/get/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(Map_prototype_get_get, "Map/prototype/get/get.js");
    test262_builtin_fixture!(Map_prototype_get_length, "Map/prototype/get/length.js");
    test262_builtin_fixture!(Map_prototype_get_name, "Map/prototype/get/name.js");
    test262_builtin_fixture!(
        Map_prototype_get_not_a_constructor,
        "Map/prototype/get/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_returns_undefined,
        "Map/prototype/get/returns-undefined.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_returns_value_different_key_types,
        "Map/prototype/get/returns-value-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_returns_value_normalized_zero_key,
        "Map/prototype/get/returns-value-normalized-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_get_this_not_object_throw,
        "Map/prototype/get/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_append_new_values_normalizes_zero_key,
        "Map/prototype/getOrInsert/append-new-values-normalizes-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_append_new_values,
        "Map/prototype/getOrInsert/append-new-values.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_append_value_if_key_is_not_present_different_key_types,
        "Map/prototype/getOrInsert/append-value-if-key-is-not-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/getOrInsert/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/getOrInsert/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_does_not_have_mapdata_internal_slot,
        "Map/prototype/getOrInsert/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_getOrInsert,
        "Map/prototype/getOrInsert/getOrInsert.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_length,
        "Map/prototype/getOrInsert/length.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_name,
        "Map/prototype/getOrInsert/name.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_not_a_constructor,
        "Map/prototype/getOrInsert/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_returns_value_if_key_is_not_present_different_key_types,
        "Map/prototype/getOrInsert/returns-value-if-key-is-not-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_returns_value_if_key_is_present_different_key_types,
        "Map/prototype/getOrInsert/returns-value-if-key-is-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_returns_value_normalized_zero_key,
        "Map/prototype/getOrInsert/returns-value-normalized-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsert_this_not_object_throw,
        "Map/prototype/getOrInsert/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_append_new_values_normalizes_zero_key,
        "Map/prototype/getOrInsertComputed/append-new-values-normalizes-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_append_new_values,
        "Map/prototype/getOrInsertComputed/append-new-values.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_append_value_if_key_is_not_present_different_key_types,
        "Map/prototype/getOrInsertComputed/append-value-if-key-is-not-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_callbackfn_throws,
        "Map/prototype/getOrInsertComputed/callbackfn-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_canonical_key_passed_to_callback,
        "Map/prototype/getOrInsertComputed/canonical-key-passed-to-callback.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_check_callback_fn_args,
        "Map/prototype/getOrInsertComputed/check-callback-fn-args.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_check_state_after_callback_fn_throws,
        "Map/prototype/getOrInsertComputed/check-state-after-callback-fn-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_different_types_function_callbackfn_does_not_throw,
        "Map/prototype/getOrInsertComputed/different-types-function-callbackfn-does-not-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_does_not_evaluate_callbackfn_if_key_present,
        "Map/prototype/getOrInsertComputed/does-not-evaluate-callbackfn-if-key-present.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/getOrInsertComputed/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/getOrInsertComputed/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_getOrInsertComputed,
        "Map/prototype/getOrInsertComputed/getOrInsertComputed.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_not_a_constructor,
        "Map/prototype/getOrInsertComputed/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_not_a_function_callbackfn_throws,
        "Map/prototype/getOrInsertComputed/not-a-function-callbackfn-throws.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_overwrites_mutation_from_callbackfn,
        "Map/prototype/getOrInsertComputed/overwrites-mutation-from-callbackfn.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_returns_value_if_key_is_not_present_different_key_types,
        "Map/prototype/getOrInsertComputed/returns-value-if-key-is-not-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_returns_value_if_key_is_present_different_key_types,
        "Map/prototype/getOrInsertComputed/returns-value-if-key-is-present-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_returns_value_normalized_zero_key,
        "Map/prototype/getOrInsertComputed/returns-value-normalized-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_getOrInsertComputed_this_not_object_throw,
        "Map/prototype/getOrInsertComputed/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/has/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/has/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_does_not_have_mapdata_internal_slot,
        "Map/prototype/has/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(Map_prototype_has_has, "Map/prototype/has/has.js");
    test262_builtin_fixture!(Map_prototype_has_length, "Map/prototype/has/length.js");
    test262_builtin_fixture!(Map_prototype_has_name, "Map/prototype/has/name.js");
    test262_builtin_fixture!(
        Map_prototype_has_normalizes_zero_key,
        "Map/prototype/has/normalizes-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_not_a_constructor,
        "Map/prototype/has/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_return_false_different_key_types,
        "Map/prototype/has/return-false-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_return_true_different_key_types,
        "Map/prototype/has/return-true-different-key-types.js"
    );
    test262_builtin_fixture!(
        Map_prototype_has_this_not_object_throw,
        "Map/prototype/has/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/keys/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/keys/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_does_not_have_mapdata_internal_slot,
        "Map/prototype/keys/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(Map_prototype_keys_keys, "Map/prototype/keys/keys.js");
    test262_builtin_fixture!(Map_prototype_keys_length, "Map/prototype/keys/length.js");
    test262_builtin_fixture!(Map_prototype_keys_name, "Map/prototype/keys/name.js");
    test262_builtin_fixture!(
        Map_prototype_keys_not_a_constructor,
        "Map/prototype/keys/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_returns_iterator_empty,
        "Map/prototype/keys/returns-iterator-empty.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_returns_iterator,
        "Map/prototype/keys/returns-iterator.js"
    );
    test262_builtin_fixture!(
        Map_prototype_keys_this_not_object_throw,
        "Map/prototype/keys/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_append_new_values_normalizes_zero_key,
        "Map/prototype/set/append-new-values-normalizes-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_append_new_values_return_map,
        "Map/prototype/set/append-new-values-return-map.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_append_new_values,
        "Map/prototype/set/append-new-values.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/set/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/set/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_does_not_have_mapdata_internal_slot,
        "Map/prototype/set/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(Map_prototype_set_length, "Map/prototype/set/length.js");
    test262_builtin_fixture!(Map_prototype_set_name, "Map/prototype/set/name.js");
    test262_builtin_fixture!(
        Map_prototype_set_not_a_constructor,
        "Map/prototype/set/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_replaces_a_value_normalizes_zero_key,
        "Map/prototype/set/replaces-a-value-normalizes-zero-key.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_replaces_a_value_returns_map,
        "Map/prototype/set/replaces-a-value-returns-map.js"
    );
    test262_builtin_fixture!(
        Map_prototype_set_replaces_a_value,
        "Map/prototype/set/replaces-a-value.js"
    );
    test262_builtin_fixture!(Map_prototype_set_set, "Map/prototype/set/set.js");
    test262_builtin_fixture!(
        Map_prototype_set_this_not_object_throw,
        "Map/prototype/set/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/size/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/size/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_does_not_have_mapdata_internal_slot,
        "Map/prototype/size/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(Map_prototype_size_length, "Map/prototype/size/length.js");
    test262_builtin_fixture!(Map_prototype_size_name, "Map/prototype/size/name.js");
    test262_builtin_fixture!(
        Map_prototype_size_returns_count_of_present_values_before_after_set_clear,
        "Map/prototype/size/returns-count-of-present-values-before-after-set-clear.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_returns_count_of_present_values_before_after_set_delete,
        "Map/prototype/size/returns-count-of-present-values-before-after-set-delete.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_returns_count_of_present_values_by_insertion,
        "Map/prototype/size/returns-count-of-present-values-by-insertion.js"
    );
    test262_builtin_fixture!(
        Map_prototype_size_returns_count_of_present_values_by_iterable,
        "Map/prototype/size/returns-count-of-present-values-by-iterable.js"
    );
    test262_builtin_fixture!(Map_prototype_size_size, "Map/prototype/size/size.js");
    test262_builtin_fixture!(
        Map_prototype_size_this_not_object_throw,
        "Map/prototype/size/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_Symbol_iterator_not_a_constructor,
        "Map/prototype/Symbol.iterator/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_Symbol_iterator,
        "Map/prototype/Symbol.iterator.js"
    );
    test262_builtin_fixture!(
        Map_prototype_Symbol_toStringTag,
        "Map/prototype/Symbol.toStringTag.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_does_not_have_mapdata_internal_slot_set,
        "Map/prototype/values/does-not-have-mapdata-internal-slot-set.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_does_not_have_mapdata_internal_slot_weakmap,
        "Map/prototype/values/does-not-have-mapdata-internal-slot-weakmap.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_does_not_have_mapdata_internal_slot,
        "Map/prototype/values/does-not-have-mapdata-internal-slot.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_length,
        "Map/prototype/values/length.js"
    );
    test262_builtin_fixture!(Map_prototype_values_name, "Map/prototype/values/name.js");
    test262_builtin_fixture!(
        Map_prototype_values_not_a_constructor,
        "Map/prototype/values/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_returns_iterator_empty,
        "Map/prototype/values/returns-iterator-empty.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_returns_iterator,
        "Map/prototype/values/returns-iterator.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_this_not_object_throw,
        "Map/prototype/values/this-not-object-throw.js"
    );
    test262_builtin_fixture!(
        Map_prototype_values_values,
        "Map/prototype/values/values.js"
    );
    test262_builtin_fixture!(Map_prototype_of_map, "Map/prototype-of-map.js");
    test262_builtin_fixture!(Map_Symbol_species_length, "Map/Symbol.species/length.js");
    test262_builtin_fixture!(
        Map_Symbol_species_return_value,
        "Map/Symbol.species/return-value.js"
    );
    test262_builtin_fixture!(
        Map_Symbol_species_symbol_species_name,
        "Map/Symbol.species/symbol-species-name.js"
    );
    test262_builtin_fixture!(
        Map_Symbol_species_symbol_species,
        "Map/Symbol.species/symbol-species.js"
    );
    test262_builtin_fixture!(Map_undefined_newtarget, "Map/undefined-newtarget.js");
    test262_builtin_fixture!(Map_valid_keys, "Map/valid-keys.js");
    test262_builtin_fixture!(
        Set_bigint_number_same_value,
        "Set/bigint-number-same-value.js"
    );
    test262_builtin_fixture!(Set_constructor, "Set/constructor.js");
    test262_builtin_fixture!(Set_is_a_constructor, "Set/is-a-constructor.js");
    test262_builtin_fixture!(Set_length, "Set/length.js");
    test262_builtin_fixture!(Set_name, "Set/name.js");
    test262_builtin_fixture!(
        Set_properties_of_the_set_prototype_object,
        "Set/properties-of-the-set-prototype-object.js"
    );
    test262_builtin_fixture!(Set_prototype_add_add, "Set/prototype/add/add.js");
    test262_builtin_fixture!(
        Set_prototype_add_does_not_have_setdata_internal_slot_array,
        "Set/prototype/add/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_does_not_have_setdata_internal_slot_map,
        "Set/prototype/add/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_does_not_have_setdata_internal_slot_object,
        "Set/prototype/add/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/add/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/add/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(Set_prototype_add_length, "Set/prototype/add/length.js");
    test262_builtin_fixture!(Set_prototype_add_name, "Set/prototype/add/name.js");
    test262_builtin_fixture!(
        Set_prototype_add_not_a_constructor,
        "Set/prototype/add/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_preserves_insertion_order,
        "Set/prototype/add/preserves-insertion-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_returns_this_when_ignoring_duplicate,
        "Set/prototype/add/returns-this-when-ignoring-duplicate.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_returns_this,
        "Set/prototype/add/returns-this.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_boolean,
        "Set/prototype/add/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_null,
        "Set/prototype/add/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_number,
        "Set/prototype/add/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_string,
        "Set/prototype/add/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_symbol,
        "Set/prototype/add/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_this_not_object_throw_undefined,
        "Set/prototype/add/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_will_not_add_duplicate_entry_initial_iterable,
        "Set/prototype/add/will-not-add-duplicate-entry-initial-iterable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_will_not_add_duplicate_entry_normalizes_zero,
        "Set/prototype/add/will-not-add-duplicate-entry-normalizes-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_add_will_not_add_duplicate_entry,
        "Set/prototype/add/will-not-add-duplicate-entry.js"
    );
    test262_builtin_fixture!(Set_prototype_clear_clear, "Set/prototype/clear/clear.js");
    test262_builtin_fixture!(
        Set_prototype_clear_clears_all_contents_from_iterable,
        "Set/prototype/clear/clears-all-contents-from-iterable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_clears_all_contents,
        "Set/prototype/clear/clears-all-contents.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_clears_an_empty_set,
        "Set/prototype/clear/clears-an-empty-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_does_not_have_setdata_internal_slot_array,
        "Set/prototype/clear/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_does_not_have_setdata_internal_slot_map,
        "Set/prototype/clear/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_does_not_have_setdata_internal_slot_object,
        "Set/prototype/clear/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/clear/does-not-have-setdata-internal-slot-set.prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/clear/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(Set_prototype_clear_length, "Set/prototype/clear/length.js");
    test262_builtin_fixture!(Set_prototype_clear_name, "Set/prototype/clear/name.js");
    test262_builtin_fixture!(
        Set_prototype_clear_not_a_constructor,
        "Set/prototype/clear/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_returns_undefined,
        "Set/prototype/clear/returns-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_boolean,
        "Set/prototype/clear/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_null,
        "Set/prototype/clear/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_number,
        "Set/prototype/clear/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_string,
        "Set/prototype/clear/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_symbol,
        "Set/prototype/clear/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_clear_this_not_object_throw_undefined,
        "Set/prototype/clear/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_constructor_set_prototype_constructor_intrinsic,
        "Set/prototype/constructor/set-prototype-constructor-intrinsic.js"
    );
    test262_builtin_fixture!(
        Set_prototype_constructor_set_prototype_constructor,
        "Set/prototype/constructor/set-prototype-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_delete_entry_initial_iterable,
        "Set/prototype/delete/delete-entry-initial-iterable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_delete_entry_normalizes_zero,
        "Set/prototype/delete/delete-entry-normalizes-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_delete_entry,
        "Set/prototype/delete/delete-entry.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_delete,
        "Set/prototype/delete/delete.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_does_not_have_setdata_internal_slot_array,
        "Set/prototype/delete/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_does_not_have_setdata_internal_slot_map,
        "Set/prototype/delete/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_does_not_have_setdata_internal_slot_object,
        "Set/prototype/delete/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/delete/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/delete/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_length,
        "Set/prototype/delete/length.js"
    );
    test262_builtin_fixture!(Set_prototype_delete_name, "Set/prototype/delete/name.js");
    test262_builtin_fixture!(
        Set_prototype_delete_not_a_constructor,
        "Set/prototype/delete/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_returns_false_when_delete_is_noop,
        "Set/prototype/delete/returns-false-when-delete-is-noop.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_returns_true_when_delete_operation_occurs,
        "Set/prototype/delete/returns-true-when-delete-operation-occurs.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_boolean,
        "Set/prototype/delete/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_null,
        "Set/prototype/delete/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_number,
        "Set/prototype/delete/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_string,
        "Set/prototype/delete/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_symbol,
        "Set/prototype/delete/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_delete_this_not_object_throw_undefined,
        "Set/prototype/delete/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_add_not_called,
        "Set/prototype/difference/add-not-called.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_allows_set_like_class,
        "Set/prototype/difference/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_allows_set_like_object,
        "Set/prototype/difference/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_array_throws,
        "Set/prototype/difference/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_builtins,
        "Set/prototype/difference/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_called_with_object,
        "Set/prototype/difference/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_combines_empty_sets,
        "Set/prototype/difference/combines-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_combines_itself,
        "Set/prototype/difference/combines-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_combines_Map,
        "Set/prototype/difference/combines-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_combines_same_sets,
        "Set/prototype/difference/combines-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_combines_sets,
        "Set/prototype/difference/combines-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_converts_negative_zero,
        "Set/prototype/difference/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_difference,
        "Set/prototype/difference/difference.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_has_is_callable,
        "Set/prototype/difference/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_keys_is_callable,
        "Set/prototype/difference/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_length,
        "Set/prototype/difference/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_name,
        "Set/prototype/difference/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_not_a_constructor,
        "Set/prototype/difference/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_receiver_not_set,
        "Set/prototype/difference/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_require_internal_slot,
        "Set/prototype/difference/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_result_order,
        "Set/prototype/difference/result-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_set_like_array,
        "Set/prototype/difference/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_set_like_class_mutation,
        "Set/prototype/difference/set-like-class-mutation.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_set_like_class_order,
        "Set/prototype/difference/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_size_is_a_number,
        "Set/prototype/difference/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_subclass_receiver_methods,
        "Set/prototype/difference/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_subclass_symbol_species,
        "Set/prototype/difference/subclass-symbol-species.js"
    );
    test262_builtin_fixture!(
        Set_prototype_difference_subclass,
        "Set/prototype/difference/subclass.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_does_not_have_setdata_internal_slot_array,
        "Set/prototype/entries/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_does_not_have_setdata_internal_slot_map,
        "Set/prototype/entries/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_does_not_have_setdata_internal_slot_object,
        "Set/prototype/entries/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/entries/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/entries/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_entries,
        "Set/prototype/entries/entries.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_length,
        "Set/prototype/entries/length.js"
    );
    test262_builtin_fixture!(Set_prototype_entries_name, "Set/prototype/entries/name.js");
    test262_builtin_fixture!(
        Set_prototype_entries_not_a_constructor,
        "Set/prototype/entries/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_returns_iterator_empty,
        "Set/prototype/entries/returns-iterator-empty.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_returns_iterator,
        "Set/prototype/entries/returns-iterator.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_boolean,
        "Set/prototype/entries/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_null,
        "Set/prototype/entries/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_number,
        "Set/prototype/entries/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_string,
        "Set/prototype/entries/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_symbol,
        "Set/prototype/entries/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_entries_this_not_object_throw_undefined,
        "Set/prototype/entries/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_boolean,
        "Set/prototype/forEach/callback-not-callable-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_null,
        "Set/prototype/forEach/callback-not-callable-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_number,
        "Set/prototype/forEach/callback-not-callable-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_string,
        "Set/prototype/forEach/callback-not-callable-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_symbol,
        "Set/prototype/forEach/callback-not-callable-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_callback_not_callable_undefined,
        "Set/prototype/forEach/callback-not-callable-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_does_not_have_setdata_internal_slot_array,
        "Set/prototype/forEach/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_does_not_have_setdata_internal_slot_map,
        "Set/prototype/forEach/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_does_not_have_setdata_internal_slot_object,
        "Set/prototype/forEach/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/forEach/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/forEach/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_forEach,
        "Set/prototype/forEach/forEach.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_in_insertion_order,
        "Set/prototype/forEach/iterates-in-insertion-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_in_iterable_entry_order,
        "Set/prototype/forEach/iterates-in-iterable-entry-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_values_added_after_foreach_begins,
        "Set/prototype/forEach/iterates-values-added-after-foreach-begins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_values_deleted_then_readded,
        "Set/prototype/forEach/iterates-values-deleted-then-readded.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_values_not_deleted,
        "Set/prototype/forEach/iterates-values-not-deleted.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_iterates_values_revisits_after_delete_re_add,
        "Set/prototype/forEach/iterates-values-revisits-after-delete-re-add.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_length,
        "Set/prototype/forEach/length.js"
    );
    test262_builtin_fixture!(Set_prototype_forEach_name, "Set/prototype/forEach/name.js");
    test262_builtin_fixture!(
        Set_prototype_forEach_not_a_constructor,
        "Set/prototype/forEach/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_returns_undefined,
        "Set/prototype/forEach/returns-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_arg_explicit_cannot_override_lexical_this_arrow,
        "Set/prototype/forEach/this-arg-explicit-cannot-override-lexical-this-arrow.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_arg_explicit,
        "Set/prototype/forEach/this-arg-explicit.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_non_strict,
        "Set/prototype/forEach/this-non-strict.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_boolean,
        "Set/prototype/forEach/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_null,
        "Set/prototype/forEach/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_number,
        "Set/prototype/forEach/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_string,
        "Set/prototype/forEach/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_symbol,
        "Set/prototype/forEach/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_not_object_throw_undefined,
        "Set/prototype/forEach/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_this_strict,
        "Set/prototype/forEach/this-strict.js"
    );
    test262_builtin_fixture!(
        Set_prototype_forEach_throws_when_callback_throws,
        "Set/prototype/forEach/throws-when-callback-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_does_not_have_setdata_internal_slot_array,
        "Set/prototype/has/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_does_not_have_setdata_internal_slot_map,
        "Set/prototype/has/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_does_not_have_setdata_internal_slot_object,
        "Set/prototype/has/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/has/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/has/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(Set_prototype_has_has, "Set/prototype/has/has.js");
    test262_builtin_fixture!(Set_prototype_has_length, "Set/prototype/has/length.js");
    test262_builtin_fixture!(Set_prototype_has_name, "Set/prototype/has/name.js");
    test262_builtin_fixture!(
        Set_prototype_has_not_a_constructor,
        "Set/prototype/has/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_undefined_added_deleted_not_present_undefined,
        "Set/prototype/has/returns-false-when-undefined-added-deleted-not-present-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_boolean,
        "Set/prototype/has/returns-false-when-value-not-present-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_nan,
        "Set/prototype/has/returns-false-when-value-not-present-nan.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_null,
        "Set/prototype/has/returns-false-when-value-not-present-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_number,
        "Set/prototype/has/returns-false-when-value-not-present-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_string,
        "Set/prototype/has/returns-false-when-value-not-present-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_symbol,
        "Set/prototype/has/returns-false-when-value-not-present-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_false_when_value_not_present_undefined,
        "Set/prototype/has/returns-false-when-value-not-present-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_boolean,
        "Set/prototype/has/returns-true-when-value-present-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_nan,
        "Set/prototype/has/returns-true-when-value-present-nan.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_null,
        "Set/prototype/has/returns-true-when-value-present-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_number,
        "Set/prototype/has/returns-true-when-value-present-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_string,
        "Set/prototype/has/returns-true-when-value-present-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_symbol,
        "Set/prototype/has/returns-true-when-value-present-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_returns_true_when_value_present_undefined,
        "Set/prototype/has/returns-true-when-value-present-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_boolean,
        "Set/prototype/has/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_null,
        "Set/prototype/has/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_number,
        "Set/prototype/has/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_string,
        "Set/prototype/has/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_symbol,
        "Set/prototype/has/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_has_this_not_object_throw_undefined,
        "Set/prototype/has/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_add_not_called,
        "Set/prototype/intersection/add-not-called.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_allows_set_like_class,
        "Set/prototype/intersection/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_allows_set_like_object,
        "Set/prototype/intersection/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_array_throws,
        "Set/prototype/intersection/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_builtins,
        "Set/prototype/intersection/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_called_with_object,
        "Set/prototype/intersection/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_combines_empty_sets,
        "Set/prototype/intersection/combines-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_combines_itself,
        "Set/prototype/intersection/combines-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_combines_Map,
        "Set/prototype/intersection/combines-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_combines_same_sets,
        "Set/prototype/intersection/combines-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_combines_sets,
        "Set/prototype/intersection/combines-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_converts_negative_zero,
        "Set/prototype/intersection/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_has_is_callable,
        "Set/prototype/intersection/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_intersection,
        "Set/prototype/intersection/intersection.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_keys_is_callable,
        "Set/prototype/intersection/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_length,
        "Set/prototype/intersection/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_name,
        "Set/prototype/intersection/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_not_a_constructor,
        "Set/prototype/intersection/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_receiver_not_set,
        "Set/prototype/intersection/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_require_internal_slot,
        "Set/prototype/intersection/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_result_order,
        "Set/prototype/intersection/result-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_set_like_array,
        "Set/prototype/intersection/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_set_like_class_mutation,
        "Set/prototype/intersection/set-like-class-mutation.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_set_like_class_order,
        "Set/prototype/intersection/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_size_is_a_number,
        "Set/prototype/intersection/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_subclass_receiver_methods,
        "Set/prototype/intersection/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_subclass_symbol_species,
        "Set/prototype/intersection/subclass-symbol-species.js"
    );
    test262_builtin_fixture!(
        Set_prototype_intersection_subclass,
        "Set/prototype/intersection/subclass.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_allows_set_like_class,
        "Set/prototype/isDisjointFrom/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_allows_set_like_object,
        "Set/prototype/isDisjointFrom/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_array_throws,
        "Set/prototype/isDisjointFrom/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_builtins,
        "Set/prototype/isDisjointFrom/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_called_with_object,
        "Set/prototype/isDisjointFrom/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_compares_empty_sets,
        "Set/prototype/isDisjointFrom/compares-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_compares_itself,
        "Set/prototype/isDisjointFrom/compares-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_compares_Map,
        "Set/prototype/isDisjointFrom/compares-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_compares_same_sets,
        "Set/prototype/isDisjointFrom/compares-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_compares_sets,
        "Set/prototype/isDisjointFrom/compares-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_converts_negative_zero,
        "Set/prototype/isDisjointFrom/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_has_is_callable,
        "Set/prototype/isDisjointFrom/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_isDisjointFrom,
        "Set/prototype/isDisjointFrom/isDisjointFrom.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_keys_is_callable,
        "Set/prototype/isDisjointFrom/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_length,
        "Set/prototype/isDisjointFrom/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_name,
        "Set/prototype/isDisjointFrom/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_not_a_constructor,
        "Set/prototype/isDisjointFrom/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_receiver_not_set,
        "Set/prototype/isDisjointFrom/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_require_internal_slot,
        "Set/prototype/isDisjointFrom/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_set_like_array,
        "Set/prototype/isDisjointFrom/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_set_like_class_order,
        "Set/prototype/isDisjointFrom/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_set_like_iter_return,
        "Set/prototype/isDisjointFrom/set-like-iter-return.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_size_is_a_number,
        "Set/prototype/isDisjointFrom/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isDisjointFrom_subclass_receiver_methods,
        "Set/prototype/isDisjointFrom/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_allows_set_like_class,
        "Set/prototype/isSubsetOf/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_allows_set_like_object,
        "Set/prototype/isSubsetOf/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_array_throws,
        "Set/prototype/isSubsetOf/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_builtins,
        "Set/prototype/isSubsetOf/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_called_with_object,
        "Set/prototype/isSubsetOf/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_compares_empty_sets,
        "Set/prototype/isSubsetOf/compares-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_compares_itself,
        "Set/prototype/isSubsetOf/compares-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_compares_Map,
        "Set/prototype/isSubsetOf/compares-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_compares_same_sets,
        "Set/prototype/isSubsetOf/compares-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_compares_sets,
        "Set/prototype/isSubsetOf/compares-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_has_is_callable,
        "Set/prototype/isSubsetOf/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_isSubsetOf,
        "Set/prototype/isSubsetOf/isSubsetOf.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_keys_is_callable,
        "Set/prototype/isSubsetOf/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_length,
        "Set/prototype/isSubsetOf/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_name,
        "Set/prototype/isSubsetOf/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_not_a_constructor,
        "Set/prototype/isSubsetOf/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_receiver_not_set,
        "Set/prototype/isSubsetOf/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_require_internal_slot,
        "Set/prototype/isSubsetOf/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_set_like_array,
        "Set/prototype/isSubsetOf/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_set_like_class_order,
        "Set/prototype/isSubsetOf/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_size_is_a_number,
        "Set/prototype/isSubsetOf/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSubsetOf_subclass_receiver_methods,
        "Set/prototype/isSubsetOf/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_allows_set_like_class,
        "Set/prototype/isSupersetOf/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_allows_set_like_object,
        "Set/prototype/isSupersetOf/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_array_throws,
        "Set/prototype/isSupersetOf/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_builtins,
        "Set/prototype/isSupersetOf/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_called_with_object,
        "Set/prototype/isSupersetOf/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_compares_empty_sets,
        "Set/prototype/isSupersetOf/compares-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_compares_itself,
        "Set/prototype/isSupersetOf/compares-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_compares_Map,
        "Set/prototype/isSupersetOf/compares-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_compares_same_sets,
        "Set/prototype/isSupersetOf/compares-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_compares_sets,
        "Set/prototype/isSupersetOf/compares-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_converts_negative_zero,
        "Set/prototype/isSupersetOf/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_has_is_callable,
        "Set/prototype/isSupersetOf/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_isSupersetOf,
        "Set/prototype/isSupersetOf/isSupersetOf.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_keys_is_callable,
        "Set/prototype/isSupersetOf/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_length,
        "Set/prototype/isSupersetOf/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_name,
        "Set/prototype/isSupersetOf/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_not_a_constructor,
        "Set/prototype/isSupersetOf/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_receiver_not_set,
        "Set/prototype/isSupersetOf/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_require_internal_slot,
        "Set/prototype/isSupersetOf/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_set_like_array,
        "Set/prototype/isSupersetOf/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_set_like_class_mutation,
        "Set/prototype/isSupersetOf/set-like-class-mutation.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_set_like_class_order,
        "Set/prototype/isSupersetOf/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_set_like_iter_return,
        "Set/prototype/isSupersetOf/set-like-iter-return.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_size_is_a_number,
        "Set/prototype/isSupersetOf/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_isSupersetOf_subclass_receiver_methods,
        "Set/prototype/isSupersetOf/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(Set_prototype_keys_keys, "Set/prototype/keys/keys.js");
    test262_builtin_fixture!(Set_prototype_size_length, "Set/prototype/size/length.js");
    test262_builtin_fixture!(Set_prototype_size_name, "Set/prototype/size/name.js");
    test262_builtin_fixture!(
        Set_prototype_size_returns_count_of_present_values_before_after_add_delete,
        "Set/prototype/size/returns-count-of-present-values-before-after-add-delete.js"
    );
    test262_builtin_fixture!(
        Set_prototype_size_returns_count_of_present_values_by_insertion,
        "Set/prototype/size/returns-count-of-present-values-by-insertion.js"
    );
    test262_builtin_fixture!(
        Set_prototype_size_returns_count_of_present_values_by_iterable,
        "Set/prototype/size/returns-count-of-present-values-by-iterable.js"
    );
    test262_builtin_fixture!(Set_prototype_size_size, "Set/prototype/size/size.js");
    test262_builtin_fixture!(
        Set_prototype_Symbol_iterator_not_a_constructor,
        "Set/prototype/Symbol.iterator/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_Symbol_iterator,
        "Set/prototype/Symbol.iterator.js"
    );
    test262_builtin_fixture!(
        Set_prototype_Symbol_toStringTag_property_descriptor,
        "Set/prototype/Symbol.toStringTag/property-descriptor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_Symbol_toStringTag,
        "Set/prototype/Symbol.toStringTag.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_add_not_called,
        "Set/prototype/symmetricDifference/add-not-called.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_allows_set_like_class,
        "Set/prototype/symmetricDifference/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_allows_set_like_object,
        "Set/prototype/symmetricDifference/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_array_throws,
        "Set/prototype/symmetricDifference/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_builtins,
        "Set/prototype/symmetricDifference/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_called_with_object,
        "Set/prototype/symmetricDifference/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_combines_empty_sets,
        "Set/prototype/symmetricDifference/combines-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_combines_itself,
        "Set/prototype/symmetricDifference/combines-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_combines_Map,
        "Set/prototype/symmetricDifference/combines-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_combines_same_sets,
        "Set/prototype/symmetricDifference/combines-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_combines_sets,
        "Set/prototype/symmetricDifference/combines-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_converts_negative_zero,
        "Set/prototype/symmetricDifference/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_has_is_callable,
        "Set/prototype/symmetricDifference/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_keys_is_callable,
        "Set/prototype/symmetricDifference/keys-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_length,
        "Set/prototype/symmetricDifference/length.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_name,
        "Set/prototype/symmetricDifference/name.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_not_a_constructor,
        "Set/prototype/symmetricDifference/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_receiver_not_set,
        "Set/prototype/symmetricDifference/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_require_internal_slot,
        "Set/prototype/symmetricDifference/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_result_order,
        "Set/prototype/symmetricDifference/result-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_set_like_array,
        "Set/prototype/symmetricDifference/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_set_like_class_order,
        "Set/prototype/symmetricDifference/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_size_is_a_number,
        "Set/prototype/symmetricDifference/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_subclass_receiver_methods,
        "Set/prototype/symmetricDifference/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_subclass_symbol_species,
        "Set/prototype/symmetricDifference/subclass-symbol-species.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_subclass,
        "Set/prototype/symmetricDifference/subclass.js"
    );
    test262_builtin_fixture!(
        Set_prototype_symmetricDifference_symmetricDifference,
        "Set/prototype/symmetricDifference/symmetricDifference.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_add_not_called,
        "Set/prototype/union/add-not-called.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_allows_set_like_class,
        "Set/prototype/union/allows-set-like-class.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_allows_set_like_object,
        "Set/prototype/union/allows-set-like-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_appends_new_values,
        "Set/prototype/union/appends-new-values.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_array_throws,
        "Set/prototype/union/array-throws.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_builtins,
        "Set/prototype/union/builtins.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_called_with_object,
        "Set/prototype/union/called-with-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_combines_empty_sets,
        "Set/prototype/union/combines-empty-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_combines_itself,
        "Set/prototype/union/combines-itself.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_combines_Map,
        "Set/prototype/union/combines-Map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_combines_same_sets,
        "Set/prototype/union/combines-same-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_combines_sets,
        "Set/prototype/union/combines-sets.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_converts_negative_zero,
        "Set/prototype/union/converts-negative-zero.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_has_is_callable,
        "Set/prototype/union/has-is-callable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_keys_is_callable,
        "Set/prototype/union/keys-is-callable.js"
    );
    test262_builtin_fixture!(Set_prototype_union_length, "Set/prototype/union/length.js");
    test262_builtin_fixture!(Set_prototype_union_name, "Set/prototype/union/name.js");
    test262_builtin_fixture!(
        Set_prototype_union_not_a_constructor,
        "Set/prototype/union/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_receiver_not_set,
        "Set/prototype/union/receiver-not-set.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_require_internal_slot,
        "Set/prototype/union/require-internal-slot.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_result_order,
        "Set/prototype/union/result-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_set_like_array,
        "Set/prototype/union/set-like-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_set_like_class_mutation,
        "Set/prototype/union/set-like-class-mutation.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_set_like_class_order,
        "Set/prototype/union/set-like-class-order.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_size_is_a_number,
        "Set/prototype/union/size-is-a-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_subclass_receiver_methods,
        "Set/prototype/union/subclass-receiver-methods.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_subclass_symbol_species,
        "Set/prototype/union/subclass-symbol-species.js"
    );
    test262_builtin_fixture!(
        Set_prototype_union_subclass,
        "Set/prototype/union/subclass.js"
    );
    test262_builtin_fixture!(Set_prototype_union_union, "Set/prototype/union/union.js");
    test262_builtin_fixture!(
        Set_prototype_values_does_not_have_setdata_internal_slot_array,
        "Set/prototype/values/does-not-have-setdata-internal-slot-array.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_does_not_have_setdata_internal_slot_map,
        "Set/prototype/values/does-not-have-setdata-internal-slot-map.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_does_not_have_setdata_internal_slot_object,
        "Set/prototype/values/does-not-have-setdata-internal-slot-object.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_does_not_have_setdata_internal_slot_set_prototype,
        "Set/prototype/values/does-not-have-setdata-internal-slot-set-prototype.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_does_not_have_setdata_internal_slot_weakset,
        "Set/prototype/values/does-not-have-setdata-internal-slot-weakset.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_length,
        "Set/prototype/values/length.js"
    );
    test262_builtin_fixture!(Set_prototype_values_name, "Set/prototype/values/name.js");
    test262_builtin_fixture!(
        Set_prototype_values_not_a_constructor,
        "Set/prototype/values/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_returns_iterator_empty,
        "Set/prototype/values/returns-iterator-empty.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_returns_iterator,
        "Set/prototype/values/returns-iterator.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_boolean,
        "Set/prototype/values/this-not-object-throw-boolean.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_null,
        "Set/prototype/values/this-not-object-throw-null.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_number,
        "Set/prototype/values/this-not-object-throw-number.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_string,
        "Set/prototype/values/this-not-object-throw-string.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_symbol,
        "Set/prototype/values/this-not-object-throw-symbol.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_this_not_object_throw_undefined,
        "Set/prototype/values/this-not-object-throw-undefined.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_values_iteration_mutable,
        "Set/prototype/values/values-iteration-mutable.js"
    );
    test262_builtin_fixture!(
        Set_prototype_values_values,
        "Set/prototype/values/values.js"
    );
    test262_builtin_fixture!(Set_prototype_of_set, "Set/prototype-of-set.js");
    test262_builtin_fixture!(
        Set_set_does_not_throw_when_add_is_not_callable,
        "Set/set-does-not-throw-when-add-is-not-callable.js"
    );
    test262_builtin_fixture!(
        Set_set_get_add_method_failure,
        "Set/set-get-add-method-failure.js"
    );
    test262_builtin_fixture!(Set_set_iterable_calls_add, "Set/set-iterable-calls-add.js");
    test262_builtin_fixture!(
        Set_set_iterable_empty_does_not_call_add,
        "Set/set-iterable-empty-does-not-call-add.js"
    );
    test262_builtin_fixture!(
        Set_set_iterable_throws_when_add_is_not_callable,
        "Set/set-iterable-throws-when-add-is-not-callable.js"
    );
    test262_builtin_fixture!(Set_set_iterable, "Set/set-iterable.js");
    test262_builtin_fixture!(
        Set_set_iterator_close_after_add_failure,
        "Set/set-iterator-close-after-add-failure.js"
    );
    test262_builtin_fixture!(
        Set_set_iterator_next_failure,
        "Set/set-iterator-next-failure.js"
    );
    test262_builtin_fixture!(
        Set_set_iterator_value_failure,
        "Set/set-iterator-value-failure.js"
    );
    test262_builtin_fixture!(Set_set_newtarget, "Set/set-newtarget.js");
    test262_builtin_fixture!(Set_set_no_iterable, "Set/set-no-iterable.js");
    test262_builtin_fixture!(
        Set_set_undefined_newtarget,
        "Set/set-undefined-newtarget.js"
    );
    test262_builtin_fixture!(Set_set, "Set/set.js");
    test262_builtin_fixture!(Set_Symbol_species_length, "Set/Symbol.species/length.js");
    test262_builtin_fixture!(
        Set_Symbol_species_return_value,
        "Set/Symbol.species/return-value.js"
    );
    test262_builtin_fixture!(
        Set_Symbol_species_symbol_species_name,
        "Set/Symbol.species/symbol-species-name.js"
    );
    test262_builtin_fixture!(
        Set_Symbol_species_symbol_species,
        "Set/Symbol.species/symbol-species.js"
    );
    test262_builtin_fixture!(Set_valid_values, "Set/valid-values.js");
    // Phase 14 structured-data surface (ArrayBuffer/SharedArrayBuffer/
    // DataView/Atomics/JSON; the list was produced by the scanner, so it is
    // data, not aspiration).
    test262_builtin_fixture!(Atomics_add_descriptor, "Atomics/add/descriptor.js");
    test262_builtin_fixture!(
        Atomics_add_expected_return_value,
        "Atomics/add/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_add_length, "Atomics/add/length.js");
    test262_builtin_fixture!(Atomics_add_name, "Atomics/add/name.js");
    test262_builtin_fixture!(Atomics_add_non_views, "Atomics/add/non-views.js");
    test262_builtin_fixture!(
        Atomics_add_not_a_constructor,
        "Atomics/add/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_add_validate_arraytype_before_index_coercion,
        "Atomics/add/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_add_validate_arraytype_before_value_coercion,
        "Atomics/add/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(Atomics_and_descriptor, "Atomics/and/descriptor.js");
    test262_builtin_fixture!(
        Atomics_and_expected_return_value,
        "Atomics/and/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_and_length, "Atomics/and/length.js");
    test262_builtin_fixture!(Atomics_and_name, "Atomics/and/name.js");
    test262_builtin_fixture!(Atomics_and_non_views, "Atomics/and/non-views.js");
    test262_builtin_fixture!(
        Atomics_and_not_a_constructor,
        "Atomics/and/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_and_validate_arraytype_before_index_coercion,
        "Atomics/and/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_and_validate_arraytype_before_value_coercion,
        "Atomics/and/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_descriptor,
        "Atomics/compareExchange/descriptor.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_expected_return_value,
        "Atomics/compareExchange/expected-return-value.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_length,
        "Atomics/compareExchange/length.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_name,
        "Atomics/compareExchange/name.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_non_views,
        "Atomics/compareExchange/non-views.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_not_a_constructor,
        "Atomics/compareExchange/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_validate_arraytype_before_expectedValue_coercion,
        "Atomics/compareExchange/validate-arraytype-before-expectedValue-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_validate_arraytype_before_index_coercion,
        "Atomics/compareExchange/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_compareExchange_validate_arraytype_before_replacementValue_coercion,
        "Atomics/compareExchange/validate-arraytype-before-replacementValue-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_exchange_descriptor,
        "Atomics/exchange/descriptor.js"
    );
    test262_builtin_fixture!(
        Atomics_exchange_expected_return_value,
        "Atomics/exchange/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_exchange_length, "Atomics/exchange/length.js");
    test262_builtin_fixture!(Atomics_exchange_name, "Atomics/exchange/name.js");
    test262_builtin_fixture!(Atomics_exchange_non_views, "Atomics/exchange/non-views.js");
    test262_builtin_fixture!(
        Atomics_exchange_not_a_constructor,
        "Atomics/exchange/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_exchange_validate_arraytype_before_index_coercion,
        "Atomics/exchange/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_exchange_validate_arraytype_before_value_coercion,
        "Atomics/exchange/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_isLockFree_bigint_expected_return_value,
        "Atomics/isLockFree/bigint/expected-return-value.js"
    );
    test262_builtin_fixture!(
        Atomics_isLockFree_descriptor,
        "Atomics/isLockFree/descriptor.js"
    );
    test262_builtin_fixture!(
        Atomics_isLockFree_expected_return_value,
        "Atomics/isLockFree/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_isLockFree_length, "Atomics/isLockFree/length.js");
    test262_builtin_fixture!(Atomics_isLockFree_name, "Atomics/isLockFree/name.js");
    test262_builtin_fixture!(
        Atomics_isLockFree_not_a_constructor,
        "Atomics/isLockFree/not-a-constructor.js"
    );
    test262_builtin_fixture!(Atomics_load_descriptor, "Atomics/load/descriptor.js");
    test262_builtin_fixture!(
        Atomics_load_expected_return_value,
        "Atomics/load/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_load_length, "Atomics/load/length.js");
    test262_builtin_fixture!(Atomics_load_name, "Atomics/load/name.js");
    test262_builtin_fixture!(Atomics_load_non_views, "Atomics/load/non-views.js");
    test262_builtin_fixture!(
        Atomics_load_not_a_constructor,
        "Atomics/load/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_load_validate_arraytype_before_index_coercion,
        "Atomics/load/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_bigint_non_bigint64_typedarray_throws,
        "Atomics/notify/bigint/non-bigint64-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_bigint_non_shared_bufferdata_non_shared_int_views_throws,
        "Atomics/notify/bigint/non-shared-bufferdata-non-shared-int-views-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_bigint_null_bufferdata_throws,
        "Atomics/notify/bigint/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_count_boundary_cases,
        "Atomics/notify/count-boundary-cases.js"
    );
    test262_builtin_fixture!(Atomics_notify_descriptor, "Atomics/notify/descriptor.js");
    test262_builtin_fixture!(Atomics_notify_length, "Atomics/notify/length.js");
    test262_builtin_fixture!(Atomics_notify_name, "Atomics/notify/name.js");
    test262_builtin_fixture!(
        Atomics_notify_negative_index_throws,
        "Atomics/notify/negative-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_non_int32_typedarray_throws,
        "Atomics/notify/non-int32-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_non_shared_bufferdata_non_shared_int_views_throws,
        "Atomics/notify/non-shared-bufferdata-non-shared-int-views-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_non_shared_int_views,
        "Atomics/notify/non-shared-int-views.js"
    );
    test262_builtin_fixture!(Atomics_notify_non_views, "Atomics/notify/non-views.js");
    test262_builtin_fixture!(
        Atomics_notify_not_a_constructor,
        "Atomics/notify/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_not_a_typedarray_throws,
        "Atomics/notify/not-a-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_not_an_object_throws,
        "Atomics/notify/not-an-object-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_null_bufferdata_throws,
        "Atomics/notify/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_out_of_range_index_throws,
        "Atomics/notify/out-of-range-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_validate_arraytype_before_count_coercion,
        "Atomics/notify/validate-arraytype-before-count-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_notify_validate_arraytype_before_index_coercion,
        "Atomics/notify/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(Atomics_or_descriptor, "Atomics/or/descriptor.js");
    test262_builtin_fixture!(
        Atomics_or_expected_return_value,
        "Atomics/or/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_or_length, "Atomics/or/length.js");
    test262_builtin_fixture!(Atomics_or_name, "Atomics/or/name.js");
    test262_builtin_fixture!(Atomics_or_non_views, "Atomics/or/non-views.js");
    test262_builtin_fixture!(
        Atomics_or_not_a_constructor,
        "Atomics/or/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_or_validate_arraytype_before_index_coercion,
        "Atomics/or/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_or_validate_arraytype_before_value_coercion,
        "Atomics/or/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(Atomics_pause_descriptor, "Atomics/pause/descriptor.js");
    test262_builtin_fixture!(Atomics_pause_name, "Atomics/pause/name.js");
    test262_builtin_fixture!(
        Atomics_pause_not_a_constructor,
        "Atomics/pause/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_pause_returns_undefined,
        "Atomics/pause/returns-undefined.js"
    );
    test262_builtin_fixture!(Atomics_prop_desc, "Atomics/prop-desc.js");
    test262_builtin_fixture!(Atomics_proto, "Atomics/proto.js");
    test262_builtin_fixture!(Atomics_store_descriptor, "Atomics/store/descriptor.js");
    test262_builtin_fixture!(
        Atomics_store_expected_return_value,
        "Atomics/store/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_store_length, "Atomics/store/length.js");
    test262_builtin_fixture!(Atomics_store_name, "Atomics/store/name.js");
    test262_builtin_fixture!(Atomics_store_non_views, "Atomics/store/non-views.js");
    test262_builtin_fixture!(
        Atomics_store_not_a_constructor,
        "Atomics/store/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_store_validate_arraytype_before_index_coercion,
        "Atomics/store/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_store_validate_arraytype_before_value_coercion,
        "Atomics/store/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(Atomics_sub_descriptor, "Atomics/sub/descriptor.js");
    test262_builtin_fixture!(
        Atomics_sub_expected_return_value,
        "Atomics/sub/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_sub_length, "Atomics/sub/length.js");
    test262_builtin_fixture!(Atomics_sub_name, "Atomics/sub/name.js");
    test262_builtin_fixture!(Atomics_sub_non_views, "Atomics/sub/non-views.js");
    test262_builtin_fixture!(
        Atomics_sub_not_a_constructor,
        "Atomics/sub/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_sub_validate_arraytype_before_index_coercion,
        "Atomics/sub/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_sub_validate_arraytype_before_value_coercion,
        "Atomics/sub/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(Atomics_Symbol_toStringTag, "Atomics/Symbol.toStringTag.js");
    test262_builtin_fixture!(
        Atomics_wait_bigint_cannot_suspend_throws,
        "Atomics/wait/bigint/cannot-suspend-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_bigint_negative_index_throws,
        "Atomics/wait/bigint/negative-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_bigint_non_bigint64_typedarray_throws,
        "Atomics/wait/bigint/non-bigint64-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_bigint_non_shared_bufferdata_throws,
        "Atomics/wait/bigint/non-shared-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_bigint_null_bufferdata_throws,
        "Atomics/wait/bigint/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_bigint_out_of_range_index_throws,
        "Atomics/wait/bigint/out-of-range-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_cannot_suspend_throws,
        "Atomics/wait/cannot-suspend-throws.js"
    );
    test262_builtin_fixture!(Atomics_wait_descriptor, "Atomics/wait/descriptor.js");
    test262_builtin_fixture!(Atomics_wait_length, "Atomics/wait/length.js");
    test262_builtin_fixture!(Atomics_wait_name, "Atomics/wait/name.js");
    test262_builtin_fixture!(
        Atomics_wait_negative_index_throws,
        "Atomics/wait/negative-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_non_int32_typedarray_throws,
        "Atomics/wait/non-int32-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_non_shared_bufferdata_throws,
        "Atomics/wait/non-shared-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_not_a_typedarray_throws,
        "Atomics/wait/not-a-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_not_an_object_throws,
        "Atomics/wait/not-an-object-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_null_bufferdata_throws,
        "Atomics/wait/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_out_of_range_index_throws,
        "Atomics/wait/out-of-range-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_validate_arraytype_before_index_coercion,
        "Atomics/wait/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_validate_arraytype_before_timeout_coercion,
        "Atomics/wait/validate-arraytype-before-timeout-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_wait_validate_arraytype_before_value_coercion,
        "Atomics/wait/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_negative_index_throws,
        "Atomics/waitAsync/bigint/negative-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_non_bigint64_typedarray_throws,
        "Atomics/waitAsync/bigint/non-bigint64-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_non_shared_bufferdata_throws,
        "Atomics/waitAsync/bigint/non-shared-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_not_a_typedarray_throws,
        "Atomics/waitAsync/bigint/not-a-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_not_an_object_throws,
        "Atomics/waitAsync/bigint/not-an-object-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_null_bufferdata_throws,
        "Atomics/waitAsync/bigint/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_bigint_out_of_range_index_throws,
        "Atomics/waitAsync/bigint/out-of-range-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_descriptor,
        "Atomics/waitAsync/descriptor.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_is_function,
        "Atomics/waitAsync/is-function.js"
    );
    test262_builtin_fixture!(Atomics_waitAsync_name, "Atomics/waitAsync/name.js");
    test262_builtin_fixture!(
        Atomics_waitAsync_negative_index_throws,
        "Atomics/waitAsync/negative-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_non_int32_typedarray_throws,
        "Atomics/waitAsync/non-int32-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_non_shared_bufferdata_throws,
        "Atomics/waitAsync/non-shared-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_not_a_typedarray_throws,
        "Atomics/waitAsync/not-a-typedarray-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_not_an_object_throws,
        "Atomics/waitAsync/not-an-object-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_null_bufferdata_throws,
        "Atomics/waitAsync/null-bufferdata-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_out_of_range_index_throws,
        "Atomics/waitAsync/out-of-range-index-throws.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_validate_arraytype_before_index_coercion,
        "Atomics/waitAsync/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_validate_arraytype_before_timeout_coercion,
        "Atomics/waitAsync/validate-arraytype-before-timeout-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_waitAsync_validate_arraytype_before_value_coercion,
        "Atomics/waitAsync/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(Atomics_xor_descriptor, "Atomics/xor/descriptor.js");
    test262_builtin_fixture!(
        Atomics_xor_expected_return_value,
        "Atomics/xor/expected-return-value.js"
    );
    test262_builtin_fixture!(Atomics_xor_length, "Atomics/xor/length.js");
    test262_builtin_fixture!(Atomics_xor_name, "Atomics/xor/name.js");
    test262_builtin_fixture!(Atomics_xor_non_views, "Atomics/xor/non-views.js");
    test262_builtin_fixture!(
        Atomics_xor_not_a_constructor,
        "Atomics/xor/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        Atomics_xor_validate_arraytype_before_index_coercion,
        "Atomics/xor/validate-arraytype-before-index-coercion.js"
    );
    test262_builtin_fixture!(
        Atomics_xor_validate_arraytype_before_value_coercion,
        "Atomics/xor/validate-arraytype-before-value-coercion.js"
    );
    test262_builtin_fixture!(
        DataView_buffer_does_not_have_arraybuffer_data_throws_sab,
        "DataView/buffer-does-not-have-arraybuffer-data-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_buffer_does_not_have_arraybuffer_data_throws,
        "DataView/buffer-does-not-have-arraybuffer-data-throws.js"
    );
    test262_builtin_fixture!(
        DataView_buffer_not_object_throws,
        "DataView/buffer-not-object-throws.js"
    );
    test262_builtin_fixture!(
        DataView_buffer_reference_sab,
        "DataView/buffer-reference-sab.js"
    );
    test262_builtin_fixture!(DataView_buffer_reference, "DataView/buffer-reference.js");
    test262_builtin_fixture!(
        DataView_byteoffset_is_negative_throws_sab,
        "DataView/byteoffset-is-negative-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_byteoffset_is_negative_throws,
        "DataView/byteoffset-is-negative-throws.js"
    );
    test262_builtin_fixture!(DataView_constructor, "DataView/constructor.js");
    test262_builtin_fixture!(DataView_dataview, "DataView/dataview.js");
    test262_builtin_fixture!(
        DataView_defined_bytelength_and_byteoffset_sab,
        "DataView/defined-bytelength-and-byteoffset-sab.js"
    );
    test262_builtin_fixture!(
        DataView_defined_bytelength_and_byteoffset,
        "DataView/defined-bytelength-and-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_defined_byteoffset_sab,
        "DataView/defined-byteoffset-sab.js"
    );
    test262_builtin_fixture!(
        DataView_defined_byteoffset_undefined_bytelength_sab,
        "DataView/defined-byteoffset-undefined-bytelength-sab.js"
    );
    test262_builtin_fixture!(
        DataView_defined_byteoffset_undefined_bytelength,
        "DataView/defined-byteoffset-undefined-bytelength.js"
    );
    test262_builtin_fixture!(
        DataView_defined_byteoffset,
        "DataView/defined-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_excessive_bytelength_throws_sab,
        "DataView/excessive-bytelength-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_excessive_bytelength_throws,
        "DataView/excessive-bytelength-throws.js"
    );
    test262_builtin_fixture!(
        DataView_excessive_byteoffset_throws_sab,
        "DataView/excessive-byteoffset-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_excessive_byteoffset_throws,
        "DataView/excessive-byteoffset-throws.js"
    );
    test262_builtin_fixture!(DataView_extensibility, "DataView/extensibility.js");
    test262_builtin_fixture!(
        DataView_instance_extensibility_sab,
        "DataView/instance-extensibility-sab.js"
    );
    test262_builtin_fixture!(
        DataView_instance_extensibility,
        "DataView/instance-extensibility.js"
    );
    test262_builtin_fixture!(DataView_is_a_constructor, "DataView/is-a-constructor.js");
    test262_builtin_fixture!(DataView_length, "DataView/length.js");
    test262_builtin_fixture!(DataView_name, "DataView/name.js");
    test262_builtin_fixture!(
        DataView_negative_bytelength_throws_sab,
        "DataView/negative-bytelength-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_negative_bytelength_throws,
        "DataView/negative-bytelength-throws.js"
    );
    test262_builtin_fixture!(
        DataView_negative_byteoffset_throws_sab,
        "DataView/negative-byteoffset-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_negative_byteoffset_throws,
        "DataView/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_newtarget_undefined_throws_sab,
        "DataView/newtarget-undefined-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_newtarget_undefined_throws,
        "DataView/newtarget-undefined-throws.js"
    );
    test262_builtin_fixture!(DataView_proto, "DataView/proto.js");
    test262_builtin_fixture!(
        DataView_prototype_buffer_detached_buffer,
        "DataView/prototype/buffer/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_invoked_as_accessor,
        "DataView/prototype/buffer/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_invoked_as_func,
        "DataView/prototype/buffer/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_length,
        "DataView/prototype/buffer/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_name,
        "DataView/prototype/buffer/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_prop_desc,
        "DataView/prototype/buffer/prop-desc.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_return_buffer_sab,
        "DataView/prototype/buffer/return-buffer-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_return_buffer,
        "DataView/prototype/buffer/return-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_this_has_no_dataview_internal_sab,
        "DataView/prototype/buffer/this-has-no-dataview-internal-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_this_has_no_dataview_internal,
        "DataView/prototype/buffer/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_buffer_this_is_not_object,
        "DataView/prototype/buffer/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_invoked_as_accessor,
        "DataView/prototype/byteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_invoked_as_func,
        "DataView/prototype/byteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_length,
        "DataView/prototype/byteLength/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_name,
        "DataView/prototype/byteLength/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_prop_desc,
        "DataView/prototype/byteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_return_bytelength_sab,
        "DataView/prototype/byteLength/return-bytelength-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_return_bytelength,
        "DataView/prototype/byteLength/return-bytelength.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_this_has_no_dataview_internal_sab,
        "DataView/prototype/byteLength/this-has-no-dataview-internal-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_this_has_no_dataview_internal,
        "DataView/prototype/byteLength/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteLength_this_is_not_object,
        "DataView/prototype/byteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_invoked_as_accessor,
        "DataView/prototype/byteOffset/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_invoked_as_func,
        "DataView/prototype/byteOffset/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_length,
        "DataView/prototype/byteOffset/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_name,
        "DataView/prototype/byteOffset/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_prop_desc,
        "DataView/prototype/byteOffset/prop-desc.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_return_byteoffset_sab,
        "DataView/prototype/byteOffset/return-byteoffset-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_return_byteoffset,
        "DataView/prototype/byteOffset/return-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_this_has_no_dataview_internal_sab,
        "DataView/prototype/byteOffset/this-has-no-dataview-internal-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_this_has_no_dataview_internal,
        "DataView/prototype/byteOffset/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_byteOffset_this_is_not_object,
        "DataView/prototype/byteOffset/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getBigInt64/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_detached_buffer,
        "DataView/prototype/getBigInt64/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_index_is_out_of_range,
        "DataView/prototype/getBigInt64/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_length,
        "DataView/prototype/getBigInt64/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_name,
        "DataView/prototype/getBigInt64/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_negative_byteoffset_throws,
        "DataView/prototype/getBigInt64/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_not_a_constructor,
        "DataView/prototype/getBigInt64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getBigInt64/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_return_value_clean_arraybuffer,
        "DataView/prototype/getBigInt64/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_return_values_custom_offset,
        "DataView/prototype/getBigInt64/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_return_values,
        "DataView/prototype/getBigInt64/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_this_has_no_dataview_internal,
        "DataView/prototype/getBigInt64/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_this_is_not_object,
        "DataView/prototype/getBigInt64/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_to_boolean_littleendian,
        "DataView/prototype/getBigInt64/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigInt64_toindex_byteoffset_errors,
        "DataView/prototype/getBigInt64/toindex-byteoffset-errors.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getBigUint64/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_detached_buffer,
        "DataView/prototype/getBigUint64/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_index_is_out_of_range,
        "DataView/prototype/getBigUint64/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_length,
        "DataView/prototype/getBigUint64/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_name,
        "DataView/prototype/getBigUint64/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_negative_byteoffset_throws,
        "DataView/prototype/getBigUint64/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_not_a_constructor,
        "DataView/prototype/getBigUint64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getBigUint64/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_return_value_clean_arraybuffer,
        "DataView/prototype/getBigUint64/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_return_values_custom_offset,
        "DataView/prototype/getBigUint64/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_return_values,
        "DataView/prototype/getBigUint64/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_this_has_no_dataview_internal,
        "DataView/prototype/getBigUint64/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_this_is_not_object,
        "DataView/prototype/getBigUint64/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_to_boolean_littleendian,
        "DataView/prototype/getBigUint64/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getBigUint64_toindex_byteoffset_errors,
        "DataView/prototype/getBigUint64/toindex-byteoffset-errors.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getFloat16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_detached_buffer,
        "DataView/prototype/getFloat16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_index_is_out_of_range,
        "DataView/prototype/getFloat16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_length,
        "DataView/prototype/getFloat16/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_minus_zero,
        "DataView/prototype/getFloat16/minus-zero.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_name,
        "DataView/prototype/getFloat16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_negative_byteoffset_throws,
        "DataView/prototype/getFloat16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_not_a_constructor,
        "DataView/prototype/getFloat16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getFloat16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_infinity,
        "DataView/prototype/getFloat16/return-infinity.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_nan,
        "DataView/prototype/getFloat16/return-nan.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_value_clean_arraybuffer,
        "DataView/prototype/getFloat16/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_values_custom_offset,
        "DataView/prototype/getFloat16/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_return_values,
        "DataView/prototype/getFloat16/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_this_has_no_dataview_internal,
        "DataView/prototype/getFloat16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_this_is_not_object,
        "DataView/prototype/getFloat16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat16_to_boolean_littleendian,
        "DataView/prototype/getFloat16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getFloat32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_detached_buffer,
        "DataView/prototype/getFloat32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_index_is_out_of_range,
        "DataView/prototype/getFloat32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_length,
        "DataView/prototype/getFloat32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_minus_zero,
        "DataView/prototype/getFloat32/minus-zero.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_name,
        "DataView/prototype/getFloat32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_negative_byteoffset_throws,
        "DataView/prototype/getFloat32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_not_a_constructor,
        "DataView/prototype/getFloat32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getFloat32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_infinity,
        "DataView/prototype/getFloat32/return-infinity.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_nan,
        "DataView/prototype/getFloat32/return-nan.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_value_clean_arraybuffer,
        "DataView/prototype/getFloat32/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_values_custom_offset,
        "DataView/prototype/getFloat32/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_return_values,
        "DataView/prototype/getFloat32/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_this_has_no_dataview_internal,
        "DataView/prototype/getFloat32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_this_is_not_object,
        "DataView/prototype/getFloat32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat32_to_boolean_littleendian,
        "DataView/prototype/getFloat32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getFloat64/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_detached_buffer,
        "DataView/prototype/getFloat64/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_index_is_out_of_range,
        "DataView/prototype/getFloat64/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_length,
        "DataView/prototype/getFloat64/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_minus_zero,
        "DataView/prototype/getFloat64/minus-zero.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_name,
        "DataView/prototype/getFloat64/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_negative_byteoffset_throws,
        "DataView/prototype/getFloat64/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_not_a_constructor,
        "DataView/prototype/getFloat64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getFloat64/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_infinity,
        "DataView/prototype/getFloat64/return-infinity.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_nan,
        "DataView/prototype/getFloat64/return-nan.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_value_clean_arraybuffer,
        "DataView/prototype/getFloat64/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_values_custom_offset,
        "DataView/prototype/getFloat64/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_return_values,
        "DataView/prototype/getFloat64/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_this_has_no_dataview_internal,
        "DataView/prototype/getFloat64/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_this_is_not_object,
        "DataView/prototype/getFloat64/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getFloat64_to_boolean_littleendian,
        "DataView/prototype/getFloat64/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getInt16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_detached_buffer,
        "DataView/prototype/getInt16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_index_is_out_of_range,
        "DataView/prototype/getInt16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_name,
        "DataView/prototype/getInt16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_negative_byteoffset_throws,
        "DataView/prototype/getInt16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_not_a_constructor,
        "DataView/prototype/getInt16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getInt16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_return_value_clean_arraybuffer,
        "DataView/prototype/getInt16/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_return_values_custom_offset,
        "DataView/prototype/getInt16/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_return_values,
        "DataView/prototype/getInt16/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_this_has_no_dataview_internal,
        "DataView/prototype/getInt16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_this_is_not_object,
        "DataView/prototype/getInt16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt16_to_boolean_littleendian,
        "DataView/prototype/getInt16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getInt32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_detached_buffer,
        "DataView/prototype/getInt32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_index_is_out_of_range_sab,
        "DataView/prototype/getInt32/index-is-out-of-range-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_index_is_out_of_range,
        "DataView/prototype/getInt32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_length,
        "DataView/prototype/getInt32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_name,
        "DataView/prototype/getInt32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_negative_byteoffset_throws_sab,
        "DataView/prototype/getInt32/negative-byteoffset-throws-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_negative_byteoffset_throws,
        "DataView/prototype/getInt32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_not_a_constructor,
        "DataView/prototype/getInt32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_abrupt_from_tonumber_byteoffset_symbol_sab,
        "DataView/prototype/getInt32/return-abrupt-from-tonumber-byteoffset-symbol-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getInt32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_value_clean_arraybuffer_sab,
        "DataView/prototype/getInt32/return-value-clean-arraybuffer-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_value_clean_arraybuffer,
        "DataView/prototype/getInt32/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_values_custom_offset_sab,
        "DataView/prototype/getInt32/return-values-custom-offset-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_values_custom_offset,
        "DataView/prototype/getInt32/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_values_sab,
        "DataView/prototype/getInt32/return-values-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_return_values,
        "DataView/prototype/getInt32/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_this_has_no_dataview_internal_sab,
        "DataView/prototype/getInt32/this-has-no-dataview-internal-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_this_has_no_dataview_internal,
        "DataView/prototype/getInt32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_this_is_not_object,
        "DataView/prototype/getInt32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_to_boolean_littleendian_sab,
        "DataView/prototype/getInt32/to-boolean-littleendian-sab.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt32_to_boolean_littleendian,
        "DataView/prototype/getInt32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getInt8/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_detached_buffer,
        "DataView/prototype/getInt8/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_index_is_out_of_range,
        "DataView/prototype/getInt8/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_length,
        "DataView/prototype/getInt8/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_name,
        "DataView/prototype/getInt8/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_negative_byteoffset_throws,
        "DataView/prototype/getInt8/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_not_a_constructor,
        "DataView/prototype/getInt8/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getInt8/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_return_value_clean_arraybuffer,
        "DataView/prototype/getInt8/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_return_values_custom_offset,
        "DataView/prototype/getInt8/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_return_values,
        "DataView/prototype/getInt8/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_this_has_no_dataview_internal,
        "DataView/prototype/getInt8/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getInt8_this_is_not_object,
        "DataView/prototype/getInt8/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getUint16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_detached_buffer,
        "DataView/prototype/getUint16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_index_is_out_of_range,
        "DataView/prototype/getUint16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_length,
        "DataView/prototype/getUint16/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_name,
        "DataView/prototype/getUint16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_negative_byteoffset_throws,
        "DataView/prototype/getUint16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_not_a_constructor,
        "DataView/prototype/getUint16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getUint16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_return_value_clean_arraybuffer,
        "DataView/prototype/getUint16/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_return_values_custom_offset,
        "DataView/prototype/getUint16/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_return_values,
        "DataView/prototype/getUint16/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_this_has_no_dataview_internal,
        "DataView/prototype/getUint16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_this_is_not_object,
        "DataView/prototype/getUint16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint16_to_boolean_littleendian,
        "DataView/prototype/getUint16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getUint32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_detached_buffer,
        "DataView/prototype/getUint32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_index_is_out_of_range,
        "DataView/prototype/getUint32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_length,
        "DataView/prototype/getUint32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_name,
        "DataView/prototype/getUint32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_negative_byteoffset_throws,
        "DataView/prototype/getUint32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_not_a_constructor,
        "DataView/prototype/getUint32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getUint32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_return_value_clean_arraybuffer,
        "DataView/prototype/getUint32/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_return_values_custom_offset,
        "DataView/prototype/getUint32/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_return_values,
        "DataView/prototype/getUint32/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_this_has_no_dataview_internal,
        "DataView/prototype/getUint32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_this_is_not_object,
        "DataView/prototype/getUint32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint32_to_boolean_littleendian,
        "DataView/prototype/getUint32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/getUint8/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_detached_buffer,
        "DataView/prototype/getUint8/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_index_is_out_of_range,
        "DataView/prototype/getUint8/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_length,
        "DataView/prototype/getUint8/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_name,
        "DataView/prototype/getUint8/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_negative_byteoffset_throws,
        "DataView/prototype/getUint8/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_not_a_constructor,
        "DataView/prototype/getUint8/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/getUint8/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_return_value_clean_arraybuffer,
        "DataView/prototype/getUint8/return-value-clean-arraybuffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_return_values_custom_offset,
        "DataView/prototype/getUint8/return-values-custom-offset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_return_values,
        "DataView/prototype/getUint8/return-values.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_this_has_no_dataview_internal,
        "DataView/prototype/getUint8/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_getUint8_this_is_not_object,
        "DataView/prototype/getUint8/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setBigInt64/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_detached_buffer,
        "DataView/prototype/setBigInt64/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_index_check_before_value_conversion,
        "DataView/prototype/setBigInt64/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_index_is_out_of_range,
        "DataView/prototype/setBigInt64/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_length,
        "DataView/prototype/setBigInt64/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_name,
        "DataView/prototype/setBigInt64/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_negative_byteoffset_throws,
        "DataView/prototype/setBigInt64/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_no_value_arg,
        "DataView/prototype/setBigInt64/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_not_a_constructor,
        "DataView/prototype/setBigInt64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_return_abrupt_from_tobigint_value_symbol,
        "DataView/prototype/setBigInt64/return-abrupt-from-tobigint-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setBigInt64/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_set_values_little_endian_order,
        "DataView/prototype/setBigInt64/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_this_has_no_dataview_internal,
        "DataView/prototype/setBigInt64/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_this_is_not_object,
        "DataView/prototype/setBigInt64/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigInt64_to_boolean_littleendian,
        "DataView/prototype/setBigInt64/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setBigUint64_not_a_constructor,
        "DataView/prototype/setBigUint64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setFloat16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_detached_buffer,
        "DataView/prototype/setFloat16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_index_check_before_value_conversion,
        "DataView/prototype/setFloat16/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_index_is_out_of_range,
        "DataView/prototype/setFloat16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_length,
        "DataView/prototype/setFloat16/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_name,
        "DataView/prototype/setFloat16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_negative_byteoffset_throws,
        "DataView/prototype/setFloat16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_no_value_arg,
        "DataView/prototype/setFloat16/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_not_a_constructor,
        "DataView/prototype/setFloat16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setFloat16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setFloat16/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_set_values_little_endian_order,
        "DataView/prototype/setFloat16/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_this_has_no_dataview_internal,
        "DataView/prototype/setFloat16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_this_is_not_object,
        "DataView/prototype/setFloat16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat16_to_boolean_littleendian,
        "DataView/prototype/setFloat16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setFloat32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_detached_buffer,
        "DataView/prototype/setFloat32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_index_check_before_value_conversion,
        "DataView/prototype/setFloat32/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_index_is_out_of_range,
        "DataView/prototype/setFloat32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_length,
        "DataView/prototype/setFloat32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_name,
        "DataView/prototype/setFloat32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_negative_byteoffset_throws,
        "DataView/prototype/setFloat32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_no_value_arg,
        "DataView/prototype/setFloat32/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_not_a_constructor,
        "DataView/prototype/setFloat32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setFloat32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setFloat32/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_set_values_little_endian_order,
        "DataView/prototype/setFloat32/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_this_has_no_dataview_internal,
        "DataView/prototype/setFloat32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_this_is_not_object,
        "DataView/prototype/setFloat32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat32_to_boolean_littleendian,
        "DataView/prototype/setFloat32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setFloat64/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_detached_buffer,
        "DataView/prototype/setFloat64/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_index_check_before_value_conversion,
        "DataView/prototype/setFloat64/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_index_is_out_of_range,
        "DataView/prototype/setFloat64/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_length,
        "DataView/prototype/setFloat64/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_name,
        "DataView/prototype/setFloat64/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_negative_byteoffset_throws,
        "DataView/prototype/setFloat64/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_no_value_arg,
        "DataView/prototype/setFloat64/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_not_a_constructor,
        "DataView/prototype/setFloat64/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setFloat64/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setFloat64/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_set_values_little_endian_order,
        "DataView/prototype/setFloat64/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_this_has_no_dataview_internal,
        "DataView/prototype/setFloat64/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_this_is_not_object,
        "DataView/prototype/setFloat64/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setFloat64_to_boolean_littleendian,
        "DataView/prototype/setFloat64/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setInt16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_detached_buffer,
        "DataView/prototype/setInt16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_index_check_before_value_conversion,
        "DataView/prototype/setInt16/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_index_is_out_of_range,
        "DataView/prototype/setInt16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_length,
        "DataView/prototype/setInt16/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_name,
        "DataView/prototype/setInt16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_negative_byteoffset_throws,
        "DataView/prototype/setInt16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_no_value_arg,
        "DataView/prototype/setInt16/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_not_a_constructor,
        "DataView/prototype/setInt16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setInt16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setInt16/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_set_values_little_endian_order,
        "DataView/prototype/setInt16/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_this_has_no_dataview_internal,
        "DataView/prototype/setInt16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_this_is_not_object,
        "DataView/prototype/setInt16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt16_to_boolean_littleendian,
        "DataView/prototype/setInt16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setInt32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_detached_buffer,
        "DataView/prototype/setInt32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_index_check_before_value_conversion,
        "DataView/prototype/setInt32/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_index_is_out_of_range,
        "DataView/prototype/setInt32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_length,
        "DataView/prototype/setInt32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_name,
        "DataView/prototype/setInt32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_negative_byteoffset_throws,
        "DataView/prototype/setInt32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_no_value_arg,
        "DataView/prototype/setInt32/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_not_a_constructor,
        "DataView/prototype/setInt32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setInt32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setInt32/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_set_values_little_endian_order,
        "DataView/prototype/setInt32/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_this_has_no_dataview_internal,
        "DataView/prototype/setInt32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_this_is_not_object,
        "DataView/prototype/setInt32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt32_to_boolean_littleendian,
        "DataView/prototype/setInt32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setInt8/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_detached_buffer,
        "DataView/prototype/setInt8/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_index_check_before_value_conversion,
        "DataView/prototype/setInt8/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_index_is_out_of_range,
        "DataView/prototype/setInt8/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_length,
        "DataView/prototype/setInt8/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_name,
        "DataView/prototype/setInt8/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_negative_byteoffset_throws,
        "DataView/prototype/setInt8/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_no_value_arg,
        "DataView/prototype/setInt8/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_not_a_constructor,
        "DataView/prototype/setInt8/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setInt8/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setInt8/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_this_has_no_dataview_internal,
        "DataView/prototype/setInt8/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setInt8_this_is_not_object,
        "DataView/prototype/setInt8/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setUint16/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_detached_buffer,
        "DataView/prototype/setUint16/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_index_check_before_value_conversion,
        "DataView/prototype/setUint16/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_index_is_out_of_range,
        "DataView/prototype/setUint16/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_length,
        "DataView/prototype/setUint16/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_name,
        "DataView/prototype/setUint16/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_negative_byteoffset_throws,
        "DataView/prototype/setUint16/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_no_value_arg,
        "DataView/prototype/setUint16/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_not_a_constructor,
        "DataView/prototype/setUint16/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setUint16/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setUint16/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_set_values_little_endian_order,
        "DataView/prototype/setUint16/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_this_has_no_dataview_internal,
        "DataView/prototype/setUint16/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_this_is_not_object,
        "DataView/prototype/setUint16/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint16_to_boolean_littleendian,
        "DataView/prototype/setUint16/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setUint32/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_detached_buffer,
        "DataView/prototype/setUint32/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_index_check_before_value_conversion,
        "DataView/prototype/setUint32/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_index_is_out_of_range,
        "DataView/prototype/setUint32/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_length,
        "DataView/prototype/setUint32/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_name,
        "DataView/prototype/setUint32/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_negative_byteoffset_throws,
        "DataView/prototype/setUint32/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_no_value_arg,
        "DataView/prototype/setUint32/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_not_a_constructor,
        "DataView/prototype/setUint32/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setUint32/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setUint32/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_set_values_little_endian_order,
        "DataView/prototype/setUint32/set-values-little-endian-order.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_this_has_no_dataview_internal,
        "DataView/prototype/setUint32/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_this_is_not_object,
        "DataView/prototype/setUint32/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint32_to_boolean_littleendian,
        "DataView/prototype/setUint32/to-boolean-littleendian.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_detached_buffer_before_outofrange_byteoffset,
        "DataView/prototype/setUint8/detached-buffer-before-outofrange-byteoffset.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_detached_buffer,
        "DataView/prototype/setUint8/detached-buffer.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_index_check_before_value_conversion,
        "DataView/prototype/setUint8/index-check-before-value-conversion.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_index_is_out_of_range,
        "DataView/prototype/setUint8/index-is-out-of-range.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_length,
        "DataView/prototype/setUint8/length.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_name,
        "DataView/prototype/setUint8/name.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_negative_byteoffset_throws,
        "DataView/prototype/setUint8/negative-byteoffset-throws.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_no_value_arg,
        "DataView/prototype/setUint8/no-value-arg.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_not_a_constructor,
        "DataView/prototype/setUint8/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_return_abrupt_from_tonumber_byteoffset_symbol,
        "DataView/prototype/setUint8/return-abrupt-from-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_return_abrupt_from_tonumber_value_symbol,
        "DataView/prototype/setUint8/return-abrupt-from-tonumber-value-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_this_has_no_dataview_internal,
        "DataView/prototype/setUint8/this-has-no-dataview-internal.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_setUint8_this_is_not_object,
        "DataView/prototype/setUint8/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        DataView_prototype_Symbol_toStringTag,
        "DataView/prototype/Symbol.toStringTag.js"
    );
    test262_builtin_fixture!(DataView_prototype, "DataView/prototype.js");
    test262_builtin_fixture!(
        DataView_return_abrupt_tonumber_bytelength_symbol_sab,
        "DataView/return-abrupt-tonumber-bytelength-symbol-sab.js"
    );
    test262_builtin_fixture!(
        DataView_return_abrupt_tonumber_bytelength_symbol,
        "DataView/return-abrupt-tonumber-bytelength-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_return_abrupt_tonumber_byteoffset_symbol_sab,
        "DataView/return-abrupt-tonumber-byteoffset-symbol-sab.js"
    );
    test262_builtin_fixture!(
        DataView_return_abrupt_tonumber_byteoffset_symbol,
        "DataView/return-abrupt-tonumber-byteoffset-symbol.js"
    );
    test262_builtin_fixture!(
        DataView_return_instance_sab,
        "DataView/return-instance-sab.js"
    );
    test262_builtin_fixture!(DataView_return_instance, "DataView/return-instance.js");
    test262_builtin_fixture!(
        ArrayBuffer_allocation_limit,
        "ArrayBuffer/allocation-limit.js"
    );
    test262_builtin_fixture!(ArrayBuffer_init_zero, "ArrayBuffer/init-zero.js");
    test262_builtin_fixture!(
        ArrayBuffer_is_a_constructor,
        "ArrayBuffer/is-a-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_has_no_viewedarraybuffer,
        "ArrayBuffer/isView/arg-has-no-viewedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_arraybuffer,
        "ArrayBuffer/isView/arg-is-arraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_dataview_buffer,
        "ArrayBuffer/isView/arg-is-dataview-buffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_dataview_constructor,
        "ArrayBuffer/isView/arg-is-dataview-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_dataview_subclass_instance,
        "ArrayBuffer/isView/arg-is-dataview-subclass-instance.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_dataview,
        "ArrayBuffer/isView/arg-is-dataview.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_not_object,
        "ArrayBuffer/isView/arg-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_arg_is_typedarray_constructor,
        "ArrayBuffer/isView/arg-is-typedarray-constructor.js"
    );
    test262_builtin_fixture!(ArrayBuffer_isView_length, "ArrayBuffer/isView/length.js");
    test262_builtin_fixture!(ArrayBuffer_isView_name, "ArrayBuffer/isView/name.js");
    test262_builtin_fixture!(ArrayBuffer_isView_no_arg, "ArrayBuffer/isView/no-arg.js");
    test262_builtin_fixture!(
        ArrayBuffer_isView_not_a_constructor,
        "ArrayBuffer/isView/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_isView_prop_desc,
        "ArrayBuffer/isView/prop-desc.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_length_is_absent,
        "ArrayBuffer/length-is-absent.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_length_is_too_large_throws,
        "ArrayBuffer/length-is-too-large-throws.js"
    );
    test262_builtin_fixture!(ArrayBuffer_length, "ArrayBuffer/length.js");
    test262_builtin_fixture!(ArrayBuffer_name, "ArrayBuffer/name.js");
    test262_builtin_fixture!(
        ArrayBuffer_negative_length_throws,
        "ArrayBuffer/negative-length-throws.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_diminuitive,
        "ArrayBuffer/options-maxbytelength-diminuitive.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_excessive,
        "ArrayBuffer/options-maxbytelength-excessive.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_negative,
        "ArrayBuffer/options-maxbytelength-negative.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_object,
        "ArrayBuffer/options-maxbytelength-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_poisoned,
        "ArrayBuffer/options-maxbytelength-poisoned.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_maxbytelength_undefined,
        "ArrayBuffer/options-maxbytelength-undefined.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_options_non_object,
        "ArrayBuffer/options-non-object.js"
    );
    test262_builtin_fixture!(ArrayBuffer_prop_desc, "ArrayBuffer/prop-desc.js");
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_detached_buffer,
        "ArrayBuffer/prototype/byteLength/detached-buffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_invoked_as_accessor,
        "ArrayBuffer/prototype/byteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_invoked_as_func,
        "ArrayBuffer/prototype/byteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_length,
        "ArrayBuffer/prototype/byteLength/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_name,
        "ArrayBuffer/prototype/byteLength/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_prop_desc,
        "ArrayBuffer/prototype/byteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_return_bytelength,
        "ArrayBuffer/prototype/byteLength/return-bytelength.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_this_has_no_typedarrayname_internal,
        "ArrayBuffer/prototype/byteLength/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_this_is_not_object,
        "ArrayBuffer/prototype/byteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_byteLength_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/byteLength/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_constructor,
        "ArrayBuffer/prototype/constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_detached_buffer_resizable,
        "ArrayBuffer/prototype/detached/detached-buffer-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_detached_buffer,
        "ArrayBuffer/prototype/detached/detached-buffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_invoked_as_accessor,
        "ArrayBuffer/prototype/detached/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_invoked_as_func,
        "ArrayBuffer/prototype/detached/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_length,
        "ArrayBuffer/prototype/detached/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_name,
        "ArrayBuffer/prototype/detached/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_prop_desc,
        "ArrayBuffer/prototype/detached/prop-desc.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_this_has_no_arraybufferdata_internal,
        "ArrayBuffer/prototype/detached/this-has-no-arraybufferdata-internal.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_this_is_not_object,
        "ArrayBuffer/prototype/detached/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_this_is_sharedarraybuffer_resizable,
        "ArrayBuffer/prototype/detached/this-is-sharedarraybuffer-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_detached_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/detached/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_detached_buffer,
        "ArrayBuffer/prototype/maxByteLength/detached-buffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_invoked_as_accessor,
        "ArrayBuffer/prototype/maxByteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_invoked_as_func,
        "ArrayBuffer/prototype/maxByteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_length,
        "ArrayBuffer/prototype/maxByteLength/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_name,
        "ArrayBuffer/prototype/maxByteLength/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_prop_desc,
        "ArrayBuffer/prototype/maxByteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_return_maxbytelength_non_resizable,
        "ArrayBuffer/prototype/maxByteLength/return-maxbytelength-non-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_return_maxbytelength_resizable,
        "ArrayBuffer/prototype/maxByteLength/return-maxbytelength-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_this_has_no_arraybufferdata_internal,
        "ArrayBuffer/prototype/maxByteLength/this-has-no-arraybufferdata-internal.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_this_is_not_object,
        "ArrayBuffer/prototype/maxByteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_maxByteLength_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/maxByteLength/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_detached_buffer,
        "ArrayBuffer/prototype/resizable/detached-buffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_invoked_as_accessor,
        "ArrayBuffer/prototype/resizable/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_invoked_as_func,
        "ArrayBuffer/prototype/resizable/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_length,
        "ArrayBuffer/prototype/resizable/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_name,
        "ArrayBuffer/prototype/resizable/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_prop_desc,
        "ArrayBuffer/prototype/resizable/prop-desc.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_return_resizable,
        "ArrayBuffer/prototype/resizable/return-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_this_has_no_arraybufferdata_internal,
        "ArrayBuffer/prototype/resizable/this-has-no-arraybufferdata-internal.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_this_is_not_object,
        "ArrayBuffer/prototype/resizable/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resizable_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/resizable/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_descriptor,
        "ArrayBuffer/prototype/resize/descriptor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_extensible,
        "ArrayBuffer/prototype/resize/extensible.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_length,
        "ArrayBuffer/prototype/resize/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_name,
        "ArrayBuffer/prototype/resize/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_new_length_excessive,
        "ArrayBuffer/prototype/resize/new-length-excessive.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_new_length_negative,
        "ArrayBuffer/prototype/resize/new-length-negative.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_nonconstructor,
        "ArrayBuffer/prototype/resize/nonconstructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_grow,
        "ArrayBuffer/prototype/resize/resize-grow.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_same_size_zero_explicit,
        "ArrayBuffer/prototype/resize/resize-same-size-zero-explicit.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_same_size_zero_implicit,
        "ArrayBuffer/prototype/resize/resize-same-size-zero-implicit.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_same_size,
        "ArrayBuffer/prototype/resize/resize-same-size.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_shrink_zero_explicit,
        "ArrayBuffer/prototype/resize/resize-shrink-zero-explicit.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_shrink_zero_implicit,
        "ArrayBuffer/prototype/resize/resize-shrink-zero-implicit.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_resize_shrink,
        "ArrayBuffer/prototype/resize/resize-shrink.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_this_is_detached,
        "ArrayBuffer/prototype/resize/this-is-detached.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_this_is_not_arraybuffer_object,
        "ArrayBuffer/prototype/resize/this-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_this_is_not_object,
        "ArrayBuffer/prototype/resize/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_this_is_not_resizable_arraybuffer_object,
        "ArrayBuffer/prototype/resize/this-is-not-resizable-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_resize_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/resize/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_context_is_not_arraybuffer_object,
        "ArrayBuffer/prototype/slice/context-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_context_is_not_object,
        "ArrayBuffer/prototype/slice/context-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_descriptor,
        "ArrayBuffer/prototype/slice/descriptor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_end_default_if_absent,
        "ArrayBuffer/prototype/slice/end-default-if-absent.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_end_default_if_undefined,
        "ArrayBuffer/prototype/slice/end-default-if-undefined.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_end_exceeds_length,
        "ArrayBuffer/prototype/slice/end-exceeds-length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_extensible,
        "ArrayBuffer/prototype/slice/extensible.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_length,
        "ArrayBuffer/prototype/slice/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_name,
        "ArrayBuffer/prototype/slice/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_negative_end,
        "ArrayBuffer/prototype/slice/negative-end.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_negative_start,
        "ArrayBuffer/prototype/slice/negative-start.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_nonconstructor,
        "ArrayBuffer/prototype/slice/nonconstructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_not_a_constructor,
        "ArrayBuffer/prototype/slice/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_constructor_is_not_object,
        "ArrayBuffer/prototype/slice/species-constructor-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_constructor_is_undefined,
        "ArrayBuffer/prototype/slice/species-constructor-is-undefined.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_is_not_constructor,
        "ArrayBuffer/prototype/slice/species-is-not-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_is_not_object,
        "ArrayBuffer/prototype/slice/species-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_is_null,
        "ArrayBuffer/prototype/slice/species-is-null.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_is_undefined,
        "ArrayBuffer/prototype/slice/species-is-undefined.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_returns_larger_arraybuffer,
        "ArrayBuffer/prototype/slice/species-returns-larger-arraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_returns_not_arraybuffer,
        "ArrayBuffer/prototype/slice/species-returns-not-arraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species_returns_smaller_arraybuffer,
        "ArrayBuffer/prototype/slice/species-returns-smaller-arraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_species,
        "ArrayBuffer/prototype/slice/species.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_start_default_if_absent,
        "ArrayBuffer/prototype/slice/start-default-if-absent.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_start_default_if_undefined,
        "ArrayBuffer/prototype/slice/start-default-if-undefined.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_start_exceeds_end,
        "ArrayBuffer/prototype/slice/start-exceeds-end.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_start_exceeds_length,
        "ArrayBuffer/prototype/slice/start-exceeds-length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/slice/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_tointeger_conversion_end,
        "ArrayBuffer/prototype/slice/tointeger-conversion-end.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_slice_tointeger_conversion_start,
        "ArrayBuffer/prototype/slice/tointeger-conversion-start.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_sliceToImmutable_not_a_constructor,
        "ArrayBuffer/prototype/sliceToImmutable/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_sliceToImmutable_this_is_not_detached,
        "ArrayBuffer/prototype/sliceToImmutable/this-is-not-detached.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_Symbol_toStringTag,
        "ArrayBuffer/prototype/Symbol.toStringTag.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_descriptor,
        "ArrayBuffer/prototype/transfer/descriptor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_extensible,
        "ArrayBuffer/prototype/transfer/extensible.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_larger_no_resizable,
        "ArrayBuffer/prototype/transfer/from-fixed-to-larger-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_larger,
        "ArrayBuffer/prototype/transfer/from-fixed-to-larger.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_same_no_resizable,
        "ArrayBuffer/prototype/transfer/from-fixed-to-same-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_same,
        "ArrayBuffer/prototype/transfer/from-fixed-to-same.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_smaller_no_resizable,
        "ArrayBuffer/prototype/transfer/from-fixed-to-smaller-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_smaller,
        "ArrayBuffer/prototype/transfer/from-fixed-to-smaller.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_zero_no_resizable,
        "ArrayBuffer/prototype/transfer/from-fixed-to-zero-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_fixed_to_zero,
        "ArrayBuffer/prototype/transfer/from-fixed-to-zero.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_resizable_to_larger,
        "ArrayBuffer/prototype/transfer/from-resizable-to-larger.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_resizable_to_same,
        "ArrayBuffer/prototype/transfer/from-resizable-to-same.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_resizable_to_smaller,
        "ArrayBuffer/prototype/transfer/from-resizable-to-smaller.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_from_resizable_to_zero,
        "ArrayBuffer/prototype/transfer/from-resizable-to-zero.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_name,
        "ArrayBuffer/prototype/transfer/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_new_length_excessive,
        "ArrayBuffer/prototype/transfer/new-length-excessive.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_nonconstructor,
        "ArrayBuffer/prototype/transfer/nonconstructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_this_is_not_arraybuffer_object,
        "ArrayBuffer/prototype/transfer/this-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_this_is_not_object,
        "ArrayBuffer/prototype/transfer/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transfer_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/transfer/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_descriptor,
        "ArrayBuffer/prototype/transferToFixedLength/descriptor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_extensible,
        "ArrayBuffer/prototype/transferToFixedLength/extensible.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_larger_no_resizable,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-larger-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_larger,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-larger.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_same_no_resizable,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-same-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_same,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-same.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_smaller_no_resizable,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-smaller-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_smaller,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-smaller.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_zero_no_resizable,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-zero-no-resizable.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_fixed_to_zero,
        "ArrayBuffer/prototype/transferToFixedLength/from-fixed-to-zero.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_resizable_to_larger,
        "ArrayBuffer/prototype/transferToFixedLength/from-resizable-to-larger.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_resizable_to_same,
        "ArrayBuffer/prototype/transferToFixedLength/from-resizable-to-same.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_resizable_to_smaller,
        "ArrayBuffer/prototype/transferToFixedLength/from-resizable-to-smaller.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_from_resizable_to_zero,
        "ArrayBuffer/prototype/transferToFixedLength/from-resizable-to-zero.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_name,
        "ArrayBuffer/prototype/transferToFixedLength/name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_new_length_excessive,
        "ArrayBuffer/prototype/transferToFixedLength/new-length-excessive.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_nonconstructor,
        "ArrayBuffer/prototype/transferToFixedLength/nonconstructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_this_is_not_arraybuffer_object,
        "ArrayBuffer/prototype/transferToFixedLength/this-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_this_is_not_object,
        "ArrayBuffer/prototype/transferToFixedLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToFixedLength_this_is_sharedarraybuffer,
        "ArrayBuffer/prototype/transferToFixedLength/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_prototype_transferToImmutable_not_a_constructor,
        "ArrayBuffer/prototype/transferToImmutable/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_return_abrupt_from_length_symbol,
        "ArrayBuffer/return-abrupt-from-length-symbol.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_Symbol_species_length,
        "ArrayBuffer/Symbol.species/length.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_Symbol_species_return_value,
        "ArrayBuffer/Symbol.species/return-value.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_Symbol_species_symbol_species_name,
        "ArrayBuffer/Symbol.species/symbol-species-name.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_Symbol_species_symbol_species,
        "ArrayBuffer/Symbol.species/symbol-species.js"
    );
    test262_builtin_fixture!(
        ArrayBuffer_undefined_newtarget_throws,
        "ArrayBuffer/undefined-newtarget-throws.js"
    );
    test262_builtin_fixture!(ArrayBuffer_zero_length, "ArrayBuffer/zero-length.js");
    test262_builtin_fixture!(
        SharedArrayBuffer_allocation_limit,
        "SharedArrayBuffer/allocation-limit.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_init_zero,
        "SharedArrayBuffer/init-zero.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_is_a_constructor,
        "SharedArrayBuffer/is-a-constructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_length_is_absent,
        "SharedArrayBuffer/length-is-absent.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_length_is_too_large_throws,
        "SharedArrayBuffer/length-is-too-large-throws.js"
    );
    test262_builtin_fixture!(SharedArrayBuffer_length, "SharedArrayBuffer/length.js");
    test262_builtin_fixture!(
        SharedArrayBuffer_negative_length_throws,
        "SharedArrayBuffer/negative-length-throws.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_diminuitive,
        "SharedArrayBuffer/options-maxbytelength-diminuitive.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_excessive,
        "SharedArrayBuffer/options-maxbytelength-excessive.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_negative,
        "SharedArrayBuffer/options-maxbytelength-negative.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_object,
        "SharedArrayBuffer/options-maxbytelength-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_poisoned,
        "SharedArrayBuffer/options-maxbytelength-poisoned.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_maxbytelength_undefined,
        "SharedArrayBuffer/options-maxbytelength-undefined.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_options_non_object,
        "SharedArrayBuffer/options-non-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_invoked_as_accessor,
        "SharedArrayBuffer/prototype/byteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_invoked_as_func,
        "SharedArrayBuffer/prototype/byteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_length,
        "SharedArrayBuffer/prototype/byteLength/length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_name,
        "SharedArrayBuffer/prototype/byteLength/name.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_prop_desc,
        "SharedArrayBuffer/prototype/byteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_return_bytelength,
        "SharedArrayBuffer/prototype/byteLength/return-bytelength.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_this_has_no_typedarrayname_internal,
        "SharedArrayBuffer/prototype/byteLength/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_this_is_arraybuffer,
        "SharedArrayBuffer/prototype/byteLength/this-is-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_byteLength_this_is_not_object,
        "SharedArrayBuffer/prototype/byteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_constructor,
        "SharedArrayBuffer/prototype/constructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_descriptor,
        "SharedArrayBuffer/prototype/grow/descriptor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_extensible,
        "SharedArrayBuffer/prototype/grow/extensible.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_grow_larger_size,
        "SharedArrayBuffer/prototype/grow/grow-larger-size.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_grow_same_size,
        "SharedArrayBuffer/prototype/grow/grow-same-size.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_grow_smaller_size,
        "SharedArrayBuffer/prototype/grow/grow-smaller-size.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_length,
        "SharedArrayBuffer/prototype/grow/length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_name,
        "SharedArrayBuffer/prototype/grow/name.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_new_length_excessive,
        "SharedArrayBuffer/prototype/grow/new-length-excessive.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_new_length_negative,
        "SharedArrayBuffer/prototype/grow/new-length-negative.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_nonconstructor,
        "SharedArrayBuffer/prototype/grow/nonconstructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_this_is_not_arraybuffer_object,
        "SharedArrayBuffer/prototype/grow/this-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_this_is_not_object,
        "SharedArrayBuffer/prototype/grow/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_this_is_not_resizable_arraybuffer_object,
        "SharedArrayBuffer/prototype/grow/this-is-not-resizable-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_grow_this_is_sharedarraybuffer,
        "SharedArrayBuffer/prototype/grow/this-is-sharedarraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_invoked_as_accessor,
        "SharedArrayBuffer/prototype/growable/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_invoked_as_func,
        "SharedArrayBuffer/prototype/growable/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_length,
        "SharedArrayBuffer/prototype/growable/length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_name,
        "SharedArrayBuffer/prototype/growable/name.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_prop_desc,
        "SharedArrayBuffer/prototype/growable/prop-desc.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_return_growable,
        "SharedArrayBuffer/prototype/growable/return-growable.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_this_has_no_arraybufferdata_internal,
        "SharedArrayBuffer/prototype/growable/this-has-no-arraybufferdata-internal.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_this_is_arraybuffer,
        "SharedArrayBuffer/prototype/growable/this-is-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_growable_this_is_not_object,
        "SharedArrayBuffer/prototype/growable/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_invoked_as_accessor,
        "SharedArrayBuffer/prototype/maxByteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_invoked_as_func,
        "SharedArrayBuffer/prototype/maxByteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_length,
        "SharedArrayBuffer/prototype/maxByteLength/length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_name,
        "SharedArrayBuffer/prototype/maxByteLength/name.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_prop_desc,
        "SharedArrayBuffer/prototype/maxByteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_return_maxbytelength_growable,
        "SharedArrayBuffer/prototype/maxByteLength/return-maxbytelength-growable.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_return_maxbytelength_non_growable,
        "SharedArrayBuffer/prototype/maxByteLength/return-maxbytelength-non-growable.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_this_has_no_arraybufferdata_internal,
        "SharedArrayBuffer/prototype/maxByteLength/this-has-no-arraybufferdata-internal.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_this_is_arraybuffer,
        "SharedArrayBuffer/prototype/maxByteLength/this-is-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_maxByteLength_this_is_not_object,
        "SharedArrayBuffer/prototype/maxByteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_prop_desc,
        "SharedArrayBuffer/prototype/prop-desc.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_context_is_not_arraybuffer_object,
        "SharedArrayBuffer/prototype/slice/context-is-not-arraybuffer-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_context_is_not_object,
        "SharedArrayBuffer/prototype/slice/context-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_descriptor,
        "SharedArrayBuffer/prototype/slice/descriptor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_end_default_if_absent,
        "SharedArrayBuffer/prototype/slice/end-default-if-absent.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_end_default_if_undefined,
        "SharedArrayBuffer/prototype/slice/end-default-if-undefined.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_end_exceeds_length,
        "SharedArrayBuffer/prototype/slice/end-exceeds-length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_extensible,
        "SharedArrayBuffer/prototype/slice/extensible.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_length,
        "SharedArrayBuffer/prototype/slice/length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_name,
        "SharedArrayBuffer/prototype/slice/name.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_negative_end,
        "SharedArrayBuffer/prototype/slice/negative-end.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_negative_start,
        "SharedArrayBuffer/prototype/slice/negative-start.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_nonconstructor,
        "SharedArrayBuffer/prototype/slice/nonconstructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_not_a_constructor,
        "SharedArrayBuffer/prototype/slice/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_constructor_is_not_object,
        "SharedArrayBuffer/prototype/slice/species-constructor-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_constructor_is_undefined,
        "SharedArrayBuffer/prototype/slice/species-constructor-is-undefined.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_is_not_constructor,
        "SharedArrayBuffer/prototype/slice/species-is-not-constructor.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_is_not_object,
        "SharedArrayBuffer/prototype/slice/species-is-not-object.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_is_null,
        "SharedArrayBuffer/prototype/slice/species-is-null.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_is_undefined,
        "SharedArrayBuffer/prototype/slice/species-is-undefined.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_returns_larger_arraybuffer,
        "SharedArrayBuffer/prototype/slice/species-returns-larger-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_returns_not_arraybuffer,
        "SharedArrayBuffer/prototype/slice/species-returns-not-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species_returns_smaller_arraybuffer,
        "SharedArrayBuffer/prototype/slice/species-returns-smaller-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_species,
        "SharedArrayBuffer/prototype/slice/species.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_start_default_if_absent,
        "SharedArrayBuffer/prototype/slice/start-default-if-absent.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_start_default_if_undefined,
        "SharedArrayBuffer/prototype/slice/start-default-if-undefined.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_start_exceeds_end,
        "SharedArrayBuffer/prototype/slice/start-exceeds-end.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_start_exceeds_length,
        "SharedArrayBuffer/prototype/slice/start-exceeds-length.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_this_is_arraybuffer,
        "SharedArrayBuffer/prototype/slice/this-is-arraybuffer.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_tointeger_conversion_end,
        "SharedArrayBuffer/prototype/slice/tointeger-conversion-end.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_slice_tointeger_conversion_start,
        "SharedArrayBuffer/prototype/slice/tointeger-conversion-start.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_prototype_Symbol_toStringTag,
        "SharedArrayBuffer/prototype/Symbol.toStringTag.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_return_abrupt_from_length_symbol,
        "SharedArrayBuffer/return-abrupt-from-length-symbol.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_undefined_newtarget_throws,
        "SharedArrayBuffer/undefined-newtarget-throws.js"
    );
    test262_builtin_fixture!(
        SharedArrayBuffer_zero_length,
        "SharedArrayBuffer/zero-length.js"
    );
    test262_builtin_fixture!(JSON_15_12_0_1, "JSON/15.12-0-1.js");
    test262_builtin_fixture!(JSON_15_12_0_2, "JSON/15.12-0-2.js");
    test262_builtin_fixture!(JSON_15_12_0_3, "JSON/15.12-0-3.js");
    test262_builtin_fixture!(JSON_15_12_0_4, "JSON/15.12-0-4.js");
    test262_builtin_fixture!(JSON_isRawJSON_basic, "JSON/isRawJSON/basic.js");
    test262_builtin_fixture!(JSON_isRawJSON_builtin, "JSON/isRawJSON/builtin.js");
    test262_builtin_fixture!(JSON_isRawJSON_length, "JSON/isRawJSON/length.js");
    test262_builtin_fixture!(JSON_isRawJSON_name, "JSON/isRawJSON/name.js");
    test262_builtin_fixture!(
        JSON_isRawJSON_not_a_constructor,
        "JSON/isRawJSON/not-a-constructor.js"
    );
    test262_builtin_fixture!(JSON_isRawJSON_prop_desc, "JSON/isRawJSON/prop-desc.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_1, "JSON/parse/15.12.1.1-0-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_2, "JSON/parse/15.12.1.1-0-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_3, "JSON/parse/15.12.1.1-0-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_4, "JSON/parse/15.12.1.1-0-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_5, "JSON/parse/15.12.1.1-0-5.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_6, "JSON/parse/15.12.1.1-0-6.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_8, "JSON/parse/15.12.1.1-0-8.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_0_9, "JSON/parse/15.12.1.1-0-9.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g1_1, "JSON/parse/15.12.1.1-g1-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g1_2, "JSON/parse/15.12.1.1-g1-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g1_3, "JSON/parse/15.12.1.1-g1-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g1_4, "JSON/parse/15.12.1.1-g1-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g2_1, "JSON/parse/15.12.1.1-g2-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g2_2, "JSON/parse/15.12.1.1-g2-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g2_3, "JSON/parse/15.12.1.1-g2-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g2_4, "JSON/parse/15.12.1.1-g2-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g2_5, "JSON/parse/15.12.1.1-g2-5.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g4_1, "JSON/parse/15.12.1.1-g4-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g4_2, "JSON/parse/15.12.1.1-g4-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g4_3, "JSON/parse/15.12.1.1-g4-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g4_4, "JSON/parse/15.12.1.1-g4-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g5_1, "JSON/parse/15.12.1.1-g5-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g5_2, "JSON/parse/15.12.1.1-g5-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g5_3, "JSON/parse/15.12.1.1-g5-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_1, "JSON/parse/15.12.1.1-g6-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_2, "JSON/parse/15.12.1.1-g6-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_3, "JSON/parse/15.12.1.1-g6-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_4, "JSON/parse/15.12.1.1-g6-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_5, "JSON/parse/15.12.1.1-g6-5.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_6, "JSON/parse/15.12.1.1-g6-6.js");
    test262_builtin_fixture!(JSON_parse_15_12_1_1_g6_7, "JSON/parse/15.12.1.1-g6-7.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_1, "JSON/parse/15.12.2-2-1.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_10, "JSON/parse/15.12.2-2-10.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_2, "JSON/parse/15.12.2-2-2.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_3, "JSON/parse/15.12.2-2-3.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_4, "JSON/parse/15.12.2-2-4.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_5, "JSON/parse/15.12.2-2-5.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_6, "JSON/parse/15.12.2-2-6.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_7, "JSON/parse/15.12.2-2-7.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_8, "JSON/parse/15.12.2-2-8.js");
    test262_builtin_fixture!(JSON_parse_15_12_2_2_9, "JSON/parse/15.12.2-2-9.js");
    test262_builtin_fixture!(JSON_parse_builtin, "JSON/parse/builtin.js");
    test262_builtin_fixture!(JSON_parse_duplicate_proto, "JSON/parse/duplicate-proto.js");
    test262_builtin_fixture!(
        JSON_parse_invalid_whitespace,
        "JSON/parse/invalid-whitespace.js"
    );
    test262_builtin_fixture!(JSON_parse_length, "JSON/parse/length.js");
    test262_builtin_fixture!(JSON_parse_name, "JSON/parse/name.js");
    test262_builtin_fixture!(
        JSON_parse_not_a_constructor,
        "JSON/parse/not-a-constructor.js"
    );
    test262_builtin_fixture!(JSON_parse_prop_desc, "JSON/parse/prop-desc.js");
    test262_builtin_fixture!(
        JSON_parse_reviver_array_get_prop_from_prototype,
        "JSON/parse/reviver-array-get-prop-from-prototype.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_array_non_configurable_prop_delete,
        "JSON/parse/reviver-array-non-configurable-prop-delete.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_call_args_after_forward_modification,
        "JSON/parse/reviver-call-args-after-forward-modification.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_call_err,
        "JSON/parse/reviver-call-err.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_call_order,
        "JSON/parse/reviver-call-order.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_context_source_array_literal,
        "JSON/parse/reviver-context-source-array-literal.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_context_source_object_literal,
        "JSON/parse/reviver-context-source-object-literal.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_context_source_primitive_literal,
        "JSON/parse/reviver-context-source-primitive-literal.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_get_name_err,
        "JSON/parse/reviver-get-name-err.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_object_get_prop_from_prototype,
        "JSON/parse/reviver-object-get-prop-from-prototype.js"
    );
    test262_builtin_fixture!(
        JSON_parse_reviver_object_non_configurable_prop_delete,
        "JSON/parse/reviver-object-non-configurable-prop-delete.js"
    );
    test262_builtin_fixture!(JSON_parse_reviver_wrapper, "JSON/parse/reviver-wrapper.js");
    test262_builtin_fixture!(JSON_parse_S15_12_2_A1, "JSON/parse/S15.12.2_A1.js");
    test262_builtin_fixture!(
        JSON_parse_text_negative_zero,
        "JSON/parse/text-negative-zero.js"
    );
    test262_builtin_fixture!(
        JSON_parse_text_non_string_primitive,
        "JSON/parse/text-non-string-primitive.js"
    );
    test262_builtin_fixture!(
        JSON_parse_text_object_abrupt,
        "JSON/parse/text-object-abrupt.js"
    );
    test262_builtin_fixture!(JSON_parse_text_object, "JSON/parse/text-object.js");
    test262_builtin_fixture!(JSON_prop_desc, "JSON/prop-desc.js");
    test262_builtin_fixture!(JSON_rawJSON_basic, "JSON/rawJSON/basic.js");
    test262_builtin_fixture!(JSON_rawJSON_builtin, "JSON/rawJSON/builtin.js");
    test262_builtin_fixture!(
        JSON_rawJSON_illegal_empty_and_start_end_chars,
        "JSON/rawJSON/illegal-empty-and-start-end-chars.js"
    );
    test262_builtin_fixture!(
        JSON_rawJSON_invalid_JSON_text,
        "JSON/rawJSON/invalid-JSON-text.js"
    );
    test262_builtin_fixture!(JSON_rawJSON_length, "JSON/rawJSON/length.js");
    test262_builtin_fixture!(JSON_rawJSON_name, "JSON/rawJSON/name.js");
    test262_builtin_fixture!(
        JSON_rawJSON_not_a_constructor,
        "JSON/rawJSON/not-a-constructor.js"
    );
    test262_builtin_fixture!(JSON_rawJSON_prop_desc, "JSON/rawJSON/prop-desc.js");
    test262_builtin_fixture!(
        JSON_rawJSON_returns_expected_object,
        "JSON/rawJSON/returns-expected-object.js"
    );
    test262_builtin_fixture!(JSON_stringify_builtin, "JSON/stringify/builtin.js");
    test262_builtin_fixture!(JSON_stringify_length, "JSON/stringify/length.js");
    test262_builtin_fixture!(JSON_stringify_name, "JSON/stringify/name.js");
    test262_builtin_fixture!(
        JSON_stringify_not_a_constructor,
        "JSON/stringify/not-a-constructor.js"
    );
    test262_builtin_fixture!(JSON_stringify_prop_desc, "JSON/stringify/prop-desc.js");
    test262_builtin_fixture!(
        JSON_stringify_property_order,
        "JSON/stringify/property-order.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_duplicates,
        "JSON/stringify/replacer-array-duplicates.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_empty,
        "JSON/stringify/replacer-array-empty.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_number_object,
        "JSON/stringify/replacer-array-number-object.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_number,
        "JSON/stringify/replacer-array-number.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_order,
        "JSON/stringify/replacer-array-order.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_string_object,
        "JSON/stringify/replacer-array-string-object.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_array_undefined,
        "JSON/stringify/replacer-array-undefined.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_abrupt,
        "JSON/stringify/replacer-function-abrupt.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_arguments,
        "JSON/stringify/replacer-function-arguments.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_array_circular,
        "JSON/stringify/replacer-function-array-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_object_circular,
        "JSON/stringify/replacer-function-object-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_object_deleted_property,
        "JSON/stringify/replacer-function-object-deleted-property.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_result_undefined,
        "JSON/stringify/replacer-function-result-undefined.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_result,
        "JSON/stringify/replacer-function-result.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_tojson,
        "JSON/stringify/replacer-function-tojson.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_function_wrapper,
        "JSON/stringify/replacer-function-wrapper.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_replacer_wrong_type,
        "JSON/stringify/replacer-wrong-type.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_number_float,
        "JSON/stringify/space-number-float.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_number_object,
        "JSON/stringify/space-number-object.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_number_range,
        "JSON/stringify/space-number-range.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_number,
        "JSON/stringify/space-number.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_string_range,
        "JSON/stringify/space-string-range.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_space_string,
        "JSON/stringify/space-string.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_array_circular,
        "JSON/stringify/value-array-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_bigint_order,
        "JSON/stringify/value-bigint-order.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_bigint_replacer,
        "JSON/stringify/value-bigint-replacer.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_bigint_tojson,
        "JSON/stringify/value-bigint-tojson.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_bigint,
        "JSON/stringify/value-bigint.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_boolean_object,
        "JSON/stringify/value-boolean-object.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_function,
        "JSON/stringify/value-function.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_number_negative_zero,
        "JSON/stringify/value-number-negative-zero.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_number_non_finite,
        "JSON/stringify/value-number-non-finite.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_object_abrupt,
        "JSON/stringify/value-object-abrupt.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_object_circular,
        "JSON/stringify/value-object-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_primitive_top_level,
        "JSON/stringify/value-primitive-top-level.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_string_escape_ascii,
        "JSON/stringify/value-string-escape-ascii.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_string_escape_unicode,
        "JSON/stringify/value-string-escape-unicode.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_symbol,
        "JSON/stringify/value-symbol.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_abrupt,
        "JSON/stringify/value-tojson-abrupt.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_arguments,
        "JSON/stringify/value-tojson-arguments.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_array_circular,
        "JSON/stringify/value-tojson-array-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_not_function,
        "JSON/stringify/value-tojson-not-function.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_object_circular,
        "JSON/stringify/value-tojson-object-circular.js"
    );
    test262_builtin_fixture!(
        JSON_stringify_value_tojson_result,
        "JSON/stringify/value-tojson-result.js"
    );
    test262_builtin_fixture!(JSON_Symbol_toStringTag, "JSON/Symbol.toStringTag.js");
    // Phase 14 TypedArray surface (the integer-indexed exotic methods; the
    // list was produced by the scanner, so it is data, not aspiration).
    test262_builtin_fixture!(
        TypedArray_from_invoked_as_func,
        "TypedArray/from/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_from_invoked_as_method,
        "TypedArray/from/invoked-as-method.js"
    );
    test262_builtin_fixture!(TypedArray_from_length, "TypedArray/from/length.js");
    test262_builtin_fixture!(
        TypedArray_from_mapfn_is_not_callable,
        "TypedArray/from/mapfn-is-not-callable.js"
    );
    test262_builtin_fixture!(TypedArray_from_name, "TypedArray/from/name.js");
    test262_builtin_fixture!(
        TypedArray_from_not_a_constructor,
        "TypedArray/from/not-a-constructor.js"
    );
    test262_builtin_fixture!(TypedArray_from_prop_desc, "TypedArray/from/prop-desc.js");
    test262_builtin_fixture!(
        TypedArray_from_this_is_not_constructor,
        "TypedArray/from/this-is-not-constructor.js"
    );
    test262_builtin_fixture!(TypedArray_invoked, "TypedArray/invoked.js");
    test262_builtin_fixture!(TypedArray_length, "TypedArray/length.js");
    test262_builtin_fixture!(TypedArray_name, "TypedArray/name.js");
    test262_builtin_fixture!(
        TypedArray_of_invoked_as_func,
        "TypedArray/of/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_of_invoked_as_method,
        "TypedArray/of/invoked-as-method.js"
    );
    test262_builtin_fixture!(TypedArray_of_length, "TypedArray/of/length.js");
    test262_builtin_fixture!(TypedArray_of_name, "TypedArray/of/name.js");
    test262_builtin_fixture!(
        TypedArray_of_not_a_constructor,
        "TypedArray/of/not-a-constructor.js"
    );
    test262_builtin_fixture!(TypedArray_of_prop_desc, "TypedArray/of/prop-desc.js");
    test262_builtin_fixture!(
        TypedArray_of_this_is_not_constructor,
        "TypedArray/of/this-is-not-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_at_length,
        "TypedArray/prototype/at/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_at_name,
        "TypedArray/prototype/at/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_at_prop_desc,
        "TypedArray/prototype/at/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_at_return_abrupt_from_this,
        "TypedArray/prototype/at/return-abrupt-from-this.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_invoked_as_accessor,
        "TypedArray/prototype/buffer/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_invoked_as_func,
        "TypedArray/prototype/buffer/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_length,
        "TypedArray/prototype/buffer/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_name,
        "TypedArray/prototype/buffer/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_prop_desc,
        "TypedArray/prototype/buffer/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_this_has_no_typedarrayname_internal,
        "TypedArray/prototype/buffer/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_buffer_this_is_not_object,
        "TypedArray/prototype/buffer/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_invoked_as_accessor,
        "TypedArray/prototype/byteLength/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_invoked_as_func,
        "TypedArray/prototype/byteLength/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_length,
        "TypedArray/prototype/byteLength/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_name,
        "TypedArray/prototype/byteLength/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_prop_desc,
        "TypedArray/prototype/byteLength/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_this_has_no_typedarrayname_internal,
        "TypedArray/prototype/byteLength/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteLength_this_is_not_object,
        "TypedArray/prototype/byteLength/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_invoked_as_accessor,
        "TypedArray/prototype/byteOffset/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_invoked_as_func,
        "TypedArray/prototype/byteOffset/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_length,
        "TypedArray/prototype/byteOffset/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_name,
        "TypedArray/prototype/byteOffset/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_prop_desc,
        "TypedArray/prototype/byteOffset/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_this_has_no_typedarrayname_internal,
        "TypedArray/prototype/byteOffset/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_byteOffset_this_is_not_object,
        "TypedArray/prototype/byteOffset/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_constructor,
        "TypedArray/prototype/constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/copyWithin/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/copyWithin/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_get_length_ignores_length_prop,
        "TypedArray/prototype/copyWithin/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_invoked_as_func,
        "TypedArray/prototype/copyWithin/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_invoked_as_method,
        "TypedArray/prototype/copyWithin/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_length,
        "TypedArray/prototype/copyWithin/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_name,
        "TypedArray/prototype/copyWithin/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_not_a_constructor,
        "TypedArray/prototype/copyWithin/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_prop_desc,
        "TypedArray/prototype/copyWithin/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/copyWithin/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_this_is_not_object,
        "TypedArray/prototype/copyWithin/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_copyWithin_this_is_not_typedarray_instance,
        "TypedArray/prototype/copyWithin/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_invoked_as_func,
        "TypedArray/prototype/entries/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_invoked_as_method,
        "TypedArray/prototype/entries/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_length,
        "TypedArray/prototype/entries/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_name,
        "TypedArray/prototype/entries/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_not_a_constructor,
        "TypedArray/prototype/entries/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_prop_desc,
        "TypedArray/prototype/entries/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_this_is_not_object,
        "TypedArray/prototype/entries/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_entries_this_is_not_typedarray_instance,
        "TypedArray/prototype/entries/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/every/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_get_length_uses_internal_arraylength,
        "TypedArray/prototype/every/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_invoked_as_func,
        "TypedArray/prototype/every/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_invoked_as_method,
        "TypedArray/prototype/every/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_length,
        "TypedArray/prototype/every/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_name,
        "TypedArray/prototype/every/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_not_a_constructor,
        "TypedArray/prototype/every/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_prop_desc,
        "TypedArray/prototype/every/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_this_is_not_object,
        "TypedArray/prototype/every/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_every_this_is_not_typedarray_instance,
        "TypedArray/prototype/every/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/fill/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/fill/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_get_length_ignores_length_prop,
        "TypedArray/prototype/fill/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_invoked_as_func,
        "TypedArray/prototype/fill/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_invoked_as_method,
        "TypedArray/prototype/fill/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_length,
        "TypedArray/prototype/fill/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_name,
        "TypedArray/prototype/fill/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_not_a_constructor,
        "TypedArray/prototype/fill/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_prop_desc,
        "TypedArray/prototype/fill/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/fill/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_this_is_not_object,
        "TypedArray/prototype/fill/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_fill_this_is_not_typedarray_instance,
        "TypedArray/prototype/fill/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_arraylength_internal,
        "TypedArray/prototype/filter/arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_BigInt_arraylength_internal,
        "TypedArray/prototype/filter/BigInt/arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/filter/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_BigInt_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/filter/BigInt/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_invoked_as_func,
        "TypedArray/prototype/filter/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_invoked_as_method,
        "TypedArray/prototype/filter/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_length,
        "TypedArray/prototype/filter/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_name,
        "TypedArray/prototype/filter/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_not_a_constructor,
        "TypedArray/prototype/filter/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_prop_desc,
        "TypedArray/prototype/filter/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/filter/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/filter/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_this_is_not_object,
        "TypedArray/prototype/filter/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_filter_this_is_not_typedarray_instance,
        "TypedArray/prototype/filter/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/find/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/find/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_get_length_ignores_length_prop,
        "TypedArray/prototype/find/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_invoked_as_func,
        "TypedArray/prototype/find/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_invoked_as_method,
        "TypedArray/prototype/find/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_length,
        "TypedArray/prototype/find/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_name,
        "TypedArray/prototype/find/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_not_a_constructor,
        "TypedArray/prototype/find/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_prop_desc,
        "TypedArray/prototype/find/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/find/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_this_is_not_object,
        "TypedArray/prototype/find/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_find_this_is_not_typedarray_instance,
        "TypedArray/prototype/find/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/findIndex/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findIndex/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_get_length_ignores_length_prop,
        "TypedArray/prototype/findIndex/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_invoked_as_func,
        "TypedArray/prototype/findIndex/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_invoked_as_method,
        "TypedArray/prototype/findIndex/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_length,
        "TypedArray/prototype/findIndex/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_name,
        "TypedArray/prototype/findIndex/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_not_a_constructor,
        "TypedArray/prototype/findIndex/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_prop_desc,
        "TypedArray/prototype/findIndex/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findIndex/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_this_is_not_object,
        "TypedArray/prototype/findIndex/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findIndex_this_is_not_typedarray_instance,
        "TypedArray/prototype/findIndex/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/findLast/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findLast/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_get_length_ignores_length_prop,
        "TypedArray/prototype/findLast/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_invoked_as_func,
        "TypedArray/prototype/findLast/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_invoked_as_method,
        "TypedArray/prototype/findLast/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_length,
        "TypedArray/prototype/findLast/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_name,
        "TypedArray/prototype/findLast/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_not_a_constructor,
        "TypedArray/prototype/findLast/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_prop_desc,
        "TypedArray/prototype/findLast/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findLast/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_this_is_not_object,
        "TypedArray/prototype/findLast/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLast_this_is_not_typedarray_instance,
        "TypedArray/prototype/findLast/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_BigInt_get_length_ignores_length_prop,
        "TypedArray/prototype/findLastIndex/BigInt/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findLastIndex/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_get_length_ignores_length_prop,
        "TypedArray/prototype/findLastIndex/get-length-ignores-length-prop.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_invoked_as_func,
        "TypedArray/prototype/findLastIndex/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_invoked_as_method,
        "TypedArray/prototype/findLastIndex/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_length,
        "TypedArray/prototype/findLastIndex/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_name,
        "TypedArray/prototype/findLastIndex/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_not_a_constructor,
        "TypedArray/prototype/findLastIndex/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_prop_desc,
        "TypedArray/prototype/findLastIndex/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/findLastIndex/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_this_is_not_object,
        "TypedArray/prototype/findLastIndex/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_findLastIndex_this_is_not_typedarray_instance,
        "TypedArray/prototype/findLastIndex/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/forEach/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_invoked_as_func,
        "TypedArray/prototype/forEach/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_invoked_as_method,
        "TypedArray/prototype/forEach/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_length,
        "TypedArray/prototype/forEach/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_name,
        "TypedArray/prototype/forEach/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_not_a_constructor,
        "TypedArray/prototype/forEach/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_prop_desc,
        "TypedArray/prototype/forEach/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/forEach/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_this_is_not_object,
        "TypedArray/prototype/forEach/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_forEach_this_is_not_typedarray_instance,
        "TypedArray/prototype/forEach/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/includes/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_get_length_uses_internal_arraylength,
        "TypedArray/prototype/includes/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_invoked_as_func,
        "TypedArray/prototype/includes/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_invoked_as_method,
        "TypedArray/prototype/includes/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_length,
        "TypedArray/prototype/includes/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_name,
        "TypedArray/prototype/includes/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_not_a_constructor,
        "TypedArray/prototype/includes/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_prop_desc,
        "TypedArray/prototype/includes/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_this_is_not_object,
        "TypedArray/prototype/includes/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_includes_this_is_not_typedarray_instance,
        "TypedArray/prototype/includes/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/indexOf/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_get_length_uses_internal_arraylength,
        "TypedArray/prototype/indexOf/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_invoked_as_func,
        "TypedArray/prototype/indexOf/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_invoked_as_method,
        "TypedArray/prototype/indexOf/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_length,
        "TypedArray/prototype/indexOf/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_name,
        "TypedArray/prototype/indexOf/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_not_a_constructor,
        "TypedArray/prototype/indexOf/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_prop_desc,
        "TypedArray/prototype/indexOf/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_this_is_not_object,
        "TypedArray/prototype/indexOf/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_indexOf_this_is_not_typedarray_instance,
        "TypedArray/prototype/indexOf/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/join/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/join/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_get_length_uses_internal_arraylength,
        "TypedArray/prototype/join/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_invoked_as_func,
        "TypedArray/prototype/join/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_invoked_as_method,
        "TypedArray/prototype/join/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_length,
        "TypedArray/prototype/join/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_name,
        "TypedArray/prototype/join/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_not_a_constructor,
        "TypedArray/prototype/join/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_prop_desc,
        "TypedArray/prototype/join/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/join/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_this_is_not_object,
        "TypedArray/prototype/join/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_join_this_is_not_typedarray_instance,
        "TypedArray/prototype/join/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_invoked_as_func,
        "TypedArray/prototype/keys/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_invoked_as_method,
        "TypedArray/prototype/keys/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_length,
        "TypedArray/prototype/keys/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_name,
        "TypedArray/prototype/keys/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_not_a_constructor,
        "TypedArray/prototype/keys/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_prop_desc,
        "TypedArray/prototype/keys/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_this_is_not_object,
        "TypedArray/prototype/keys/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_keys_this_is_not_typedarray_instance,
        "TypedArray/prototype/keys/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/lastIndexOf/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/lastIndexOf/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_get_length_uses_internal_arraylength,
        "TypedArray/prototype/lastIndexOf/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_invoked_as_func,
        "TypedArray/prototype/lastIndexOf/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_invoked_as_method,
        "TypedArray/prototype/lastIndexOf/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_length,
        "TypedArray/prototype/lastIndexOf/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_name,
        "TypedArray/prototype/lastIndexOf/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_not_a_constructor,
        "TypedArray/prototype/lastIndexOf/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_prop_desc,
        "TypedArray/prototype/lastIndexOf/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/lastIndexOf/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_this_is_not_object,
        "TypedArray/prototype/lastIndexOf/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_lastIndexOf_this_is_not_typedarray_instance,
        "TypedArray/prototype/lastIndexOf/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_invoked_as_accessor,
        "TypedArray/prototype/length/invoked-as-accessor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_invoked_as_func,
        "TypedArray/prototype/length/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_length,
        "TypedArray/prototype/length/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_name,
        "TypedArray/prototype/length/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_prop_desc,
        "TypedArray/prototype/length/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_this_has_no_typedarrayname_internal,
        "TypedArray/prototype/length/this-has-no-typedarrayname-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_length_this_is_not_object,
        "TypedArray/prototype/length/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/map/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_BigInt_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/map/BigInt/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_invoked_as_func,
        "TypedArray/prototype/map/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_invoked_as_method,
        "TypedArray/prototype/map/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_length,
        "TypedArray/prototype/map/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_name,
        "TypedArray/prototype/map/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_not_a_constructor,
        "TypedArray/prototype/map/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_prop_desc,
        "TypedArray/prototype/map/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/map/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/map/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_this_is_not_object,
        "TypedArray/prototype/map/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_map_this_is_not_typedarray_instance,
        "TypedArray/prototype/map/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reduce/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reduce/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reduce/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_invoked_as_func,
        "TypedArray/prototype/reduce/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_invoked_as_method,
        "TypedArray/prototype/reduce/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_length,
        "TypedArray/prototype/reduce/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_name,
        "TypedArray/prototype/reduce/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_not_a_constructor,
        "TypedArray/prototype/reduce/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_prop_desc,
        "TypedArray/prototype/reduce/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reduce/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_this_is_not_object,
        "TypedArray/prototype/reduce/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduce_this_is_not_typedarray_instance,
        "TypedArray/prototype/reduce/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reduceRight/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reduceRight/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reduceRight/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_invoked_as_func,
        "TypedArray/prototype/reduceRight/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_invoked_as_method,
        "TypedArray/prototype/reduceRight/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_length,
        "TypedArray/prototype/reduceRight/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_name,
        "TypedArray/prototype/reduceRight/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_not_a_constructor,
        "TypedArray/prototype/reduceRight/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_prop_desc,
        "TypedArray/prototype/reduceRight/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reduceRight/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_this_is_not_object,
        "TypedArray/prototype/reduceRight/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reduceRight_this_is_not_typedarray_instance,
        "TypedArray/prototype/reduceRight/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reverse/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reverse/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_BigInt_returns_original_object,
        "TypedArray/prototype/reverse/BigInt/returns-original-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_get_length_uses_internal_arraylength,
        "TypedArray/prototype/reverse/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_invoked_as_func,
        "TypedArray/prototype/reverse/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_invoked_as_method,
        "TypedArray/prototype/reverse/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_length,
        "TypedArray/prototype/reverse/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_name,
        "TypedArray/prototype/reverse/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_not_a_constructor,
        "TypedArray/prototype/reverse/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_prop_desc,
        "TypedArray/prototype/reverse/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/reverse/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_returns_original_object,
        "TypedArray/prototype/reverse/returns-original-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_this_is_not_object,
        "TypedArray/prototype/reverse/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_reverse_this_is_not_typedarray_instance,
        "TypedArray/prototype/reverse/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_array_arg_target_arraylength_internal,
        "TypedArray/prototype/set/array-arg-target-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_array_arg_target_arraylength_internal,
        "TypedArray/prototype/set/BigInt/array-arg-target-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_bigint_tobigint64,
        "TypedArray/prototype/set/BigInt/bigint-tobigint64.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_bigint_tobiguint64,
        "TypedArray/prototype/set/BigInt/bigint-tobiguint64.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_typedarray_arg_src_arraylength_internal,
        "TypedArray/prototype/set/BigInt/typedarray-arg-src-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_typedarray_arg_src_byteoffset_internal,
        "TypedArray/prototype/set/BigInt/typedarray-arg-src-byteoffset-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_BigInt_typedarray_arg_target_arraylength_internal,
        "TypedArray/prototype/set/BigInt/typedarray-arg-target-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_invoked_as_func,
        "TypedArray/prototype/set/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_invoked_as_method,
        "TypedArray/prototype/set/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_length,
        "TypedArray/prototype/set/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_name,
        "TypedArray/prototype/set/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_not_a_constructor,
        "TypedArray/prototype/set/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_prop_desc,
        "TypedArray/prototype/set/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_this_is_not_object,
        "TypedArray/prototype/set/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_this_is_not_typedarray_instance,
        "TypedArray/prototype/set/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_typedarray_arg_src_arraylength_internal,
        "TypedArray/prototype/set/typedarray-arg-src-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_typedarray_arg_src_byteoffset_internal,
        "TypedArray/prototype/set/typedarray-arg-src-byteoffset-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_typedarray_arg_target_arraylength_internal,
        "TypedArray/prototype/set/typedarray-arg-target-arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_set_typedarray_arg_target_byteoffset_internal,
        "TypedArray/prototype/set/typedarray-arg-target-byteoffset-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_arraylength_internal,
        "TypedArray/prototype/slice/arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_BigInt_arraylength_internal,
        "TypedArray/prototype/slice/BigInt/arraylength-internal.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/slice/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_BigInt_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/slice/BigInt/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_invoked_as_func,
        "TypedArray/prototype/slice/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_invoked_as_method,
        "TypedArray/prototype/slice/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_length,
        "TypedArray/prototype/slice/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_name,
        "TypedArray/prototype/slice/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_not_a_constructor,
        "TypedArray/prototype/slice/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_prop_desc,
        "TypedArray/prototype/slice/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/slice/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_speciesctor_get_ctor_inherited,
        "TypedArray/prototype/slice/speciesctor-get-ctor-inherited.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_this_is_not_object,
        "TypedArray/prototype/slice/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_slice_this_is_not_typedarray_instance,
        "TypedArray/prototype/slice/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/some/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/some/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_get_length_uses_internal_arraylength,
        "TypedArray/prototype/some/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_invoked_as_func,
        "TypedArray/prototype/some/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_invoked_as_method,
        "TypedArray/prototype/some/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_length,
        "TypedArray/prototype/some/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_name,
        "TypedArray/prototype/some/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_not_a_constructor,
        "TypedArray/prototype/some/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_prop_desc,
        "TypedArray/prototype/some/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/some/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_this_is_not_object,
        "TypedArray/prototype/some/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_some_this_is_not_typedarray_instance,
        "TypedArray/prototype/some/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/sort/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_invoked_as_func,
        "TypedArray/prototype/sort/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_invoked_as_method,
        "TypedArray/prototype/sort/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_length,
        "TypedArray/prototype/sort/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_name,
        "TypedArray/prototype/sort/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_not_a_constructor,
        "TypedArray/prototype/sort/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_prop_desc,
        "TypedArray/prototype/sort/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/sort/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_this_is_not_object,
        "TypedArray/prototype/sort/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_sort_this_is_not_typedarray_instance,
        "TypedArray/prototype/sort/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_invoked_as_func,
        "TypedArray/prototype/subarray/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_invoked_as_method,
        "TypedArray/prototype/subarray/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_length,
        "TypedArray/prototype/subarray/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_name,
        "TypedArray/prototype/subarray/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_not_a_constructor,
        "TypedArray/prototype/subarray/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_prop_desc,
        "TypedArray/prototype/subarray/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_this_is_not_object,
        "TypedArray/prototype/subarray/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_subarray_this_is_not_typedarray_instance,
        "TypedArray/prototype/subarray/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_Symbol_iterator_not_a_constructor,
        "TypedArray/prototype/Symbol.iterator/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_Symbol_iterator,
        "TypedArray/prototype/Symbol.iterator.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_BigInt_get_length_uses_internal_arraylength,
        "TypedArray/prototype/toLocaleString/BigInt/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_BigInt_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/toLocaleString/BigInt/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_get_length_uses_internal_arraylength,
        "TypedArray/prototype/toLocaleString/get-length-uses-internal-arraylength.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_invoked_as_func,
        "TypedArray/prototype/toLocaleString/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_invoked_as_method,
        "TypedArray/prototype/toLocaleString/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_length,
        "TypedArray/prototype/toLocaleString/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_name,
        "TypedArray/prototype/toLocaleString/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_not_a_constructor,
        "TypedArray/prototype/toLocaleString/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_prop_desc,
        "TypedArray/prototype/toLocaleString/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_return_abrupt_from_this_out_of_bounds,
        "TypedArray/prototype/toLocaleString/return-abrupt-from-this-out-of-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_this_is_not_object,
        "TypedArray/prototype/toLocaleString/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toLocaleString_this_is_not_typedarray_instance,
        "TypedArray/prototype/toLocaleString/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toReversed_length,
        "TypedArray/prototype/toReversed/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toReversed_name,
        "TypedArray/prototype/toReversed/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toReversed_not_a_constructor,
        "TypedArray/prototype/toReversed/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toReversed_property_descriptor,
        "TypedArray/prototype/toReversed/property-descriptor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toReversed_reverses,
        "TypedArray/prototype/toReversed/reverses.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_comparefn_controls_sort,
        "TypedArray/prototype/toSorted/comparefn-controls-sort.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_comparefn_default,
        "TypedArray/prototype/toSorted/comparefn-default.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_length,
        "TypedArray/prototype/toSorted/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_name,
        "TypedArray/prototype/toSorted/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_not_a_constructor,
        "TypedArray/prototype/toSorted/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toSorted_property_descriptor,
        "TypedArray/prototype/toSorted/property-descriptor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_toString_not_a_constructor,
        "TypedArray/prototype/toString/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_invoked_as_func,
        "TypedArray/prototype/values/invoked-as-func.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_invoked_as_method,
        "TypedArray/prototype/values/invoked-as-method.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_length,
        "TypedArray/prototype/values/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_name,
        "TypedArray/prototype/values/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_not_a_constructor,
        "TypedArray/prototype/values/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_prop_desc,
        "TypedArray/prototype/values/prop-desc.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_this_is_not_object,
        "TypedArray/prototype/values/this-is-not-object.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_values_this_is_not_typedarray_instance,
        "TypedArray/prototype/values/this-is-not-typedarray-instance.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_length,
        "TypedArray/prototype/with/length.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_name,
        "TypedArray/prototype/with/name.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_negative_index_resize_to_in_bounds,
        "TypedArray/prototype/with/negative-index-resize-to-in-bounds.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_not_a_constructor,
        "TypedArray/prototype/with/not-a-constructor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_property_descriptor,
        "TypedArray/prototype/with/property-descriptor.js"
    );
    test262_builtin_fixture!(
        TypedArray_prototype_with_this_value_invalid,
        "TypedArray/prototype/with/this-value-invalid.js"
    );
    test262_builtin_fixture!(TypedArray_prototype, "TypedArray/prototype.js");
    /// `crates/test262` sits one level below the repo root, where the
    /// `test262` submodule is pinned.
    fn builtins_dir() -> PathBuf {
        Area::Builtins.root()
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
        // The preludes need to call user-level functions (assert.throws runs
        // `func`, compareArray reads properties, $262.detachArrayBuffer calls
        // transfer, verifyProperty calls Object.getOwnPropertyDescriptor),
        // which the native closures cannot (no agent access); define them as
        // scripts.
        agent
            .run_script(ASSERT_THROWS_PRELUDE)
            .map_err(|e| e.message)?;
        agent.run_script(HARNESS_PRELUDE).map_err(|e| e.message)?;
        // The real harness include files (testTypedArray.js, propertyHelper.js,
        // testAtomics.js, …) are plain JS built on the globals above; load
        // them from the submodule so the vendored fixtures get their exact
        // helper surface.
        for include in &fm.includes {
            let source = harness_include_source(include)?;
            agent.run_script(&source).map_err(|e| e.message)?;
        }
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

    /// The source of a harness include file, read from the pinned submodule.
    /// Only the large helper files the preludes cannot replicate are loaded;
    /// the rest (isConstructor.js needs Reflect, propertyHelper.js restores
    /// writability through defineProperty) are provided by the preludes
    /// instead.
    fn harness_include_source(name: &str) -> Result<String, String> {
        if !matches!(name, "testAtomics.js" | "testTypedArray.js") {
            return Ok(String::new());
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test262/harness")
            .join(name);
        std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))
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
        // The global `assert` is the callable bare function with the helper
        // methods attached (real test262 assert.js defines `assert` as a
        // function), so fixtures calling `assert(x)` directly work.
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
            ("sameValue", same_value),
            ("notSameValue", not_same_value),
            ("true", assert_true),
            ("false", assert_false),
        ] {
            bare.object
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

        // isConstructor (harness isConstructor.js needs Reflect.construct,
        // which is Phase 16); the abstract op itself is crux-level, so the
        // native closure answers it directly.
        let is_constructor = Function::create_builtin(
            Some(JsString::from_utf8("isConstructor")),
            1,
            Box::new(|_, args| {
                let Some(value) = args.first() else {
                    return Err(arity_error("isConstructor"));
                };
                Ok(Value::Boolean(crux::value::is_constructor(value)))
            }),
            None,
            None,
        )
        .map_err(|e| e.message)?;

        global
            .create_data_property(&JsString::from_utf8("assert"), Value::Function(bare))
            .map_err(|e| e.message)?;
        global
            .create_data_property(
                &JsString::from_utf8("isConstructor"),
                Value::Function(is_constructor),
            )
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

    /// Recursively collect the `.js` files under `dir` (the flat scanner
    /// needs a recursive walk for the nested built-ins directories).
    fn collect_js_files(
        dir: &std::path::Path,
        out: &mut Vec<std::path::PathBuf>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                collect_js_files(&path, out)?;
            } else if path.extension().and_then(|e| e.to_str()) == Some("js") {
                out.push(path);
            }
        }
        Ok(())
    }

    /// Run a fixture file (both modes unless `flags:` says otherwise).
    fn run_fixture(area: Area, relative: &str) -> FixtureResult {
        let path = area.root().join(relative);
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
            .filter(|include| {
                !matches!(
                    *include,
                    "assert.js"
                        | "compareArray.js"
                        | "detachArrayBuffer.js"
                        | "isConstructor.js"
                        | "propertyHelper.js"
                        | "testAtomics.js"
                        | "testTypedArray.js"
                )
            })
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

    /// Run one fixture as its own test. Skips print and pass; a missing
    /// submodule (fresh clone) makes every fixture pass vacuously.
    fn assert_fixture(area: Area, relative: &str) {
        static NOTICE: std::sync::Once = std::sync::Once::new();
        if !area.root().exists() {
            NOTICE.call_once(|| {
                eprintln!("test262 submodule not checked out; run `git submodule update --init`");
            });
            return;
        }
        match run_fixture(area, relative) {
            FixtureResult::Pass => {}
            FixtureResult::Skip(reason) => eprintln!("SKIP {relative}: {reason}"),
            FixtureResult::Fail(reason) => panic!("FAIL {relative}: {reason}"),
        }
    }

    /// Directory pass-rate scanner: `cargo test -p test262 -- --ignored
    /// scan_builtins --nocapture` prints how much of each Phase 8 built-ins
    /// directory already passes, so the next batch of `test262_builtin_fixture!`
    /// entries is data-driven. Skipped for regular runs.
    #[test]
    #[ignore = "directory pass-rate scanner"]
    fn scan_builtins_directories() {
        let dirs = [
            "global",
            "globalThis",
            "undefined",
            "NaN",
            "Infinity",
            "eval",
            "decodeURI",
            "decodeURIComponent",
            "encodeURI",
            "encodeURIComponent",
            "isFinite",
            "isNaN",
            "parseFloat",
            "parseInt",
            "Boolean",
            "Symbol",
            "Error",
            "AggregateError",
            "Function",
            "Object",
            "Math",
            "Number",
            "BigInt",
            "Date",
            "String",
            "RegExp",
            "Array",
            "Uint8Array",
            "Map",
            "Set",
            "WeakMap",
            "WeakSet",
        ];
        for dir in dirs {
            let mut pass = 0;
            let mut skip = 0;
            let mut fail = 0;
            let mut failures = Vec::new();
            let root = builtins_dir().join(dir);
            let mut files = Vec::new();
            if let Err(e) = collect_js_files(&root, &mut files) {
                println!("{dir}: cannot read ({e})");
                continue;
            }
            for path in files {
                let relative = path
                    .strip_prefix(builtins_dir())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                match run_fixture(Area::Builtins, &relative) {
                    FixtureResult::Pass => {
                        pass += 1;
                        let ident = relative
                            .replace(['/', '-', '.'], "_")
                            .trim_end_matches("_js")
                            .to_string();
                        println!("test262_builtin_fixture!({ident}, \"{relative}\");");
                    }
                    FixtureResult::Skip(reason) => {
                        skip += 1;
                        println!("SKIP {relative}: {reason}");
                    }
                    FixtureResult::Fail(reason) => {
                        fail += 1;
                        failures.push(reason);
                    }
                }
            }
            println!(
                "{dir}: {pass} pass, {skip} skip, {fail} fail ({} total)",
                pass + skip + fail
            );
            for reason in failures.iter().take(8) {
                println!("  FAIL {reason}");
            }
        }
    }
}
