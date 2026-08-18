/*
 * JSPropertyNameArrayRef.h — clean-room reimplementation of the JavaScriptCore
 * C API surface. See JSBase.h for provenance.
 */

#ifndef JSPropertyNameArrayRef_h
#define JSPropertyNameArrayRef_h

#include <JSBase.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Retains a JavaScript property name array. */
JSPropertyNameArrayRef JSPropertyNameArrayRetain(JSPropertyNameArrayRef array);
/* Releases a JavaScript property name array. */
void JSPropertyNameArrayRelease(JSPropertyNameArrayRef array);
/* Gets the count of property names in a JavaScript property name array. */
size_t JSPropertyNameArrayGetCount(JSPropertyNameArrayRef array);
/* Gets a property name at a given index in a JavaScript property name array. */
JSStringRef JSPropertyNameArrayGetNameAtIndex(JSPropertyNameArrayRef array, size_t index);

/* Adds a property name to a property name accumulator. */
void JSPropertyNameAccumulatorAddName(JSPropertyNameAccumulatorRef accumulator,
    JSStringRef propertyName);

#ifdef __cplusplus
}
#endif

#endif /* JSPropertyNameArrayRef_h */
