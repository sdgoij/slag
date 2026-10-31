#include "include/slag_a0.h"
#include "include/v8.h"

#include <cstring>
#include <deque>
#include <string>
#include <vector>

namespace {

/* The one handle stack for this process. `HandleScope` marks it and restores
 * the mark; `Local<T>::New` allocates from it. A rooted slot retains its value
 * in the C ABI's root registry; a facade pointer slot does not. */
thread_local std::deque<v8::HandleSlot> g_handles;

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

uint64_t HostTrampoline(void* data, const void* call, uint64_t this_value,
                        const uint64_t* args, size_t len) {
  auto* function = static_cast<v8::FunctionTemplate*>(data);
  v8::FunctionCallbackInfo<v8::Value> info(v8::g_current_isolate, call,
                                           this_value, args,
                                           static_cast<int>(len));
  function->callback()(info);
  return info.return_value();
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

bool PumpMessageLoop(Platform*, Isolate*) { return false; }

}  // namespace platform

ArrayBuffer::Allocator* ArrayBuffer::Allocator::NewDefaultAllocator() {
  return new Allocator();
}

bool Value::IsUndefined() const {
  return slag_value_is_undefined(Raw(this)) != 0;
}

bool Value::IsString() const { return slag_value_is_string(Raw(this)) != 0; }

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

MaybeLocal<Value> Object::Get(Local<Context> context, Local<Value> key) {
  if (context.IsEmpty() || key.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  if (slag_object_get_key(current->slag(), Raw(this), key.raw(), &out) != 0) {
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
  if (slag_object_set_key(current->slag(), Raw(this), key.raw(), value.raw()) !=
      0) {
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
  if (slag_object_has_key(current->slag(), Raw(this), key.raw(), &found) != 0) {
    return Maybe<bool>();
  }
  return Maybe<bool>(found != 0);
}

Maybe<bool> Object::Has(Local<Context> context, uint32_t index) {
  return Has(context,
             Local<Value>::New(slag_number_new(static_cast<double>(index))));
}

bool Object::Delete(Local<Context> context, Local<Value> key) {
  if (context.IsEmpty() || key.IsEmpty()) return false;
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  int32_t deleted = 0;
  return slag_object_delete_key(current->slag(), Raw(this), key.raw(),
                                &deleted) == 0 &&
         deleted != 0;
}

bool Object::Delete(Local<Context> context, uint32_t index) {
  return Delete(context,
                Local<Value>::New(slag_number_new(static_cast<double>(index))));
}

MaybeLocal<Value> Object::GetRealNamedProperty(Local<Context> context,
                                               Local<Name> key) {
  if (context.IsEmpty() || key.IsEmpty()) return MaybeLocal<Value>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  uint64_t out = 0;
  int32_t found = 0;
  if (slag_object_get_own_key(current->slag(), Raw(this), key.raw(), &out,
                              &found) != 0) {
    return MaybeLocal<Value>();
  }
  if (found == 0) return MaybeLocal<Value>();
  return Local<Value>::New(out);
}

Local<Number> Number::New(Isolate*, double value) {
  return Local<Number>::New(slag_number_new(value));
}

Local<Integer> Integer::New(Isolate*, int32_t value) {
  return Local<Integer>::New(slag_number_new(static_cast<double>(value)));
}

Maybe<uint32_t> Value::Uint32Value(Local<Context>) const {
  double number = slag_value_to_number(Raw(this));
  if (!(number >= 0.0) || number > 4294967295.0) return Maybe<uint32_t>();
  return Maybe<uint32_t>(static_cast<uint32_t>(number));
}

Maybe<int32_t> Value::Int32Value(Local<Context>) const {
  double number = slag_value_to_number(Raw(this));
  if (!(number >= -2147483648.0) || number > 2147483647.0) {
    return Maybe<int32_t>();
  }
  return Maybe<int32_t>(static_cast<int32_t>(number));
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

int String::Length() const {
  return static_cast<int>(slag_value_to_utf8(Raw(this), nullptr, 0));
}

String::Utf8Value::Utf8Value(uint64_t value) {
  int64_t len = slag_value_to_utf8(value, nullptr, 0);
  if (len < 0) return;
  data_ = new char[static_cast<size_t>(len) + 1];
  slag_value_to_utf8(value, data_, static_cast<size_t>(len) + 1);
}

MaybeLocal<Script> Script::Compile(Local<Context> context,
                                   Local<String> source, ScriptOrigin*) {
  if (context.IsEmpty() || source.IsEmpty()) return MaybeLocal<Script>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  std::string text = ValueText(source.raw());
  SlagScript* script = nullptr;
  if (slag_compile(current->slag(),
                   reinterpret_cast<const uint8_t*>(text.data()), text.size(),
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
  return Local<Value>::New(out);
}

bool TryCatch::HasCaught() const {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  return context != nullptr && slag_has_error(context->slag()) != 0;
}

Local<Value> TryCatch::Exception() const {
  auto* context = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(g_current_context));
  if (context == nullptr) return Local<Value>();
  char buffer[1024];
  int64_t len = slag_take_error(context->slag(), buffer, sizeof(buffer));
  if (len < 0) return Local<Value>();
  return Local<Value>::New(slag_string_new(
      reinterpret_cast<const uint8_t*>(buffer), static_cast<size_t>(len)));
}

Local<FunctionTemplate> FunctionTemplate::New(Isolate*,
                                              FunctionCallback callback) {
  return Local<FunctionTemplate>::New(
      reinterpret_cast<uint64_t>(new FunctionTemplate(callback)));
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

MaybeLocal<Object> ObjectTemplate::NewInstance(Local<Context> context) {
  if (context.IsEmpty()) return MaybeLocal<Object>();
  auto* current = reinterpret_cast<Context*>(
      static_cast<uintptr_t>(context.raw()));
  SlagContext* slag = current->slag();

  uint64_t object = 0;
  if (slag_object_new(slag, static_cast<size_t>(internal_fields_), 1, &object) !=
      0) {
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
  return instance;
}

Local<Context> Context::New(Isolate* isolate, std::nullptr_t,
                            Local<ObjectTemplate> global_template) {
  if (isolate == nullptr) return Local<Context>();
  SlagContext* slag = slag_context_new(isolate->slag_);
  if (slag == nullptr) return Local<Context>();

  auto* context = new Context();
  context->slag_ = slag;

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
  return Local<Context>::New(reinterpret_cast<uint64_t>(context));
}

Context::~Context() {
  if (slag_ != nullptr) slag_context_free(slag_);
}

Local<Object> Context::Global() const {
  return Local<Object>::New(slag_global(slag_));
}

Isolate* Isolate::New(const CreateParams&) {
  auto* isolate = new Isolate();
  isolate->slag_ = slag_isolate_new();
  if (isolate->slag_ == nullptr) {
    delete isolate;
    return nullptr;
  }
  return isolate;
}

void Isolate::Dispose() {
  for (Context* context : contexts_) delete context;
  contexts_.clear();
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
  return Local<Context>::New(g_current_context);
}

}  // namespace v8
