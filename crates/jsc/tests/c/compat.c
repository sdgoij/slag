/*
 * C compat tests for the JSC C API drop-in. Each function exercises a slice
 * of the documented public API through the headers in include/JavaScriptCore
 * — the exact contract a real embedder compiles against.
 */

#include <JavaScriptCore/JSContextRef.h>
#include <JavaScriptCore/JSClassRef.h>
#include <JavaScriptCore/JSObjectRef.h>
#include <JavaScriptCore/JSPropertyNameArrayRef.h>
#include <JavaScriptCore/JSStringRef.h>
#include <JavaScriptCore/JSValueRef.h>

#include <assert.h>
#include <stdlib.h>
#include <string.h>

static JSStringRef str(const char* utf8) {
    return JSStringCreateWithUTF8CString(utf8);
}

static void assert_utf8(JSStringRef s, const char* expected) {
    size_t size = JSStringGetMaximumUTF8CStringSize(s);
    char* buffer = (char*)malloc(size);
    size_t written = JSStringGetUTF8CString(s, buffer, size);
    assert(written == strlen(expected) + 1);
    assert(strcmp(buffer, expected) == 0);
    free(buffer);
}

void test_strings(void) {
    JSStringRef s = str("h\u00e9llo");
    assert(JSStringGetLength(s) == 5);
    const JSChar* units = JSStringGetCharactersPtr(s);
    assert(units[0] == 'h');
    assert(units[1] == 0xE9);
    assert_utf8(s, "h\u00e9llo");

    JSStringRef same = JSStringCreateWithUTF8CString("h\u00e9llo");
    assert(JSStringIsEqual(s, same));
    assert(JSStringIsEqualToUTF8CString(s, "h\u00e9llo"));
    assert(!JSStringIsEqualToUTF8CString(s, "nope"));

    // UTF-16 entry point keeps lone surrogates exactly.
    JSChar units16[] = { 0xD800, 0x0041 };
    JSStringRef surrogate = JSStringCreateWithCharacters(units16, 2);
    assert(JSStringGetLength(surrogate) == 2);
    const JSChar* back = JSStringGetCharactersPtr(surrogate);
    assert(back[0] == 0xD800 && back[1] == 0x0041);

    JSStringRelease(s);
    JSStringRelease(same);
    JSStringRelease(surrogate);
}

void test_eval_numbers(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    assert(ctx != NULL);
    JSValueRef exception = NULL;

    JSStringRef script = str("1 + 2");
    JSValueRef result = JSEvaluateScript(ctx, script, NULL, NULL, 1, &exception);
    assert(exception == NULL);
    assert(result != NULL);
    assert(JSValueIsNumber(ctx, result));
    assert(JSValueGetType(ctx, result) == kJSTypeNumber);
    assert(JSValueToNumber(ctx, result, NULL) == 3.0);
    JSStringRelease(script);

    // The global object is reachable and writable from C.
    JSObjectRef global = JSContextGetGlobalObject(ctx);
    assert(global != NULL);
    JSStringRef name = str("answer");
    JSObjectSetProperty(ctx, global, name, JSValueMakeNumber(ctx, 42),
                        kJSPropertyAttributeNone, &exception);
    assert(exception == NULL);
    assert(JSObjectHasProperty(ctx, global, name));
    JSValueRef got = JSObjectGetProperty(ctx, global, name, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, got, NULL) == 42.0);

    JSStringRelease(name);
    JSGlobalContextRelease(ctx);
}

void test_exceptions(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSValueRef exception = NULL;

    JSStringRef script = str("throw new TypeError('boom')");
    JSValueRef result = JSEvaluateScript(ctx, script, NULL, NULL, 1, &exception);
    assert(result == NULL);
    assert(exception != NULL);
    assert(JSValueIsObject(ctx, exception));
    JSStringRef text = JSValueToStringCopy(ctx, exception, NULL);
    assert_utf8(text, "TypeError: boom");
    JSStringRelease(text);

    // The pending exception is cleared by a successful evaluation.
    exception = NULL;
    JSStringRef ok = str("1");
    result = JSEvaluateScript(ctx, ok, NULL, NULL, 1, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, result, NULL) == 1.0);

    // Syntax errors surface at JSCheckScriptSyntax.
    exception = NULL;
    JSStringRef bad = str("function (");
    assert(!JSCheckScriptSyntax(ctx, bad, NULL, 1, &exception));
    assert(exception != NULL);

    JSStringRelease(script);
    JSStringRelease(ok);
    JSStringRelease(bad);
    JSGlobalContextRelease(ctx);
}

void test_object_properties(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSValueRef exception = NULL;

    // Build an object from JSON and poke at it from C.
    JSStringRef json = str("{\"a\": 1, \"b\": \"two\"}");
    JSValueRef parsed = JSValueMakeFromJSONString(ctx, json);
    assert(parsed != NULL);
    assert(JSValueIsObject(ctx, parsed));

    JSStringRef a = str("a");
    JSValueRef v = JSObjectGetProperty(ctx, (JSObjectRef)parsed, a, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, v, NULL) == 1.0);

    JSStringRef b = str("b");
    JSValueRef bv = JSObjectGetProperty(ctx, (JSObjectRef)parsed, b, &exception);
    JSStringRef btext = JSValueToStringCopy(ctx, bv, NULL);
    assert_utf8(btext, "two");
    JSStringRelease(btext);

    // Property-name arrays enumerate the object's own enumerable names.
    JSPropertyNameArrayRef names = JSObjectCopyPropertyNames(ctx, (JSObjectRef)parsed);
    assert(names != NULL);
    size_t count = JSPropertyNameArrayGetCount(names);
    assert(count == 2);
    int saw_a = 0, saw_b = 0;
    for (size_t i = 0; i < count; i++) {
        JSStringRef name = JSPropertyNameArrayGetNameAtIndex(names, i);
        if (JSStringIsEqualToUTF8CString(name, "a")) saw_a = 1;
        if (JSStringIsEqualToUTF8CString(name, "b")) saw_b = 1;
        JSStringRelease(name);
    }
    assert(saw_a && saw_b);
    JSPropertyNameArrayRelease(names);

    // Deletion works from C.
    assert(JSObjectDeleteProperty(ctx, (JSObjectRef)parsed, a, &exception));
    assert(exception == NULL);
    assert(!JSObjectHasProperty(ctx, (JSObjectRef)parsed, a));

    JSStringRelease(json);
    JSStringRelease(a);
    JSStringRelease(b);
    JSGlobalContextRelease(ctx);
}

static JSValueRef add_callback(JSContextRef ctx, JSObjectRef function, JSObjectRef thisObject,
                               size_t argc, const JSValueRef argv[], JSValueRef* exception) {
    (void)function;
    (void)thisObject;
    (void)argc;
    double a = JSValueToNumber(ctx, argv[0], exception);
    double b = JSValueToNumber(ctx, argv[1], exception);
    return JSValueMakeNumber(ctx, a + b);
}

void test_host_function(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSValueRef exception = NULL;

    JSStringRef name = str("add");
    JSObjectRef fn = JSObjectMakeFunctionWithCallback(ctx, name, add_callback);
    assert(fn != NULL);
    assert(JSObjectIsFunction(ctx, fn));

    JSObjectRef global = JSContextGetGlobalObject(ctx);
    JSObjectSetProperty(ctx, global, name, fn, kJSPropertyAttributeNone, &exception);
    assert(exception == NULL);

    JSStringRef script = str("add(20, 22)");
    JSValueRef result = JSEvaluateScript(ctx, script, NULL, NULL, 1, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, result, NULL) == 42.0);
    JSStringRelease(script);

    // Host functions are callable directly from C too.
    JSValueRef args[] = { JSValueMakeNumber(ctx, 1), JSValueMakeNumber(ctx, 2) };
    JSValueRef direct = JSObjectCallAsFunction(ctx, fn, NULL, 2, args, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, direct, NULL) == 3.0);

    // `new Function` from source (JSObjectMakeFunction).
    JSStringRef body = str("return x + 1");
    JSStringRef param = str("x");
    const JSStringRef params[] = { param };
    JSObjectRef fn2 = JSObjectMakeFunction(ctx, NULL, 1, params, body, NULL, 1, &exception);
    assert(exception == NULL);
    JSValueRef call_args[] = { JSValueMakeNumber(ctx, 41) };
    JSValueRef made = JSObjectCallAsFunction(ctx, fn2, NULL, 1, call_args, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, made, NULL) == 42.0);

    JSStringRelease(name);
    JSStringRelease(body);
    JSStringRelease(param);
    JSGlobalContextRelease(ctx);
}

void test_json(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSStringRef json = str("{\"a\": [1, 2, 3]}");
    JSValueRef parsed = JSValueMakeFromJSONString(ctx, json);
    assert(parsed != NULL);
    JSStringRef out = JSValueCreateJSONString(ctx, parsed, 0, NULL);
    assert_utf8(out, "{\"a\":[1,2,3]}");
    JSStringRelease(out);
    JSStringRelease(json);
    JSGlobalContextRelease(ctx);
}

static void* g_private_marker = (void*)0x1234;
static int g_get_calls = 0;

static JSValueRef get_thing(JSContextRef ctx, JSObjectRef object, JSStringRef propertyName,
                            JSValueRef* exception) {
    (void)object;
    (void)exception;
    if (JSStringIsEqualToUTF8CString(propertyName, "thing")) {
        g_get_calls++;
        return JSValueMakeNumber(ctx, 7);
    }
    return NULL;
}

static JSValueRef call_thing(JSContextRef ctx, JSObjectRef function, JSObjectRef thisObject,
                             size_t argc, const JSValueRef argv[], JSValueRef* exception) {
    (void)function;
    (void)thisObject;
    (void)argc;
    (void)argv;
    (void)exception;
    JSStringRef text = str("called");
    JSValueRef result = JSValueMakeString(ctx, text);
    JSStringRelease(text);
    return result;
}

void test_class_callbacks(void) {
    JSClassDefinition def;
    memset(&def, 0, sizeof(def));
    def.getProperty = get_thing;
    def.callAsFunction = call_thing;
    JSClassRef cls = JSClassCreate(&def);
    assert(cls != NULL);

    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSValueRef exception = NULL;

    JSObjectRef obj = JSObjectMake(ctx, cls, g_private_marker);
    assert(obj != NULL);
    assert(JSObjectGetPrivate(obj) == g_private_marker);

    // The getProperty callback intercepts property reads.
    JSStringRef thing = str("thing");
    JSValueRef v = JSObjectGetProperty(ctx, obj, thing, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, v, NULL) == 7.0);
    assert(g_get_calls == 1);
    JSStringRelease(thing);

    // The class object is callable and dispatches to callAsFunction.
    assert(JSObjectIsFunction(ctx, obj));
    JSValueRef result = JSObjectCallAsFunction(ctx, obj, NULL, 0, NULL, &exception);
    assert(exception == NULL);
    JSStringRef text = JSValueToStringCopy(ctx, result, NULL);
    assert_utf8(text, "called");
    JSStringRelease(text);

    // JSValueIsObjectOfClass recognizes the class.
    assert(JSValueIsObjectOfClass(ctx, obj, cls));

    // JSObjectSetPrivate replaces the private data.
    void* new_marker = (void*)0x5678;
    assert(JSObjectSetPrivate(obj, new_marker));
    assert(JSObjectGetPrivate(obj) == new_marker);

    JSClassRelease(cls);
    JSGlobalContextRelease(ctx);
}

void test_arrays_and_indices(void) {
    JSGlobalContextRef ctx = JSGlobalContextCreate(NULL);
    JSValueRef exception = NULL;

    JSStringRef script = str("[10, 20, 30]");
    JSValueRef array = JSEvaluateScript(ctx, script, NULL, NULL, 1, &exception);
    assert(exception == NULL);
    assert(JSValueIsArray(ctx, array));

    JSValueRef v = JSObjectGetPropertyAtIndex(ctx, (JSObjectRef)array, 1, &exception);
    assert(exception == NULL);
    assert(JSValueToNumber(ctx, v, NULL) == 20.0);

    JSObjectSetPropertyAtIndex(ctx, (JSObjectRef)array, 1, JSValueMakeNumber(ctx, 99), &exception);
    assert(exception == NULL);
    JSValueRef changed = JSObjectGetPropertyAtIndex(ctx, (JSObjectRef)array, 1, &exception);
    assert(JSValueToNumber(ctx, changed, NULL) == 99.0);

    JSStringRelease(script);
    JSGlobalContextRelease(ctx);
}
