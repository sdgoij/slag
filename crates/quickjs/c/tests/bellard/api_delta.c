#include "quickjs.h"
#include <assert.h>
#include <string.h>

// Bellard ABI deltas: JS_NewClassID takes one argument (no runtime),
// JS_NewArrayBuffer takes a free callback instead of a realloc callback, and
// the JS_BOOL predicates return int (not bool) and take a context where NG
// does not.

static int freed = 0;

static void free_buffer(JSRuntime *rt, void *opaque, void *ptr) {
    freed = 1;
}

static void test_api_delta(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSClassID class_id = 0;
    class_id = JS_NewClassID(&class_id);
    assert(class_id != 0);

    JSClassDef def;
    memset(&def, 0, sizeof(def));
    def.class_name = "DeltaClass";
    assert(JS_NewClass(rt, class_id, &def) == 0);
    assert(JS_IsRegisteredClass(rt, class_id) != 0);

    JSValue obj = JS_NewObjectClass(ctx, class_id);
    assert(JS_GetClassID(obj) == class_id);
    JS_FreeValue(ctx, obj);

    uint8_t bytes[] = { 1, 2, 3, 4 };
    freed = 0;
    JSValue ab = JS_NewArrayBuffer(ctx, bytes, 4, free_buffer, NULL, 0);

    size_t size = 0;
    uint8_t *data = JS_GetArrayBuffer(ctx, &size, ab);
    assert(data != NULL && size == 4);
    assert(data == bytes);
    assert(data[0] == 1 && data[3] == 4);

    JS_FreeValue(ctx, ab);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_predicates(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue fn = JS_Eval(ctx, "function f(){}; f", strlen("function f(){}; f"),
                         "test.js", 0);
    assert(JS_IsFunction(ctx, fn) != 0);
    assert(JS_IsConstructor(ctx, fn) != 0);
    JS_FreeValue(ctx, fn);

    JSValue arr = JS_Eval(ctx, "[1,2,3]", strlen("[1,2,3]"), "test.js", 0);
    assert(JS_IsArray(ctx, arr) != 0);
    JS_FreeValue(ctx, arr);

    JSValue err = JS_NewError(ctx);
    assert(JS_IsError(ctx, err) != 0);
    JS_FreeValue(ctx, err);

    JSValue num = JS_NewInt32(ctx, 1);
    assert(JS_IsFunction(ctx, num) == 0);
    assert(JS_IsArray(ctx, num) == 0);
    assert(JS_IsError(ctx, num) == 0);
    assert(!JS_HasException(ctx));
    JS_FreeValue(ctx, num);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_bellard_only(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue nan = JS_Eval(ctx, "NaN", strlen("NaN"), "test.js", 0);
    assert(JS_SameValue(ctx, nan, nan) != 0);
    JS_FreeValue(ctx, nan);

    JSValue a = JS_Eval(ctx, "NaN", strlen("NaN"), "test.js", 0);
    JSValue b = JS_Eval(ctx, "NaN", strlen("NaN"), "test.js", 0);
    assert(JS_StrictEq(ctx, a, b) == 0);
    JS_FreeValue(ctx, a);
    JS_FreeValue(ctx, b);

    JSValue one = JS_Eval(ctx, "1", strlen("1"), "test.js", 0);
    JSValue two = JS_Eval(ctx, "1", strlen("1"), "test.js", 0);
    assert(JS_StrictEq(ctx, one, two) != 0);
    assert(JS_SameValueZero(ctx, one, two) != 0);
    JS_FreeValue(ctx, one);
    JS_FreeValue(ctx, two);

    const char *json = "{\"x\": 42}";
    JSValue parsed = JS_ParseJSON2(ctx, json, strlen(json), "test.json", 0);
    assert(!JS_IsException(parsed));
    JSValue x = JS_GetPropertyStr(ctx, parsed, "x");
    assert(JS_VALUE_GET_FLOAT64(x) == 42.0);
    JS_FreeValue(ctx, x);
    JS_FreeValue(ctx, parsed);

    assert(JS_GetStripInfo(rt) == 0);
    JS_SetStripInfo(rt, JS_STRIP_SOURCE | JS_STRIP_DEBUG);
    assert(JS_GetStripInfo(rt) == (JS_STRIP_SOURCE | JS_STRIP_DEBUG));
    JS_SetStripInfo(rt, 0);

    JS_AddIntrinsicStringNormalize(ctx);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static char print_buffer[256];
static size_t print_len = 0;
static void print_write(void *opaque, const char *buf, size_t len) {
    (void)opaque;
    if (print_len + len < sizeof(print_buffer)) {
        memcpy(print_buffer + print_len, buf, len);
        print_len += len;
    }
}

static void test_print_value(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSPrintValueOptions options;
    JS_PrintValueSetDefaultOptions(&options);
    assert(options.max_depth == 0);
    assert(options.max_string_length == 0);

    JSValue val = JS_Eval(ctx, "42", strlen("42"), "test.js", 0);
    print_len = 0;
    JS_PrintValue(ctx, print_write, NULL, val, &options);
    assert(print_len > 0);
    print_buffer[print_len] = 0;
    assert(strcmp(print_buffer, "42") == 0);
    JS_FreeValue(ctx, val);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

void qjs_test_api_delta(void) { test_api_delta(); }
void qjs_test_predicates(void) { test_predicates(); }
void qjs_test_bellard_only(void) { test_bellard_only(); }
void qjs_test_print_value(void) { test_print_value(); }
