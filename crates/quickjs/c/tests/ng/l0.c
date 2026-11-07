#include "quickjs.h"
#include <assert.h>
#include <string.h>

static void test_eval_and_values(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue string = JS_Eval(ctx, "'hello'", 7, "test.js", 0);
    assert(JS_IsString(string));
    const char *text = JS_ToCStringLen2(ctx, NULL, string, 0);
    assert(strcmp(text, "hello") == 0);
    JS_FreeCString(ctx, text);
    JS_FreeValue(ctx, string);

    JSValue number = JS_Eval(ctx, "3.5", 3, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(number) == 3.5);
    JS_FreeValue(ctx, number);

    JSContext *dup = JS_DupContext(ctx);
    assert(dup == ctx);
    JS_FreeContext(dup);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_objects(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_IsObject(global));

    JSValue object = JS_NewObject(ctx);
    assert(JS_IsObject(object));

    JSValue key = JS_NewInt32(ctx, 42);
    assert(JS_SetPropertyStr(ctx, object, "x", key) == 0);

    JSValue got = JS_GetPropertyStr(ctx, object, "x");
    int32_t result = 0;
    assert(JS_ToInt32(ctx, &result, got) == 0);
    assert(result == 42);

    JS_FreeValue(ctx, got);
    JS_FreeValue(ctx, key);
    JS_FreeValue(ctx, object);
    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_exceptions(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue result = JS_Eval(ctx, "throw 'boom'", 12, "test.js", 0);
    assert(JS_IsException(result));
    assert(JS_HasException(ctx));
    JSValue exc = JS_GetException(ctx);
    assert(JS_IsString(exc));
    JS_FreeValue(ctx, exc);
    assert(!JS_HasException(ctx));

    JSValue err = JS_NewString(ctx, "custom");
    JSValue thrown = JS_Throw(ctx, err);
    assert(JS_IsException(thrown));
    assert(JS_HasException(ctx));
    JS_FreeValue(ctx, JS_GetException(ctx));
    JS_FreeValue(ctx, err);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_atoms_and_conversions(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSAtom atom = JS_NewAtom(ctx, "hello");
    assert(atom != 0);
    JSValue value = JS_AtomToString(ctx, atom);
    const char *text = JS_ToCStringLen2(ctx, NULL, value, 0);
    assert(strcmp(text, "hello") == 0);
    JS_FreeCString(ctx, text);
    JS_FreeValue(ctx, value);
    JS_FreeAtom(ctx, atom);

    JSValue str = JS_NewString(ctx, "42");
    JSValue number = JS_ToNumber(ctx, str);
    JS_FreeValue(ctx, str);
    assert(JS_VALUE_GET_FLOAT64(number) == 42.0);
    JS_FreeValue(ctx, number);

    JSValue empty = JS_NewString(ctx, "");
    int truth = JS_ToBool(ctx, empty);
    JS_FreeValue(ctx, empty);
    assert(truth == 0);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_errors(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue thrown = JS_ThrowTypeError(ctx, "oops %d", 42);
    assert(JS_IsException(thrown));
    assert(JS_HasException(ctx));
    JSValue exc = JS_GetException(ctx);
    const char *msg = JS_ToCStringLen2(ctx, NULL, exc, 0);
    assert(strstr(msg, "oops 42") != NULL);
    JS_FreeCString(ctx, msg);
    JS_FreeValue(ctx, exc);

    JSValue err = JS_NewError(ctx);
    assert(JS_IsObject(err));
    assert(JS_IsError(err));
    JS_FreeValue(ctx, err);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_allocator(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    void *p = js_malloc(ctx, 16);
    assert(p != NULL);
    assert(js_malloc_usable_size(ctx, p) >= 16);
    void *q = js_realloc(ctx, p, 64);
    assert(q != NULL);
    assert(js_malloc_usable_size(ctx, q) >= 64);
    js_free(ctx, q);

    void *z = js_mallocz(ctx, 8);
    assert(z != NULL);
    assert(((char *)z)[0] == 0);
    js_free(ctx, z);

    char *s = js_strdup(ctx, "dup");
    assert(strcmp(s, "dup") == 0);
    js_free(ctx, s);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_classes(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSClassID class_id = 0;
    JS_NewClassID(rt, &class_id);
    assert(class_id != 0);

    JSClassDef def;
    memset(&def, 0, sizeof(def));
    def.class_name = "TestClass";
    assert(JS_NewClass(rt, class_id, &def) == 0);
    assert(JS_IsRegisteredClass(rt, class_id));

    JSValue obj = JS_NewObjectClass(ctx, class_id);
    assert(JS_IsObject(obj));
    assert(JS_GetClassID(obj) == class_id);

    int marker = 0;
    assert(JS_SetOpaque(obj, &marker) == 0);
    assert(JS_GetOpaque(obj, class_id) == (void *)&marker);
    assert(JS_GetOpaque(obj, 999) == NULL);

    JSAtom name = JS_GetClassName(rt, class_id);
    const char *name_str = JS_AtomToCString(ctx, name);
    assert(strcmp(name_str, "TestClass") == 0);
    JS_FreeCString(ctx, name_str);
    JS_FreeAtom(ctx, name);

    JS_FreeValue(ctx, obj);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static JSClassID finalize_class_id = 0;
static int finalized = 0;

static void test_finalizer_callback(JSRuntime *rt, JSValueConst val) {
    int *marker = (int *)JS_GetOpaque(val, finalize_class_id);
    if (marker)
        finalized = *marker;
}

static JSValue test_call_callback(JSContext *ctx, JSValueConst func_obj,
                                  JSValueConst this_val, int argc,
                                  JSValueConst *argv, int flags) {
    return JS_NewInt32(ctx, 99);
}

static void test_class_callbacks(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    finalize_class_id = 0;
    JS_NewClassID(rt, &finalize_class_id);
    JSClassDef finalizer_def;
    memset(&finalizer_def, 0, sizeof(finalizer_def));
    finalizer_def.class_name = "FinalizeClass";
    finalizer_def.finalizer = test_finalizer_callback;
    assert(JS_NewClass(rt, finalize_class_id, &finalizer_def) == 0);

    JSValue obj = JS_NewObjectClass(ctx, finalize_class_id);
    int marker = 42;
    assert(JS_SetOpaque(obj, &marker) == 0);
    JS_FreeValue(ctx, obj);
    JS_RunGC(rt);
    JS_Eval(ctx, "1", 1, "test.js", 0);
    assert(finalized == 42);

    JSClassID call_id = 0;
    JS_NewClassID(rt, &call_id);
    JSClassDef call_def;
    memset(&call_def, 0, sizeof(call_def));
    call_def.class_name = "CallableClass";
    call_def.call = test_call_callback;
    assert(JS_NewClass(rt, call_id, &call_def) == 0);

    JSValue callable = JS_NewObjectClass(ctx, call_id);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "f", callable) == 0);
    JSValue result = JS_Eval(ctx, "f()", 3, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(result) == 99.0);
    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, callable);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int exotic_has_property(JSContext *ctx, JSValueConst obj, JSAtom atom) {
    const char *name = JS_AtomToCString(ctx, atom);
    int result = (name != NULL && strcmp(name, "answer") == 0);
    if (name)
        JS_FreeCString(ctx, name);
    return result;
}

static int exotic_get_own_property(JSContext *ctx, JSPropertyDescriptor *desc,
                                   JSValueConst obj, JSAtom prop) {
    const char *name = JS_AtomToCString(ctx, prop);
    int is_answer = (name != NULL && strcmp(name, "answer") == 0);
    if (name)
        JS_FreeCString(ctx, name);
    if (!is_answer)
        return 0;
    desc->flags = JS_PROP_HAS_VALUE | JS_PROP_HAS_WRITABLE |
                  JS_PROP_HAS_ENUMERABLE | JS_PROP_HAS_CONFIGURABLE |
                  JS_PROP_WRITABLE | JS_PROP_ENUMERABLE | JS_PROP_CONFIGURABLE;
    desc->value = JS_NewInt32(ctx, 42);
    desc->getter = JS_UNDEFINED;
    desc->setter = JS_UNDEFINED;
    return 1;
}

static void test_exotic_methods(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSClassID class_id = 0;
    JS_NewClassID(rt, &class_id);
    JSClassDef def;
    memset(&def, 0, sizeof(def));
    def.class_name = "ExoticClass";
    JSClassExoticMethods exotic;
    memset(&exotic, 0, sizeof(exotic));
    exotic.get_own_property = exotic_get_own_property;
    exotic.has_property = exotic_has_property;
    def.exotic = &exotic;
    assert(JS_NewClass(rt, class_id, &def) == 0);

    JSValue obj = JS_NewObjectClass(ctx, class_id);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "obj", obj) == 0);

    JSValue answer = JS_Eval(ctx, "obj.answer", 10, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(answer) == 42.0);
    JS_FreeValue(ctx, answer);

    JSValue has = JS_Eval(ctx, "'answer' in obj", 15, "test.js", 0);
    assert(JS_VALUE_GET_BOOL(has) == 1);
    JS_FreeValue(ctx, has);

    JS_FreeValue(ctx, obj);
    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static JSValue cfunc_double(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    int32_t a;
    JS_ToInt32(ctx, &a, argv[0]);
    return JS_NewInt32(ctx, a * 2);
}

static JSValue cfunc_add(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    int32_t a, b;
    JS_ToInt32(ctx, &a, argv[0]);
    JS_ToInt32(ctx, &b, argv[1]);
    return JS_NewInt32(ctx, a + b);
}

static void test_cfunctions(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue global = JS_GetGlobalObject(ctx);
    JSValue fn = JS_NewCFunction(ctx, cfunc_add, "add", 2);
    assert(JS_SetPropertyStr(ctx, global, "add", fn) == 0);
    JS_FreeValue(ctx, fn);

    JSValue sum = JS_Eval(ctx, "add(3, 4)", 9, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(sum) == 7.0);
    JS_FreeValue(ctx, sum);

    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_function_lists(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    static const JSCFunctionListEntry methods[] = {
        JS_CFUNC_DEF("double", 1, cfunc_double),
        JS_PROP_INT32_DEF("answer", 42, JS_PROP_C_W_E),
        JS_PROP_DOUBLE_DEF("pi", 3.5, JS_PROP_C_W_E),
    };

    JSValue obj = JS_NewObject(ctx);
    assert(JS_SetPropertyFunctionList(ctx, obj, methods, 3) == 0);

    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "obj", obj) == 0);

    JSValue d = JS_Eval(ctx, "obj.double(21)", 14, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(d) == 42.0);
    JS_FreeValue(ctx, d);

    JSValue answer = JS_Eval(ctx, "obj.answer", 10, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(answer) == 42.0);
    JS_FreeValue(ctx, answer);

    JSValue pi = JS_Eval(ctx, "obj.pi", 6, "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(pi) == 3.5);
    JS_FreeValue(ctx, pi);

    JS_FreeValue(ctx, obj);
    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_boxed_string_gc(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue s = JS_Eval(ctx, "'hello world hello world'", 25, "test.js", 0);
    JS_RunGC(rt);
    const char *text = JS_ToCStringLen2(ctx, NULL, s, 0);
    assert(text != NULL && strcmp(text, "hello world hello world") == 0);
    JS_FreeCString(ctx, text);
    JS_FreeValue(ctx, s);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_introspection(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    assert(strcmp(JS_GetVersion(), "0.18.0") == 0);

    JSValue fn = JS_NewCFunction(ctx, cfunc_add, "add", 2);
    assert(JS_IsFunction(ctx, fn));
    assert(JS_IsConstructor(ctx, fn));
    JS_FreeValue(ctx, fn);

    JSValue num = JS_NewInt32(ctx, 42);
    assert(!JS_IsFunction(ctx, num));
    assert(!JS_IsConstructor(ctx, num));
    assert(!JS_IsArray(num));
    assert(!JS_IsProxy(num));
    JS_FreeValue(ctx, num);

    JSValue arr = JS_Eval(ctx, "[1,2,3]", 7, "test.js", 0);
    assert(JS_IsArray(arr));
    assert(!JS_IsProxy(arr));
    JS_FreeValue(ctx, arr);

    JSValue proxy = JS_Eval(ctx, "new Proxy({}, {})", 17, "test.js", 0);
    assert(JS_IsProxy(proxy));
    assert(!JS_IsArray(proxy));
    JS_FreeValue(ctx, proxy);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_array_buffer(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    const uint8_t bytes[] = { 1, 2, 3, 4 };
    JSValue ab = JS_NewArrayBufferCopy(ctx, bytes, 4);
    assert(JS_IsArrayBuffer(ab));

    size_t size = 0;
    uint8_t *data = JS_GetArrayBuffer(ctx, &size, ab);
    assert(data != NULL);
    assert(size == 4);
    assert(data[0] == 1 && data[3] == 4);

    JS_DetachArrayBuffer(ctx, ab);
    uint8_t *detached = JS_GetArrayBuffer(ctx, &size, ab);
    assert(detached == NULL);

    JS_FreeValue(ctx, ab);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_typed_array(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    const uint8_t bytes[] = { 10, 20, 30 };
    JSValue arr = JS_NewUint8ArrayCopy(ctx, bytes, 3);
    assert(JS_GetTypedArrayType(arr) == JS_TYPED_ARRAY_UINT8);

    size_t offset = 0, length = 0, bytes_per_element = 0;
    JSValue buffer = JS_GetTypedArrayBuffer(ctx, arr, &offset, &length, &bytes_per_element);
    assert(JS_IsArrayBuffer(buffer));
    assert(offset == 0);
    assert(length == 3);
    assert(bytes_per_element == 1);

    size_t size = 0;
    uint8_t *data = JS_GetArrayBuffer(ctx, &size, buffer);
    assert(data != NULL);
    assert(size == 3);
    assert(data[0] == 10 && data[2] == 30);

    JS_FreeValue(ctx, buffer);
    JS_FreeValue(ctx, arr);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_promises(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue resolved = JS_NewSettledPromise(ctx, 0, JS_NewInt32(ctx, 42));
    assert(JS_IsPromise(resolved));
    assert(JS_PromiseState(ctx, resolved) == JS_PROMISE_FULFILLED);
    JSValue result = JS_PromiseResult(ctx, resolved);
    assert(JS_VALUE_GET_FLOAT64(result) == 42.0);
    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, resolved);

    JSValue rejected = JS_NewSettledPromise(ctx, 1, JS_NewInt32(ctx, 99));
    assert(JS_IsPromise(rejected));
    assert(JS_PromiseState(ctx, rejected) == JS_PROMISE_REJECTED);
    JS_FreeValue(ctx, rejected);

    JSValue num = JS_NewInt32(ctx, 1);
    assert(!JS_IsPromise(num));
    assert(JS_PromiseState(ctx, num) == JS_PROMISE_NOT_A_PROMISE);
    JS_FreeValue(ctx, num);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int job_ran = 0;

static JSValue job_func(JSContext *ctx, int argc, JSValueConst *argv) {
    job_ran = 1;
    return JS_UNDEFINED;
}

static void test_jobs(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    job_ran = 0;
    assert(!JS_IsJobPending(rt));
    assert(JS_EnqueueJob(ctx, job_func, 0, NULL) == 0);
    assert(JS_IsJobPending(rt));

    JSContext *job_ctx = NULL;
    assert(JS_ExecutePendingJob(rt, &job_ctx) == 0);
    assert(job_ran == 1);
    assert(!JS_IsJobPending(rt));

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_source_module(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    const char *src =
        "export const answer = 42; export function double(x) { return x * 2; }";
    JSValue module = JS_Eval(ctx, src, strlen(src), "mod.js",
                             JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
    assert(JS_IsModule(module));

    assert(JS_ResolveModule(ctx, module) == 0);

    JSValue promise = JS_EvalFunction(ctx, module);
    assert(JS_IsPromise(promise));

    JSModuleDef *m = (JSModuleDef *)JS_VALUE_GET_PTR(module);
    JSValue ns = JS_GetModuleNamespace(ctx, m);
    assert(JS_IsObject(ns));

    JSValue answer = JS_GetPropertyStr(ctx, ns, "answer");
    assert(JS_VALUE_GET_FLOAT64(answer) == 42.0);

    JSValue double_fn = JS_GetPropertyStr(ctx, ns, "double");
    assert(JS_IsFunction(ctx, double_fn));

    JS_FreeValue(ctx, double_fn);
    JS_FreeValue(ctx, answer);
    JS_FreeValue(ctx, ns);
    JS_FreeValue(ctx, promise);
    JS_FreeValue(ctx, module);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int c_module_init(JSContext *ctx, JSModuleDef *m) {
    JSValue greeting = JS_NewString(ctx, "hi");
    assert(JS_AddModuleExport(ctx, m, "value") == 0);
    assert(JS_AddModuleExport(ctx, m, "greeting") == 0);
    assert(JS_SetModuleExport(ctx, m, "value", JS_NewInt32(ctx, 7)) == 0);
    assert(JS_SetModuleExport(ctx, m, "greeting", greeting) == 0);
    JS_FreeValue(ctx, greeting);
    return 0;
}

static void test_c_module(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSModuleDef *m = JS_NewCModule(ctx, "mymod", c_module_init);
    assert(m != NULL);

    JSValue ns = JS_GetModuleNamespace(ctx, m);
    assert(JS_IsObject(ns));

    JSValue value = JS_GetPropertyStr(ctx, ns, "value");
    assert(JS_VALUE_GET_FLOAT64(value) == 7.0);

    JSValue greeting = JS_GetPropertyStr(ctx, ns, "greeting");
    const char *text = JS_ToCStringLen2(ctx, NULL, greeting, 0);
    assert(text != NULL && strcmp(text, "hi") == 0);
    JS_FreeCString(ctx, text);

    JSAtom name = JS_GetModuleName(ctx, m);
    const char *name_str = JS_AtomToCString(ctx, name);
    assert(strcmp(name_str, "mymod") == 0);
    JS_FreeCString(ctx, name_str);
    JS_FreeAtom(ctx, name);

    JS_FreeValue(ctx, greeting);
    JS_FreeValue(ctx, value);
    JS_FreeValue(ctx, ns);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_promise_capability(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue resolving[2];
    JSValue promise = JS_NewPromiseCapability(ctx, resolving);
    assert(JS_IsPromise(promise));
    assert(JS_PromiseState(ctx, promise) == JS_PROMISE_PENDING);
    assert(JS_IsFunction(ctx, resolving[0]));
    assert(JS_IsFunction(ctx, resolving[1]));

    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "resolve", resolving[0]) == 0);
    assert(JS_SetPropertyStr(ctx, global, "p", promise) == 0);

    JSValue result = JS_Eval(ctx, "resolve(42)", strlen("resolve(42)"), "test.js", 0);
    assert(!JS_IsException(result));
    JS_FreeValue(ctx, result);

    assert(JS_PromiseState(ctx, promise) == JS_PROMISE_FULFILLED);
    JSValue value = JS_PromiseResult(ctx, promise);
    assert(JS_VALUE_GET_FLOAT64(value) == 42.0);

    JS_FreeValue(ctx, value);
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, resolving[0]);
    JS_FreeValue(ctx, resolving[1]);
    JS_FreeValue(ctx, promise);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_builtin_introspection(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue date = JS_Eval(ctx, "new Date(0)", strlen("new Date(0)"), "test.js", 0);
    assert(JS_IsDate(date));
    assert(!JS_IsRegExp(date));
    assert(!JS_IsMap(date));
    JS_FreeValue(ctx, date);

    JSValue re = JS_Eval(ctx, "/abc/", strlen("/abc/"), "test.js", 0);
    assert(JS_IsRegExp(re));
    assert(!JS_IsDate(re));
    JS_FreeValue(ctx, re);

    JSValue map = JS_Eval(ctx, "new Map()", strlen("new Map()"), "test.js", 0);
    assert(JS_IsMap(map));
    assert(!JS_IsSet(map));
    JS_FreeValue(ctx, map);

    JSValue set = JS_Eval(ctx, "new Set()", strlen("new Set()"), "test.js", 0);
    assert(JS_IsSet(set));
    assert(!JS_IsMap(set));
    JS_FreeValue(ctx, set);

    JSValue dv = JS_Eval(ctx, "new DataView(new ArrayBuffer(8))",
                          strlen("new DataView(new ArrayBuffer(8))"), "test.js", 0);
    assert(JS_IsDataView(dv));
    assert(!JS_IsArray(dv));
    JS_FreeValue(ctx, dv);

    JSValue obj = JS_Eval(ctx, "({})", strlen("({})"), "test.js", 0);
    assert(!JS_IsDate(obj));
    assert(!JS_IsRegExp(obj));
    assert(!JS_IsMap(obj));
    assert(!JS_IsSet(obj));
    assert(!JS_IsDataView(obj));
    JS_FreeValue(ctx, obj);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int getset_observed = 0;

static JSValue test_getter(JSContext *ctx, JSValueConst this_val) {
    return JS_NewInt32(ctx, 123);
}

static JSValue test_setter(JSContext *ctx, JSValueConst this_val, JSValueConst val) {
    int32_t v = 0;
    JS_ToInt32(ctx, &v, val);
    getset_observed = v;
    return JS_UNDEFINED;
}

static void test_function_list_accessors(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    static const JSCFunctionListEntry nested[] = {
        JS_PROP_INT32_DEF("answer", 42, JS_PROP_C_W_E),
    };
    static const JSCFunctionListEntry entries[] = {
        JS_CGETSET_DEF("value", test_getter, test_setter),
        JS_OBJECT_DEF("nested", nested, 1, JS_PROP_C_W_E),
        JS_PROP_INT32_DEF("base", 7, JS_PROP_C_W_E),
        JS_ALIAS_DEF("alias", "base"),
    };

    JSValue obj = JS_NewObject(ctx);
    assert(JS_SetPropertyFunctionList(ctx, obj, entries, 4) == 0);

    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "obj", obj) == 0);

    JSValue v = JS_Eval(ctx, "obj.value", strlen("obj.value"), "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(v) == 123.0);
    JS_FreeValue(ctx, v);

    getset_observed = 0;
    JSValue s = JS_Eval(ctx, "obj.value = 42", strlen("obj.value = 42"), "test.js", 0);
    assert(!JS_IsException(s));
    JS_FreeValue(ctx, s);
    assert(getset_observed == 42);

    JSValue n = JS_Eval(ctx, "obj.nested.answer", strlen("obj.nested.answer"), "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(n) == 42.0);
    JS_FreeValue(ctx, n);

    JSValue a = JS_Eval(ctx, "obj.alias", strlen("obj.alias"), "test.js", 0);
    assert(JS_VALUE_GET_FLOAT64(a) == 7.0);
    JS_FreeValue(ctx, a);

    JS_FreeValue(ctx, obj);
    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int define_called = 0;

static int exotic_define(JSContext *ctx, JSValueConst obj, JSAtom prop,
                         JSValueConst value, JSValueConst getter,
                         JSValueConst setter, int flags) {
    const char *name = JS_AtomToCString(ctx, prop);
    if (name && strcmp(name, "x") == 0)
        define_called = 1;
    if (name)
        JS_FreeCString(ctx, name);
    return 1;
}

static int exotic_names(JSContext *ctx, JSPropertyEnum **ptab, uint32_t *plen,
                        JSValueConst obj) {
    JSPropertyEnum *tab = js_malloc(ctx, 2 * sizeof(JSPropertyEnum));
    if (!tab)
        return -1;
    tab[0].is_enumerable = 1;
    tab[0].atom = JS_NewAtom(ctx, "alpha");
    tab[1].is_enumerable = 1;
    tab[1].atom = JS_NewAtom(ctx, "beta");
    *ptab = tab;
    *plen = 2;
    return 0;
}

static void test_exotic_define_and_names(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSClassID class_id = 0;
    JS_NewClassID(rt, &class_id);
    JSClassDef def;
    memset(&def, 0, sizeof(def));
    def.class_name = "DefineClass";
    JSClassExoticMethods exotic;
    memset(&exotic, 0, sizeof(exotic));
    exotic.define_own_property = exotic_define;
    exotic.get_own_property_names = exotic_names;
    def.exotic = &exotic;
    assert(JS_NewClass(rt, class_id, &def) == 0);

    JSValue obj = JS_NewObjectClass(ctx, class_id);
    JSValue global = JS_GetGlobalObject(ctx);
    assert(JS_SetPropertyStr(ctx, global, "obj", obj) == 0);

    define_called = 0;
    JSValue r = JS_Eval(ctx, "Object.defineProperty(obj, 'x', {value: 1})",
                        strlen("Object.defineProperty(obj, 'x', {value: 1})"), "test.js", 0);
    assert(!JS_IsException(r));
    JS_FreeValue(ctx, r);
    assert(define_called == 1);

    JSValue names = JS_Eval(ctx, "Object.getOwnPropertyNames(obj)",
                            strlen("Object.getOwnPropertyNames(obj)"), "test.js", 0);
    assert(!JS_IsException(names));
    JSValue alpha = JS_GetPropertyUint32(ctx, names, 0);
    const char *alpha_s = JS_ToCStringLen2(ctx, NULL, alpha, 0);
    assert(alpha_s && strcmp(alpha_s, "alpha") == 0);
    JS_FreeCString(ctx, alpha_s);
    JS_FreeValue(ctx, alpha);

    JSValue beta = JS_GetPropertyUint32(ctx, names, 1);
    const char *beta_s = JS_ToCStringLen2(ctx, NULL, beta, 0);
    assert(beta_s && strcmp(beta_s, "beta") == 0);
    JS_FreeCString(ctx, beta_s);
    JS_FreeValue(ctx, beta);

    JS_FreeValue(ctx, names);
    JS_FreeValue(ctx, obj);
    JS_FreeValue(ctx, global);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_host_buffer(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    uint8_t bytes[] = { 5, 6, 7, 8 };
    JSValue ab = JS_NewArrayBuffer(ctx, bytes, 4, 0, NULL, NULL, 0);
    assert(JS_IsArrayBuffer(ab));

    size_t size = 0;
    uint8_t *data = JS_GetArrayBuffer(ctx, &size, ab);
    assert(data != NULL);
    assert(size == 4);
    assert(data == bytes);
    assert(data[0] == 5 && data[3] == 8);

    JS_FreeValue(ctx, ab);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_host_uint8_array(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    uint8_t bytes[] = { 10, 20, 30 };
    JSValue arr = JS_NewUint8Array(ctx, bytes, 3, NULL, NULL, 0);
    assert(JS_GetTypedArrayType(arr) == JS_TYPED_ARRAY_UINT8);

    size_t offset = 0, length = 0, bpe = 0;
    JSValue buffer = JS_GetTypedArrayBuffer(ctx, arr, &offset, &length, &bpe);
    assert(JS_IsArrayBuffer(buffer));
    assert(length == 3);
    assert(bpe == 1);

    size_t size = 0;
    uint8_t *data = JS_GetArrayBuffer(ctx, &size, buffer);
    assert(data != NULL && size == 3);
    assert(data == bytes);
    assert(data[0] == 10 && data[2] == 30);

    JS_FreeValue(ctx, buffer);
    JS_FreeValue(ctx, arr);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static void test_new_typed_array(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue len = JS_NewInt32(ctx, 3);
    JSValue arr = JS_NewTypedArray(ctx, 1, &len, JS_TYPED_ARRAY_FLOAT64);
    assert(JS_GetTypedArrayType(arr) == JS_TYPED_ARRAY_FLOAT64);
    JS_FreeValue(ctx, len);

    size_t offset = 0, length = 0, bpe = 0;
    JSValue buffer = JS_GetTypedArrayBuffer(ctx, arr, &offset, &length, &bpe);
    assert(JS_IsArrayBuffer(buffer));
    assert(length == 24);
    assert(bpe == 8);

    JS_FreeValue(ctx, buffer);
    JS_FreeValue(ctx, arr);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int loader_called = 0;

static JSModuleDef *test_loader(JSContext *ctx, const char *module_name, void *opaque) {
    loader_called = 1;
    if (strcmp(module_name, "dep") == 0) {
        const char *src = "export const answer = 42;";
        JSValue mod = JS_Eval(ctx, src, strlen(src), "dep",
                              JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
        if (JS_IsException(mod))
            return NULL;
        return (JSModuleDef *)JS_VALUE_GET_PTR(mod);
    }
    return NULL;
}

static void test_module_loader(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    loader_called = 0;
    JS_SetModuleLoaderFunc(rt, NULL, test_loader, NULL);

    const char *src = "import { answer } from 'dep'; export const doubled = answer * 2;";
    JSValue mod = JS_Eval(ctx, src, strlen(src), "main.js",
                          JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
    assert(JS_IsModule(mod));
    assert(loader_called == 0);

    assert(JS_ResolveModule(ctx, mod) == 0);
    assert(loader_called == 1);

    JSValue promise = JS_EvalFunction(ctx, mod);
    assert(!JS_IsException(promise));

    JSModuleDef *m = (JSModuleDef *)JS_VALUE_GET_PTR(mod);
    JSValue ns = JS_GetModuleNamespace(ctx, m);
    assert(JS_IsObject(ns));
    JSValue doubled = JS_GetPropertyStr(ctx, ns, "doubled");
    assert(JS_VALUE_GET_FLOAT64(doubled) == 84.0);

    JS_FreeValue(ctx, doubled);
    JS_FreeValue(ctx, ns);
    JS_FreeValue(ctx, promise);
    JS_FreeValue(ctx, mod);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int mark_called = 0;
static void mark_func(JSRuntime *rt, JSGCObjectHeader *gp) {
    (void)rt;
    (void)gp;
    mark_called = 1;
}

static void test_mark_value(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    JSValue val = JS_Eval(ctx, "42", strlen("42"), "test.js", 0);
    mark_called = 0;
    JS_MarkValue(rt, val, mark_func);
    // No-op: the facade's box registry roots host-held values, so the mark
    // walk has nothing to record.
    assert(mark_called == 0);
    JS_FreeValue(ctx, val);

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int loader2_called = 0;
static JSModuleDef *test_loader2(JSContext *ctx, const char *module_name, void *opaque,
                                 JSValueConst attributes) {
    (void)opaque;
    loader2_called = 1;
    assert(JS_IsObject(attributes));
    if (strcmp(module_name, "dep") == 0) {
        const char *src = "export const answer = 42;";
        JSValue mod = JS_Eval(ctx, src, strlen(src), "dep",
                              JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
        if (JS_IsException(mod))
            return NULL;
        return (JSModuleDef *)JS_VALUE_GET_PTR(mod);
    }
    return NULL;
}

static void test_module_loader_attrs(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    loader2_called = 0;
    JS_SetModuleLoaderFunc2(rt, NULL, test_loader2, NULL, NULL);

    const char *src = "import { answer } from 'dep'; export const doubled = answer * 2;";
    JSValue mod = JS_Eval(ctx, src, strlen(src), "main.js",
                          JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
    assert(JS_IsModule(mod));
    assert(loader2_called == 0);

    assert(JS_ResolveModule(ctx, mod) == 0);
    assert(loader2_called == 1);

    JSValue promise = JS_EvalFunction(ctx, mod);
    assert(!JS_IsException(promise));

    JSModuleDef *m = (JSModuleDef *)JS_VALUE_GET_PTR(mod);
    JSValue ns = JS_GetModuleNamespace(ctx, m);
    assert(JS_IsObject(ns));
    JSValue doubled = JS_GetPropertyStr(ctx, ns, "doubled");
    assert(JS_VALUE_GET_FLOAT64(doubled) == 84.0);

    JS_FreeValue(ctx, doubled);
    JS_FreeValue(ctx, ns);
    JS_FreeValue(ctx, promise);
    JS_FreeValue(ctx, mod);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

static int dynamic_called = 0;
static JSValue dynamic_token;
static void dynamic_import_cb(JSContext *ctx, const char *basename, const char *filename,
                              JSValue token, JSValueConst attributes, void *opaque) {
    (void)ctx;
    (void)basename;
    (void)filename;
    (void)attributes;
    (void)opaque;
    dynamic_called = 1;
    dynamic_token = token;
}

static void test_dynamic_import(void) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);

    dynamic_called = 0;
    JS_SetDynamicImportFunc(rt, dynamic_import_cb, NULL);

    const char *src = "import('dep')";
    JSValue result = JS_Eval(ctx, src, strlen(src), "main.js", 0);
    assert(!JS_IsException(result));
    assert(JS_IsPromise(result));
    assert(dynamic_called == 1);

    const char *mod_src = "export const answer = 42;";
    JSValue mod = JS_Eval(ctx, mod_src, strlen(mod_src), "dep",
                          JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
    assert(!JS_IsException(mod));

    assert(JS_FinishDynamicImport(ctx, dynamic_token, mod, JS_UNDEFINED) == 0);
    assert(JS_PromiseState(ctx, result) == JS_PROMISE_FULFILLED);

    JS_FreeValue(ctx, dynamic_token);
    JS_FreeValue(ctx, mod);
    JS_FreeValue(ctx, result);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
}

void qjs_test_eval_and_values(void) { test_eval_and_values(); }
void qjs_test_objects(void) { test_objects(); }
void qjs_test_exceptions(void) { test_exceptions(); }
void qjs_test_atoms_and_conversions(void) { test_atoms_and_conversions(); }
void qjs_test_errors(void) { test_errors(); }
void qjs_test_allocator(void) { test_allocator(); }
void qjs_test_classes(void) { test_classes(); }
void qjs_test_class_callbacks(void) { test_class_callbacks(); }
void qjs_test_exotic_methods(void) { test_exotic_methods(); }
void qjs_test_exotic_define_and_names(void) { test_exotic_define_and_names(); }
void qjs_test_cfunctions(void) { test_cfunctions(); }
void qjs_test_function_lists(void) { test_function_lists(); }
void qjs_test_function_list_accessors(void) { test_function_list_accessors(); }
void qjs_test_boxed_string_gc(void) { test_boxed_string_gc(); }
void qjs_test_introspection(void) { test_introspection(); }
void qjs_test_builtin_introspection(void) { test_builtin_introspection(); }
void qjs_test_array_buffer(void) { test_array_buffer(); }
void qjs_test_typed_array(void) { test_typed_array(); }
void qjs_test_host_buffer(void) { test_host_buffer(); }
void qjs_test_host_uint8_array(void) { test_host_uint8_array(); }
void qjs_test_new_typed_array(void) { test_new_typed_array(); }
void qjs_test_promises(void) { test_promises(); }
void qjs_test_promise_capability(void) { test_promise_capability(); }
void qjs_test_jobs(void) { test_jobs(); }
void qjs_test_source_module(void) { test_source_module(); }
void qjs_test_c_module(void) { test_c_module(); }
void qjs_test_module_loader(void) { test_module_loader(); }
void qjs_test_mark_value(void) { test_mark_value(); }
void qjs_test_module_loader_attrs(void) { test_module_loader_attrs(); }
void qjs_test_dynamic_import(void) { test_dynamic_import(); }
