// Variadic error constructors/throwers. Stable Rust cannot define C variadics,
// so this shim formats the message with vsnprintf and forwards to the Rust
// helpers `qjs_new_error` / `qjs_throw_error`.

#include "quickjs.h"
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define QJS_ERR_ERROR 0
#define QJS_ERR_RANGE 2
#define QJS_ERR_REFERENCE 3
#define QJS_ERR_SYNTAX 4
#define QJS_ERR_TYPE 5

extern JSValue qjs_new_error(JSContext *ctx, int kind, const char *message);
extern void qjs_throw_error(JSContext *ctx, int kind, const char *message);

static char *format_message(const char *fmt, va_list ap) {
    va_list ap2;
    char *buf;
    int len;
    va_copy(ap2, ap);
    len = vsnprintf(NULL, 0, fmt, ap2);
    va_end(ap2);
    if (len < 0)
        return NULL;
    buf = (char *)malloc((size_t)len + 1);
    if (!buf)
        return NULL;
    vsnprintf(buf, (size_t)len + 1, fmt, ap);
    return buf;
}

#define DEFINE_NEW_ERROR(name, kind)                                          \
    JSValue name(JSContext *ctx, const char *fmt, ...) {                       \
        va_list ap;                                                            \
        char *msg;                                                             \
        JSValue value;                                                         \
        va_start(ap, fmt);                                                     \
        msg = format_message(fmt, ap);                                         \
        va_end(ap);                                                            \
        if (!msg)                                                              \
            return JS_EXCEPTION;                                               \
        value = qjs_new_error(ctx, kind, msg);                                 \
        free(msg);                                                             \
        return value;                                                          \
    }

#define DEFINE_THROW_ERROR(name, kind)                                        \
    JSValue name(JSContext *ctx, const char *fmt, ...) {                       \
        va_list ap;                                                            \
        char *msg;                                                             \
        va_start(ap, fmt);                                                     \
        msg = format_message(fmt, ap);                                         \
        va_end(ap);                                                            \
        if (!msg)                                                              \
            return JS_EXCEPTION;                                               \
        qjs_throw_error(ctx, kind, msg);                                       \
        free(msg);                                                             \
        return JS_EXCEPTION;                                                   \
    }

#ifdef QJS_NG
DEFINE_NEW_ERROR(JS_NewPlainError, QJS_ERR_ERROR)
DEFINE_NEW_ERROR(JS_NewRangeError, QJS_ERR_RANGE)
DEFINE_NEW_ERROR(JS_NewReferenceError, QJS_ERR_REFERENCE)
DEFINE_NEW_ERROR(JS_NewSyntaxError, QJS_ERR_SYNTAX)
DEFINE_NEW_ERROR(JS_NewTypeError, QJS_ERR_TYPE)
DEFINE_NEW_ERROR(JS_NewInternalError, QJS_ERR_ERROR)

DEFINE_THROW_ERROR(JS_ThrowPlainError, QJS_ERR_ERROR)
#endif

DEFINE_THROW_ERROR(JS_ThrowRangeError, QJS_ERR_RANGE)
DEFINE_THROW_ERROR(JS_ThrowReferenceError, QJS_ERR_REFERENCE)
DEFINE_THROW_ERROR(JS_ThrowSyntaxError, QJS_ERR_SYNTAX)
DEFINE_THROW_ERROR(JS_ThrowTypeError, QJS_ERR_TYPE)
DEFINE_THROW_ERROR(JS_ThrowInternalError, QJS_ERR_ERROR)

#ifdef QJS_NG
JSValue JS_ThrowDOMException(JSContext *ctx, const char *name, const char *fmt, ...) {
    va_list ap;
    char *body;
    char *msg;
    va_start(ap, fmt);
    body = format_message(fmt, ap);
    va_end(ap);
    if (!body)
        return JS_EXCEPTION;
    msg = (char *)malloc(strlen(name) + strlen(body) + 3);
    if (!msg) {
        free(body);
        return JS_EXCEPTION;
    }
    sprintf(msg, "%s: %s", name, body);
    free(body);
    qjs_throw_error(ctx, QJS_ERR_ERROR, msg);
    free(msg);
    return JS_EXCEPTION;
}
#endif
