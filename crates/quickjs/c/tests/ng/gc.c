#include "quickjs.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

// Phase 6.1: lifetime and GC. These exercise the shared box registry and
// finalizer machinery (identical across the ng and bellard skins).

// The finalizer class shares one static def/class-id/counter: the facade keeps
// a *pointer* to the def, so it must outlive the registration (a stack-local
// def would dangle), and the counter is only ever touched by the one
// sequential `test_gc_lifetime` entry.
static JSClassID gc_id = 0;
static int gc_count = 0;
static JSClassDef gc_def;

static void gc_finalizer(JSRuntime *rt, JSValueConst val) {
    (void)rt;
    (void)val;
    gc_count++;
}

static void register_gc_class(JSRuntime *rt) {
    gc_id = 0;
    JS_NewClassID(rt, &gc_id);
    memset(&gc_def, 0, sizeof(gc_def));
    gc_def.class_name = "GCFinalizeClass";
    gc_def.finalizer = gc_finalizer;
    assert(JS_NewClass(rt, gc_id, &gc_def) == 0);
}

// 1. A host-held (boxed) value survives a deterministic collection.
static void test_host_value_survives_gc(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue obj = JS_NewObject(ctx);
    JSValue marker = JS_NewInt32(ctx, 7);
    assert(JS_SetPropertyStr(ctx, obj, "marker", marker) >= 0);
    JS_FreeValue(ctx, marker);

    JS_RunGC(rt);

    JSValue out = JS_GetPropertyStr(ctx, obj, "marker");
    assert(!JS_IsException(out));
    assert(JS_VALUE_GET_FLOAT64(out) == 7.0);
    JS_FreeValue(ctx, out);
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

// 3. Two refs to one object share a box pointer (canonicalized by object id).
static void test_box_canonicalization(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue obj = JS_NewObject(ctx);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "obj", obj) >= 0);

    JSValue again = JS_GetPropertyStr(ctx, global, "obj");
    assert(!JS_IsException(again));
    assert(JS_VALUE_GET_PTR(obj) == JS_VALUE_GET_PTR(again));

    JS_FreeValue(ctx, again);
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

// 4. Retained C-string pointers survive table growth and free correctly.
static void test_cstring_table_growth(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    const int n = 128;
    const char *strings[128];
    for (int i = 0; i < n; i++) {
        char buf[32];
        snprintf(buf, sizeof(buf), "string-%d", i);
        JSValue v = JS_NewStringLen(ctx, buf, strlen(buf));
        strings[i] = JS_ToCStringLen2(ctx, NULL, v, 0);
        assert(strings[i] != NULL);
        JS_FreeValue(ctx, v);
    }

    assert(strcmp(strings[0], "string-0") == 0);
    assert(strcmp(strings[n - 1], "string-127") == 0);

    for (int i = 0; i < n; i++)
        JS_FreeCString(ctx, strings[i]);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

// 5. Atoms are process-wide, so one made in context a reads in context b.
static void test_atom_lifetime_across_contexts(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *a = JS_NewContext(rt);
    JSContext *b = JS_NewContext(rt);

    JSAtom atom = JS_NewAtom(a, "shared");
    assert(atom != 0);

    const char *text = JS_AtomToCString(b, atom);
    assert(strcmp(text, "shared") == 0);
    JS_FreeCString(b, text);

    JS_FreeAtom(a, atom);
    JS_FreeContext(a);
    JS_FreeContext(b);
    JS_FreeRuntime(rt);
}

// 2, 6: the finalizer runs exactly once after the last host ref and a sweep.
static void test_gc_lifetime(void) {
    // 2. The last JS_FreeValue unroots; the following sweep finalizes it.
    {
        JSRuntime *rt = JS_NewRuntime();
        JSContext *ctx = JS_NewContext(rt);
        register_gc_class(rt);

        JSValue obj = JS_NewObjectClass(ctx, gc_id);
        gc_count = 0;
        // Rooted by the host box: a collection must not finalize it yet.
        JS_RunGC(rt);
        JS_Eval(ctx, "1", 1, "test.js", 0);
        assert(gc_count == 0);

        JS_FreeValue(ctx, obj);
        JS_RunGC(rt);
        JS_Eval(ctx, "1", 1, "test.js", 0);
        assert(gc_count == 1);

        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
    }

    // 6. The finalizer runs once, not on every later collection.
    {
        JSRuntime *rt = JS_NewRuntime();
        JSContext *ctx = JS_NewContext(rt);
        register_gc_class(rt);

        JSValue obj = JS_NewObjectClass(ctx, gc_id);
        gc_count = 0;
        JS_FreeValue(ctx, obj);

        JS_RunGC(rt);
        JS_Eval(ctx, "1", 1, "test.js", 0);
        assert(gc_count == 1);
        JS_RunGC(rt);
        JS_Eval(ctx, "1", 1, "test.js", 0);
        assert(gc_count == 1);

        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
    }
}

// 9. A host callback that releases its own context: the realm survives until
// the outer entry completes, so the call returns normally rather than tearing
// the running execution down mid-callback.
static JSValue free_own_context_cb(JSContext *ctx, JSValueConst this_val, int argc,
                                   JSValueConst *argv) {
    (void)this_val;
    (void)argc;
    (void)argv;
    JS_FreeContext(ctx);
    return JS_NewInt32(ctx, 42);
}

static void test_context_released_in_callback(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue fn = JS_NewCFunction(ctx, free_own_context_cb, "free_own", 0);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "free_own", fn) >= 0);

    JSValue result = JS_Eval(ctx, "free_own()", 10, "test.js", 0);
    assert(!JS_IsException(result));
    assert(JS_VALUE_GET_FLOAT64(result) == 42.0);
    JS_FreeValue(ctx, result);

    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, fn);
    JS_FreeRuntime(rt);
}

void qjs_test_host_value_survives_gc(void) { test_host_value_survives_gc(); }
void qjs_test_box_canonicalization(void) { test_box_canonicalization(); }
void qjs_test_cstring_table_growth(void) { test_cstring_table_growth(); }
void qjs_test_atom_lifetime_across_contexts(void) { test_atom_lifetime_across_contexts(); }
void qjs_test_gc_lifetime(void) { test_gc_lifetime(); }
void qjs_test_context_released_in_callback(void) { test_context_released_in_callback(); }
