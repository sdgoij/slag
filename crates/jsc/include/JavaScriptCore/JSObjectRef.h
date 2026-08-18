/*
 * JSObjectRef.h — clean-room reimplementation of the JavaScriptCore C API
 * surface. See JSBase.h for provenance.
 */

#ifndef JSObjectRef_h
#define JSObjectRef_h

#include <JSBase.h>
#include <JSStringRef.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Property attributes. */
typedef enum {
    kJSPropertyAttributeNone = 0,
    kJSPropertyAttributeReadOnly = 1 << 1,
    kJSPropertyAttributeDontEnum = 1 << 2,
    kJSPropertyAttributeDontDelete = 1 << 3
} JSPropertyAttributes;

/* A JavaScript callback for getting a static value. */
typedef JSValueRef (*JSObjectGetPropertyCallback)(JSContextRef ctx,
    JSObjectRef object, JSStringRef propertyName, JSValueRef* exception);
/* A JavaScript callback for setting a static value. */
typedef bool (*JSObjectSetPropertyCallback)(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef value, JSValueRef* exception);

/* A JavaScript callback for calling a function. */
typedef JSValueRef (*JSObjectCallAsFunctionCallback)(JSContextRef ctx,
    JSObjectRef function, JSObjectRef thisObject, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* A JavaScript callback for constructing an object. */
typedef JSObjectRef (*JSObjectCallAsConstructorCallback)(JSContextRef ctx,
    JSObjectRef constructor, size_t argumentCount, const JSValueRef arguments[],
    JSValueRef* exception);

/* A static value in a JSClassDefinition. */
typedef struct JSStaticValue {
    const char* name;
    JSObjectGetPropertyCallback getProperty;
    JSObjectSetPropertyCallback setProperty;
    JSPropertyAttributes attributes;
} JSStaticValue;
/* A static function in a JSClassDefinition. */
typedef struct JSStaticFunction {
    const char* name;
    JSObjectCallAsFunctionCallback callAsFunction;
    JSPropertyAttributes attributes;
} JSStaticFunction;

/* Creates a JavaScript object. */
JSObjectRef JSObjectMake(JSContextRef ctx, JSClassRef jsClass, void* data);
/* Creates a JavaScript function. */
JSObjectRef JSObjectMakeFunctionWithCallback(JSContextRef ctx,
    JSStringRef name, JSObjectCallAsFunctionCallback callAsFunction);
/* Creates a JavaScript constructor. */
JSObjectRef JSObjectMakeConstructor(JSContextRef ctx, JSClassRef jsClass,
    JSObjectCallAsConstructorCallback callAsConstructor);
/* Creates a JavaScript array. */
JSObjectRef JSObjectMakeArray(JSContextRef ctx, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* Creates a JavaScript Date. */
JSObjectRef JSObjectMakeDate(JSContextRef ctx, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* Creates a JavaScript Error. */
JSObjectRef JSObjectMakeError(JSContextRef ctx, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* Creates a JavaScript RegExp. */
JSObjectRef JSObjectMakeRegExp(JSContextRef ctx, size_t argumentCount,
    const JSValueRef arguments[], JSValueRef* exception);
/* Creates a JavaScript function from a string of JavaScript. */
JSObjectRef JSObjectMakeFunction(JSContextRef ctx, JSStringRef name,
    unsigned parameterCount, const JSStringRef parameterNames[],
    JSStringRef body, JSStringRef sourceURL, int startingLineNumber,
    JSValueRef* exception);

/* Gets the prototype of an object. */
JSValueRef JSObjectGetPrototype(JSContextRef ctx, JSObjectRef object);
/* Sets the prototype of an object. */
void JSObjectSetPrototype(JSContextRef ctx, JSObjectRef object, JSValueRef value);

/* Tests whether an object has a given property. */
bool JSObjectHasProperty(JSContextRef ctx, JSObjectRef object, JSStringRef propertyName);
/* Gets a property from an object. */
JSValueRef JSObjectGetProperty(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef* exception);
/* Sets a property on an object. */
void JSObjectSetProperty(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef value, JSPropertyAttributes attributes,
    JSValueRef* exception);
/* Deletes a property from an object. */
bool JSObjectDeleteProperty(JSContextRef ctx, JSObjectRef object,
    JSStringRef propertyName, JSValueRef* exception);
/* Gets a property from an object by numeric index. */
JSValueRef JSObjectGetPropertyAtIndex(JSContextRef ctx, JSObjectRef object,
    unsigned propertyIndex, JSValueRef* exception);
/* Sets a property on an object by numeric index. */
void JSObjectSetPropertyAtIndex(JSContextRef ctx, JSObjectRef object,
    unsigned propertyIndex, JSValueRef value, JSValueRef* exception);

/* Gets an object's private data. */
void* JSObjectGetPrivate(JSObjectRef object);
/* Sets an object's private data. */
bool JSObjectSetPrivate(JSObjectRef object, void* data);

/* Tests whether an object is a function. */
bool JSObjectIsFunction(JSContextRef ctx, JSObjectRef object);
/* Tests whether an object is a constructor. */
bool JSObjectIsConstructor(JSContextRef ctx, JSObjectRef object);
/* Calls an object as a function. */
JSValueRef JSObjectCallAsFunction(JSContextRef ctx, JSObjectRef object,
    JSObjectRef thisObject, size_t argumentCount, const JSValueRef arguments[],
    JSValueRef* exception);
/* Calls an object as a constructor. */
JSObjectRef JSObjectCallAsConstructor(JSContextRef ctx, JSObjectRef object,
    size_t argumentCount, const JSValueRef arguments[], JSValueRef* exception);

/* Copies an object's property names. */
JSPropertyNameArrayRef JSObjectCopyPropertyNames(JSContextRef ctx, JSObjectRef object);

#ifdef __cplusplus
}
#endif

#endif /* JSObjectRef_h */
