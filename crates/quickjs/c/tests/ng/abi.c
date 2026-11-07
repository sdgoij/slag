#include "quickjs.h"
#include <stddef.h>

// Phase 6.3: ABI layout for the QuickJS-NG header. A mismatch fails the build
// (negative array size) rather than only at runtime. `offsetof` also pins the
// implied alignment: an 8-byte `tag` at offset 8 means the union is 8-aligned.

#define QJS_STATIC_ASSERT(name, cond) typedef char name[(cond) ? 1 : -1]

QJS_STATIC_ASSERT(jsvalue_size_16, sizeof(JSValue) == 16);
QJS_STATIC_ASSERT(jsvalue_tag_offset_8, offsetof(JSValue, tag) == 8);

QJS_STATIC_ASSERT(jsproperty_enum_size_8, sizeof(JSPropertyEnum) == 8);
QJS_STATIC_ASSERT(jsproperty_enum_atom_offset_4, offsetof(JSPropertyEnum, atom) == 4);

QJS_STATIC_ASSERT(jsproperty_desc_size_56, sizeof(JSPropertyDescriptor) == 56);
QJS_STATIC_ASSERT(jsproperty_desc_value_offset_8, offsetof(JSPropertyDescriptor, value) == 8);
QJS_STATIC_ASSERT(jsproperty_desc_getter_offset_24, offsetof(JSPropertyDescriptor, getter) == 24);
QJS_STATIC_ASSERT(jsproperty_desc_setter_offset_40, offsetof(JSPropertyDescriptor, setter) == 40);

QJS_STATIC_ASSERT(jsclass_def_size_40, sizeof(JSClassDef) == 40);

QJS_STATIC_ASSERT(jsclass_exotic_size_56, sizeof(JSClassExoticMethods) == 7 * sizeof(void *));

void qjs_test_abi_layout(void) {}
