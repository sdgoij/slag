#include "quickjs.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

// Phase 6.3 regression probe: a string stored as a data property of an
// ordinary object must survive JS_RunGC. Each scenario is independent.

static int read_string_equals(JSContext *ctx, JSValue obj, const char *key,
                              const char *expected) {
    JSValue out = JS_GetPropertyStr(ctx, obj, key);
    if (JS_IsException(out))
        return 0;
    const char *text = JS_ToCStringLen2(ctx, NULL, out, 0);
    int ok = text != NULL && strcmp(text, expected) == 0;
    JS_FreeCString(ctx, text);
    JS_FreeValue(ctx, out);
    return ok;
}

static void test_short_string_via_new(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    JSValue obj = JS_NewObject(ctx);
    JSValue str = JS_NewString(ctx, "hello");
    assert(JS_SetPropertyStr(ctx, obj, "k", str) == 0);
    JS_FreeValue(ctx, str);
    JS_RunGC(rt);
    assert(read_string_equals(ctx, obj, "k", "hello"));
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_long_string_via_new(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    JSValue obj = JS_NewObject(ctx);
    const char *long_text =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    JSValue str = JS_NewString(ctx, long_text);
    assert(JS_SetPropertyStr(ctx, obj, "k", str) == 0);
    JS_FreeValue(ctx, str);
    JS_RunGC(rt);
    assert(read_string_equals(ctx, obj, "k", long_text));
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_string_via_eval(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    JSValue obj = JS_NewObject(ctx);
    JSValue str = JS_Eval(ctx, "'hello world hello world'", 25, "probe.js", 0);
    assert(!JS_IsException(str));
    assert(JS_SetPropertyStr(ctx, obj, "k", str) == 0);
    JS_FreeValue(ctx, str);
    JS_RunGC(rt);
    assert(read_string_equals(ctx, obj, "k", "hello world hello world"));
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_function_list_string_row(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    static const JSCFunctionListEntry methods[] = {
        JS_PROP_STRING_DEF("name", "hello", JS_PROP_C_W_E),
    };
    JSValue obj = JS_NewObject(ctx);
    assert(JS_SetPropertyFunctionList(ctx, obj, methods, 1) == 0);
    JS_RunGC(rt);
    assert(read_string_equals(ctx, obj, "name", "hello"));
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_rope_string_via_eval(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    JSValue obj = JS_NewObject(ctx);
    const char *script =
        "'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'"
        " + "
        "'ghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuv"
        "ghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuv"
        "ghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuvghijklmnopqrstuv'";
    JSValue str = JS_Eval(ctx, script, strlen(script), "rope.js", 0);
    assert(!JS_IsException(str));
    assert(JS_SetPropertyStr(ctx, obj, "k", str) == 0);
    JS_FreeValue(ctx, str);
    JS_RunGC(rt);
    JSValue out = JS_GetPropertyStr(ctx, obj, "k");
    assert(!JS_IsException(out));
    JS_FreeValue(ctx, out);
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

void qjs_test_string_property_gc(void) {
    test_short_string_via_new();
    test_long_string_via_new();
    test_string_via_eval();
    test_function_list_string_row();
    test_rope_string_via_eval();
}
