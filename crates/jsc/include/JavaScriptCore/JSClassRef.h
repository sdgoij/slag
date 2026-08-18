/*
 * JSClassRef.h — clean-room reimplementation of the JavaScriptCore C API
 * surface. See JSBase.h for provenance.
 */

#ifndef JSClassRef_h
#define JSClassRef_h

#include <JSBase.h>
#include <JSObjectRef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Class attributes. */
typedef enum {
    kJSClassAttributeNone = 0,
    kJSClassAttributeNoAutomaticPrototype = 1 << 1
} JSClassAttributes;

/* A JavaScript callback to initialize an object. */
typedef void (*JSObjectInitializeCallback)(JSContextRef ctx, JSObjectRef object);
/* A JavaScript callback to finalize an object. */
typedef void (*JSObjectFinalizeCallback)(JSObjectRef object);
/* A JavaScript callback to test whether an object has a property. */
typedef bool (*JSObjectHasPropertyCallback)(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName);
/* A JavaScript callback to get a property from an object. */
typedef JSValueRef (*JSObjectGetPropertyCallback)(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef* exception);
/* A JavaScript callback to set a property on an object. */
typedef bool (*JSObjectSetPropertyCallback)(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef value, JSValueRef* exception);
/* A JavaScript callback to delete a property from an object. */
typedef bool (*JSObjectDeletePropertyCallback)(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef* exception);
/* A JavaScript callback to collect the property names of an object. */
typedef void (*JSObjectGetPropertyNamesCallback)(JSContextRef ctx, JSObjectRef object,
    JSPropertyNameAccumulatorRef propertyNames);
/* A JavaScript callback to call an object as a function. */
typedef JSValueRef (*JSObjectCallAsFunctionCallback)(JSContextRef ctx,
    JSObjectRef function, JSObjectRef thisObject, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* A JavaScript callback to construct an object. */
typedef JSObjectRef (*JSObjectCallAsConstructorCallback)(JSContextRef ctx,
    JSObjectRef constructor, size_t argumentCount, const JSValueRef arguments[],
    JSValueRef* exception);
/* A JavaScript callback to test whether an object is an instance of a
 * constructor. */
typedef bool (*JSObjectHasInstanceCallback)(JSContextRef ctx, JSObjectRef constructor,
    JSValueRef possibleInstance, JSValueRef* exception);
/* A JavaScript callback to convert an object to a type. */
typedef JSValueRef (*JSObjectConvertToTypeCallback)(JSContextRef ctx, JSObjectRef object,
    JSType type, JSValueRef* exception);

/* The definition of a JavaScript class. */
typedef struct JSClassDefinition {
    int version;
    JSClassAttributes attributes;
    const char* className;
    JSClassRef parentClass;

    const JSStaticValue* staticValues;
    const JSStaticFunction* staticFunctions;

    JSObjectInitializeCallback initialize;
    JSObjectFinalizeCallback finalize;
    JSObjectHasPropertyCallback hasProperty;
    JSObjectGetPropertyCallback getProperty;
    JSObjectSetPropertyCallback setProperty;
    JSObjectDeletePropertyCallback deleteProperty;
    JSObjectGetPropertyNamesCallback getPropertyNames;
    JSObjectCallAsFunctionCallback callAsFunction;
    JSObjectCallAsConstructorCallback callAsConstructor;
    JSObjectHasInstanceCallback hasInstance;
    JSObjectConvertToTypeCallback convertToType;
} JSClassDefinition;

/* Creates a JavaScript class. */
JSClassRef JSClassCreate(const JSClassDefinition* definition);
/* Retains a JavaScript class. */
JSClassRef JSClassRetain(JSClassRef jsClass);
/* Releases a JavaScript class. */
void JSClassRelease(JSClassRef jsClass);

#ifdef __cplusplus
}
#endif

#endif /* JSClassRef_h */
