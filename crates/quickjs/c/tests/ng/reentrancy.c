#include "quickjs.h"
#include <assert.h>
#include <string.h>

// Phase 6.2: re-entrancy and errors.

// 1. A host callback re-enters the engine (eval, object creation) and throws;
//    the throw surfaces to the outer eval as a pending exception.
static JSValue reentrant_cb(JSContext *ctx, JSValueConst this_val, int argc,
                            JSValueConst *argv) {
    (void)this_val;
    (void)argc;
    (void)argv;
    JSValue obj = JS_NewObject(ctx);
    assert(JS_IsObject(obj));
    JSValue result = JS_Eval(ctx, "1 + 2", 5, "reentrant.js", 0);
    assert(!JS_IsException(result));
    assert(JS_VALUE_GET_FLOAT64(result) == 3.0);
    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, obj);
    return JS_ThrowTypeError(ctx, "reentrant boom");
}

static void test_reentrant_callback_throws(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue fn = JS_NewCFunction(ctx, reentrant_cb, "reentrant", 0);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "reentrant", fn) >= 0);

    JSValue result = JS_Eval(ctx, "reentrant()", strlen("reentrant()"), "test.js", 0);
    assert(JS_IsException(result));
    assert(JS_HasException(ctx));
    JSValue exc = JS_GetException(ctx);
    assert(!JS_HasException(ctx));
    const char *msg = JS_ToCStringLen2(ctx, NULL, exc, 0);
    assert(strstr(msg, "reentrant boom") != NULL);
    JS_FreeCString(ctx, msg);
    JS_FreeValue(ctx, exc);

    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, fn);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

// 4. JS_Throw/JS_GetException nesting: a throw sets the pending exception, a
//    get clears it, and a second throw works while none is pending.
static void test_exception_nesting(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue thrown1 = JS_ThrowTypeError(ctx, "first");
    assert(JS_IsException(thrown1));
    assert(JS_HasException(ctx));
    JSValue got1 = JS_GetException(ctx);
    assert(!JS_HasException(ctx));

    JSValue thrown2 = JS_ThrowTypeError(ctx, "second");
    assert(JS_IsException(thrown2));
    assert(JS_HasException(ctx));
    JSValue got2 = JS_GetException(ctx);
    assert(!JS_HasException(ctx));

    JS_FreeValue(ctx, thrown1);
    JS_FreeValue(ctx, got1);
    JS_FreeValue(ctx, thrown2);
    JS_FreeValue(ctx, got2);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

// 5. JS_ThrowOutOfMemory sets a pending exception.
static void test_throw_out_of_memory(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue thrown = JS_ThrowOutOfMemory(ctx);
    assert(JS_IsException(thrown));
    assert(JS_HasException(ctx));
    JSValue exc = JS_GetException(ctx);
    assert(!JS_HasException(ctx));

    JS_FreeValue(ctx, exc);
    JS_FreeValue(ctx, thrown);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

void qjs_test_reentrant_callback_throws(void) { test_reentrant_callback_throws(); }
void qjs_test_exception_nesting(void) { test_exception_nesting(); }
void qjs_test_throw_out_of_memory(void) { test_throw_out_of_memory(); }
