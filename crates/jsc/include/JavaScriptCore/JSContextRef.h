/*
 * JSContextRef.h — clean-room reimplementation of the JavaScriptCore C API
 * surface. See JSBase.h for provenance.
 */

#ifndef JSContextRef_h
#define JSContextRef_h

#include <JSBase.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Creates a JavaScript global context. */
JSGlobalContextRef JSGlobalContextCreate(JSClassRef globalObjectClass);
/* Creates a JavaScript global context within the given group. */
JSGlobalContextRef JSGlobalContextCreateInGroup(JSContextGroupRef group,
    JSClassRef globalObjectClass);
/* Retains a global context. */
JSGlobalContextRef JSGlobalContextRetain(JSGlobalContextRef ctx);
/* Releases a global context. */
void JSGlobalContextRelease(JSGlobalContextRef ctx);

/* Creates a JavaScript context group. */
JSContextGroupRef JSContextGroupCreate(void);
/* Retains a JavaScript context group. */
JSContextGroupRef JSContextGroupRetain(JSContextGroupRef group);
/* Releases a JavaScript context group. */
void JSContextGroupRelease(JSContextGroupRef group);

/* Gets the global object of a JavaScript context. */
JSObjectRef JSContextGetGlobalObject(JSContextRef ctx);
/* Gets the context group to which a JavaScript context belongs. */
JSContextGroupRef JSContextGetGroup(JSContextRef ctx);

/* Returns a description of the most recent exception, if any. */
JSStringRef JSContextCreateBacktrace(JSContextRef ctx, unsigned maxStackSize);

/* Evaluates a string of JavaScript. */
JSValueRef JSEvaluateScript(JSContextRef ctx, JSStringRef script,
    JSObjectRef thisObject, JSStringRef sourceURL, int startingLineNumber,
    JSValueRef* exception);
/* Checks for syntax errors in a string of JavaScript. */
bool JSCheckScriptSyntax(JSContextRef ctx, JSStringRef script,
    JSStringRef sourceURL, int startingLineNumber, JSValueRef* exception);

/* Gets the most recent exception, if any. */
JSValueRef JSContextGetException(JSContextRef ctx);
/* Sets the most recent exception. */
void JSContextSetException(JSContextRef ctx, JSValueRef value);

#ifdef __cplusplus
}
#endif

#endif /* JSContextRef_h */
