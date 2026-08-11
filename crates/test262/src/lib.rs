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
        ];
        for dir in dirs {
            let mut pass = 0;
            let mut skip = 0;
            let mut fail = 0;
            let mut failures = Vec::new();
            let root = builtins_dir().join(dir);
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(e) => {
                    println!("{dir}: cannot read ({e})");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("js") {
                    continue;
                }
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
