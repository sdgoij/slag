// A minimal QuickJS C host used by `tools/link-test-quickjs.sh` to prove the
// facade's staticlib and cdylib are consumable from C. It evaluates `1 + 2`
// and prints the result; the script asserts the output.
#include "quickjs.h"
#include <stdio.h>
#include <string.h>

int main(void) {
    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        fprintf(stderr, "JS_NewRuntime failed\n");
        return 1;
    }
    JSContext *ctx = JS_NewContext(rt);
    if (!ctx) {
        fprintf(stderr, "JS_NewContext failed\n");
        JS_FreeRuntime(rt);
        return 1;
    }

    const char *src = "1 + 2";
    JSValue result = JS_Eval(ctx, src, strlen(src), "link-test.js", 0);
    if (JS_IsException(result)) {
        fprintf(stderr, "JS_Eval threw\n");
        JS_FreeValue(ctx, result);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        return 1;
    }

    printf("%d\n", (int)JS_VALUE_GET_FLOAT64(result));

    JS_FreeValue(ctx, result);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    return 0;
}
