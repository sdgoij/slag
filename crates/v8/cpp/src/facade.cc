#include "include/slag_a0.h"
#include "include/v8.h"

#include <cstring>
#include <cstdlib>
#include <deque>
#include <string>
#include <vector>

namespace {

/* The one handle stack for this process. `HandleScope` marks it and restores
 * the mark; `Local<T>::New` allocates from it. A rooted slot retains its value
 * in the C ABI's root registry; a facade pointer slot does not. */
thread_local std::deque<v8::HandleSlot> g_handles;

/* The token of the host callback that is running, or null. A callback's property
 * work goes through the window it names; outside one, the ordinary entry points
 * run. Declared here because the own-property helper below reads it. */
thread_local const void* g_callback_token = nullptr;

std::size_t HandleMark() { return g_handles.size(); }

v8::HandleSlot* HandleCreate(uint64_t value, bool rooted) {
  if (rooted) slag_root_retain(value);
  g_handles.push_back(v8::HandleSlot{value, rooted});
  return &g_handles.back();
}

void HandleRestore(std::size_t mark) {
  if (mark > g_handles.size()) return;
  for (std::size_t i = mark; i < g_handles.size(); ++i) {
    if (g_handles[i].rooted) slag_root_release(g_handles[i].value);
  }
  g_handles.resize(mark);
}

std::string ValueText(uint64_t value) {
  int64_t len = slag_value_to_utf8(value, nullptr, 0);
  if (len < 0) return {};
  std::vector<char> buffer(static_cast<size_t>(len) + 1);
  slag_value_to_utf8(value, buffer.data(), buffer.size());
  return std::string(buffer.data(), static_cast<size_t>(len));
}

/* Which of V8's own-property kinds `key` names on `object`: 0 absent, 1 data
 * named, 2 data indexed, 3 callback named, 4 callback indexed. -1 when the
 * engine reported an error, which is then pending. */
int OwnPropertyKind(v8::Local<v8::Context> context, const v8::Object* object,
                    uint64_t key) {
  auto* current = reinterpret_cast<v8::Context*>(
      static_cast<uintptr_t>(context.raw()));
  int32_t kind = -1;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_own_property_kind(
                current->slag(), g_callback_token, v8::Raw(object), key, &kind)
          : slag_object_own_property_kind(current->slag(), v8::Raw(object), key,
                                          &kind);
  if (status != 0) {
    return -1;
  }
  return kind;
}

/* The A0 context entered on this thread, or null. V8's predicates read the
 * isolate off the handle's location; the facade keeps the entered context in a
 * thread-local instead, which is the same scope. */
SlagContext* CurrentSlagContext() {
  const auto* context = reinterpret_cast<const v8::Context*>(
      static_cast<uintptr_t>(v8::g_current_context));
  return context == nullptr ? nullptr : context->slag();
}

/* Whether `receiver` is an instance of the type `prototype` names: the engine
 * walks the chain (`slag_value_has_prototype_in_chain`), through the callback's
 * window when a trampoline is running and the ordinary path otherwise. */
bool InstanceOf(uint64_t receiver, uint64_t prototype) {
  if (prototype == 0) return false;
  const int answer =
      g_callback_token != nullptr
          ? slag_callback_value_has_prototype_in_chain(
                CurrentSlagContext(), g_callback_token, receiver, prototype)
          : slag_value_has_prototype_in_chain(CurrentSlagContext(), receiver,
                                              prototype);
  return answer == 1;
}

uint64_t HostTrampoline(void* data, const void* call, uint64_t this_value,
                        const uint64_t* args, size_t len) {
  auto* function = static_cast<v8::FunctionTemplate*>(data);
  if (function == nullptr) return slag_undefined();
  // A host callback may allocate or write through the entry points, which are
  // not re-entered from a callback unless the token names the running window.
  // The signature check reads the receiver's prototype chain, which is one of
  // those entry points, so the window is published before it runs.
  const void* previous = g_callback_token;
  g_callback_token = slag_callback_token();
  uint64_t result = slag_undefined();
  if (!function->AllowsReceiver(this_value)) {
    // V8's access check: a method installed with a signature does not run on a
    // receiver that is not an instance, and the call fails with a TypeError.
    static constexpr char kRefusal[] = "Illegal invocation";
    slag_throw_error_kind(CurrentSlagContext(), 5,
                          reinterpret_cast<const uint8_t*>(kRefusal),
                          sizeof(kRefusal) - 1);
  } else {
    v8::FunctionCallbackInfo<v8::Value> info(v8::g_current_isolate, call,
                                             this_value, args,
                                             static_cast<int>(len),
                                             function->data().raw());
    function->callback()(info);
    result = info.return_value();
  }
  g_callback_token = previous;
  return result;
}

/* The body of a callable template instance
 * (`ObjectTemplate::SetCallAsFunctionHandler`): the template is the data, so the
 * handler and its data value are read at call time. */
uint64_t CallAsFunctionTrampoline(void* data, const void* call,
                                  uint64_t this_value, const uint64_t* args,
                                  size_t len) {
  auto* tmpl = static_cast<v8::ObjectTemplate*>(data);
  if (tmpl == nullptr || tmpl->call_as_function() == nullptr) {
    return slag_undefined();
  }
  v8::FunctionCallbackInfo<v8::Value> info(
      v8::g_current_isolate, call, this_value, args, static_cast<int>(len),
      tmpl->call_as_function_data().raw());
  const void* previous = g_callback_token;
  g_callback_token = slag_callback_token();
  tmpl->call_as_function()(info);
  g_callback_token = previous;
  return info.return_value();
}

/* The property-handler callbacks, adapted from the flat ABI to the C++ shapes.
 * `state` is the configuration the template installed, so the trampoline can
 * reach the host's own callback; the answer slot is where the host writes. */
v8::NamedPropertyHandlerConfiguration* Configuration(void* state) {
  return static_cast<v8::NamedPropertyHandlerConfiguration*>(state);
}

/* The token of the host callback that is running, for as long as it runs. The
 * ABI reports it (`slag_callback_token`), and the allocation entry points admit
 * only a token that names a running callback, so a host's `Object::New` inside a
 * getter allocates through the operation that called the handler. */
class CallbackToken {
 public:
  CallbackToken() : previous_(g_callback_token) {
    token_ = slag_callback_token();
    g_callback_token = token_;
  }
  ~CallbackToken() { g_callback_token = previous_; }

  const void* get() const { return token_; }

 private:
  const void* previous_ = nullptr;
  const void* token_ = nullptr;
};

v8::IndexedPropertyHandlerConfiguration* IndexedConfiguration(void* state) {
  return static_cast<v8::IndexedPropertyHandlerConfiguration*>(state);
}

int32_t IndexedGetterTrampoline(void* state, uint64_t data, uint64_t holder,
                                uint32_t index, uint64_t receiver,
                                uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->getter == nullptr ||
      out == nullptr) {
    return 0;
  }
  (void)receiver;
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Value> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(configuration->getter(index, info));
}

int32_t IndexedSetterTrampoline(void* state, uint64_t data, uint64_t holder,
                                uint32_t index, uint64_t value,
                                uint64_t receiver, int32_t throw_on_error) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->setter == nullptr) return 0;
  (void)receiver;
  (void)throw_on_error;
  uint64_t answer = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<void> info(v8::g_current_isolate, holder, data,
                                      &answer, token.get());
  return static_cast<int32_t>(
      configuration->setter(index, v8::Local<v8::Value>::New(value), info));
}

int32_t IndexedQueryTrampoline(void* state, uint64_t data, uint64_t holder,
                               uint32_t index, uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->query == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Integer> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(configuration->query(index, info));
}

int32_t IndexedDeleterTrampoline(void* state, uint64_t data, uint64_t holder,
                                 uint32_t index, uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->deleter == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Boolean> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(configuration->deleter(index, info));
}

int32_t GetterTrampoline(void* state, uint64_t data, uint64_t holder,
                         uint64_t key, uint64_t receiver, uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->getter == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Value> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(
      configuration->getter(v8::Local<v8::Name>::New(key), info));
}

int32_t SetterTrampoline(void* state, uint64_t data, uint64_t holder,
                         uint64_t key, uint64_t value, uint64_t receiver,
                         int32_t throw_on_error) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->setter == nullptr) return 0;
  (void)throw_on_error;
  uint64_t answer = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<void> info(v8::g_current_isolate, holder, data,
                                      &answer, token.get());
  return static_cast<int32_t>(configuration->setter(
      v8::Local<v8::Name>::New(key), v8::Local<v8::Value>::New(value), info));
}

int32_t QueryTrampoline(void* state, uint64_t data, uint64_t holder,
                        uint64_t key, uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->query == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Integer> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(
      configuration->query(v8::Local<v8::Name>::New(key), info));
}

int32_t DeleterTrampoline(void* state, uint64_t data, uint64_t holder,
                          uint64_t key, uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->deleter == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Boolean> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(
      configuration->deleter(v8::Local<v8::Name>::New(key), info));
}

int32_t EnumeratorTrampoline(void* state, uint64_t data, uint64_t holder,
                             uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->enumerator == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Array> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(configuration->enumerator(info));
}

int32_t IndexedEnumeratorTrampoline(void* state, uint64_t data, uint64_t holder,
                                    uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->enumerator == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Array> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(configuration->enumerator(info));
}

int32_t AccessorGetterTrampoline(void* data, uint64_t holder, uint64_t key,
                                 uint64_t* out) {
  auto* accessor = static_cast<v8::ObjectTemplate::Accessor*>(data);
  if (accessor == nullptr || accessor->getter == nullptr || out == nullptr) {
    return 1;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Value> info(v8::g_current_isolate, holder,
                                           accessor->data_bits, out,
                                           token.get());
  accessor->getter(v8::Local<v8::Name>::New(key), info);
  return 0;
}

int32_t AccessorSetterTrampoline(void* data, uint64_t holder, uint64_t key,
                                 uint64_t value) {
  auto* accessor = static_cast<v8::ObjectTemplate::Accessor*>(data);
  if (accessor == nullptr || accessor->setter == nullptr) return 1;
  CallbackToken token;
  uint64_t answer = slag_undefined();
  v8::PropertyCallbackInfo<void> info(v8::g_current_isolate, holder,
                                      accessor->data_bits, &answer,
                                      token.get());
  accessor->setter(v8::Local<v8::Name>::New(key),
                   v8::Local<v8::Value>::New(value), info);
  return 0;
}

/* The FunctionTemplate pair behind `Object::SetAccessorProperty`: each run
 * builds the info a FunctionCallback takes, with the receiver as `this` and (for
 * a setter) the assigned value as the only argument. */
struct TemplateAccessor {
  v8::FunctionCallback getter = nullptr;
  v8::FunctionCallback setter = nullptr;
};

int32_t TemplateAccessorGetter(void* data, uint64_t holder, uint64_t,
                               uint64_t* out) {
  auto* accessor = static_cast<TemplateAccessor*>(data);
  if (accessor == nullptr || accessor->getter == nullptr || out == nullptr) {
    return 1;
  }
  *out = slag_undefined();
  v8::FunctionCallbackInfo<v8::Value> info(v8::g_current_isolate, nullptr,
                                           holder, nullptr, 0);
  // The getter runs inside the engine's callback window, so the conversion and
  // allocation entry points have to see the token the way a host function's
  // `HostTrampoline` publishes it.
  CallbackToken token;
  accessor->getter(info);
  *out = info.return_value();
  return 0;
}

int32_t TemplateAccessorSetter(void* data, uint64_t holder, uint64_t,
                               uint64_t value) {
  auto* accessor = static_cast<TemplateAccessor*>(data);
  if (accessor == nullptr || accessor->setter == nullptr) return 1;
  uint64_t args[1] = {value};
  v8::FunctionCallbackInfo<v8::Value> info(v8::g_current_isolate, nullptr,
                                           holder, args, 1);
  CallbackToken token;
  accessor->setter(info);
  return 0;
}

/* The descriptor a definer is handed: built from the ABI's present-or-absent
 * fields, into the `v8::PropertyDescriptor` the callback takes. */
v8::PropertyDescriptor DescriptorOf(const SlagPropertyDescriptor& source) {
  v8::PropertyDescriptor descriptor;
  if (source.has_value != 0) {
    descriptor = v8::PropertyDescriptor(
        v8::Local<v8::Value>::New(source.value));
    if (source.has_writable != 0) {
      descriptor = v8::PropertyDescriptor(
          v8::Local<v8::Value>::New(source.value), source.writable != 0);
    }
  } else if (source.has_get != 0 || source.has_set != 0) {
    descriptor = v8::PropertyDescriptor(
        v8::Local<v8::Value>::New(source.get),
        v8::Local<v8::Value>::New(source.set));
  }
  if (source.has_enumerable != 0) descriptor.set_enumerable(source.enumerable != 0);
  if (source.has_configurable != 0) {
    descriptor.set_configurable(source.configurable != 0);
  }
  return descriptor;
}

int32_t DescriptorTrampoline(void* state, uint64_t data, uint64_t holder,
                             uint64_t key, uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->descriptor == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Value> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(
      configuration->descriptor(v8::Local<v8::Name>::New(key), info));
}

int32_t DefinerTrampoline(void* state, uint64_t data, uint64_t holder,
                          uint64_t key, const SlagPropertyDescriptor* source,
                          uint64_t* out) {
  auto* configuration = Configuration(state);
  if (configuration == nullptr || configuration->definer == nullptr ||
      source == nullptr || out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Boolean> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(configuration->definer(
      v8::Local<v8::Name>::New(key), DescriptorOf(*source), info));
}

int32_t IndexedDescriptorTrampoline(void* state, uint64_t data,
                                    uint64_t holder, uint32_t index,
                                    uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->descriptor == nullptr ||
      out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Value> info(v8::g_current_isolate, holder, data,
                                           out, token.get());
  return static_cast<int32_t>(configuration->descriptor(index, info));
}

int32_t IndexedDefinerTrampoline(void* state, uint64_t data, uint64_t holder,
                                 uint32_t index,
                                 const SlagPropertyDescriptor* source,
                                 uint64_t* out) {
  auto* configuration = IndexedConfiguration(state);
  if (configuration == nullptr || configuration->definer == nullptr ||
      source == nullptr || out == nullptr) {
    return 0;
  }
  *out = slag_undefined();
  CallbackToken token;
  v8::PropertyCallbackInfo<v8::Boolean> info(v8::g_current_isolate, holder,
                                             data, out, token.get());
  return static_cast<int32_t>(
      configuration->definer(index, DescriptorOf(*source), info));
}

}  // namespace

namespace v8 {

HandleScope::HandleScope(Isolate*) : mark_(HandleMark()) {}

HandleScope::~HandleScope() { HandleRestore(mark_); }

HandleSlot* HandleScope::CreateHandle(uint64_t value, bool rooted) {
  return HandleCreate(value, rooted);
}

bool V8::InitializeICUDefaultLocation(const char*) { return true; }
void V8::InitializeExternalStartupData(const char*) {}
void V8::InitializePlatform(Platform*) {}
void V8::Initialize() {}
void V8::Dispose() {}
void V8::DisposePlatform() {}
void V8::SetFlagsFromCommandLine(int*, char**, bool) {}
const char* V8::GetVersion() { return slag_version(); }

namespace platform {

std::unique_ptr<Platform> NewDefaultPlatform() {
  return std::make_unique<Platform>();
}

bool PumpMessageLoop(Platform*, Isolate* isolate) {
  return isolate != nullptr && isolate->PumpMessages();
}

}  // namespace platform

bool Isolate::PumpMessages() {
  bool ran = false;
  for (Context* context : contexts_) {
    if (context == nullptr) continue;
    // -1 means a job threw and left the error pending; that still counts as a
    // task the pump ran.
    if (slag_drain_jobs(context->slag()) != 0) ran = true;
  }
  return ran;
}

void Isolate::PerformMicrotaskCheckpoint() {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context != nullptr) slag_drain_jobs(context->slag());
}

/* The allocator `NewDefaultAllocator` answers with: `calloc` for a zeroed
 * block, as V8's `Allocate` promises, and `malloc` for the uninitialized form. */
class DefaultAllocator : public ArrayBuffer::Allocator {
 public:
  void* Allocate(size_t length) override {
    return length == 0 ? nullptr : std::calloc(length, 1);
  }

  void* AllocateUninitialized(size_t length) override {
    return length == 0 ? nullptr : std::malloc(length);
  }

  void Free(void* data, size_t) override { std::free(data); }
};

/* A backing store's bytes go back to the allocator that lent them.
 *
 * The store's lifetime is the engine's, not the host's: this runs when the last
 * buffer and store over the bytes are gone. */
void ReleaseThroughAllocator(void* data, size_t length, void* deleter_data) {
  auto* allocator = static_cast<v8::ArrayBuffer::Allocator*>(deleter_data);
  if (allocator != nullptr) allocator->Free(data, length);
}

ArrayBuffer::Allocator* ArrayBuffer::Allocator::NewDefaultAllocator() {
  return new DefaultAllocator();
}

void* ArrayBuffer::Allocator::AllocateUninitialized(size_t length) {
  return Allocate(length);
}

void* ArrayBuffer::Allocator::Reallocate(void* data, size_t old_length,
                                         size_t new_length) {
  void* moved = Allocate(new_length);
  if (moved == nullptr || data == nullptr) return moved;
  std::memcpy(moved, data, old_length < new_length ? old_length : new_length);
  Free(data, old_length);
  return moved;
}

namespace {

/* Which `v8::Value` predicates a value answers. The brands need the agent, and
 * the entered context owns it; with none there is no table to ask, so only the
 * shape bits answer. */
uint32_t TypeMaskOf(const void* self) {
  return slag_value_type_mask(CurrentSlagContext(), Raw(self));
}

/* The engine's collection observer: one per context, with the isolate as its
 * data, so every collection the agent drives reaches the host's prologue and
 * epilogue lists. `phase` is 0 for the prologue and 1 for the epilogue. */
void CollectionObserver(void* data, int32_t phase, int32_t gc_type) {
  auto* isolate = static_cast<v8::Isolate*>(data);
  if (isolate == nullptr) return;
  isolate->NotifyGcCallbacks(
      gc_type == SLAG_GC_TYPE_SCAVENGE ? v8::kGCTypeScavenge
                                       : v8::kGCTypeMarkSweepCompact,
      phase == 0);
}

}  // namespace

bool Value::IsUndefined() const {
  return (TypeMaskOf(this) & SLAG_VALUE_UNDEFINED) != 0;
}

bool Value::IsNull() const { return (TypeMaskOf(this) & SLAG_VALUE_NULL) != 0; }

bool Value::IsNullOrUndefined() const {
  return (TypeMaskOf(this) & (SLAG_VALUE_NULL | SLAG_VALUE_UNDEFINED)) != 0;
}

bool Value::IsBoolean() const {
  return (TypeMaskOf(this) & SLAG_VALUE_BOOLEAN) != 0;
}

bool Value::IsNumber() const {
  return (TypeMaskOf(this) & SLAG_VALUE_NUMBER) != 0;
}

bool Value::IsInt32() const { return (TypeMaskOf(this) & SLAG_VALUE_INT32) != 0; }

bool Value::IsUint32() const {
  return (TypeMaskOf(this) & SLAG_VALUE_UINT32) != 0;
}

bool Value::IsBigInt() const { return (TypeMaskOf(this) & SLAG_VALUE_BIGINT) != 0; }

bool Value::IsSymbol() const { return (TypeMaskOf(this) & SLAG_VALUE_SYMBOL) != 0; }

bool Value::IsString() const { return (TypeMaskOf(this) & SLAG_VALUE_STRING) != 0; }

bool Value::IsFunction() const {
  return (TypeMaskOf(this) & SLAG_VALUE_FUNCTION) != 0;
}

bool Value::IsObject() const { return (TypeMaskOf(this) & SLAG_VALUE_OBJECT) != 0; }

bool Value::IsArray() const { return (TypeMaskOf(this) & SLAG_VALUE_ARRAY) != 0; }

bool Value::IsExternal() const {
  return (TypeMaskOf(this) & SLAG_VALUE_EXTERNAL) != 0;
}

bool Value::IsTypedArray() const {
  return (TypeMaskOf(this) & SLAG_VALUE_TYPED_ARRAY) != 0;
}

bool Value::IsProxy() const { return (TypeMaskOf(this) & SLAG_VALUE_PROXY) != 0; }

bool Value::IsArgumentsObject() const {
  return (TypeMaskOf(this) & SLAG_VALUE_ARGUMENTS) != 0;
}

bool Value::IsDate() const { return (TypeMaskOf(this) & SLAG_VALUE_DATE) != 0; }

bool Value::IsRegExp() const { return (TypeMaskOf(this) & SLAG_VALUE_REGEXP) != 0; }

bool Value::IsPromise() const { return (TypeMaskOf(this) & SLAG_VALUE_PROMISE) != 0; }

bool Value::IsMap() const { return (TypeMaskOf(this) & SLAG_VALUE_MAP) != 0; }

bool Value::IsSet() const { return (TypeMaskOf(this) & SLAG_VALUE_SET) != 0; }

bool Value::IsArrayBuffer() const {
  return (TypeMaskOf(this) & SLAG_VALUE_ARRAY_BUFFER) != 0;
}

bool Value::IsSharedArrayBuffer() const {
  return (TypeMaskOf(this) & SLAG_VALUE_SHARED_ARRAY_BUFFER) != 0;
}

bool Value::IsDataView() const {
  return (TypeMaskOf(this) & SLAG_VALUE_DATA_VIEW) != 0;
}

bool Value::IsNativeError() const {
  return (TypeMaskOf(this) & SLAG_VALUE_NATIVE_ERROR) != 0;
}

bool Value::BooleanValue(Isolate*) const {
  return slag_value_boolean(Raw(this)) != 0;
}

MaybeLocal<String> Value::TypeOf(Local<Context>) const {
  return MaybeLocal<String>(Local<String>::New(slag_value_typeof(Raw(this))));
}

MaybeLocal<String> Value::ToString(Local<Context> context) const {
  if (context.IsEmpty()) return MaybeLocal<String>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  if (slag_value_to_string(current->slag(), Raw(this), &out) != 0) {
    return MaybeLocal<String>();
  }
  return MaybeLocal<String>(Local<String>::New(out));
}

Maybe<double> Value::ToNumber(Local<Context> context) const {
  if (context.IsEmpty()) return Maybe<double>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  double out = 0;
  if (slag_value_to_number_context(current->slag(), Raw(this), &out) != 0) {
    return Maybe<double>();
  }
  return Maybe<double>(out);
}

MaybeLocal<Object> Value::ToObject(Local<Context> context) const {
  if (context.IsEmpty()) return MaybeLocal<Object>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  if (slag_value_to_object(current->slag(), Raw(this), &out) != 0) {
    return MaybeLocal<Object>();
  }
  return MaybeLocal<Object>(Local<Object>::New(out));
}

bool Value::StrictEquals(Local<Value> that) const {
  return slag_value_strict_equals(Raw(this), that.raw()) != 0;
}

bool Value::SameValue(Local<Value> that) const {
  return slag_value_same_value(Raw(this), that.raw()) != 0;
}

bool Object::IsCallable() const { return slag_value_is_callable(Raw(this)) != 0; }

bool Object::IsConstructor() const {
  return slag_value_is_constructor(CurrentSlagContext(), Raw(this)) != 0;
}

Local<Value> Object::GetPrototype() {
  SlagContext* slag = CurrentSlagContext();
  uint64_t out = 0;
  if (slag == nullptr || slag_object_prototype(slag, Raw(this), &out) != 0) {
    return Local<Value>();
  }
  return Local<Value>::New(out);
}

bool Object::SetPrototype(Local<Context> context, Local<Value> prototype) {
  if (context.IsEmpty()) return false;
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  return slag_object_set_prototype(current->slag(), Raw(this),
                                   prototype.IsEmpty() ? slag_null()
                                                       : prototype.raw()) != 0;
}

MaybeLocal<Value> Function::Call(Local<Context> context, Local<Value> recv,
                                 int argc, Local<Value> argv[]) {
  if (context.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  std::vector<uint64_t> args;
  args.reserve(argc < 0 ? 0 : static_cast<size_t>(argc));
  for (int i = 0; i < argc; ++i) args.push_back(argv[i].raw());
  uint64_t out = 0;
  if (slag_call(current->slag(), Raw(this),
                recv.IsEmpty() ? slag_undefined() : recv.raw(), args.data(),
                args.size(), &out) != 0) {
    return MaybeLocal<Value>();
  }
  return Local<Value>::New(out);
}

MaybeLocal<Object> Function::NewInstance(Local<Context> context, int argc,
                                         Local<Value> argv[]) {
  if (context.IsEmpty()) return MaybeLocal<Object>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  std::vector<uint64_t> args;
  args.reserve(argc < 0 ? 0 : static_cast<size_t>(argc));
  for (int i = 0; i < argc; ++i) args.push_back(argv[i].raw());
  uint64_t out = 0;
  if (slag_construct(current->slag(), Raw(this), args.data(), args.size(),
                     &out) != 0) {
    return MaybeLocal<Object>();
  }
  return Local<Object>::New(out);
}

Local<Function> Function::New(Local<Context> context, FunctionCallback callback,
                              Local<Value> data, int length,
                              ConstructorBehavior behavior,
                              SideEffectType side_effect_type) {
  v8::Local<v8::FunctionTemplate> tmpl = v8::FunctionTemplate::New(
      g_current_isolate, callback, data, v8::Local<v8::Signature>(), length,
      behavior, side_effect_type);
  v8::Local<v8::Function> function;
  if (tmpl.IsEmpty() || !tmpl->GetFunction(context).ToLocal(&function)) {
    return Local<Function>();
  }
  return function;
}

Local<Signature> Signature::New(Isolate*, Local<FunctionTemplate> function) {
  if (function.IsEmpty()) return Local<Signature>();
  auto* signature = new Signature(reinterpret_cast<FunctionTemplate*>(
      static_cast<uintptr_t>(function.raw())));
  return Local<Signature>::New(reinterpret_cast<uint64_t>(signature));
}

Local<FunctionTemplate> Signature::GetFunctionTemplate() const {
  if (function_ == nullptr) return Local<FunctionTemplate>();
  return Local<FunctionTemplate>::New(reinterpret_cast<uint64_t>(function_));
}

Local<AccessorSignature> AccessorSignature::New(Isolate*,
                                                Local<FunctionTemplate>) {
  return Local<AccessorSignature>::New(
      reinterpret_cast<uint64_t>(new AccessorSignature()));
}

Local<FunctionTemplate> FunctionTemplate::New(
    Isolate*, FunctionCallback callback, Local<Value> data,
    Local<Signature> signature, int length, ConstructorBehavior behavior,
    SideEffectType) {
  auto* tmpl = new FunctionTemplate(callback);
  tmpl->data_bits_ = data.IsEmpty() ? slag_undefined() : data.raw();
  slag_root_retain(tmpl->data_bits_);
  if (!signature.IsEmpty()) {
    tmpl->signature_ =
        reinterpret_cast<Signature*>(static_cast<uintptr_t>(signature.raw()));
  }
  tmpl->length_ = length;
  tmpl->allow_construction_ = behavior != ConstructorBehavior::kThrow;
  return Local<FunctionTemplate>::New(reinterpret_cast<uint64_t>(tmpl));
}

void FunctionTemplate::SetCallHandler(FunctionCallback callback,
                                      Local<Value> data) {
  callback_ = callback;
  if (data_bits_ != 0) slag_root_release(data_bits_);
  data_bits_ = data.IsEmpty() ? slag_undefined() : data.raw();
  slag_root_retain(data_bits_);
}

void FunctionTemplate::SetClassName(const char* name) {
  class_name_ = name == nullptr ? std::string() : std::string(name);
}

void FunctionTemplate::Inherit(Local<FunctionTemplate> parent) {
  parent_ = parent.IsEmpty()
                ? nullptr
                : reinterpret_cast<FunctionTemplate*>(
                      static_cast<uintptr_t>(parent.raw()));
}

Local<ObjectTemplate> FunctionTemplate::InstanceTemplate() {
  if (instance_template_ == nullptr) {
    Local<ObjectTemplate> created = ObjectTemplate::New(g_current_isolate);
    instance_template_ = reinterpret_cast<ObjectTemplate*>(
        static_cast<uintptr_t>(created.raw()));
  }
  return Local<ObjectTemplate>::New(
      reinterpret_cast<uint64_t>(instance_template_));
}

Local<ObjectTemplate> FunctionTemplate::PrototypeTemplate() {
  if (prototype_template_ == nullptr) {
    Local<ObjectTemplate> created = ObjectTemplate::New(g_current_isolate);
    prototype_template_ = reinterpret_cast<ObjectTemplate*>(
        static_cast<uintptr_t>(created.raw()));
  }
  return Local<ObjectTemplate>::New(
      reinterpret_cast<uint64_t>(prototype_template_));
}

bool FunctionTemplate::AllowsReceiver(uint64_t receiver) const {
  if (signature_ == nullptr || signature_->function_ == nullptr) return true;
  return InstanceOf(receiver, signature_->function_->prototype_bits_);
}

MaybeLocal<Function> FunctionTemplate::GetFunction(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Function>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  Local<Object> prototype;
  if (!BuildPrototypeObject(context).ToLocal(&prototype)) {
    return MaybeLocal<Function>();
  }
  const std::string name =
      class_name_.empty() ? "FunctionTemplate" : class_name_;
  uint64_t out = 0;
  Local<ObjectTemplate> instance = InstanceTemplate();
  if (slag_template_constructor(
          current->slag(), reinterpret_cast<const uint8_t*>(name.data()),
          name.size(), static_cast<uint32_t>(length_), prototype.raw(),
          static_cast<size_t>(instance->InternalFieldCount()), 1,
          allow_construction_ ? 1 : 0, &HostTrampoline, this, &out) != 0) {
    return MaybeLocal<Function>();
  }
  return MaybeLocal<Function>(Local<Function>::New(out));
}

MaybeLocal<Object> FunctionTemplate::BuildPrototypeObject(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Object>();
  // One prototype per template: every instance shares it, which is what makes a
  // signature's receiver check an identity walk.
  if (prototype_bits_ != 0) {
    return MaybeLocal<Object>(Local<Object>::New(prototype_bits_));
  }
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  Local<ObjectTemplate> instance = InstanceTemplate();
  Local<Object> prototype;
  if (!instance->NewInstance(context).ToLocal(&prototype)) {
    return MaybeLocal<Object>();
  }
  // Inherit chains the prototypes: the parent's built prototype is this one's
  // prototype, so an instance reaches the parent's members.
  if (parent_ != nullptr) {
    Local<Object> parent_prototype;
    if (parent_->BuildPrototypeObject(context).ToLocal(&parent_prototype)) {
      prototype->SetPrototype(context, parent_prototype);
    }
  }
  // The prototype template's members live on the function's prototype object.
  if (prototype_template_ != nullptr) {
    for (const auto& entry : prototype_template_->entries()) {
      slag_object_register_fn(
          current->slag(), prototype.raw(),
          reinterpret_cast<const uint8_t*>(entry.name.data()),
          entry.name.size(), &HostTrampoline, entry.function);
    }
    for (const auto& accessor : prototype_template_->accessors()) {
      const slag_accessor_getter getter =
          accessor.getter == nullptr ? nullptr : &AccessorGetterTrampoline;
      const slag_accessor_setter setter =
          accessor.setter == nullptr ? nullptr : &AccessorSetterTrampoline;
      slag_object_set_accessor(
          current->slag(), prototype.raw(), accessor.name_bits, getter, setter,
          const_cast<v8::ObjectTemplate::Accessor*>(&accessor),
          accessor.attributes);
    }
  }
  // Rooted for the template's life (templates are never freed), so the signature
  // check can name it and a later instance inherits from the same object.
  prototype_bits_ = prototype.raw();
  slag_root_retain(prototype_bits_);
  return prototype;
}

Isolate* Isolate::GetCurrent() { return g_current_isolate; }

Isolate* Isolate::TryGetCurrent() { return g_current_isolate; }

namespace {

thread_local int g_microtasks_scope_depth = 0;

}  // namespace

MicrotasksScope::MicrotasksScope(Isolate* isolate, Type type)
    : isolate_(isolate), type_(type) {
  ++g_microtasks_scope_depth;
}

MicrotasksScope::~MicrotasksScope() {
  if (g_microtasks_scope_depth > 0) --g_microtasks_scope_depth;
  if (g_microtasks_scope_depth == 0 && type_ == kRunMicrotasks &&
      isolate_ != nullptr) {
    isolate_->PerformMicrotaskCheckpoint();
  }
}

int MicrotasksScope::GetCurrentDepth(Isolate*) {
  return g_microtasks_scope_depth;
}

bool MicrotasksScope::IsRunningMicrotasks(Isolate*) { return false; }

BackingStore::~BackingStore() { slag_buffer_free(buffer_); }

void* BackingStore::Data() const { return slag_buffer_data(buffer_); }

size_t BackingStore::ByteLength() const {
  return slag_buffer_byte_length(buffer_);
}

bool BackingStore::IsShared() const {
  return slag_buffer_is_shared(buffer_) != 0;
}

Local<ArrayBuffer> ArrayBuffer::New(Isolate* isolate, size_t byte_length) {
  // A store from the host's allocator when the isolate has one, and the
  // engine's own block otherwise.
  std::unique_ptr<BackingStore> store = NewBackingStore(isolate, byte_length);
  if (!store) return Local<ArrayBuffer>();
  uint64_t out = 0;
  const int32_t created =
      slag_buffer_to_array_buffer(CurrentSlagContext(), store->buffer_, &out);
  return created == 0 ? Local<ArrayBuffer>::New(out) : Local<ArrayBuffer>();
}

Local<ArrayBuffer> ArrayBuffer::New(Isolate*,
                                    std::shared_ptr<BackingStore> backing_store) {
  if (!backing_store) return Local<ArrayBuffer>();
  uint64_t out = 0;
  const int32_t created = slag_buffer_to_array_buffer(
      CurrentSlagContext(), backing_store->buffer_, &out);
  return created == 0 ? Local<ArrayBuffer>::New(out) : Local<ArrayBuffer>();
}

std::unique_ptr<BackingStore> ArrayBuffer::NewBackingStore(Isolate* isolate,
                                                          size_t byte_length) {
  auto* allocator =
      isolate != nullptr ? isolate->array_buffer_allocator_ : nullptr;
  if (allocator == nullptr || byte_length == 0) {
    return std::unique_ptr<BackingStore>(
        new BackingStore(slag_buffer_new(byte_length)));
  }
  void* data = allocator->Allocate(byte_length);
  if (data == nullptr) return nullptr;
  // The block is the host's until the last buffer over it is gone, which is
  // when the deleter hands the bytes back. The isolate accounts for the bytes,
  // unlike an address range the host lends through its own deleter.
  return std::unique_ptr<BackingStore>(new BackingStore(slag_buffer_borrowed(
      static_cast<uint8_t*>(data), byte_length, &ReleaseThroughAllocator,
      allocator, 1)));
}

std::unique_ptr<BackingStore> ArrayBuffer::NewBackingStore(
    void* data, size_t byte_length, BackingStoreDeleterCallback deleter,
    void* deleter_data) {
  // A host's own address range: its deleter releases it, and the engine leaves
  // the bytes out of the isolate's external-memory account.
  return std::unique_ptr<BackingStore>(new BackingStore(slag_buffer_borrowed(
      static_cast<uint8_t*>(data), byte_length, deleter, deleter_data, 0)));
}

size_t ArrayBuffer::ByteLength() const {
  return slag_array_buffer_byte_length(CurrentSlagContext(), Raw(this));
}

void* ArrayBuffer::Data() const {
  return slag_array_buffer_data(CurrentSlagContext(), Raw(this));
}

std::shared_ptr<BackingStore> ArrayBuffer::GetBackingStore() const {
  SlagBuffer* store = slag_buffer_of(CurrentSlagContext(), Raw(this));
  if (store == nullptr) return std::shared_ptr<BackingStore>();
  return std::shared_ptr<BackingStore>(new BackingStore(store));
}

bool ArrayBuffer::Detach(Local<Value> key) {
  const bool has_key = !key.IsEmpty();
  return slag_array_buffer_detach(CurrentSlagContext(), Raw(this),
                                  has_key ? key.raw() : 0,
                                  has_key ? 1 : 0) != 0;
}

bool ArrayBuffer::IsDetachable() const {
  return slag_array_buffer_is_detachable(CurrentSlagContext(), Raw(this)) != 0;
}

bool ArrayBuffer::WasDetached() const {
  return slag_array_buffer_is_detached(CurrentSlagContext(), Raw(this)) != 0;
}

void ArrayBuffer::SetDetachKey(Local<Value> key) {
  slag_array_buffer_set_detach_key(CurrentSlagContext(), Raw(this),
                                   key.IsEmpty() ? slag_undefined() : key.raw());
}

Local<SharedArrayBuffer> SharedArrayBuffer::New(Isolate*, size_t byte_length) {
  SlagBuffer* store = slag_buffer_new(byte_length);
  uint64_t out = 0;
  const int32_t created =
      slag_shared_array_buffer_new(CurrentSlagContext(), store, &out);
  slag_buffer_free(store);
  return created == 0 ? Local<SharedArrayBuffer>::New(out)
                      : Local<SharedArrayBuffer>();
}

Local<SharedArrayBuffer> SharedArrayBuffer::New(
    Isolate*, std::shared_ptr<BackingStore> backing_store) {
  if (!backing_store) return Local<SharedArrayBuffer>();
  uint64_t out = 0;
  const int32_t created = slag_shared_array_buffer_new(
      CurrentSlagContext(), backing_store->buffer_, &out);
  return created == 0 ? Local<SharedArrayBuffer>::New(out)
                      : Local<SharedArrayBuffer>();
}

std::shared_ptr<BackingStore> SharedArrayBuffer::NewBackingStore(
    Isolate*, size_t byte_length) {
  return std::shared_ptr<BackingStore>(
      new BackingStore(slag_buffer_new(byte_length)));
}

size_t SharedArrayBuffer::ByteLength() const {
  return slag_array_buffer_byte_length(CurrentSlagContext(), Raw(this));
}

void* SharedArrayBuffer::Data() const {
  return slag_array_buffer_data(CurrentSlagContext(), Raw(this));
}

std::shared_ptr<BackingStore> SharedArrayBuffer::GetBackingStore() const {
  SlagBuffer* store = slag_buffer_of(CurrentSlagContext(), Raw(this));
  if (store == nullptr) return std::shared_ptr<BackingStore>();
  return std::shared_ptr<BackingStore>(new BackingStore(store));
}

Local<DataView> DataView::New(Local<ArrayBuffer> array_buffer,
                             size_t byte_offset, size_t byte_length) {
  if (array_buffer.IsEmpty()) return Local<DataView>();
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return Local<DataView>();
  uint64_t out = 0;
  if (slag_data_view_new(slag, array_buffer.raw(), byte_offset, byte_length,
                         &out) != 0) {
    return Local<DataView>();
  }
  return Local<DataView>::New(out);
}

Local<DataView> DataView::New(Local<SharedArrayBuffer> shared_array_buffer,
                             size_t byte_offset, size_t byte_length) {
  if (shared_array_buffer.IsEmpty()) return Local<DataView>();
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return Local<DataView>();
  uint64_t out = 0;
  if (slag_data_view_new(slag, shared_array_buffer.raw(), byte_offset,
                         byte_length, &out) != 0) {
    return Local<DataView>();
  }
  return Local<DataView>::New(out);
}

size_t ArrayBufferView::ByteLength() const {
  return slag_array_view_byte_length(CurrentSlagContext(), Raw(this));
}
size_t ArrayBufferView::ByteOffset() const {
  return slag_array_view_byte_offset(CurrentSlagContext(), Raw(this));
}

bool ArrayBufferView::HasBuffer() const {
  uint64_t buffer = 0;
  if (slag_typed_array_buffer(Raw(this), &buffer) != 0) return false;
  return !Local<Value>::New(buffer)->IsSharedArrayBuffer();
}

Local<ArrayBuffer> ArrayBufferView::Buffer() const {
  uint64_t buffer = 0;
  if (slag_typed_array_buffer(Raw(this), &buffer) != 0) {
    return Local<ArrayBuffer>();
  }
  if (Local<Value>::New(buffer)->IsSharedArrayBuffer()) {
    return Local<ArrayBuffer>();
  }
  return Local<ArrayBuffer>::New(buffer);
}

size_t TypedArray::Length() const { return slag_typed_array_length(Raw(this)); }

namespace {

/* One constructor body for the twelve typed arrays: the engine validates the
 * geometry, so a range that does not fit leaves the error pending and answers
 * an empty handle here. */
template <class T>
Local<T> NewTypedArray(int32_t kind, Local<ArrayBuffer> buffer, size_t byte_offset,
                       size_t length) {
  uint64_t out = 0;
  if (slag_typed_array_new(CurrentSlagContext(), kind, buffer.raw(), byte_offset,
                           length, &out) != 0) {
    return Local<T>();
  }
  return Local<T>::New(out);
}

}  // namespace

#define SLAG_TYPED_ARRAY_NEW(Class, Kind)                                  \
  Local<Class> Class::New(Local<ArrayBuffer> array_buffer, size_t byte_offset, \
                          size_t length) {                                 \
    return NewTypedArray<Class>(Kind, array_buffer, byte_offset, length);    \
  }

SLAG_TYPED_ARRAY_NEW(Int8Array, SLAG_TYPED_INT8)
SLAG_TYPED_ARRAY_NEW(Uint8Array, SLAG_TYPED_UINT8)
SLAG_TYPED_ARRAY_NEW(Uint8ClampedArray, SLAG_TYPED_UINT8_CLAMPED)
SLAG_TYPED_ARRAY_NEW(Int16Array, SLAG_TYPED_INT16)
SLAG_TYPED_ARRAY_NEW(Uint16Array, SLAG_TYPED_UINT16)
SLAG_TYPED_ARRAY_NEW(Int32Array, SLAG_TYPED_INT32)
SLAG_TYPED_ARRAY_NEW(Uint32Array, SLAG_TYPED_UINT32)
SLAG_TYPED_ARRAY_NEW(Float16Array, SLAG_TYPED_FLOAT16)
SLAG_TYPED_ARRAY_NEW(Float32Array, SLAG_TYPED_FLOAT32)
SLAG_TYPED_ARRAY_NEW(Float64Array, SLAG_TYPED_FLOAT64)
SLAG_TYPED_ARRAY_NEW(BigInt64Array, SLAG_TYPED_BIGINT64)
SLAG_TYPED_ARRAY_NEW(BigUint64Array, SLAG_TYPED_BIGUINT64)

#undef SLAG_TYPED_ARRAY_NEW

int64_t Isolate::AdjustAmountOfExternalAllocatedMemory(int64_t change) {
  int64_t total = slag_isolate_adjust_external_memory(this->slag_, change);
  for (Context* context : contexts_) {
    if (context == nullptr) continue;
    total += slag_context_external_memory(context->slag());
  }
  return total < 0 ? 0 : total;
}

ExternalMemoryAccounter::~ExternalMemoryAccounter() {
  if (isolate_ != nullptr) {
    isolate_->AdjustAmountOfExternalAllocatedMemory(
        -static_cast<int64_t>(size_));
  }
}

void ExternalMemoryAccounter::Update(Isolate* isolate, size_t size) {
  isolate_ = isolate;
  isolate->AdjustAmountOfExternalAllocatedMemory(
      static_cast<int64_t>(size) - static_cast<int64_t>(size_));
  size_ = size;
}

void ExternalMemoryAccounter::Increase(Isolate* isolate, size_t size) {
  Update(isolate, size_ + size);
}

void ExternalMemoryAccounter::Decrease(Isolate* isolate, size_t size) {
  Update(isolate, size_ > size ? size_ - size : 0);
}

int Object::InternalFieldCount() const {
  return static_cast<int>(slag_object_internal_field_count(Raw(this)));
}

bool Object::IsApiWrapper() const {
  return slag_object_is_api_wrapper(Raw(this)) != 0;
}

void Object::SetAlignedPointerInInternalField(int index, void* value,
                                              CppHeapPointerTag) {
  if (index < 0) return;
  slag_object_set_internal_field(Raw(this), static_cast<size_t>(index), value);
}

void* Object::GetAlignedPointerFromInternalField(int index) const {
  if (index < 0) return nullptr;
  return slag_object_internal_field(Raw(this), static_cast<size_t>(index));
}

void* Object::GetAlignedPointerFromInternalField(Isolate*, int index) const {
  return GetAlignedPointerFromInternalField(index);
}

void* Object::GetAlignedPointerFromInternalField(Local<Context>,
                                                 int index) const {
  return GetAlignedPointerFromInternalField(index);
}

Local<Object> Object::New(Local<Context> context) {
  if (context.IsEmpty()) return Local<Object>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  // Inside a handler this is the only allocation path there is: the ordinary
  // one borrows the context, which the operation that called the handler holds.
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_new(current->slag(), g_callback_token, &out)
          : slag_object_new(current->slag(), 0, 0, &out);
  if (status != 0) return Local<Object>();
  return Local<Object>::New(out);
}

Local<Object> Object::New(Isolate* isolate) {
  if (isolate == nullptr) return Local<Object>();
  return New(isolate->GetCurrentContext());
}

MaybeLocal<Value> Object::Get(Local<Context> context, Local<Value> key) {
  if (context.IsEmpty() || key.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  // Inside a handler the ordinary path cannot be re-entered, so a read goes
  // through the running callback's window instead.
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_get_key(current->slag(), g_callback_token,
                                         Raw(this), key.raw(), &out)
          : slag_object_get_key(current->slag(), Raw(this), key.raw(), &out);
  if (status != 0) {
    return MaybeLocal<Value>();
  }
  return Local<Value>::New(out);
}

Maybe<bool> Object::Set(Local<Context> context, Local<Value> key,
                        Local<Value> value) {
  if (context.IsEmpty() || key.IsEmpty() || value.IsEmpty()) {
    return Maybe<bool>();
  }
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_set_key(current->slag(), g_callback_token,
                                         Raw(this), key.raw(), value.raw())
          : slag_object_set_key(current->slag(), Raw(this), key.raw(),
                                value.raw());
  if (status != 0) {
    return Maybe<bool>();
  }
  return Maybe<bool>(true);
}

MaybeLocal<Value> Object::Get(Local<Context> context, uint32_t index) {
  return Get(context,
             Local<Value>::New(slag_number_new(static_cast<double>(index))));
}

Maybe<bool> Object::Set(Local<Context> context, uint32_t index,
                        Local<Value> value) {
  return Set(context,
             Local<Value>::New(slag_number_new(static_cast<double>(index))),
             value);
}

Maybe<bool> Object::Has(Local<Context> context, Local<Value> key) {
  if (context.IsEmpty() || key.IsEmpty()) return Maybe<bool>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  int32_t found = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_has_key(current->slag(), g_callback_token,
                                         Raw(this), key.raw(), &found)
          : slag_object_has_key(current->slag(), Raw(this), key.raw(), &found);
  if (status != 0) {
    return Maybe<bool>();
  }
  return Maybe<bool>(found != 0);
}

Maybe<bool> Object::Has(Local<Context> context, uint32_t index) {
  return Has(context,
             Local<Value>::New(slag_number_new(static_cast<double>(index))));
}

void Object::SetAccessor(Local<Context> context, Local<Name> name,
                         AccessorNameGetterCallback getter,
                         AccessorNameSetterCallback setter, Local<Value> data,
                         PropertyAttribute attribute) {
  if (context.IsEmpty() || name.IsEmpty() || getter == nullptr) return;
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  // The Rust side keeps this pointer, so the specification is owned by the engine
  // for the object's lifetime, the same way a template is. The name and data are
  // roots for the process, like a `Global` the host never resets.
  auto* accessor = new ObjectTemplate::Accessor();
  accessor->getter = getter;
  accessor->setter = setter;
  accessor->name_bits = name.raw();
  accessor->data_bits = data.IsEmpty() ? slag_undefined() : data.raw();
  accessor->attributes = static_cast<int32_t>(attribute);
  slag_root_retain(accessor->name_bits);
  slag_root_retain(accessor->data_bits);
  slag_object_set_accessor(
      current->slag(), Raw(this), accessor->name_bits, &AccessorGetterTrampoline,
      accessor->setter == nullptr ? nullptr : &AccessorSetterTrampoline,
      accessor, accessor->attributes);
}

void Object::SetAccessorProperty(Local<Name> name,
                                 Local<FunctionTemplate> getter,
                                 Local<FunctionTemplate> setter,
                                 PropertyAttribute attribute) {
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr || name.IsEmpty() || getter.IsEmpty()) return;
  // The specification is owned for the object's life, like `SetAccessor`'s.
  auto* accessor = new TemplateAccessor();
  accessor->getter = getter->callback();
  accessor->setter = setter.IsEmpty() ? nullptr : setter->callback();
  slag_object_set_accessor(
      slag, Raw(this), name.raw(), &TemplateAccessorGetter,
      accessor->setter == nullptr ? nullptr : &TemplateAccessorSetter, accessor,
      static_cast<int32_t>(attribute));
}

MaybeLocal<Value> Object::GetPrivate(Local<Context> context, Local<Private> key) {
  return Get(context, Local<Value>(key));
}

Maybe<bool> Object::SetPrivate(Local<Context> context, Local<Private> key,
                               Local<Value> value) {
  return Set(context, Local<Value>(key), value);
}

Maybe<bool> Object::HasPrivate(Local<Context> context, Local<Private> key) {
  return Has(context, Local<Value>(key));
}

bool Object::DeletePrivate(Local<Context> context, Local<Private> key) {
  return Delete(context, Local<Value>(key));
}

namespace {

Local<Value> MakeException(SlagExceptionKind kind, Local<String> message) {
  v8::String::Utf8Value text(v8::g_current_isolate, message);
  const char* data = *text == nullptr ? "" : *text;
  uint64_t out = 0;
  if (slag_exception_new(CurrentSlagContext(), static_cast<int32_t>(kind),
                         reinterpret_cast<const uint8_t*>(data),
                         data == nullptr ? 0 : std::strlen(data), &out) != 0) {
    return Local<Value>();
  }
  return Local<Value>::New(out);
}

}  // namespace

Local<Value> Exception::Error(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_ERROR, message);
}

Local<Value> Exception::EvalError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_EVAL_ERROR, message);
}

Local<Value> Exception::RangeError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_RANGE_ERROR, message);
}

Local<Value> Exception::ReferenceError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_REFERENCE_ERROR, message);
}

Local<Value> Exception::SyntaxError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_SYNTAX_ERROR, message);
}

Local<Value> Exception::TypeError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_TYPE_ERROR, message);
}

Local<Value> Exception::UriError(Local<String> message) {
  return MakeException(SLAG_EXCEPTION_URI_ERROR, message);
}

Local<v8::Message> Exception::CreateMessage(Isolate*, Local<Value>) {
  // A position exists only for a compile error (see `TryCatch::Message`), and a
  // value here has none.
  return Local<v8::Message>();
}

Local<Private> Private::ForApi(Isolate* isolate, Local<String> name) {
  if (isolate == nullptr) return Local<Private>();
  v8::String::Utf8Value text(isolate, name);
  std::string key = *text == nullptr ? std::string() : std::string(*text);
  auto found = isolate->private_names_.find(key);
  if (found != isolate->private_names_.end()) {
    return Local<Private>::New(found->second);
  }
  uint64_t value = 0;
  if (slag_symbol_new(reinterpret_cast<const uint8_t*>(key.data()), key.size(),
                      &value) != 0) {
    return Local<Private>();
  }
  // One name, one private: the registry keeps the symbol alive across handle
  // scopes, which is what makes a later request for the same name the same key.
  slag_root_retain(value);
  isolate->private_names_.emplace(std::move(key), value);
  return Local<Private>::New(value);
}

Local<Private> Private::New(Isolate* isolate, Local<String> name) {
  if (isolate == nullptr) return Local<Private>();
  v8::String::Utf8Value text(isolate, name);
  const char* data = *text == nullptr ? "" : *text;
  uint64_t value = 0;
  if (slag_symbol_new(reinterpret_cast<const uint8_t*>(data),
                      data == nullptr ? 0 : std::strlen(data), &value) != 0) {
    return Local<Private>();
  }
  return Local<Private>::New(value);
}

Local<Value> Private::Name() const {
  uint64_t description = 0;
  if (slag_symbol_description(Raw(this), &description) != 0) {
    return Local<Value>();
  }
  return Local<Value>::New(description);
}

MaybeLocal<Context> Object::GetCreationContext() const {
  if (g_current_context == 0) return MaybeLocal<Context>();
  return MaybeLocal<Context>(Local<Context>::New(g_current_context));
}

Local<Context> Object::GetCreationContextChecked() const {
  if (g_current_context == 0) return Local<Context>();
  return Local<Context>::New(g_current_context);
}

void Context::SetAlignedPointerInEmbedderData(int index, void* value) {
  if (index < 0) return;
  const auto slot = static_cast<size_t>(index);
  if (pointer_slots_.size() <= slot) pointer_slots_.resize(slot + 1, nullptr);
  pointer_slots_[slot] = value;
}

void* Context::GetAlignedPointerFromEmbedderData(int index) {
  if (index < 0) return nullptr;
  const auto slot = static_cast<size_t>(index);
  return slot < pointer_slots_.size() ? pointer_slots_[slot] : nullptr;
}

void Context::SetEmbedderData(int index, Local<Value> value) {
  if (index < 0) return;
  const auto slot = static_cast<size_t>(index);
  if (value_slots_.size() <= slot) value_slots_.resize(slot + 1, 0);
  const uint64_t bits = value.IsEmpty() ? slag_undefined() : value.raw();
  slag_root_retain(bits);
  if (value_slots_[slot] != 0) slag_root_release(value_slots_[slot]);
  value_slots_[slot] = bits;
}

Local<Value> Context::GetEmbedderData(int index) {
  if (index < 0) return Local<Value>();
  const auto slot = static_cast<size_t>(index);
  const uint64_t bits = slot < value_slots_.size() && value_slots_[slot] != 0
                            ? value_slots_[slot]
                            : slag_undefined();
  return Local<Value>::New(bits);
}

uint32_t Context::GetNumberOfEmbedderDataFields() {
  const size_t fields = pointer_slots_.size() > value_slots_.size()
                            ? pointer_slots_.size()
                            : value_slots_.size();
  return static_cast<uint32_t>(fields);
}

void Context::ClearAllSlots() {
  for (uint64_t bits : value_slots_) {
    if (bits != 0) slag_root_release(bits);
  }
  value_slots_.clear();
  pointer_slots_.clear();
}

Local<Value> Context::GetSecurityToken() {
  if (security_token_ != 0) return Local<Value>::New(security_token_);
  Isolate* isolate = g_current_isolate;
  if (isolate == nullptr) return Local<Value>();
  uint64_t& token = isolate->default_security_token_;
  if (token == 0) {
    token = slag_security_token_new();
    slag_root_retain(token);
  }
  return Local<Value>::New(token);
}

void Context::SetSecurityToken(Local<Value> token) {
  const uint64_t bits = token.IsEmpty() ? 0 : token.raw();
  if (bits != 0) slag_root_retain(bits);
  if (security_token_ != 0) slag_root_release(security_token_);
  security_token_ = bits;
}

void Context::UseDefaultSecurityToken() {
  if (security_token_ != 0) slag_root_release(security_token_);
  security_token_ = 0;
}

void Isolate::ThrowException(Local<Value> exception) {
  if (exception.IsEmpty()) return;
  slag_throw_exception(CurrentSlagContext(), exception.raw());
}

SnapshotCreator::SnapshotCreator(const intptr_t*) {
  Isolate::CreateParams params;
  isolate_ = Isolate::New(params);
}

SnapshotCreator::~SnapshotCreator() {
  if (isolate_ != nullptr) {
    isolate_->Dispose();
    isolate_ = nullptr;
  }
}

void SnapshotCreator::SetDefaultContext(Local<Context> context) {
  if (!context.IsEmpty()) default_context_ = context.raw();
}

StartupData SnapshotCreator::CreateBlob() {
  StartupData blob;
  if (default_context_ == 0) return blob;
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(default_context_));
  SlagContext* slag = context->slag();
  const int64_t length = slag_context_write_snapshot(slag, nullptr, 0);
  if (length <= 0) return blob;
  char* data = new char[static_cast<size_t>(length)];
  const int64_t written = slag_context_write_snapshot(
      slag, reinterpret_cast<uint8_t*>(data), static_cast<size_t>(length));
  if (written != length) {
    delete[] data;
    return blob;
  }
  blob.data = data;
  blob.raw_size = static_cast<int>(length);
  return blob;
}

void Isolate::GetHeapStatistics(HeapStatistics* statistics) {
  if (statistics == nullptr) return;
  const int64_t account = slag_isolate_adjust_external_memory(slag_, 0);
  SlagHeapStatistics stats{};
  bool filled = false;
  int64_t extra_buffers = 0;
  for (Context* context : contexts_) {
    if (context == nullptr) continue;
    if (!filled) {
      if (slag_heap_statistics(context->slag(), account, &stats) != 0) continue;
      filled = true;
    } else {
      extra_buffers += slag_context_external_memory(context->slag());
    }
  }
  *statistics = HeapStatistics{};
  if (!filled) return;
  statistics->total_heap_size = stats.total_heap_size;
  statistics->total_heap_size_executable = stats.total_heap_size_executable;
  statistics->total_physical_size = stats.total_physical_size;
  statistics->total_available_size = stats.total_available_size;
  statistics->used_heap_size = stats.used_heap_size;
  statistics->heap_size_limit = stats.heap_size_limit;
  statistics->malloced_memory =
      stats.malloced_memory + static_cast<size_t>(extra_buffers);
  statistics->external_memory =
      stats.external_memory + static_cast<size_t>(extra_buffers);
  statistics->peak_malloced_memory = stats.peak_malloced_memory;
  statistics->does_zap_garbage = stats.does_zap_garbage != 0;
  statistics->number_of_native_contexts = stats.number_of_native_contexts;
  statistics->number_of_detached_contexts = stats.number_of_detached_contexts;
  statistics->total_global_handles_size = stats.total_global_handles_size;
  statistics->used_global_handles_size = stats.used_global_handles_size;
  statistics->embedded_blob_size = stats.embedded_blob_size;
}

void Isolate::NotifyGcCallbacks(GCType type, bool prologue) {
  auto& callbacks = prologue ? prologue_callbacks_ : epilogue_callbacks_;
  // A callback may add or remove one, so run a snapshot of the list.
  const std::vector<std::pair<GCCallback, void*>> snapshot = callbacks;
  for (const auto& entry : snapshot) {
    entry.first(this, type, kNoGCCallbackFlags, entry.second);
  }
}

void Isolate::RequestGarbageCollectionForTesting(GarbageCollectionType) {
  for (Context* context : contexts_) {
    if (context == nullptr) continue;
    slag_collect(context->slag());
  }
}

void Isolate::LowMemoryNotification() {
  RequestGarbageCollectionForTesting(kFullGarbageCollection);
}

void Isolate::AddGCEpilogueCallback(GCCallback callback, void* data) {
  if (callback == nullptr) return;
  epilogue_callbacks_.emplace_back(callback, data);
}

void Isolate::RemoveGCEpilogueCallback(GCCallback callback, void* data) {
  for (auto it = epilogue_callbacks_.begin(); it != epilogue_callbacks_.end();
       ++it) {
    if (it->first == callback && it->second == data) {
      epilogue_callbacks_.erase(it);
      return;
    }
  }
}

void Isolate::AddGCPrologueCallback(GCCallback callback, void* data) {
  if (callback == nullptr) return;
  prologue_callbacks_.emplace_back(callback, data);
}

void Isolate::RemoveGCPrologueCallback(GCCallback callback, void* data) {
  for (auto it = prologue_callbacks_.begin(); it != prologue_callbacks_.end();
       ++it) {
    if (it->first == callback && it->second == data) {
      prologue_callbacks_.erase(it);
      return;
    }
  }
}

namespace {

/* `v8::Array`'s allocation, shared by the length and element forms. A null
 * `values` makes `length` holes. */
Local<Array> NewArray(Local<Context> context, const uint64_t* values,
                      size_t length) {
  if (context.IsEmpty()) return Local<Array>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_array_new(current->slag(), g_callback_token, values,
                                    length, &out)
          : slag_array_new(current->slag(), values, length, &out);
  if (status != 0) return Local<Array>();
  return Local<Array>::New(out);
}

}  // namespace

Local<Array> Array::New(Local<Context> context, int length) {
  return NewArray(context, nullptr, length < 0 ? 0 : static_cast<size_t>(length));
}

Local<Array> Array::New(Local<Context> context, Local<Value>* elements,
                        size_t length) {
  if (elements == nullptr) return NewArray(context, nullptr, length);
  std::vector<uint64_t> values(length);
  for (size_t i = 0; i < length; ++i) values[i] = elements[i].raw();
  return NewArray(context, values.data(), length);
}

Local<Array> Array::New(Isolate* isolate, int length) {
  if (isolate == nullptr) return Local<Array>();
  return New(isolate->GetCurrentContext(), length);
}

Local<Array> Array::New(Isolate* isolate, Local<Value>* elements,
                        size_t length) {
  if (isolate == nullptr) return Local<Array>();
  return New(isolate->GetCurrentContext(), elements, length);
}

namespace {

/* The promise operations that V8 gives no context to (`State`, `Result`,
 * `MarkAsHandled`) read the isolate's entered context, as there. Every one of
 * them takes the running callback's window when a host callback holds the
 * engine, and the ordinary entry point otherwise. */
Promise::PromiseState StateOf(uint64_t promise) {
  const int state =
      g_callback_token != nullptr
          ? slag_callback_promise_state(CurrentSlagContext(), g_callback_token,
                                        promise)
          : slag_promise_state(CurrentSlagContext(), promise);
  if (state == 1) return Promise::kFulfilled;
  if (state == 2) return Promise::kRejected;
  return Promise::kPending;
}

Local<Value> ResultOf(uint64_t promise) {
  uint64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_result(CurrentSlagContext(), g_callback_token,
                                         promise, &out)
          : slag_promise_result(CurrentSlagContext(), promise, &out);
  if (status != 0) return Local<Value>();
  return Local<Value>::New(out);
}

Maybe<bool> MarkHandledOf(uint64_t promise) {
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_mark_handled(CurrentSlagContext(),
                                               g_callback_token, promise)
          : slag_promise_mark_handled(CurrentSlagContext(), promise);
  if (status != 0) return Maybe<bool>();
  return Maybe<bool>(true);
}

MaybeLocal<Promise> ThenOf(uint64_t promise, uint64_t on_fulfilled,
                           uint64_t on_rejected) {
  uint64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_then(CurrentSlagContext(), g_callback_token,
                                       promise, on_fulfilled, on_rejected, &out)
          : slag_promise_then(CurrentSlagContext(), promise, on_fulfilled,
                              on_rejected, &out);
  if (status != 0) return MaybeLocal<Promise>();
  return MaybeLocal<Promise>(Local<Promise>::New(out));
}

Maybe<bool> SettleResolver(uint64_t resolver, uint64_t value, int reject) {
  int32_t settled = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_resolver_settle(
                CurrentSlagContext(), g_callback_token, resolver, value, reject,
                &settled)
          : slag_promise_resolver_settle(CurrentSlagContext(), resolver, value,
                                         reject, &settled);
  if (status != 0) return Maybe<bool>();
  return Maybe<bool>(settled != 0);
}

}  // namespace

Promise::PromiseState Promise::State() { return StateOf(Raw(this)); }

MaybeLocal<Promise> Promise::Resolve(Local<Context> context,
                                    Local<Value> value) {
  if (context.IsEmpty() || value.IsEmpty()) return MaybeLocal<Promise>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_resolved(current->slag(), g_callback_token,
                                           value.raw(), &out)
          : slag_promise_resolved(current->slag(), value.raw(), &out);
  if (status != 0) return MaybeLocal<Promise>();
  return MaybeLocal<Promise>(Local<Promise>::New(out));
}

MaybeLocal<Promise> Promise::Reject(Local<Context> context, Local<Value> value) {
  if (context.IsEmpty() || value.IsEmpty()) return MaybeLocal<Promise>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_rejected(current->slag(), g_callback_token,
                                           value.raw(), &out)
          : slag_promise_rejected(current->slag(), value.raw(), &out);
  if (status != 0) return MaybeLocal<Promise>();
  return MaybeLocal<Promise>(Local<Promise>::New(out));
}

Local<Value> Promise::Result() { return ResultOf(Raw(this)); }

Maybe<bool> Promise::MarkAsHandled() { return MarkHandledOf(Raw(this)); }

MaybeLocal<Promise> Promise::Then(Local<Context> context,
                                  Local<Function> handler) {
  if (context.IsEmpty()) return MaybeLocal<Promise>();
  const uint64_t missing = slag_undefined();
  return ThenOf(Raw(this), handler.IsEmpty() ? missing : handler.raw(), missing);
}

MaybeLocal<Promise> Promise::Then2(Local<Context> context,
                                   Local<Function> on_fulfilled,
                                   Local<Function> on_rejected) {
  if (context.IsEmpty()) return MaybeLocal<Promise>();
  const uint64_t missing = slag_undefined();
  return ThenOf(Raw(this),
                on_fulfilled.IsEmpty() ? missing : on_fulfilled.raw(),
                on_rejected.IsEmpty() ? missing : on_rejected.raw());
}

MaybeLocal<Promise> Promise::Catch(Local<Context> context,
                                   Local<Function> handler) {
  if (context.IsEmpty()) return MaybeLocal<Promise>();
  const uint64_t missing = slag_undefined();
  return ThenOf(Raw(this), missing,
                handler.IsEmpty() ? missing : handler.raw());
}

MaybeLocal<Promise::Resolver> Promise::Resolver::New(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Resolver>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t resolver = 0;
  uint64_t promise = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_promise_new_resolver(current->slag(),
                                               g_callback_token, &resolver,
                                               &promise)
          : slag_promise_new_resolver(current->slag(), &resolver, &promise);
  if (status != 0) return MaybeLocal<Resolver>();
  return Local<Resolver>::New(resolver);
}

Local<Promise> Promise::Resolver::GetPromise() {
  uint64_t out = 0;
  if (slag_promise_resolver_promise(CurrentSlagContext(), Raw(this), &out) !=
      0) {
    return Local<Promise>();
  }
  return Local<Promise>::New(out);
}

Maybe<bool> Promise::Resolver::Resolve(Local<Context> context,
                                       Local<Value> value) {
  if (context.IsEmpty() || value.IsEmpty()) return Maybe<bool>();
  return SettleResolver(Raw(this), value.raw(), 0);
}

Maybe<bool> Promise::Resolver::Reject(Local<Context> context,
                                      Local<Value> value) {
  if (context.IsEmpty() || value.IsEmpty()) return Maybe<bool>();
  return SettleResolver(Raw(this), value.raw(), 1);
}

namespace {

/* The context a container operation needs, from the handle V8 passes. */
Context* ContainerContext(Local<Context> context) {
  if (context.IsEmpty()) return nullptr;
  return reinterpret_cast<Context*>(static_cast<uintptr_t>(context.raw()));
}

MaybeLocal<Value> ContainerValue(SlagContext* slag, int status, uint64_t value) {
  if (status != 0) return MaybeLocal<Value>();
  (void)slag;
  return MaybeLocal<Value>(Local<Value>::New(value));
}

Maybe<bool> ContainerFlag(int status, int32_t flag) {
  if (status != 0) return Maybe<bool>();
  return Maybe<bool>(flag != 0);
}

}  // namespace

Local<Map> Map::New(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return Local<Map>();
  uint64_t out = 0;
  if (slag_map_new(current->slag(), &out) != 0) return Local<Map>();
  return Local<Map>::New(out);
}

Local<Map> Map::New(Isolate* isolate) {
  if (isolate == nullptr) return Local<Map>();
  return New(isolate->GetCurrentContext());
}

MaybeLocal<Value> Map::Get(Local<Context> context, Local<Value> key) {
  Context* current = ContainerContext(context);
  if (current == nullptr || key.IsEmpty()) return MaybeLocal<Value>();
  uint64_t out = 0;
  const int status =
      slag_map_get(current->slag(), Raw(this), key.raw(), &out);
  return ContainerValue(current->slag(), status, out);
}

MaybeLocal<Map> Map::Set(Local<Context> context, Local<Value> key,
                         Local<Value> value) {
  Context* current = ContainerContext(context);
  if (current == nullptr || key.IsEmpty() || value.IsEmpty()) {
    return MaybeLocal<Map>();
  }
  if (slag_map_set(current->slag(), Raw(this), key.raw(), value.raw()) != 0) {
    return MaybeLocal<Map>();
  }
  return MaybeLocal<Map>(Local<Map>::New(Raw(this)));
}

Maybe<bool> Map::Has(Local<Context> context, Local<Value> key) {
  Context* current = ContainerContext(context);
  if (current == nullptr || key.IsEmpty()) return Maybe<bool>();
  int32_t flag = 0;
  const int status =
      slag_map_has(current->slag(), Raw(this), key.raw(), &flag);
  return ContainerFlag(status, flag);
}

Maybe<bool> Map::Delete(Local<Context> context, Local<Value> key) {
  Context* current = ContainerContext(context);
  if (current == nullptr || key.IsEmpty()) return Maybe<bool>();
  int32_t flag = 0;
  const int status =
      slag_map_delete(current->slag(), Raw(this), key.raw(), &flag);
  return ContainerFlag(status, flag);
}

MaybeLocal<Array> Map::AsArray(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return MaybeLocal<Array>();
  uint64_t out = 0;
  if (slag_map_as_array(current->slag(), Raw(this), &out) != 0) {
    return MaybeLocal<Array>();
  }
  return MaybeLocal<Array>(Local<Array>::New(out));
}

int Map::Size(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return -1;
  return slag_map_size(current->slag(), Raw(this));
}

void Map::Clear() {
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return;
  slag_map_clear(slag, Raw(this));
}

Local<Set> Set::New(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return Local<Set>();
  uint64_t out = 0;
  if (slag_set_new(current->slag(), &out) != 0) return Local<Set>();
  return Local<Set>::New(out);
}

Local<Set> Set::New(Isolate* isolate) {
  if (isolate == nullptr) return Local<Set>();
  return New(isolate->GetCurrentContext());
}

MaybeLocal<Set> Set::Add(Local<Context> context, Local<Value> value) {
  Context* current = ContainerContext(context);
  if (current == nullptr || value.IsEmpty()) return MaybeLocal<Set>();
  if (slag_set_add(current->slag(), Raw(this), value.raw()) != 0) {
    return MaybeLocal<Set>();
  }
  return MaybeLocal<Set>(Local<Set>::New(Raw(this)));
}

Maybe<bool> Set::Has(Local<Context> context, Local<Value> value) {
  Context* current = ContainerContext(context);
  if (current == nullptr || value.IsEmpty()) return Maybe<bool>();
  int32_t flag = 0;
  const int status =
      slag_set_has(current->slag(), Raw(this), value.raw(), &flag);
  return ContainerFlag(status, flag);
}

Maybe<bool> Set::Delete(Local<Context> context, Local<Value> value) {
  Context* current = ContainerContext(context);
  if (current == nullptr || value.IsEmpty()) return Maybe<bool>();
  int32_t flag = 0;
  const int status =
      slag_set_delete(current->slag(), Raw(this), value.raw(), &flag);
  return ContainerFlag(status, flag);
}

MaybeLocal<Array> Set::AsArray(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return MaybeLocal<Array>();
  uint64_t out = 0;
  if (slag_set_as_array(current->slag(), Raw(this), &out) != 0) {
    return MaybeLocal<Array>();
  }
  return MaybeLocal<Array>(Local<Array>::New(out));
}

int Set::Size(Local<Context> context) {
  Context* current = ContainerContext(context);
  if (current == nullptr) return -1;
  return slag_set_size(current->slag(), Raw(this));
}

void Set::Clear() {
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return;
  slag_set_clear(slag, Raw(this));
}

bool Object::Delete(Local<Context> context, Local<Value> key) {
  if (context.IsEmpty() || key.IsEmpty()) return false;
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  int32_t deleted = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_delete_key(current->slag(), g_callback_token,
                                            Raw(this), key.raw(), &deleted)
          : slag_object_delete_key(current->slag(), Raw(this), key.raw(),
                                   &deleted);
  return status == 0 && deleted != 0;
}

bool Object::Delete(Local<Context> context, uint32_t index) {
  return Delete(context,
                Local<Value>::New(slag_number_new(static_cast<double>(index))));
}

int Object::GetIdentityHash() const {
  return slag_object_identity_hash(Raw(this));
}

MaybeLocal<Value> Object::GetRealNamedProperty(Local<Context> context,
                                               Local<Name> key) {
  if (context.IsEmpty() || key.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  int32_t found = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_object_get_own_key(current->slag(), g_callback_token,
                                             Raw(this), key.raw(), &out, &found)
          : slag_object_get_own_key(current->slag(), Raw(this), key.raw(), &out,
                                    &found);
  if (status != 0) {
    return MaybeLocal<Value>();
  }
  if (found == 0) return MaybeLocal<Value>();
  return Local<Value>::New(out);
}

Maybe<bool> Object::HasRealNamedProperty(Local<Context> context,
                                         Local<Name> key) {
  if (context.IsEmpty() || key.IsEmpty()) return Maybe<bool>();
  const int kind = OwnPropertyKind(context, this, key.raw());
  if (kind < 0) return Maybe<bool>();
  return Maybe<bool>(kind == 1 || kind == 3);
}

Maybe<bool> Object::HasRealIndexedProperty(Local<Context> context,
                                           uint32_t index) {
  if (context.IsEmpty()) return Maybe<bool>();
  const uint64_t key = slag_number_new(static_cast<double>(index));
  const int kind = OwnPropertyKind(context, this, key);
  if (kind < 0) return Maybe<bool>();
  return Maybe<bool>(kind == 2 || kind == 4);
}

Maybe<bool> Object::HasRealNamedCallbackProperty(Local<Context> context,
                                                 Local<Name> key) {
  if (context.IsEmpty() || key.IsEmpty()) return Maybe<bool>();
  const int kind = OwnPropertyKind(context, this, key.raw());
  if (kind < 0) return Maybe<bool>();
  return Maybe<bool>(kind == 3);
}

Local<Number> Number::New(Isolate*, double value) {
  return Local<Number>::New(slag_number_new(value));
}

Local<Boolean> Boolean::New(Isolate*, bool value) {
  return Local<Boolean>::New(slag_boolean_new(value ? 1 : 0));
}

Local<Integer> Integer::New(Isolate*, int32_t value) {
  return Local<Integer>::New(slag_number_new(static_cast<double>(value)));
}

Maybe<uint32_t> Value::Uint32Value(Local<Context> context) const {
  if (context.IsEmpty()) return Maybe<uint32_t>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint32_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_value_to_uint32(current->slag(), g_callback_token,
                                          Raw(this), &out)
          : slag_value_to_uint32(current->slag(), Raw(this), &out);
  if (status != 0) return Maybe<uint32_t>();
  return Maybe<uint32_t>(out);
}

Maybe<int32_t> Value::Int32Value(Local<Context> context) const {
  if (context.IsEmpty()) return Maybe<int32_t>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  int32_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_value_to_int32(current->slag(), g_callback_token,
                                         Raw(this), &out)
          : slag_value_to_int32(current->slag(), Raw(this), &out);
  if (status != 0) return Maybe<int32_t>();
  return Maybe<int32_t>(out);
}

Maybe<int64_t> Value::IntegerValue(Local<Context> context) const {
  if (context.IsEmpty()) return Maybe<int64_t>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  int64_t out = 0;
  const int status =
      g_callback_token != nullptr
          ? slag_callback_value_to_integer(current->slag(), g_callback_token,
                                           Raw(this), &out)
          : slag_value_to_integer(current->slag(), Raw(this), &out);
  if (status != 0) return Maybe<int64_t>();
  return Maybe<int64_t>(out);
}

Local<String> String::NewFromUtf8Literal(Isolate*, const char* literal) {
  return Local<String>::New(slag_string_new(
      reinterpret_cast<const uint8_t*>(literal), std::strlen(literal)));
}

MaybeLocal<String> String::NewFromUtf8(Isolate*, const char* str,
                                       NewStringType, int length) {
  if (str == nullptr) return MaybeLocal<String>();
  size_t len = length < 0 ? std::strlen(str) : static_cast<size_t>(length);
  return Local<String>::New(
      slag_string_new(reinterpret_cast<const uint8_t*>(str), len));
}

MaybeLocal<String> String::NewFromOneByte(Isolate*, const uint8_t* data,
                                          NewStringType, int length) {
  if (data == nullptr) return MaybeLocal<String>();
  size_t len = length < 0 ? std::strlen(reinterpret_cast<const char*>(data))
                          : static_cast<size_t>(length);
  return Local<String>::New(slag_string_new_one_byte(data, len));
}

MaybeLocal<String> String::NewFromTwoByte(Isolate*, const uint16_t* data,
                                          NewStringType, int length) {
  if (data == nullptr) return MaybeLocal<String>();
  size_t len = 0;
  if (length < 0) {
    while (data[len] != 0) ++len;
  } else {
    len = static_cast<size_t>(length);
  }
  return Local<String>::New(slag_string_new_two_byte(data, len));
}

int String::Length() const {
  return static_cast<int>(slag_string_length(Raw(this)));
}

int String::Utf8Length(Isolate*) const {
  return static_cast<int>(slag_string_utf8_length(Raw(this)));
}

bool String::ContainsOnlyOneByte() const {
  return slag_string_contains_only_one_byte(Raw(this)) != 0;
}

namespace {

/* V8 reads a negative `start` as an offset from the end and clamps a start past
 * the end to it. */
int ResolveWriteStart(uint64_t value, int start) {
  const int len = static_cast<int>(slag_string_length(value));
  if (len < 0) return -1;
  if (start < 0) start = len + start;
  if (start < 0) return 0;
  return start > len ? len : start;
}

}  // namespace

int String::Write(uint16_t* buffer, int start, int length, int options) const {
  const uint64_t value = Raw(this);
  const int resolved = ResolveWriteStart(value, start);
  if (resolved < 0) return 0;
  const int64_t written =
      slag_string_code_units(value, resolved, length, buffer);
  if (written < 0) return 0;
  if (buffer != nullptr && (options & NO_NULL_TERMINATION) == 0) {
    buffer[written] = 0;
  }
  return static_cast<int>(written);
}

int String::Write(Isolate* isolate, uint16_t* buffer, int start, int length,
                  int options) const {
  return Write(buffer, start, length, options);
}

int String::WriteOneByte(uint8_t* buffer, int start, int length,
                         int options) const {
  const uint64_t value = Raw(this);
  const int resolved = ResolveWriteStart(value, start);
  if (resolved < 0) return 0;
  const int64_t written =
      slag_string_write_one_byte(value, resolved, length, buffer);
  if (written < 0) return 0;
  if (buffer != nullptr && (options & NO_NULL_TERMINATION) == 0) {
    buffer[written] = 0;
  }
  return static_cast<int>(written);
}

int String::WriteOneByte(Isolate* isolate, uint8_t* buffer, int start,
                         int length, int options) const {
  return WriteOneByte(buffer, start, length, options);
}

int String::WriteUtf8(Isolate*, char* buffer, int length, int* nchars_ref,
                      int options) const {
  int64_t covered = 0;
  const int64_t written = slag_string_write_utf8(
      Raw(this), 0, length, (options & REPLACE_INVALID_UTF8) != 0,
      (options & NO_NULL_TERMINATION) == 0, reinterpret_cast<uint8_t*>(buffer),
      &covered);
  if (written < 0) return 0;
  if (nchars_ref != nullptr) *nchars_ref = static_cast<int>(covered);
  return static_cast<int>(written);
}

String::Utf8Value::Utf8Value(uint64_t value) {
  // `String::Utf8Value` runs ToString, so an object's own `toString` is what a
  // host reads. That needs an agent, which needs an entered context; with none
  // there is no host code to run and the primitive rendering is all there is.
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  int64_t len = context != nullptr
                    ? slag_value_to_utf8_context(context->slag(), value, nullptr, 0)
                    : slag_value_to_utf8(value, nullptr, 0);
  if (len < 0) return;
  data_ = new char[static_cast<size_t>(len) + 1];
  if (context != nullptr) {
    slag_value_to_utf8_context(context->slag(), value, data_,
                               static_cast<size_t>(len) + 1);
  } else {
    slag_value_to_utf8(value, data_, static_cast<size_t>(len) + 1);
  }
}

MaybeLocal<Script> Script::Compile(Local<Context> context,
                                   Local<String> source, ScriptOrigin* origin) {
  if (context.IsEmpty() || source.IsEmpty()) return MaybeLocal<Script>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  std::string text = ValueText(source.raw());
  // The origin's name is what a `v8::Message` reports as the script's name; the
  // engine never sees it, so it travels with the compile.
  std::string name;
  if (origin != nullptr) {
    uint64_t resource = origin->ResourceName().raw();
    if (slag_value_is_string(resource) != 0) name = ValueText(resource);
  }
  SlagScript* script = nullptr;
  if (slag_compile(current->slag(),
                   reinterpret_cast<const uint8_t*>(text.data()), text.size(),
                   reinterpret_cast<const uint8_t*>(name.data()), name.size(),
                   &script) != 0) {
    return MaybeLocal<Script>();
  }
  return Local<Script>::New(reinterpret_cast<uint64_t>(script));
}

MaybeLocal<Value> Script::Run(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  auto* script = reinterpret_cast<SlagScript*>(
      static_cast<uintptr_t>(Raw(this)));
  if (slag_script_run(current->slag(), script, &out) != 0) {
    return MaybeLocal<Value>();
  }
  // `kAuto` (the default, and what V8 does when the call stack empties) drains
  // the run's own microtasks here; `kExplicit` leaves them for the checkpoint.
  // A run that threw leaves its error pending and queues nothing new.
  if (g_current_isolate == nullptr ||
      g_current_isolate->GetMicrotasksPolicy() == MicrotasksPolicy::kAuto) {
    slag_drain_jobs(current->slag());
  }
  return Local<Value>::New(out);
}

namespace {

/* A `Module` facade object is a handle over the engine's `SlagModule` box. */
SlagModule* ModuleOf(const Module* module) {
  return reinterpret_cast<SlagModule*>(static_cast<uintptr_t>(Raw(module)));
}

std::string TextOf(Local<Value> value) {
  if (value.IsEmpty()) return {};
  return ValueText(value.raw());
}

}  // namespace

MaybeLocal<Module> ScriptCompiler::CompileModule(Isolate* isolate,
                                                 Source* source,
                                                 CompileOptions,
                                                 NoCacheReason) {
  if (isolate == nullptr || source == nullptr) return MaybeLocal<Module>();
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return MaybeLocal<Module>();
  const std::string text = TextOf(source->source_string());
  const std::string specifier = TextOf(source->resource_name());
  SlagModule* module = nullptr;
  if (slag_module_compile(
          slag, reinterpret_cast<const uint8_t*>(specifier.data()),
          specifier.size(), nullptr, 0,
          reinterpret_cast<const uint8_t*>(text.data()), text.size(),
          &module) != 0) {
    return MaybeLocal<Module>();
  }
  return MaybeLocal<Module>(Local<Module>::New(reinterpret_cast<uint64_t>(module)));
}

ModuleStatus Module::GetStatus() const {
  switch (slag_module_status(ModuleOf(this))) {
    case SLAG_MODULE_INSTANTIATING:
      return ModuleStatus::kInstantiating;
    case SLAG_MODULE_INSTANTIATED:
      return ModuleStatus::kInstantiated;
    case SLAG_MODULE_EVALUATING:
      return ModuleStatus::kEvaluating;
    case SLAG_MODULE_EVALUATED:
      return ModuleStatus::kEvaluated;
    case SLAG_MODULE_ERRORED:
      return ModuleStatus::kErrored;
    default:
      return ModuleStatus::kUninstantiated;
  }
}

Local<Value> Module::GetException() const {
  uint64_t out = 0;
  if (slag_module_exception(ModuleOf(this), &out) != 0) return Local<Value>();
  return Local<Value>::New(out);
}

Local<Value> Module::GetModuleNamespace() {
  SlagContext* slag = CurrentSlagContext();
  uint64_t out = 0;
  if (slag == nullptr || slag_module_namespace(slag, ModuleOf(this), &out) != 0) {
    return Local<Value>();
  }
  return Local<Value>::New(out);
}

int Module::GetModuleRequestsLength() const {
  return static_cast<int>(slag_module_request_count(ModuleOf(this)));
}

Local<String> Module::GetModuleRequest(int index) const {
  if (index < 0) return Local<String>();
  uint64_t out = 0;
  if (slag_module_request(ModuleOf(this), static_cast<size_t>(index), &out) != 0) {
    return Local<String>();
  }
  return Local<String>::New(out);
}

Local<Integer> Module::GetIdentityHash() const {
  Isolate* isolate = g_current_isolate;
  return Integer::New(isolate, slag_module_identity_hash(ModuleOf(this)));
}

Maybe<bool> Module::IsSourceTextModule() const {
  return Maybe<bool>(slag_module_is_source_text(ModuleOf(this)) != 0);
}

Maybe<bool> Module::IsSyntheticModule() const {
  return Maybe<bool>(slag_module_is_synthetic(ModuleOf(this)) != 0);
}

Maybe<bool> Module::InstantiateModule(Local<Context> context,
                                      ResolveModuleCallback callback) {
  if (context.IsEmpty() || callback == nullptr) return Maybe<bool>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  // The engine links from its own registry, so every module in the graph has
  // to be reached: the resolver is asked for each request, including the ones
  // a resolved dependency declares.
  std::vector<Module*> work{this};
  for (size_t i = 0; i < work.size(); ++i) {
    Module* module = work[i];
    const int count = module->GetModuleRequestsLength();
    for (int request = 0; request < count; ++request) {
      Local<String> specifier = module->GetModuleRequest(request);
      if (specifier.IsEmpty()) return Maybe<bool>();
      Local<Module> resolved;
      if (!callback(context, specifier,
                    Local<Module>::New(reinterpret_cast<uint64_t>(module)))
               .ToLocal(&resolved)) {
        return Maybe<bool>();
      }
      if (resolved.IsEmpty()) return Maybe<bool>();
      auto* dependency = reinterpret_cast<Module*>(
          static_cast<uintptr_t>(resolved.raw()));
      if (slag_module_register(current->slag(), ModuleOf(dependency),
                               specifier.raw()) != 0) {
        return Maybe<bool>(false);
      }
      work.push_back(dependency);
    }
  }
  return Maybe<bool>(slag_module_instantiate(current->slag(), ModuleOf(this)) == 0);
}

MaybeLocal<Value> Module::Evaluate(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  if (slag_module_evaluate(current->slag(), ModuleOf(this), &out) != 0) {
    return MaybeLocal<Value>();
  }
  return Local<Value>::New(out);
}

Location Module::SourceOffsetToLocation(int offset) const {
  Location location;
  slag_module_source_offset_to_location(ModuleOf(this), offset,
                                        &location.line_number,
                                        &location.column_number);
  return location;
}

MaybeLocal<WasmModuleObject> WasmModuleObject::Compile(
    Isolate*, MemorySpan<const uint8_t> wire_bytes) {
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr) return MaybeLocal<WasmModuleObject>();
  uint64_t out = 0;
  if (slag_wasm_compile(slag, wire_bytes.data(), wire_bytes.size(), &out) != 0) {
    return MaybeLocal<WasmModuleObject>();
  }
  return MaybeLocal<WasmModuleObject>(Local<WasmModuleObject>::New(out));
}

MaybeLocal<WasmModuleObject> WasmModuleObject::FromCompiledModule(
    Isolate*, const CompiledWasmModule& module) {
  SlagContext* slag = CurrentSlagContext();
  if (slag == nullptr || module.module_ == nullptr) {
    return MaybeLocal<WasmModuleObject>();
  }
  uint64_t out = 0;
  if (slag_wasm_from_compiled(slag, module.module_, &out) != 0) {
    return MaybeLocal<WasmModuleObject>();
  }
  return MaybeLocal<WasmModuleObject>(Local<WasmModuleObject>::New(out));
}

CompiledWasmModule WasmModuleObject::GetCompiledModule() {
  SlagContext* slag = CurrentSlagContext();
  SlagWasmModule* compiled = nullptr;
  if (slag == nullptr ||
      slag_wasm_get_compiled(slag, Raw(this), &compiled) != 0) {
    return CompiledWasmModule();
  }
  return CompiledWasmModule(compiled);
}

bool TryCatch::HasCaught() const {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  return context != nullptr && slag_has_error(context->slag()) != 0;
}

TryCatch::~TryCatch() {
  delete message_;
  // V8's TryCatch resets the isolate's pending exception when it goes out of
  // scope, so the next operation starts clean.
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context != nullptr) slag_clear_error(context->slag());
}

Message::Message(uint64_t resource_name, uint64_t source_line, uint64_t text,
                 int line, int start, int end)
    : resource_name_(resource_name),
      source_line_(source_line),
      text_(text),
      line_(line),
      start_(start),
      end_(end) {
  // The three values outlive the handle scope their strings were made in.
  slag_root_retain(resource_name_);
  slag_root_retain(source_line_);
  slag_root_retain(text_);
}

Message::~Message() {
  slag_root_release(resource_name_);
  slag_root_release(source_line_);
  slag_root_release(text_);
}

Local<v8::Message> TryCatch::Message() const {
  if (message_ != nullptr) {
    return Local<v8::Message>::New(reinterpret_cast<uint64_t>(message_));
  }
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context == nullptr) return Local<v8::Message>();
  SlagMessage position{};
  char name[256];
  char source_line[1024];
  // Only a compile records a position, so a runtime error has no message to
  // report and the host prints the exception alone.
  if (slag_pending_message(context->slag(), &position, name, sizeof(name),
                           source_line, sizeof(source_line)) != 1) {
    return Local<v8::Message>();
  }
  char text[1024];
  int64_t len = slag_pending_error(context->slag(), text, sizeof(text));
  if (len < 0) return Local<v8::Message>();
  message_ = new v8::Message(
      slag_string_new(reinterpret_cast<const uint8_t*>(name), strlen(name)),
      slag_string_new(reinterpret_cast<const uint8_t*>(source_line),
                      strlen(source_line)),
      slag_string_new(reinterpret_cast<const uint8_t*>(text),
                      static_cast<size_t>(len)),
      position.line, position.column, position.column);
  return Local<v8::Message>::New(reinterpret_cast<uint64_t>(message_));
}

Local<Value> TryCatch::Exception() const {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context == nullptr) return Local<Value>();
  // The thrown value first: a script's `throw` carries one, and that is what V8
  // hands back. A host `ThrowError` records a message only, so its text is the
  // fallback. Neither consumes the pending error: the message and the stack
  // read the same one, and the destructor clears it.
  uint64_t thrown = 0;
  if (slag_pending_exception(context->slag(), &thrown) != 0) {
    return Local<Value>::New(thrown);
  }
  char buffer[1024];
  int64_t len = slag_pending_error(context->slag(), buffer, sizeof(buffer));
  if (len < 0) return Local<Value>();
  return Local<Value>::New(slag_string_new(
      reinterpret_cast<const uint8_t*>(buffer), static_cast<size_t>(len)));
}

MaybeLocal<Value> TryCatch::StackTrace(Local<Context> context) const {
  if (context.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  char buffer[8192];
  int64_t len = slag_pending_stack(current->slag(), buffer, sizeof(buffer));
  if (len <= 0) return MaybeLocal<Value>();
  return MaybeLocal<Value>(Local<Value>::New(slag_string_new(
      reinterpret_cast<const uint8_t*>(buffer), static_cast<size_t>(len))));
}

Local<ObjectTemplate> ObjectTemplate::New(Isolate*) {
  return Local<ObjectTemplate>::New(
      reinterpret_cast<uint64_t>(new ObjectTemplate()));
}

void ObjectTemplate::Set(Isolate*, const char* name,
                         Local<FunctionTemplate> function) {
  entries_.push_back(Entry{
      name == nullptr ? std::string() : std::string(name),
      reinterpret_cast<FunctionTemplate*>(
          static_cast<uintptr_t>(function.raw()))});
}

void ObjectTemplate::SetAccessor(Local<Name> name,
                                 AccessorNameGetterCallback getter,
                                 AccessorNameSetterCallback setter,
                                 Local<Value> data, PropertyAttribute attribute) {
  if (name.IsEmpty() || getter == nullptr) return;
  Accessor accessor;
  accessor.getter = getter;
  accessor.setter = setter;
  accessor.name_bits = name.raw();
  accessor.data_bits = data.IsEmpty() ? slag_undefined() : data.raw();
  accessor.attributes = static_cast<int32_t>(attribute);
  // The template outlives the scopes the name and data were made in, and it is
  // never freed, so the roots are held for the process (like `Global` holds one).
  slag_root_retain(accessor.name_bits);
  slag_root_retain(accessor.data_bits);
  accessors_.push_back(std::move(accessor));
}

void ObjectTemplate::SetNativeDataProperty(Local<Name> name,
                                           AccessorNameGetterCallback getter,
                                           AccessorNameSetterCallback setter,
                                           Local<Value> data,
                                           PropertyAttribute attribute) {
  SetAccessor(name, getter, setter, data, attribute);
}

void ObjectTemplate::SetLazyDataProperty(Local<Name> name,
                                         AccessorNameGetterCallback getter,
                                         Local<Value> data,
                                         PropertyAttribute attribute) {
  SetAccessor(name, getter, nullptr, data, attribute);
}

MaybeLocal<Object> ObjectTemplate::NewInstance(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Object>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  SlagContext* slag = current->slag();

  uint64_t object = 0;
  int status = 0;
  if (has_named_handler_ || has_indexed_handler_) {
    // The handler struct carries each configuration's callbacks plus the
    // address of the configuration the trampolines read them from. The
    // configurations live as long as this template, like the template itself.
    SlagPropertyHandler handler{};
    handler.getter = GetterTrampoline;
    handler.setter = SetterTrampoline;
    handler.query = QueryTrampoline;
    handler.deleter = DeleterTrampoline;
    handler.enumerator = EnumeratorTrampoline;
    handler.descriptor = DescriptorTrampoline;
    handler.definer = DefinerTrampoline;
    handler.indexed_getter = IndexedGetterTrampoline;
    handler.indexed_setter = IndexedSetterTrampoline;
    handler.indexed_query = IndexedQueryTrampoline;
    handler.indexed_deleter = IndexedDeleterTrampoline;
    handler.indexed_enumerator = IndexedEnumeratorTrampoline;
    handler.indexed_descriptor = IndexedDescriptorTrampoline;
    handler.indexed_definer = IndexedDefinerTrampoline;
    handler.data = named_handler_.data.IsEmpty()
                       ? slag_undefined()
                       : named_handler_.data.raw();
    handler.state = &named_handler_;
    handler.indexed_state = &indexed_handler_;
    status = slag_object_new_handler(slag, static_cast<size_t>(internal_fields_),
                                     1, &handler, &object);
  } else if (call_as_function_ != nullptr) {
    // The instance is callable: a builtin whose body is the template's handler,
    // with the template's members and internal fields on its object part. A
    // template with a property handler keeps the handler object instead, since
    // the engine's host objects have no [[Call]].
    status = slag_object_new_callable(slag,
                                      static_cast<size_t>(internal_fields_), 1,
                                      &CallAsFunctionTrampoline, this, &object);
  } else {
    status = slag_object_new(slag, static_cast<size_t>(internal_fields_), 1,
                             &object);
  }
  if (status != 0) {
    return MaybeLocal<Object>();
  }
  // The functions below allocate, so the instance has to be a root first.
  Local<Object> instance = Local<Object>::New(object);
  for (const auto& entry : entries_) {
    if (slag_object_register_fn(
            slag, object,
            reinterpret_cast<const uint8_t*>(entry.name.data()),
            entry.name.size(), &HostTrampoline, entry.function) != 0) {
      return MaybeLocal<Object>();
    }
  }
  // Each accessor's specification lives in the template, which is not freed, so
  // its address is stable for as long as an instance can call it.
  for (auto& accessor : accessors_) {
    const slag_accessor_getter getter =
        accessor.getter == nullptr ? nullptr : &AccessorGetterTrampoline;
    const slag_accessor_setter setter =
        accessor.setter == nullptr ? nullptr : &AccessorSetterTrampoline;
    if (slag_object_set_accessor(slag, object, accessor.name_bits, getter,
                                 setter, &accessor, accessor.attributes) != 0) {
      return MaybeLocal<Object>();
    }
  }
  return instance;
}

void ObjectTemplate::SetHandler(
    const NamedPropertyHandlerConfiguration& configuration) {
  named_handler_ = configuration;
  has_named_handler_ = true;
}

void ObjectTemplate::SetHandler(
    const IndexedPropertyHandlerConfiguration& configuration) {
  indexed_handler_ = configuration;
  has_indexed_handler_ = true;
}

Local<Context> Context::New(Isolate* isolate, std::nullptr_t,
                            Local<ObjectTemplate> global_template) {
  if (isolate == nullptr) return Local<Context>();
  SlagContext* slag = slag_context_new(isolate->slag_);
  if (slag == nullptr) return Local<Context>();

  auto* context = new Context();
  context->slag_ = slag;
  context->isolate_ = isolate;
  slag_watch_collections(slag, isolate, &CollectionObserver);

  if (!global_template.IsEmpty()) {
    auto* tmpl = reinterpret_cast<ObjectTemplate*>(
        static_cast<uintptr_t>(global_template.raw()));
    for (const auto& entry : tmpl->entries()) {
      if (slag_register_fn(slag,
                           reinterpret_cast<const uint8_t*>(entry.name.data()),
                           entry.name.size(), &HostTrampoline,
                           entry.function) != 0) {
        delete context;
        return Local<Context>();
      }
    }
  }

  isolate->contexts_.push_back(context);
  // The first context an isolate with a startup blob creates is its default
  // one: the realm's globals are what the blob carries, so that is where it is
  // restored.
  if (!isolate->snapshot_blob_.empty() && !isolate->snapshot_consumed_) {
    slag_context_read_snapshot(slag, isolate->snapshot_blob_.data(),
                               isolate->snapshot_blob_.size());
    isolate->snapshot_consumed_ = true;
  }
  return Local<Context>::New(reinterpret_cast<uint64_t>(context));
}

Context::~Context() {
  ClearAllSlots();
  UseDefaultSecurityToken();
  if (slag_ != nullptr) slag_context_free(slag_);
}

Local<Object> Context::Global() const {
  return Local<Object>::New(slag_global(slag_));
}

Isolate* Isolate::New(const CreateParams& params) {
  auto* isolate = new Isolate();
  isolate->slag_ = slag_isolate_new();
  if (isolate->slag_ == nullptr) {
    delete isolate;
    return nullptr;
  }
  isolate->array_buffer_allocator_ = params.array_buffer_allocator;
  // The blob is copied: V8's contract lets the caller free its `StartupData`
  // once `New` returns, and the first context restores from this copy.
  if (params.snapshot_blob != nullptr &&
      params.snapshot_blob->is_valid()) {
    const auto* begin =
        reinterpret_cast<const uint8_t*>(params.snapshot_blob->data);
    isolate->snapshot_blob_.assign(
        begin, begin + static_cast<size_t>(params.snapshot_blob->raw_size));
  }
  return isolate;
}

void Isolate::Dispose() {
  for (Context* context : contexts_) delete context;
  contexts_.clear();
  for (const auto& entry : private_names_) slag_root_release(entry.second);
  private_names_.clear();
  if (default_security_token_ != 0) {
    slag_root_release(default_security_token_);
    default_security_token_ = 0;
  }
  if (slag_ != nullptr) slag_isolate_free(slag_);
  delete this;
}

void Isolate::ThrowError(const char* message) {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context == nullptr || message == nullptr) return;
  slag_throw_error(context->slag(),
                   reinterpret_cast<const uint8_t*>(message),
                   std::strlen(message));
}

Local<Context> Isolate::GetCurrentContext() {
  // No context entered is an empty handle, not a handle over address zero: a
  // caller that checks `IsEmpty` has to see the difference (V8 answers the same
  // way, and a zero handle dereferences to null).
  if (g_current_context == 0) return Local<Context>();
  return Local<Context>::New(g_current_context);
}

Local<Context> Isolate::GetEnteredContext() { return GetCurrentContext(); }

Local<Context> Isolate::GetEnteredOrMicrotaskContext() {
  return GetCurrentContext();
}

}  // namespace v8
