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
typedef struct SlagTraced SlagTraced;

typedef uint64_t (*slag_host_fn)(void* data, const void* call, uint64_t this_value,
                                 const uint64_t* args, size_t len);

/* The C callbacks of an accessor (`v8::ObjectTemplate::SetAccessor` and
 * v8::Object::SetAccessor). The getter writes the value it answers through
 * `out`; both report 0 on success and 1 with an error pending in the context.
 * `key` is the name value the accessor was installed under. */
typedef int32_t (*slag_accessor_getter)(void* data, uint64_t holder, uint64_t key,
                                        uint64_t* out);
typedef int32_t (*slag_accessor_setter)(void* data, uint64_t holder, uint64_t key,
                                        uint64_t value);

/* The position a `v8::Message` reports: the 1-based line and the 0-based
 * column of a compile error. */
typedef struct SlagMessage {
  int32_t line;
  int32_t column;
} SlagMessage;

/* A property descriptor as the definer callback receives it
 * (v8::PropertyDescriptor). Each field is present-or-absent, because V8's
 * descriptor distinguishes "not mentioned" from "mentioned and false". */
typedef struct SlagPropertyDescriptor {
  int32_t has_value;
  uint64_t value;
  int32_t has_writable;
  int32_t writable;
  int32_t has_get;
  uint64_t get;
  int32_t has_set;
  uint64_t set;
  int32_t has_enumerable;
  int32_t enumerable;
  int32_t has_configurable;
  int32_t configurable;
} SlagPropertyDescriptor;

/* The C callbacks of a v8::NamedPropertyHandlerConfiguration. Each returns V8's
 * Intercepted (0 kNo, 1 kYes, 2 kYesKeepExisting, 3 kThrow) and writes its
 * answer through its out parameter; a null pointer is a callback the host did
 * not install. `data` is the configuration's data as tagged value bits, and
 * `state` is the facade's trampoline context. */
typedef struct SlagPropertyHandler {
  int32_t (*getter)(void* state, uint64_t data, uint64_t holder, uint64_t key,
                    uint64_t receiver, uint64_t* out);
  int32_t (*setter)(void* state, uint64_t data, uint64_t holder, uint64_t key,
                    uint64_t value, uint64_t receiver, int32_t throw_on_error);
  int32_t (*query)(void* state, uint64_t data, uint64_t holder, uint64_t key,
                   uint64_t* out);
  int32_t (*deleter)(void* state, uint64_t data, uint64_t holder, uint64_t key,
                     uint64_t* out);
  /* [[OwnPropertyKeys]]: the out slot receives the array of keys the host owns. */
  int32_t (*enumerator)(void* state, uint64_t data, uint64_t holder,
                        uint64_t* out);
  /* [[GetOwnProperty]]: the out slot receives a descriptor object, which the
   * Rust side reads with the engine's own ToPropertyDescriptor. */
  int32_t (*descriptor)(void* state, uint64_t data, uint64_t holder,
                        uint64_t key, uint64_t* out);
  /* [[DefineOwnProperty]]: the descriptor is handed in, and the out slot
   * receives a boolean. */
  int32_t (*definer)(void* state, uint64_t data, uint64_t holder, uint64_t key,
                     const SlagPropertyDescriptor* descriptor, uint64_t* out);
  int32_t (*indexed_getter)(void* state, uint64_t data, uint64_t holder,
                            uint32_t index, uint64_t receiver, uint64_t* out);
  int32_t (*indexed_setter)(void* state, uint64_t data, uint64_t holder,
                            uint32_t index, uint64_t value, uint64_t receiver,
                            int32_t throw_on_error);
  int32_t (*indexed_query)(void* state, uint64_t data, uint64_t holder,
                           uint32_t index, uint64_t* out);
  int32_t (*indexed_deleter)(void* state, uint64_t data, uint64_t holder,
                             uint32_t index, uint64_t* out);
  int32_t (*indexed_enumerator)(void* state, uint64_t data, uint64_t holder,
                                uint64_t* out);
  int32_t (*indexed_descriptor)(void* state, uint64_t data, uint64_t holder,
                                uint32_t index, uint64_t* out);
  int32_t (*indexed_definer)(void* state, uint64_t data, uint64_t holder,
                             uint32_t index,
                             const SlagPropertyDescriptor* descriptor,
                             uint64_t* out);
  uint64_t data;
  void* state;
  void* indexed_state;
} SlagPropertyHandler;

SlagIsolate* slag_isolate_new(void);
void slag_isolate_free(SlagIsolate* isolate);
SlagContext* slag_context_new(SlagIsolate* isolate);
void slag_context_free(SlagContext* context);

int32_t slag_eval(SlagContext* context, const uint8_t* source, size_t len, uint64_t* out);
int32_t slag_compile(SlagContext* context, const uint8_t* source, size_t len,
                     const uint8_t* name, size_t name_len, SlagScript** out);
int32_t slag_script_run(SlagContext* context, SlagScript* script, uint64_t* out);
/* Drain the job queues. 1 when a job was queued, 0 when none was, -1 with a
 * pending error when a job threw. */
int32_t slag_drain_jobs(SlagContext* context);
void slag_script_free(SlagScript* script);
int32_t slag_register_fn(SlagContext* context, const uint8_t* name, size_t name_len,
                         slag_host_fn callback, void* data);
int32_t slag_call_eval(const void* call, const uint8_t* source, size_t len,
                       uint64_t* out);

void slag_throw_error(SlagContext* context, const uint8_t* message, size_t len);
int32_t slag_has_error(SlagContext* context);
/* Copy the pending error's message into the buffer (NUL terminated). Returns the
 * byte length, or -1 when nothing is pending. */
int64_t slag_pending_error(SlagContext* context, char* buffer, size_t len);
int32_t slag_pending_exception(SlagContext* context, uint64_t* out);
int64_t slag_pending_stack(SlagContext* context, char* buffer, size_t len);
int32_t slag_pending_message(SlagContext* context, SlagMessage* out, char* name,
                             size_t name_len, char* source_line,
                             size_t source_line_len);
void slag_clear_error(SlagContext* context);

uint64_t slag_undefined(void);
uint64_t slag_boolean_new(int32_t value);
uint64_t slag_string_new(const uint8_t* text, size_t len);
uint64_t slag_number_new(double value);
void slag_root_retain(uint64_t value);
void slag_root_release(uint64_t value);
size_t slag_root_count(void);
SlagTraced* slag_traced_new(uint64_t value);
int32_t slag_traced_get(const SlagTraced* traced, uint64_t* out);
int32_t slag_traced_is_empty(const SlagTraced* traced);
SlagTraced* slag_traced_clone(const SlagTraced* traced);
void slag_traced_reset(SlagTraced* traced);
void slag_traced_free(SlagTraced* traced);
int32_t slag_collect(SlagContext* context);
int64_t slag_value_to_utf8(uint64_t value, char* buffer, size_t len);
int64_t slag_value_to_utf8_context(SlagContext* context, uint64_t value,
                                   char* buffer, size_t len);
double slag_value_to_number(uint64_t value);
int32_t slag_value_is_string(uint64_t value);
int32_t slag_value_is_undefined(uint64_t value);
uint64_t slag_global(SlagContext* context);
int32_t slag_object_new(SlagContext* context, size_t internal_fields,
                        int32_t api_wrapper, uint64_t* out);
int32_t slag_object_new_handler(SlagContext* context, size_t internal_fields,
                                int32_t api_wrapper, const SlagPropertyHandler* handler,
                                uint64_t* out);

/* The token of the host callback that is running, or NULL outside one. A callback
 * needs it to allocate: the operation that called the callback holds the context,
 * so an ordinary entry point (slag_object_new, slag_object_set_key) refuses
 * re-entry while the token names a window that is running. */
const void* slag_callback_token(void);
int32_t slag_callback_object_new(SlagContext* context, const void* token, uint64_t* out);
int32_t slag_array_new(SlagContext* context, const uint64_t* values, size_t len,
                       uint64_t* out);
int32_t slag_callback_array_new(SlagContext* context, const void* token,
                                const uint64_t* values, size_t len, uint64_t* out);
int32_t slag_callback_object_set_key(SlagContext* context, const void* token,
                                     uint64_t object, uint64_t key, uint64_t value);
int32_t slag_callback_object_get_key(SlagContext* context, const void* token,
                                     uint64_t object, uint64_t key, uint64_t* out);
int32_t slag_callback_object_has_key(SlagContext* context, const void* token,
                                     uint64_t object, uint64_t key, int32_t* out);
int32_t slag_callback_object_delete_key(SlagContext* context, const void* token,
                                        uint64_t object, uint64_t key, int32_t* out);
int32_t slag_callback_object_get_own_key(SlagContext* context, const void* token,
                                         uint64_t object, uint64_t key,
                                         uint64_t* out, int32_t* found);
int32_t slag_callback_object_own_property_kind(SlagContext* context,
                                               const void* token, uint64_t object,
                                               uint64_t key, int32_t* out);
size_t slag_object_internal_field_count(uint64_t object);
int32_t slag_object_identity_hash(uint64_t object);
int32_t slag_object_is_api_wrapper(uint64_t object);
int32_t slag_object_set_internal_field(uint64_t object, size_t index, void* pointer);
void* slag_object_internal_field(uint64_t object, size_t index);
int32_t slag_object_register_fn(SlagContext* context, uint64_t object, const uint8_t* name,
                                size_t name_len, slag_host_fn callback, void* data);
int32_t slag_object_set_accessor(SlagContext* context, uint64_t object, uint64_t key,
                                 slag_accessor_getter getter, slag_accessor_setter setter,
                                 void* data);
int32_t slag_object_set(uint64_t object, const uint8_t* key, size_t key_len,
                        uint64_t value);
int32_t slag_object_get_key(SlagContext* context, uint64_t object, uint64_t key,
                            uint64_t* out);
int32_t slag_object_set_key(SlagContext* context, uint64_t object, uint64_t key,
                            uint64_t value);
int32_t slag_object_has_key(SlagContext* context, uint64_t object, uint64_t key,
                            int32_t* out);
int32_t slag_object_delete_key(SlagContext* context, uint64_t object, uint64_t key,
                               int32_t* out);
int32_t slag_object_get_own_key(SlagContext* context, uint64_t object, uint64_t key,
                                uint64_t* out, int32_t* found);
int32_t slag_object_own_property_kind(SlagContext* context, uint64_t object,
                                      uint64_t key, int32_t* out);
const char* slag_version(void);

#ifdef __cplusplus
}
#endif

#endif /* SLAG_A0_H_ */
