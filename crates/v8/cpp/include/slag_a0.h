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
typedef struct SlagBuffer SlagBuffer;
typedef struct SlagWeak SlagWeak;

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

/* Which `v8::Value` predicates a value answers (`slag_value_type_mask`). The
 * numbers mirror `runtime::embed`'s `VALUE_*` constants; edit both in one go. */
enum SlagValueType {
  SLAG_VALUE_UNDEFINED = 1 << 0,
  SLAG_VALUE_NULL = 1 << 1,
  SLAG_VALUE_BOOLEAN = 1 << 2,
  SLAG_VALUE_NUMBER = 1 << 3,
  SLAG_VALUE_BIGINT = 1 << 4,
  SLAG_VALUE_SYMBOL = 1 << 5,
  SLAG_VALUE_STRING = 1 << 6,
  /* An object or a function, which is V8's `Value::IsObject`. */
  SLAG_VALUE_OBJECT = 1 << 7,
  SLAG_VALUE_FUNCTION = 1 << 8,
  SLAG_VALUE_ARRAY = 1 << 9,
  SLAG_VALUE_EXTERNAL = 1 << 10,
  SLAG_VALUE_TYPED_ARRAY = 1 << 11,
  SLAG_VALUE_PROXY = 1 << 12,
  SLAG_VALUE_ARGUMENTS = 1 << 13,
  SLAG_VALUE_INT32 = 1 << 14,
  SLAG_VALUE_UINT32 = 1 << 15,
  SLAG_VALUE_DATE = 1 << 16,
  SLAG_VALUE_REGEXP = 1 << 17,
  SLAG_VALUE_PROMISE = 1 << 18,
  SLAG_VALUE_MAP = 1 << 19,
  SLAG_VALUE_SET = 1 << 20,
  SLAG_VALUE_ARRAY_BUFFER = 1 << 21,
  SLAG_VALUE_NATIVE_ERROR = 1 << 22,
  SLAG_VALUE_SHARED_ARRAY_BUFFER = 1 << 23,
  SLAG_VALUE_DATA_VIEW = 1 << 24,
};

/* `v8::PropertyAttribute` bits, as an accessor is installed with them. The
 * numbers mirror `runtime::embed`'s `PROPERTY_*` constants. */
enum SlagPropertyAttribute {
  SLAG_PROPERTY_READ_ONLY = 1 << 0,
  SLAG_PROPERTY_DONT_ENUM = 1 << 1,
  SLAG_PROPERTY_DONT_DELETE = 1 << 2,
};

/* The built-in error constructors (`v8::Exception::Error` and its siblings). */
enum SlagExceptionKind {
  SLAG_EXCEPTION_ERROR = 0,
  SLAG_EXCEPTION_EVAL_ERROR = 1,
  SLAG_EXCEPTION_RANGE_ERROR = 2,
  SLAG_EXCEPTION_REFERENCE_ERROR = 3,
  SLAG_EXCEPTION_SYNTAX_ERROR = 4,
  SLAG_EXCEPTION_TYPE_ERROR = 5,
  SLAG_EXCEPTION_URI_ERROR = 6,
};

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

/* A block of buffer bytes the host holds independently of a JavaScript buffer
 * (v8::BackingStore). The storage is shared with any buffer built over it, so
 * freeing this handle does not free bytes a live buffer still names. */
typedef void (*SlagBufferDeleter)(void* data, size_t byte_length, void* deleter_data);
SlagBuffer* slag_buffer_new(size_t byte_length);
SlagBuffer* slag_buffer_borrowed(void* data, size_t byte_length,
                                SlagBufferDeleter deleter, void* deleter_data);
void* slag_buffer_data(const SlagBuffer* buffer);
size_t slag_buffer_byte_length(const SlagBuffer* buffer);
int32_t slag_buffer_is_shared(const SlagBuffer* buffer);
int32_t slag_buffer_is_resizable(const SlagBuffer* buffer);
int32_t slag_buffer_is_borrowed(const SlagBuffer* buffer);
void slag_buffer_free(SlagBuffer* buffer);
int32_t slag_buffer_to_array_buffer(SlagContext* context, const SlagBuffer* buffer,
                                    uint64_t* out);
SlagBuffer* slag_buffer_of(SlagContext* context, uint64_t value);
size_t slag_array_buffer_byte_length(SlagContext* context, uint64_t value);
void* slag_array_buffer_data(SlagContext* context, uint64_t value);
int32_t slag_array_buffer_is_detached(SlagContext* context, uint64_t value);
int32_t slag_array_buffer_is_detachable(SlagContext* context, uint64_t value);
int32_t slag_shared_array_buffer_new(SlagContext* context, const SlagBuffer* buffer,
                                     uint64_t* out);

/* The typed array kinds (`v8::Uint8Array` and its siblings). */
enum SlagTypedArrayKind {
  SLAG_TYPED_INT8 = 0,
  SLAG_TYPED_UINT8 = 1,
  SLAG_TYPED_UINT8_CLAMPED = 2,
  SLAG_TYPED_INT16 = 3,
  SLAG_TYPED_UINT16 = 4,
  SLAG_TYPED_INT32 = 5,
  SLAG_TYPED_UINT32 = 6,
  SLAG_TYPED_FLOAT16 = 7,
  SLAG_TYPED_FLOAT32 = 8,
  SLAG_TYPED_FLOAT64 = 9,
  SLAG_TYPED_BIGINT64 = 10,
  SLAG_TYPED_BIGUINT64 = 11,
};
int32_t slag_typed_array_new(SlagContext* context, int32_t kind, uint64_t buffer,
                            size_t byte_offset, size_t length, uint64_t* out);
int32_t slag_typed_array_buffer(uint64_t value, uint64_t* out);
size_t slag_typed_array_byte_offset(uint64_t value);
size_t slag_typed_array_byte_length(uint64_t value);
size_t slag_typed_array_length(uint64_t value);

/* The host's external-memory account (`AdjustAmountOfExternalAllocatedMemory`)
 * and one realm's buffer bytes, which the facade adds to it. */
int64_t slag_isolate_adjust_external_memory(SlagIsolate* isolate, int64_t change);
int64_t slag_context_external_memory(SlagContext* context);

/* The heap numbers `v8::Isolate::GetHeapStatistics` fills. Every field is a
 * count except `does_zap_garbage`; the facade copies them into V8's struct. */
typedef struct SlagHeapStatistics {
  size_t total_heap_size;
  size_t total_heap_size_executable;
  size_t total_physical_size;
  size_t total_available_size;
  size_t used_heap_size;
  size_t heap_size_limit;
  size_t malloced_memory;
  size_t external_memory;
  size_t peak_malloced_memory;
  int32_t does_zap_garbage;
  size_t number_of_native_contexts;
  size_t number_of_detached_contexts;
  size_t total_global_handles_size;
  size_t used_global_handles_size;
  size_t embedded_blob_size;
} SlagHeapStatistics;
int32_t slag_heap_statistics(SlagContext* context, int64_t host_account,
                             SlagHeapStatistics* out);

/* Weak persistent handles (`v8::Global<T>::SetWeak`). The value is not kept
 * alive, and the callback runs after the collection that took it. */
typedef void (*SlagWeakCallback)(void* data);
SlagWeak* slag_weak_new(uint64_t value);
int32_t slag_weak_get(const SlagWeak* weak, uint64_t* out);
void slag_weak_clear(const SlagWeak* weak);
void slag_weak_free(SlagWeak* weak);
int32_t slag_weak_set_callback(const SlagWeak* weak, void* data,
                               SlagWeakCallback callback);
int32_t slag_weak_queue_second_pass(void* data, SlagWeakCallback callback);

/* The collection observer `v8::Isolate::AddGC*Callback` sits on: the phases are
 * prologue (0) and epilogue (1), and the type is a `SlagGcType`. */
enum SlagGcType {
  SLAG_GC_TYPE_SCAVENGE = 1,
  SLAG_GC_TYPE_MARK_SWEEP_COMPACT = 2,
};
typedef void (*SlagCollectionObserver)(void* data, int32_t phase, int32_t gc_type);
int32_t slag_watch_collections(SlagContext* context, void* data,
                               SlagCollectionObserver observer);

int32_t slag_array_buffer_detach(SlagContext* context, uint64_t value, uint64_t key,
                                int32_t has_key);
void slag_array_buffer_set_detach_key(SlagContext* context, uint64_t value, uint64_t key);

int32_t slag_collect(SlagContext* context);
int64_t slag_value_to_utf8(uint64_t value, char* buffer, size_t len);
int64_t slag_value_to_utf8_context(SlagContext* context, uint64_t value,
                                   char* buffer, size_t len);
double slag_value_to_number(uint64_t value);
int32_t slag_value_is_string(uint64_t value);
int32_t slag_value_is_undefined(uint64_t value);
/* Which `v8::Value` predicates the value answers (`SlagValueType` bits). The
 * brands need the context's agent, so a call outside any `Context::Scope`
 * answers only the shape bits. */
uint32_t slag_value_type_mask(SlagContext* context, uint64_t value);
int32_t slag_value_strict_equals(uint64_t left, uint64_t right);
int32_t slag_value_same_value(uint64_t left, uint64_t right);
int32_t slag_value_boolean(uint64_t value);
uint64_t slag_value_typeof(uint64_t value);
int32_t slag_value_is_callable(uint64_t value);
int32_t slag_value_is_constructor(SlagContext* context, uint64_t value);
/* The conversions that run host code, so they need the context. */
int32_t slag_value_to_string(SlagContext* context, uint64_t value, uint64_t* out);
int32_t slag_value_to_number_context(SlagContext* context, uint64_t value, double* out);
int32_t slag_value_to_object(SlagContext* context, uint64_t value, uint64_t* out);
/* The built-in error constructors (`v8::Exception`) and an arbitrary throw
 * (`Isolate::ThrowException`). */
int32_t slag_exception_new(SlagContext* context, int32_t kind, const uint8_t* text,
                           size_t len, uint64_t* out);
int32_t slag_throw_exception(SlagContext* context, uint64_t value);
/* A symbol with a description, which is how the facade models
 * `v8::Private`. */
int32_t slag_symbol_new(const uint8_t* text, size_t len, uint64_t* out);
int32_t slag_symbol_description(uint64_t value, uint64_t* out);
/* A fresh null-prototype object for an isolate's default security token. */
uint64_t slag_security_token_new(void);
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
                                 void* data, int32_t attributes);
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
