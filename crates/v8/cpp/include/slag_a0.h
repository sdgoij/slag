#ifndef SLAG_A0_H_
#define SLAG_A0_H_

/* A0's flat C ABI, implemented by crates/slagc. */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct SlagIsolate SlagIsolate;
typedef struct SlagContext SlagContext;
typedef struct SlagScript SlagScript;

typedef uint64_t (*slag_host_fn)(void* data, const void* call, uint64_t this_value,
                                 const uint64_t* args, size_t len);

SlagIsolate* slag_isolate_new(void);
void slag_isolate_free(SlagIsolate* isolate);
SlagContext* slag_context_new(SlagIsolate* isolate);
void slag_context_free(SlagContext* context);

int32_t slag_eval(SlagContext* context, const uint8_t* source, size_t len, uint64_t* out);
int32_t slag_compile(SlagContext* context, const uint8_t* source, size_t len,
                     SlagScript** out);
int32_t slag_script_run(SlagContext* context, SlagScript* script, uint64_t* out);
void slag_script_free(SlagScript* script);
int32_t slag_register_fn(SlagContext* context, const uint8_t* name, size_t name_len,
                         slag_host_fn callback, void* data);
int32_t slag_call_eval(const void* call, const uint8_t* source, size_t len,
                       uint64_t* out);

void slag_throw_error(SlagContext* context, const uint8_t* message, size_t len);
int32_t slag_has_error(SlagContext* context);
int64_t slag_take_error(SlagContext* context, char* buffer, size_t len);

uint64_t slag_undefined(void);
uint64_t slag_string_new(const uint8_t* text, size_t len);
uint64_t slag_number_new(double value);
void slag_root_retain(uint64_t value);
void slag_root_release(uint64_t value);
size_t slag_root_count(void);
int32_t slag_collect(SlagContext* context);
int64_t slag_value_to_utf8(uint64_t value, char* buffer, size_t len);
double slag_value_to_number(uint64_t value);
int32_t slag_value_is_string(uint64_t value);
int32_t slag_value_is_undefined(uint64_t value);
uint64_t slag_global(SlagContext* context);
int32_t slag_object_new(SlagContext* context, size_t internal_fields,
                        int32_t api_wrapper, uint64_t* out);
size_t slag_object_internal_field_count(uint64_t object);
int32_t slag_object_is_api_wrapper(uint64_t object);
int32_t slag_object_set_internal_field(uint64_t object, size_t index, void* pointer);
void* slag_object_internal_field(uint64_t object, size_t index);
int32_t slag_object_register_fn(SlagContext* context, uint64_t object, const uint8_t* name,
                                size_t name_len, slag_host_fn callback, void* data);
int32_t slag_object_set(uint64_t object, const uint8_t* key, size_t key_len,
                        uint64_t value);
int32_t slag_object_get_key(SlagContext* context, uint64_t object, uint64_t key,
                            uint64_t* out);
int32_t slag_object_set_key(SlagContext* context, uint64_t object, uint64_t key,
                            uint64_t value);
const char* slag_version(void);

#ifdef __cplusplus
}
#endif

#endif /* SLAG_A0_H_ */
