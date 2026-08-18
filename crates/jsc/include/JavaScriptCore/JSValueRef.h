/*
 * JSValueRef.h — clean-room reimplementation of the JavaScriptCore C API
 * surface. See JSBase.h for provenance.
 */

#ifndef JSValueRef_h
#define JSValueRef_h

#include <JSBase.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Returns the JavaScript type of a JSValueRef. */
JSType JSValueGetType(JSContextRef ctx, JSValueRef value);

/* Tests whether a JavaScript value's type is the given type. */
bool JSValueIsUndefined(JSContextRef ctx, JSValueRef value);
bool JSValueIsNull(JSContextRef ctx, JSValueRef value);
bool JSValueIsBoolean(JSContextRef ctx, JSValueRef value);
bool JSValueIsNumber(JSContextRef ctx, JSValueRef value);
bool JSValueIsString(JSContextRef ctx, JSValueRef value);
bool JSValueIsSymbol(JSContextRef ctx, JSValueRef value);
bool JSValueIsObject(JSContextRef ctx, JSValueRef value);
bool JSValueIsArray(JSContextRef ctx, JSValueRef value);
bool JSValueIsDate(JSContextRef ctx, JSValueRef value);
bool JSValueIsObjectOfClass(JSContextRef ctx, JSValueRef value, JSClassRef jsClass);

/* Tests whether two JavaScript values are equal, as with the == operator. */
bool JSValueIsEqual(JSContextRef ctx, JSValueRef a, JSValueRef b, JSValueRef* exception);
/* Tests whether two JavaScript values are strict equal, as with the ===
 * operator. */
bool JSValueIsStrictEqual(JSContextRef ctx, JSValueRef a, JSValueRef b);
/* Tests whether a JavaScript value is an object constructed by a given
 * constructor, as with the instanceof operator. */
bool JSValueIsInstanceOfConstructor(JSContextRef ctx, JSValueRef value,
    JSObjectRef constructor, JSValueRef* exception);

/* Creates a JavaScript value of undefined type. */
JSValueRef JSValueMakeUndefined(JSContextRef ctx);
/* Creates a JavaScript value of null type. */
JSValueRef JSValueMakeNull(JSContextRef ctx);
/* Creates a JavaScript value of boolean type. */
JSValueRef JSValueMakeBoolean(JSContextRef ctx, bool boolean);
/* Creates a JavaScript value of number type. */
JSValueRef JSValueMakeNumber(JSContextRef ctx, double number);
/* Creates a JavaScript value of string type. */
JSValueRef JSValueMakeString(JSContextRef ctx, JSStringRef string);
/* Creates a JavaScript value of symbol type. */
JSValueRef JSValueMakeSymbol(JSContextRef ctx, JSStringRef description);

/* Creates a JavaScript value from a JSON string. */
JSValueRef JSValueMakeFromJSONString(JSContextRef ctx, JSStringRef string);
/* Creates a JavaScript string containing the JSON serialization of a
 * JavaScript value. */
JSStringRef JSValueCreateJSONString(JSContextRef ctx, JSValueRef value,
    unsigned indent, JSValueRef* exception);

/* Converts a JavaScript value to boolean and returns the resulting boolean. */
bool JSValueToBoolean(JSContextRef ctx, JSValueRef value);
/* Converts a JavaScript value to number and returns the resulting number. */
double JSValueToNumber(JSContextRef ctx, JSValueRef value, JSValueRef* exception);
/* Converts a JavaScript value to string and copies the result into a
 * JavaScript string. */
JSStringRef JSValueToStringCopy(JSContextRef ctx, JSValueRef value, JSValueRef* exception);
/* Converts a JavaScript value to object and returns the resulting object. */
JSObjectRef JSValueToObject(JSContextRef ctx, JSValueRef value, JSValueRef* exception);

#ifdef __cplusplus
}
#endif

#endif /* JSValueRef_h */
