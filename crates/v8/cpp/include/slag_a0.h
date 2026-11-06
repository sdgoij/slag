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
typedef struct SlagModule SlagModule;

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
  /* `v8::ObjectTemplate::SetAccessCheckCallback`: whether a cross-realm access
   * to an instance of the template may proceed. Args are `(state, data,
   * holder)`; answer 1 to allow, 0 to refuse with `TypeError: Illegal access`.
   * The host asks `Isolate::GetEnteredContext()` for the accessing context, the
   * same way V8's callback is handed one. Only a cross-realm access consults
   * it, so a null pointer here means "never check", not "never access". */
  int32_t (*access_check)(void* state, uint64_t data, uint64_t holder);
  /* The access check's own trampoline context: a template can carry a check
   * without a named handler, so it cannot share `state`. */
  void* access_check_state;
} SlagPropertyHandler;

SlagIsolate* slag_isolate_new(void);
void slag_isolate_free(SlagIsolate* isolate);
SlagContext* slag_context_new(SlagIsolate* isolate);
void slag_context_free(SlagContext* context);

int32_t slag_eval(SlagContext* context, const uint8_t* source, size_t len, uint64_t* out);
int32_t slag_compile(SlagContext* context, const uint8_t* source, size_t len,
                     const uint8_t* name, size_t name_len, SlagScript** out);
int32_t slag_script_run(SlagContext* context, SlagScript* script, uint64_t* out);

/* `slag_script_run`'s extra code: a debug session stopped the run at a debug
 * point. The state is the parked run (`slag_debug_paused`), and no error is
 * pending, so a host must not report one. */
#define SLAG_RUN_PAUSED 3

/* A parked run (`v8::Context::Paused` in the facade). `reason` is one of the
 * SLAG_PAUSE_* values; `start`/`end` are the source offsets of the debug point
 * that stopped the run (0 when there is no position). */
typedef struct SlagPause {
  uint32_t reason;
  uint32_t start;
  uint32_t end;
} SlagPause;

enum SlagPauseReason {
  SLAG_PAUSE_NONE = 0,
  /* A host `slag_debug_pause` request was observed at a debug point. */
  SLAG_PAUSE_REQUESTED = 1,
  /* A debug point's offset was one of the session's breakpoints. */
  SLAG_PAUSE_BREAKPOINT = 2,
  /* A step the host asked the previous resume for stopped the run. */
  SLAG_PAUSE_STEP = 3
};

/* The step a resume should stop after (`slag_debug_step`). */
enum SlagStepMode {
  SLAG_STEP_INTO = 1,
  SLAG_STEP_OVER = 2,
  SLAG_STEP_OUT = 3
};

/* The debugger surface (see the facade's `v8::Context` extensions). A session
 * must be attached before any debug point exists, and the engine recompiles its
 * bytecode on the attach and the detach, so the points cost nothing otherwise.
 * Each answers 0 on success, 1 with a pending error, and 2 for a bad handle. */
int32_t slag_debug_attach(SlagContext* context, int32_t attached);
/* Replace the session's breakpoints: `count` source offsets, which is what
 * resolving a (line, column) against the script text produces. */
int32_t slag_debug_set_breakpoints(SlagContext* context, const uint32_t* offsets,
                                   size_t count);
int32_t slag_debug_pause(SlagContext* context);
/* 1 when a run is parked (and `out` is filled), 0 when none is. */
int32_t slag_debug_paused(SlagContext* context, SlagPause* out);
/* Ask the next resume to stop after one step: `SLAG_STEP_INTO` stops at the
 * next point anywhere, `SLAG_STEP_OVER` at the next point of the halted
 * activation or an outer one, and `SLAG_STEP_OUT` once it has returned.
 * Answers 3 for a mode this ABI does not have. */
int32_t slag_debug_step(SlagContext* context, int32_t mode);
/* Continue the parked run; its completion value is written through `out`. */
int32_t slag_debug_resume(SlagContext* context, uint64_t* out);
/* Evaluate `source` at the stopped position (`Debugger.evaluateOnCallFrame`):
 * a direct eval under the paused run's context, answering its value. */
int32_t slag_debug_evaluate(SlagContext* context, const uint8_t* source, size_t len,
                            uint64_t* out);

/* The inspector: a Chrome DevTools Protocol session over a *context group*
 * (`v8-inspector.h` in the facade). The engine speaks in source offsets and the
 * protocol in script ids, URLs and (line, column) commands, so the session owns
 * that translation: a script registered here is what a URL breakpoint resolves
 * against, and a stop becomes a `Debugger.paused` with its call frames.
 *
 * A group is what `V8ContextInfo` names: an embedder's grouping (Chromium
 * groups by frame) whose worlds are one context each. Script ids are the
 * group's, so a client sees the same `scriptId` wherever the script was
 * compiled, and a stop names the context it happened in through
 * `executionContextId`. The protocol itself is not in C++; `dispatch` and
 * `poll` are the whole seam. */

/* One session over one context group. */
typedef struct SlagInspector SlagInspector;

/* One protocol message: a JSON text the session produced, owned by the
 * `SlagMessages` that lists it. */
typedef struct SlagProtocolMessage {
  uint8_t* ptr;
  size_t len;
} SlagProtocolMessage;

/* The messages one call produced, in the session's delivery order. A dispatch
 * answers the events it caused first and the response to its command last, so a
 * transport replies with the last message and notifies with the rest; a poll
 * answers only events. Free with `slag_messages_free`. */
typedef struct SlagMessages {
  SlagProtocolMessage* items;
  size_t len;
} SlagMessages;

/* Create a group session. Answers 0 on success, 2 for a bad argument. */
int32_t slag_inspector_new(SlagInspector** out);
/* Free the handle. Every context leaves the group first; `slag_inspector_close`
 * is what resumes a parked run, so a host calls that first. */
void slag_inspector_free(SlagInspector* inspector);
/* Add a context to the group, answering the execution context id it was given.
 * Answers 3 when the context already belongs to a group. */
int32_t slag_inspector_add_context(SlagInspector* inspector, SlagContext* context,
                                   uint32_t* out_id);
/* Remove a context (`contextDestroyed`). Answers 3 when it is not a member. */
int32_t slag_inspector_remove_context(SlagInspector* inspector, SlagContext* context);
/* Connect: start a session over the group (idempotent). */
int32_t slag_inspector_open(SlagInspector* inspector);
/* Disconnect: a parked run in any context continues, every context detaches and
 * the script registry is dropped. */
int32_t slag_inspector_close(SlagInspector* inspector);
/* Continue a parked run (`V8InspectorSession::resume`), answering the events the
 * resume produced and not the response a command would have carried. */
int32_t slag_inspector_resume(SlagInspector* inspector, SlagMessages* out);
/* Feed one protocol message, answering its events and its response. Answers 0
 * even when the response is a protocol error, which is data. */
int32_t slag_inspector_dispatch(SlagInspector* inspector, const uint8_t* message,
                                size_t len, SlagMessages* out);
/* The events the runs produced: the script announcements a compile queued, then
 * a `Debugger.paused` for a stop that has not been announced. */
int32_t slag_inspector_poll(SlagInspector* inspector, SlagMessages* out);
/* Free the messages a call answered, and the array that listed them. */
void slag_messages_free(SlagMessages* messages);
/* One frame of a reported stack: the function name and the script's URL or name
 * as UTF-8 spans borrowed for the call, and the 1-based position. */
typedef struct SlagExceptionFrame {
  const uint8_t* function_name;
  size_t function_name_len;
  const uint8_t* url;
  size_t url_len;
  uint32_t line;
  uint32_t column;
} SlagExceptionFrame;

/* An uncaught exception a host reports (`V8Inspector::exceptionThrown`).
 * `is_error` is 1 when the thrown value is an error object, and `frames` is the
 * stack the host captured where the value was created, innermost first. */
typedef struct SlagException {
  const uint8_t* message;
  size_t message_len;
  const uint8_t* url;
  size_t url_len;
  uint32_t line;
  uint32_t column;
  int32_t is_error;
  const SlagExceptionFrame* frames;
  size_t frame_count;
} SlagException;

/* Report an uncaught exception, answering through `out_id` the id that names it
 * (0 when no client enabled `Runtime`) and through `out` the events that client
 * is owed. */
int32_t slag_inspector_exception(SlagInspector* inspector,
                                 const SlagException* report, uint32_t* out_id,
                                 SlagMessages* out);
/* Install a breakpoint a browser hands over when a client attaches. Answers 3
 * when the group has no script with that URL yet, which such a breakpoint can
 * outlive. */
int32_t slag_inspector_embedder_breakpoint(SlagInspector* inspector,
                                           const uint8_t* url, size_t url_len,
                                           uint32_t line, uint32_t column);
/* Withdraw an exception a handler took (`V8Inspector::exceptionRevoked`). */
int32_t slag_inspector_exception_revoked(SlagInspector* inspector,
                                         uint32_t exception_id,
                                         const uint8_t* reason, size_t reason_len,
                                         SlagMessages* out);
/* Evaluate `source` in `context` (`V8InspectorSession::evaluate`): how a host
 * injects what it installs for the command line. */
int32_t slag_inspector_evaluate(SlagInspector* inspector, SlagContext* context,
                                const uint8_t* source, size_t len, uint64_t* out);
/* Register text the host did not compile through `slag_compile`, which
 * registers the scripts it compiles itself. The `scriptParsed` event is
 * answered by the next `slag_inspector_poll`. */
int32_t slag_inspector_add_script(SlagInspector* inspector, SlagContext* context,
                                  const uint8_t* name, size_t name_len,
                                  const uint8_t* source, size_t source_len);
/* Whether the session answers `method`
 * (`V8InspectorSession::canDispatchMethod`). */
int32_t slag_inspector_can_dispatch(const uint8_t* method, size_t len);
/* The id the command `message` answers, or -1 for a message that names none:
 * what a transport needs to tell a response from an event. */
int64_t slag_inspector_call_id(const uint8_t* message, size_t len);
/* Drain the job queues. 1 when a job was queued, 0 when none was, -1 with a
 * pending error when a job threw. */
int32_t slag_drain_jobs(SlagContext* context);

/* Host microtask queues (v8::MicrotaskQueue). Ids are never reused. */
int32_t slag_microtask_queue_new(SlagContext* context, int32_t policy);
void slag_microtask_queue_release(SlagContext* context, uint32_t id);
int32_t slag_context_set_microtask_queue(SlagContext* context, uint32_t id);
int32_t slag_microtask_queue_run(SlagContext* context, uint32_t id);
void slag_script_free(SlagScript* script);
int32_t slag_register_fn(SlagContext* context, const uint8_t* name, size_t name_len,
                         slag_host_fn callback, void* data);
int32_t slag_call_eval(const void* call, const uint8_t* source, size_t len,
                       uint64_t* out);

void slag_throw_error(SlagContext* context, const uint8_t* message, size_t len);
/* Record a pending error of `kind` (the `slag_exception_new` codes: 0 Error,
 * 5 TypeError) without building the language object, for a trampoline that holds
 * the engine. The call turns it into the thrown value. */
void slag_throw_error_kind(SlagContext* context, int32_t kind, const uint8_t* message,
                           size_t len);
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
uint64_t slag_null(void);
uint64_t slag_boolean_new(int32_t value);
uint64_t slag_string_new(const uint8_t* text, size_t len);
/* A string from Latin-1 bytes: each byte is one code unit, so 0xE9 is U+00E9
 * (`v8::String::NewFromOneByte`). */
uint64_t slag_string_new_one_byte(const uint8_t* text, size_t len);
/* A string from UTF-16 code units (`v8::String::NewFromTwoByte`). */
uint64_t slag_string_new_two_byte(const uint16_t* units, size_t len);
/* The UTF-16 code unit count (`v8::String::Length`), or -1 for a non-string. */
int64_t slag_string_length(uint64_t value);
/* Whether every code unit fits in one byte
 * (`v8::String::ContainsOnlyOneByte`). */
int32_t slag_string_contains_only_one_byte(uint64_t value);
/* The `v8::String::Write` family: `start` is a code unit index, `length` a code
 * unit (or, for the UTF-8 form, byte) count, and a negative length means the
 * rest of the string. Each answers the number of units written (-1 for a
 * non-string). `slag_string_write_utf8` takes the buffer's capacity including
 * the terminator, writes one when `null_terminate` asks for it, counts it in
 * its answer, and reports the code units it covered through `written`, which
 * is V8's `nchars_ref`. */
int64_t slag_string_code_units(uint64_t value, int64_t start, int64_t length,
                               uint16_t* out);
int64_t slag_string_write_one_byte(uint64_t value, int64_t start, int64_t length,
                                   uint8_t* out);
int64_t slag_string_write_utf8(uint64_t value, int64_t start, int64_t capacity,
                               int32_t replace_invalid, int32_t null_terminate,
                               uint8_t* out, int64_t* written);
/* The UTF-8 byte count (`v8::String::Utf8Length`), or -1 for a non-string. */
int64_t slag_string_utf8_length(uint64_t value);
/* Promises (`v8::Promise`, `v8::Promise::Resolver`). A resolver handle is the
 * pair's resolving function, as in V8; the rejecting one and the promise are
 * kept against it for the context's life, so `GetPromise` and `Reject` can name
 * them back. Each entry the engine has to run has a callback form for use inside
 * a host callback, where the engine is borrowed. `slag_promise_state` answers 0
 * pending, 1 fulfilled, or 2 rejected, and a negated status otherwise. */
int32_t slag_promise_new_resolver(SlagContext* context, uint64_t* out_resolver,
                                  uint64_t* out_promise);
int32_t slag_promise_resolver_promise(SlagContext* context, uint64_t resolver,
                                      uint64_t* out);
int32_t slag_promise_resolver_settle(SlagContext* context, uint64_t resolver,
                                     uint64_t value, int32_t reject,
                                     int32_t* settled);
int32_t slag_promise_state(SlagContext* context, uint64_t promise);
int32_t slag_promise_result(SlagContext* context, uint64_t promise, uint64_t* out);
int32_t slag_promise_mark_handled(SlagContext* context, uint64_t promise);
int32_t slag_promise_then(SlagContext* context, uint64_t promise,
                          uint64_t on_fulfilled, uint64_t on_rejected,
                          uint64_t* out);
int32_t slag_callback_promise_new_resolver(SlagContext* context, const void* token,
                                           uint64_t* out_resolver,
                                           uint64_t* out_promise);
int32_t slag_callback_promise_resolver_settle(SlagContext* context,
                                              const void* token, uint64_t resolver,
                                              uint64_t value, int32_t reject,
                                              int32_t* settled);
int32_t slag_callback_promise_state(SlagContext* context, const void* token,
                                    uint64_t promise);
int32_t slag_callback_promise_result(SlagContext* context, const void* token,
                                     uint64_t promise, uint64_t* out);
int32_t slag_callback_promise_mark_handled(SlagContext* context, const void* token,
                                           uint64_t promise);
int32_t slag_callback_promise_then(SlagContext* context, const void* token,
                                   uint64_t promise, uint64_t on_fulfilled,
                                   uint64_t on_rejected, uint64_t* out);
/* `v8::Promise::Resolve` and `Reject`: a promise settled with the value (the value
 * itself when it already is a promise of the realm). */
int32_t slag_promise_resolved(SlagContext* context, uint64_t value, uint64_t* out);
int32_t slag_promise_rejected(SlagContext* context, uint64_t reason, uint64_t* out);
int32_t slag_callback_promise_resolved(SlagContext* context, const void* token,
                                       uint64_t value, uint64_t* out);
int32_t slag_callback_promise_rejected(SlagContext* context, const void* token,
                                       uint64_t reason, uint64_t* out);
/* A DataView over a buffer (`v8::DataView::New`); the engine validates the
 * geometry. */
int32_t slag_data_view_new(SlagContext* context, uint64_t buffer,
                           size_t byte_offset, size_t byte_length, uint64_t* out);
/* Containers (`v8::Map`, `v8::Set`). Each operation runs the engine's own
 * `%Map.prototype.*%`/`%Set.prototype.*%` intrinsic and `Size` reads the
 * internal [[MapData]]/[[SetData]], so a script cannot change what a host's call
 * does. These take the ordinary route only: a host callback that calls one is
 * refused with a re-entrancy error rather than racing the engine's borrow. */
int32_t slag_map_new(SlagContext* context, uint64_t* out);
int32_t slag_map_set(SlagContext* context, uint64_t map, uint64_t key,
                     uint64_t value);
int32_t slag_map_get(SlagContext* context, uint64_t map, uint64_t key,
                     uint64_t* out);
int32_t slag_map_has(SlagContext* context, uint64_t map, uint64_t key,
                     int32_t* out);
int32_t slag_map_delete(SlagContext* context, uint64_t map, uint64_t key,
                        int32_t* out);
int32_t slag_map_clear(SlagContext* context, uint64_t map);
int32_t slag_map_size(SlagContext* context, uint64_t map);
int32_t slag_map_as_array(SlagContext* context, uint64_t map, uint64_t* out);
int32_t slag_set_new(SlagContext* context, uint64_t* out);
int32_t slag_set_add(SlagContext* context, uint64_t set, uint64_t value);
int32_t slag_set_has(SlagContext* context, uint64_t set, uint64_t value,
                     int32_t* out);
int32_t slag_set_delete(SlagContext* context, uint64_t set, uint64_t value,
                        int32_t* out);
int32_t slag_set_clear(SlagContext* context, uint64_t set);
int32_t slag_set_size(SlagContext* context, uint64_t set);
int32_t slag_set_as_array(SlagContext* context, uint64_t set, uint64_t* out);
uint64_t slag_number_new(double value);

/* BigInt (v8::BigInt). Magnitudes cross as 64-bit words, least significant
 * first, which is how V8's NewFromWords/ToWordsArray spell them. */
uint64_t slag_bigint_new(int64_t value);
uint64_t slag_bigint_new_from_unsigned(uint64_t value);
int32_t slag_bigint_new_from_words(SlagContext* context, int32_t negative,
                                   int32_t word_count, const uint64_t* words,
                                   uint64_t* out);
uint64_t slag_bigint_uint64_value(uint64_t value, int32_t* lossless);
int64_t slag_bigint_int64_value(uint64_t value, int32_t* lossless);
int32_t slag_bigint_word_count(uint64_t value);
void slag_bigint_to_words(uint64_t value, int32_t* negative,
                          int32_t* word_count, uint64_t* words);
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
                                SlagBufferDeleter deleter, void* deleter_data,
                                int32_t counted);
void* slag_buffer_data(const SlagBuffer* buffer);
size_t slag_buffer_byte_length(const SlagBuffer* buffer);
/* A view's geometry (`v8::ArrayBufferView::ByteOffset` and `ByteLength`): a typed
 * array answers from its own slots, a `DataView` from the context's record. */
size_t slag_array_view_byte_offset(SlagContext* context, uint64_t value);
size_t slag_array_view_byte_length(SlagContext* context, uint64_t value);
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

/* A realm's snapshot (`v8::StartupData`, written by `SnapshotCreator::CreateBlob`).
 * A null or too-small buffer answers the required length; -1 is a failed
 * write. */
int64_t slag_context_write_snapshot(SlagContext* context, uint8_t* buffer, size_t len);
int32_t slag_context_read_snapshot(SlagContext* context, const uint8_t* data, size_t len);

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

/* Modules (`v8::Module`). A module box is never freed: the facade gives a host
 * a handle over it for as long as the module is reachable. */
enum SlagModuleStatus {
  SLAG_MODULE_UNINSTANTIATED = 0,
  SLAG_MODULE_INSTANTIATING = 1,
  SLAG_MODULE_INSTANTIATED = 2,
  SLAG_MODULE_EVALUATING = 3,
  SLAG_MODULE_EVALUATED = 4,
  SLAG_MODULE_ERRORED = 5,
};
int32_t slag_module_compile(SlagContext* context, const uint8_t* specifier,
                            size_t specifier_len, const uint8_t* name,
                            size_t name_len, const uint8_t* source,
                            size_t source_len, SlagModule** out);
int32_t slag_module_status(const SlagModule* module);
int32_t slag_module_exception(const SlagModule* module, uint64_t* out);
int32_t slag_module_identity_hash(const SlagModule* module);
int32_t slag_module_is_source_text(const SlagModule* module);
int32_t slag_module_is_synthetic(const SlagModule* module);
size_t slag_module_request_count(const SlagModule* module);
int32_t slag_module_request(const SlagModule* module, size_t index, uint64_t* out);
int32_t slag_module_source_offset_to_location(const SlagModule* module, int32_t offset,
                                              int32_t* line, int32_t* column);
int32_t slag_module_register(SlagContext* context, const SlagModule* module,
                             uint64_t specifier);
int32_t slag_module_instantiate(SlagContext* context, const SlagModule* module);
int32_t slag_module_evaluate(SlagContext* context, const SlagModule* module,
                             uint64_t* out);
int32_t slag_module_namespace(SlagContext* context, const SlagModule* module,
                              uint64_t* out);

/* WebAssembly module objects and their decoded modules (`v8::WasmModuleObject`
 * and `v8::CompiledWasmModule`). */
typedef struct SlagWasmModule SlagWasmModule;
int32_t slag_wasm_compile(SlagContext* context, const uint8_t* wire_bytes,
                          size_t wire_len, uint64_t* out);
int32_t slag_wasm_get_compiled(SlagContext* context, uint64_t object,
                               SlagWasmModule** out);
int32_t slag_wasm_from_compiled(SlagContext* context,
                                const SlagWasmModule* compiled, uint64_t* out);
void slag_wasm_module_free(SlagWasmModule* compiled);
/* A `WebAssembly.Instance` and a `WebAssembly.Memory` (`v8::WasmInstanceObject`
 * and `v8::WasmMemoryObject`). The instance's exports are the engine's own
 * exports record, so the brand and the lookups come from the same table. */
int32_t slag_wasm_is_instance(SlagContext* context, uint64_t value);
int32_t slag_wasm_is_memory(SlagContext* context, uint64_t value);
int32_t slag_wasm_instance_export(SlagContext* context, uint64_t instance,
                                  uint32_t index, uint64_t* out, int32_t* found);
int32_t slag_wasm_instance_export_named(SlagContext* context, uint64_t instance,
                                        uint64_t name, uint64_t* out,
                                        int32_t* found);
int32_t slag_wasm_instance_function_index(SlagContext* context,
                                          uint64_t instance, uint64_t name);
int32_t slag_wasm_instance_memory(SlagContext* context, uint64_t instance,
                                  uint64_t* out, int32_t* found);
int32_t slag_wasm_memory_buffer(SlagContext* context, uint64_t memory,
                                uint64_t* out);

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
/* The ToInt32/ToUint32/ToIntegerOrInfinity forms, which convert with ToNumber
 * first (`v8::Value::Int32Value`, `Uint32Value`, `IntegerValue`). A failed
 * conversion leaves its error pending and answers 1. */
int32_t slag_value_to_int32(SlagContext* context, uint64_t value, int32_t* out);
int32_t slag_value_to_uint32(SlagContext* context, uint64_t value, uint32_t* out);
int32_t slag_value_to_integer(SlagContext* context, uint64_t value, int64_t* out);
/* The same conversions inside a running host callback, where the engine is
 * borrowed and only the callback's window can run ToNumber. Returns 2 when
 * `token` names no running callback. */
int32_t slag_callback_value_to_int32(SlagContext* context, const void* token,
                                     uint64_t value, int32_t* out);
int32_t slag_callback_value_to_uint32(SlagContext* context, const void* token,
                                      uint64_t value, uint32_t* out);
int32_t slag_callback_value_to_integer(SlagContext* context, const void* token,
                                       uint64_t value, int64_t* out);
/* The built-in error constructors (`v8::Exception`) and an arbitrary throw
 * (`Isolate::ThrowException`). */
int32_t slag_exception_new(SlagContext* context, int32_t kind, const uint8_t* text,
                           size_t len, uint64_t* out);
int32_t slag_throw_exception(SlagContext* context, uint64_t value);
/* A symbol with a description, which is how the facade models
 * `v8::Private`. */
int32_t slag_symbol_new(const uint8_t* text, size_t len, uint64_t* out);
int32_t slag_symbol_description(uint64_t value, uint64_t* out);
/* The realm an object was created in, as the realm's own signifier (`Realm`'s
 * `signifier`); 0 for a value that is not an object, and for an object no
 * realm recorded (a host built it outside any context, from a protoless
 * prototype). A function answers its object part's realm. Two values are in the
 * same realm when the two answers are equal and non-zero, which is what
 * `slag_value_same_realm` spells out. */
uint32_t slag_value_realm(uint64_t value);
int32_t slag_value_same_realm(uint64_t left, uint64_t right);
/* The realm a context's objects are created in. This is the comparing half of
 * an access check: a value's realm against the entered context's. */
uint32_t slag_context_realm(SlagContext* context);
/* Make a context's realm the one objects created from here on belong to,
 * answering the realm to restore (`v8::Context::Scope`). */
uint32_t slag_context_enter(SlagContext* context);
void slag_context_leave(uint32_t realm);

/* One captured call frame (`v8::StackFrame`): the script's id (0 when the code
 * carries none), the 1-based line and 1-based column (0 when unknown), and the
 * function and script names as tagged `Value` bits (0 when the frame has
 * none). The bits are engine strings the caller does not own. */
typedef struct SlagStackFrame {
  uint32_t script_id;
  int32_t line;
  int32_t column;
  uint64_t function_name;
  uint64_t script_name;
} SlagStackFrame;
/* How many frames the running stack has, innermost first
 * (`v8::StackTrace::GetFrameCount`). */
int32_t slag_stack_frame_count(SlagContext* context);
/* Frame `index` of the running stack, innermost first, written to `out`.
 * Answers 0 when the frame exists, 2 for a bad argument. */
int32_t slag_stack_frame(SlagContext* context, size_t index, SlagStackFrame* out);
/* The same inside a running host callback, where the engine is borrowed and
 * only the callback's window can read the stack. */
int32_t slag_callback_stack_frame_count(SlagContext* context, const void* token);
int32_t slag_callback_stack_frame(SlagContext* context, const void* token,
                                  size_t index, SlagStackFrame* out);
/* A fresh null-prototype object for an isolate's default security token. */
uint64_t slag_security_token_new(void);
uint64_t slag_global(SlagContext* context);
int32_t slag_call(SlagContext* context, uint64_t function, uint64_t this_value,
                  const uint64_t* args, size_t len, uint64_t* out);
int32_t slag_construct(SlagContext* context, uint64_t constructor,
                       const uint64_t* args, size_t len, uint64_t* out);
int32_t slag_template_constructor(SlagContext* context, const uint8_t* name,
                                  size_t name_len, uint32_t length, uint64_t proto,
                                  size_t internal_fields, int32_t api_wrapper,
                                  int32_t allow_construction, slag_host_fn callback,
                                  void* data, uint64_t* out);
int32_t slag_object_prototype(SlagContext* context, uint64_t object, uint64_t* out);
/* Whether `prototype` is on `value`'s prototype chain (`v8::Signature`'s
 * receiver check): 1 yes, 0 no. The callback form is for a trampoline, where the
 * engine is borrowed. */
int32_t slag_value_has_prototype_in_chain(SlagContext* context, uint64_t value,
                                          uint64_t prototype);
int32_t slag_callback_value_has_prototype_in_chain(SlagContext* context,
                                                   const void* token, uint64_t value,
                                                   uint64_t prototype);
int32_t slag_object_set_prototype(SlagContext* context, uint64_t object,
                                  uint64_t prototype);
int32_t slag_object_new(SlagContext* context, size_t internal_fields,
                        int32_t api_wrapper, uint64_t* out);
int32_t slag_object_new_handler(SlagContext* context, size_t internal_fields,
                                int32_t api_wrapper, const SlagPropertyHandler* handler,
                                uint64_t* out);
/* An object that is also callable (a template instance built with
 * `SetCallAsFunctionHandler`): a builtin whose body runs `callback`, with its
 * object part carrying `internal_fields`. */
int32_t slag_object_new_callable(SlagContext* context, size_t internal_fields,
                                 int32_t api_wrapper, slag_host_fn callback,
                                 void* data, uint64_t* out);

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
