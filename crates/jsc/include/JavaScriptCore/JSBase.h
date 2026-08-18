/*
 * JSBase.h — clean-room reimplementation of the JavaScriptCore C API
 * surface (https://developer.apple.com/documentation/javascriptcore).
 *
 * Written from the documented API; not derived from Apple's headers
 * (JavaScriptCore is LGPL-2.1; this crate is MIT OR Apache-2.0).
 *
 * The types below are the opaque handle types of the API. A program that
 * includes this header instead of the real JSBase.h and links against
 * libslag_jsc compiles and runs unchanged as long as it sticks to the
 * documented public API.
 */

#ifndef JSBase_h
#define JSBase_h

#ifdef __cplusplus
extern "C" {
#endif

/* A Unicode character (a UTF-16 code unit). */
typedef unsigned short JSChar;

/* Opaque handle types. */
typedef const struct OpaqueJSContext* JSContextRef;
typedef struct OpaqueJSContext* JSGlobalContextRef;
typedef struct OpaqueJSContextGroup* JSContextGroupRef;
typedef const struct OpaqueJSValue* JSValueRef;
typedef struct OpaqueJSValue* JSObjectRef;
typedef const struct OpaqueJSString* JSStringRef;
typedef struct OpaqueJSClass* JSClassRef;
typedef struct OpaqueJSPropertyNameArray* JSPropertyNameArrayRef;
typedef struct OpaqueJSPropertyNameAccumulator* JSPropertyNameAccumulatorRef;

/* The type of a JSValueRef (JSValueGetType). */
typedef enum {
    kJSTypeUndefined = 0,
    kJSTypeNull = 1,
    kJSTypeBoolean = 2,
    kJSTypeNumber = 3,
    kJSTypeString = 4,
    kJSTypeObject = 5,
    kJSTypeSymbol = 6,
    kJSTypeBigInt = 7
} JSType;

#ifdef __cplusplus
}
#endif

#endif /* JSBase_h */
