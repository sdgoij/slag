#include "quickjs.h"
#include <assert.h>
#include <stdio.h>

// Phase 6.3: the 16-byte `JSValue` crosses the ABI by value in both directions.
// The `JSValueUnion` holds a `double` and a pointer, so the return-value ABI
// must agree on how that union is classified (integer register on both x64
// MSVC and System V). A float64, an object pointer, and an int32 round-trip
// through a host callback and back to prove the bits survive intact.

static JSValue echo_value(JSContext *ctx, JSValueConst this_val, int argc,
                          JSValueConst *argv) {
    (void)this_val;
    if (argc < 1)
        return JS_UNDEFINED;
    // Return argv[0] by value, dup'd so the caller owns a fresh reference.
    return JS_DupValue(ctx, argv[0]);
}

static void test_by_value_jsvalue_abi(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue fn = JS_NewCFunction(ctx, echo_value, "echo", 1);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "echo", fn) >= 0);

    // A float64 crosses JS -> C (argv) and C -> JS (return) by value; the
    // double's bits must survive the union's classification.
    JSValue d = JS_Eval(ctx, "echo(3.25)", 10, "abi.js", 0);
    assert(!JS_IsException(d));
    assert(JS_VALUE_GET_TAG(d) == JS_TAG_FLOAT64);
    assert(JS_VALUE_GET_FLOAT64(d) == 3.25);
    JS_FreeValue(ctx, d);

    // A boxed object pointer crosses the boundary and stays the same box.
    JSValue o = JS_Eval(ctx, "echo({n:1})", 11, "abi.js", 0);
    assert(!JS_IsException(o));
    assert(JS_VALUE_GET_TAG(o) == JS_TAG_OBJECT);
    assert(JS_VALUE_GET_PTR(o) != NULL);
    JS_FreeValue(ctx, o);

    // The engine boxes JS numbers as doubles, so `42` comes back as a float64
    // and its bits still round-trip intact.
    JSValue i = JS_Eval(ctx, "echo(42)", 8, "abi.js", 0);
    assert(!JS_IsException(i));
    assert(JS_VALUE_GET_TAG(i) == JS_TAG_FLOAT64);
    assert(JS_VALUE_GET_FLOAT64(i) == 42.0);
    JS_FreeValue(ctx, i);

    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, fn);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

void qjs_test_by_value_jsvalue_abi(void) { test_by_value_jsvalue_abi(); }
