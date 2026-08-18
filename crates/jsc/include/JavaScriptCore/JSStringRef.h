/*
 * JSStringRef.h — clean-room reimplementation of the JavaScriptCore C API
 * surface. See JSBase.h for provenance.
 */

#ifndef JSStringRef_h
#define JSStringRef_h

#include <JSBase.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Creates a JavaScript string from a buffer of Unicode characters. */
JSStringRef JSStringCreateWithCharacters(const JSChar* characters, size_t length);
/* Creates a JavaScript string from a null-terminated UTF8 string. */
JSStringRef JSStringCreateWithUTF8CString(const char* string);

/* Retains a JavaScript string. */
JSStringRef JSStringRetain(JSStringRef string);
/* Releases a JavaScript string. */
void JSStringRelease(JSStringRef string);

/* Returns the length of a JavaScript string. */
size_t JSStringGetLength(JSStringRef string);
/* Returns a pointer to the Unicode character buffer. */
const JSChar* JSStringGetCharactersPtr(JSStringRef string);
/* Returns the maximum number of bytes a UTF8 representation of the string
 * can take (including the terminating NUL). */
size_t JSStringGetMaximumUTF8CStringSize(JSStringRef string);
/* Writes a UTF8 representation of the string into a buffer. */
size_t JSStringGetUTF8CString(JSStringRef string, char* buffer, size_t bufferSize);

/* Tests whether two JavaScript strings match. */
bool JSStringIsEqual(JSStringRef a, JSStringRef b);
/* Tests whether a JavaScript string matches a null-terminated UTF8 string. */
bool JSStringIsEqualToUTF8CString(JSStringRef a, const char* b);

#ifdef __cplusplus
}
#endif

#endif /* JSStringRef_h */
