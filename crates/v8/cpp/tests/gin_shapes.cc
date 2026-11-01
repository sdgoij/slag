// A gin-shaped compile/run target for A1.
//
// `gin` itself cannot be built standalone: gin/BUILD.gn depends on //base,
// //base/allocator:buildflags, //third_party/abseil-cpp:absl, and
// gin/wrappable.h needs v8::Object::Wrappable, v8::CppHeapPointerTag, and
// Object::Unwrap. This harness exercises the V8 usage patterns gin's
// arguments/converter/object_template_builder paths rely on, without
// Chromium's base library.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>

#ifdef _WIN32
#include <crtdbg.h>

namespace {
// Armed before `main` runs, so a failed assertion or an abort raised while the
// first allocation happens is reported on stderr instead of waiting for a click
// on a modal box. A harness must never block its caller.
struct SuppressDialogBoxes {
  SuppressDialogBoxes() {
    _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
    _set_error_mode(_OUT_TO_STDERR);
    _CrtSetReportMode(_CRT_ASSERT, _CRTDBG_MODE_FILE);
    _CrtSetReportFile(_CRT_ASSERT, _CRTDBG_FILE_STDERR);
    _CrtSetReportMode(_CRT_ERROR, _CRTDBG_MODE_FILE);
    _CrtSetReportFile(_CRT_ERROR, _CRTDBG_FILE_STDERR);
  }
};
const SuppressDialogBoxes kSuppressDialogBoxes;
}  // namespace
#endif

#include "include/slag_a0.h"
#include "include/v8-context.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"
#include "include/v8-template.h"

namespace {

int NumberArg(const v8::FunctionCallbackInfo<v8::Value>& info, int index) {
  if (index >= info.Length()) return 0;
  return info[index]
      ->Int32Value(info.GetIsolate()->GetCurrentContext())
      .FromMaybe(0);
}

// The C++ object a wrapper names: gin keeps its pointer in the wrapper's first
// internal field, and the wrapper's methods recover it from `this`.
struct HostCounter {
  int value;
};

// A gin-shaped named-property handler: three names it owns, a getter that answers
// one of them and declines the rest, a query that claims all three, and a setter
// and deleter that record what they were asked. The counters are the host's own
// state, which a real host would keep behind the configuration's `data` value.
struct HostNames {
  int getters = 0;
  int setters = 0;
  int deleters = 0;
  int data_misses = 0;
  int indexed_getters = 0;
  int indexed_setters = 0;
  int indexed_deleters = 0;
  int descriptors = 0;
  int definers = 0;
  int definer_descriptors = 0;
  int allocations = 0;
  int enumerators = 0;
  int indexed_enumerators = 0;
};

HostNames host_names;
const char* const kHostNames[] = {"alpha", "beta", "gamma"};

bool IsHostName(const char* text) {
  if (text == nullptr) return false;
  for (const char* name : kHostNames) {
    if (strcmp(text, name) == 0) return true;
  }
  return false;
}

v8::Intercepted HostGet(v8::Local<v8::Name> property,
                        const v8::PropertyCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::String::Utf8Value text(isolate, property);
  if (!*text) return v8::Intercepted::kNo;
  ++host_names.getters;
  // A configuration's data is a language value in V8, and it arrives here.
  if (info.Data()->Int32Value(isolate->GetCurrentContext()).FromMaybe(-1) != 7) {
    fprintf(stderr, "handler data did not reach the callback\n");
    ++host_names.data_misses;
  }
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (strcmp(*text, "alpha") == 0) {
    info.GetReturnValue().Set(v8::Integer::New(isolate, 42));
    return v8::Intercepted::kYes;
  }
  // These two answer with values the callback builds, which is the re-entrant
  // allocation path: the operation that asked the handler holds the context.
  if (strcmp(*text, "fresh") == 0) {
    v8::Local<v8::Object> made = v8::Object::New(context);
    if (made.IsEmpty()) return v8::Intercepted::kNo;
    made->Set(context, v8::String::NewFromUtf8Literal(isolate, "value"),
              v8::Integer::New(isolate, 11));
    info.GetReturnValue().Set(made);
    ++host_names.allocations;
    return v8::Intercepted::kYes;
  }
  if (strcmp(*text, "list") == 0) {
    v8::Local<v8::Value> elements[] = {
        v8::String::NewFromUtf8Literal(isolate, "a"),
        v8::String::NewFromUtf8Literal(isolate, "b")};
    v8::Local<v8::Array> made = v8::Array::New(isolate, elements, 2);
    info.GetReturnValue().Set(made);
    ++host_names.allocations;
    return v8::Intercepted::kYes;
  }
  // Declining lets the engine's own property run, which is what kNo means.
  return v8::Intercepted::kNo;
}

v8::Intercepted HostSet(v8::Local<v8::Name> property, v8::Local<v8::Value>,
                        const v8::PropertyCallbackInfo<void>& info) {
  v8::String::Utf8Value text(info.GetIsolate(), property);
  if (!*text || !IsHostName(*text)) return v8::Intercepted::kNo;
  ++host_names.setters;
  return v8::Intercepted::kYes;
}

v8::Intercepted HostQuery(v8::Local<v8::Name> property,
                          const v8::PropertyCallbackInfo<v8::Integer>& info) {
  v8::String::Utf8Value text(info.GetIsolate(), property);
  if (!*text || !IsHostName(*text)) return v8::Intercepted::kNo;
  // V8's query answers a PropertyAttribute; None (0) means "present".
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 0));
  return v8::Intercepted::kYes;
}

v8::Intercepted HostDelete(v8::Local<v8::Name> property,
                           const v8::PropertyCallbackInfo<v8::Boolean>& info) {
  v8::String::Utf8Value text(info.GetIsolate(), property);
  if (!*text || !IsHostName(*text)) return v8::Intercepted::kNo;
  ++host_names.deleters;
  info.GetReturnValue().Set(v8::Boolean::New(info.GetIsolate(), true));
  return v8::Intercepted::kYes;
}

// The enumerator answers with the names the host owns. It builds the array inside
// the callback, which the allocation window makes legal.
v8::Intercepted HostEnumerate(
    const v8::PropertyCallbackInfo<v8::Array>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  ++host_names.enumerators;
  v8::Local<v8::Value> elements[] = {
      v8::String::NewFromUtf8Literal(isolate, kHostNames[0]),
      v8::String::NewFromUtf8Literal(isolate, kHostNames[1]),
      v8::String::NewFromUtf8Literal(isolate, kHostNames[2])};
  info.GetReturnValue().Set(v8::Array::New(isolate, elements, 3));
  return v8::Intercepted::kYes;
}

// The indexed twins: the host owns elements 0..2, and an index past them falls
// back to the engine.
v8::Intercepted HostGetIndex(uint32_t index,
                             const v8::PropertyCallbackInfo<v8::Value>& info) {
  ++host_names.indexed_getters;
  if (index >= 3) return v8::Intercepted::kNo;
  info.GetReturnValue().Set(
      v8::Integer::New(info.GetIsolate(), static_cast<int32_t>(index * 10)));
  return v8::Intercepted::kYes;
}

v8::Intercepted HostSetIndex(uint32_t index, v8::Local<v8::Value>,
                             const v8::PropertyCallbackInfo<void>& info) {
  (void)info;
  if (index >= 3) return v8::Intercepted::kNo;
  ++host_names.indexed_setters;
  return v8::Intercepted::kYes;
}

v8::Intercepted HostQueryIndex(
    uint32_t index, const v8::PropertyCallbackInfo<v8::Integer>& info) {
  if (index >= 3) return v8::Intercepted::kNo;
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 0));
  return v8::Intercepted::kYes;
}

v8::Intercepted HostDeleteIndex(
    uint32_t index, const v8::PropertyCallbackInfo<v8::Boolean>& info) {
  if (index >= 3) return v8::Intercepted::kNo;
  ++host_names.indexed_deleters;
  info.GetReturnValue().Set(v8::Boolean::New(info.GetIsolate(), true));
  return v8::Intercepted::kYes;
}

// The indexed enumerator answers with numbers, which the engine reads back as
// array-index keys (V8's `KeepNumbers`).
v8::Intercepted HostEnumerateIndex(
    const v8::PropertyCallbackInfo<v8::Array>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  ++host_names.indexed_enumerators;
  v8::Local<v8::Value> elements[] = {v8::Integer::New(isolate, 0),
                                     v8::Integer::New(isolate, 1),
                                     v8::Integer::New(isolate, 2)};
  info.GetReturnValue().Set(v8::Array::New(isolate, elements, 3));
  return v8::Intercepted::kYes;
}

// An accessor installed with `SetAccessor`: the getter allocates the object it
// answers with, reads the holder through the callback window, and reports the
// property's key and the accessor's data; the setter records the value and
// deletes a scratch property, also through the window.
struct HostAccessor {
  int gets = 0;
  int sets = 0;
  int reads = 0;
  int hits = 0;
  int deleted = 0;
  int real_reads = 0;
  int real_hits = 0;
  int last = 0;
};

HostAccessor host_accessor;

// The data-property and accessor-property forms: a lazy getter (read on every
// access), a native setter, and a FunctionTemplate accessor pair.
struct HostProperties {
  int lazy_reads = 0;
  int native_writes = 0;
  int template_reads = 0;
  int template_writes = 0;
  int last_written = -1;
};

HostProperties host_properties;

// Weak handles and GC callbacks: the death callbacks a persistent's finalizer
// runs and the hooks around a requested collection.
struct WeakState {
  int first = 0;
  int second = 0;
  int cleared_survived = 0;
  void* parameter = nullptr;
};

WeakState weak_state;
int gc_prologue = 0;
int gc_epilogue = 0;

void WeakGoneSecond(const v8::WeakCallbackInfo<int>&) { ++weak_state.second; }

void WeakGone(const v8::WeakCallbackInfo<int>& info) {
  ++weak_state.first;
  weak_state.parameter = info.GetParameter();
  info.SetSecondPassCallback(&WeakGoneSecond);
}

void WeakClearedSurvived(const v8::WeakCallbackInfo<int>&) {
  ++weak_state.cleared_survived;
}

void GCCallbackPrologue(v8::Isolate*, v8::GCType, v8::GCCallbackFlags, void*) {
  ++gc_prologue;
}

void GCCallbackEpilogue(v8::Isolate*, v8::GCType type, v8::GCCallbackFlags,
                        void*) {
  if ((type & v8::kGCTypeMarkSweepCompact) != 0) ++gc_epilogue;
}

// Make a weak persistent over a value no C++ local names: the frame that held
// the local has returned, and the caller scrubs the stack, so the conservative
// scan cannot root what the weak handle is watching.
#if defined(_MSC_VER)
__declspec(noinline)
#endif
void MakeUnrootedWeak(v8::Isolate* isolate, v8::Global<v8::Object>* weak,
                      int* parameter,
                      void (*callback)(const v8::WeakCallbackInfo<int>&)) {
  v8::HandleScope scope(isolate);
  weak->Reset(isolate, v8::Object::New(isolate));
  weak->SetWeak(parameter, callback, v8::WeakCallbackType::kParameter);
}

#if defined(_MSC_VER)
__declspec(noinline)
#endif
void ScrubStack(int depth) {
  volatile uint64_t pad[16] = {};
  if (depth > 0) ScrubStack(depth - 1);
  (void)pad[0];
}

// A function template's callback and the method its prototype template carries.
void TemplateConstructor(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  v8::Local<v8::Object> self = info.This().As<v8::Object>();
  v8::Local<v8::Value> first = v8::Integer::New(isolate, -1);
  if (info.Length() > 0) first = info[0];
  self->Set(context, v8::String::NewFromUtf8Literal(isolate, "arg0"), first);
  self->Set(context, v8::String::NewFromUtf8Literal(isolate, "data"),
            info.Data());
}

void ParentKind(const v8::FunctionCallbackInfo<v8::Value>& info) {
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 42));
}

// The dependency a module resolver hands back. V8's resolver is a plain
// function pointer, so the module it answers with lives at file scope.
v8::Global<v8::Module> module_dependency;

v8::MaybeLocal<v8::Module> ResolveModule(v8::Local<v8::Context>,
                                        v8::Local<v8::String> specifier,
                                        v8::Local<v8::Module>) {
  v8::String::Utf8Value text(nullptr, specifier);
  if (*text != nullptr && strcmp(*text, "dep.js") == 0 &&
      !module_dependency.IsEmpty()) {
    return v8::MaybeLocal<v8::Module>(
        module_dependency.Get(v8::g_current_isolate));
  }
  return v8::MaybeLocal<v8::Module>();
}

void HostLazyGet(v8::Local<v8::Name>,
                 const v8::PropertyCallbackInfo<v8::Value>& info) {
  ++host_properties.lazy_reads;
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 41));
}

void HostNativeSet(v8::Local<v8::Name>, v8::Local<v8::Value> value,
                   const v8::PropertyCallbackInfo<void>& info) {
  ++host_properties.native_writes;
  host_properties.last_written =
      value->Int32Value(info.GetIsolate()->GetCurrentContext()).FromMaybe(-1);
}

void PropertyTemplateGet(const v8::FunctionCallbackInfo<v8::Value>& info) {
  ++host_properties.template_reads;
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 77));
}

void PropertyTemplateSet(const v8::FunctionCallbackInfo<v8::Value>& info) {
  ++host_properties.template_writes;
  if (info.Length() > 0) {
    host_properties.last_written =
        info[0]
            ->Int32Value(info.GetIsolate()->GetCurrentContext())
            .FromMaybe(-1);
  }
}

void HostAccessGet(v8::Local<v8::Name> property,
                   const v8::PropertyCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  ++host_accessor.gets;
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  v8::Local<v8::Object> answer = v8::Object::New(context);
  if (answer.IsEmpty()) return;
  answer->Set(context, v8::String::NewFromUtf8Literal(isolate, "name"), property);
  answer->Set(context, v8::String::NewFromUtf8Literal(isolate, "data"), info.Data());
  // A read of another property of the holder goes through the same window,
  // including the real-property (interceptor-free) forms.
  v8::Local<v8::Name> tag = v8::String::NewFromUtf8Literal(isolate, "tag");
  ++host_accessor.reads;
  if (info.Holder()->Has(context, tag).FromMaybe(false)) {
    ++host_accessor.hits;
    answer->Set(context, tag, info.Holder()->Get(context, tag).ToLocalChecked());
  }
  if (info.Holder()->HasRealNamedProperty(context, tag).FromMaybe(false)) {
    ++host_accessor.real_hits;
  }
  v8::Local<v8::Value> real;
  if (info.Holder()->GetRealNamedProperty(context, tag).ToLocal(&real) &&
      real->Int32Value(context).FromMaybe(-1) == 7) {
    ++host_accessor.real_reads;
  }
  info.GetReturnValue().Set(answer);
}

void HostAccessSet(v8::Local<v8::Name>, v8::Local<v8::Value> value,
                   const v8::PropertyCallbackInfo<void>& info) {
  ++host_accessor.sets;
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  host_accessor.last = value->Int32Value(context).FromMaybe(-1);
  host_accessor.deleted =
      info.Holder()
          ->Delete(context, v8::String::NewFromUtf8Literal(isolate, "scratch"))
      ? 1
      : 0;
}

// A descriptor callback answers with an object, so it builds one inside the
// callback: the re-entrant window is what makes that legal.
v8::Intercepted HostDescriptor(
    v8::Local<v8::Name> property,
    const v8::PropertyCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::String::Utf8Value text(isolate, property);
  if (!*text || !IsHostName(*text)) return v8::Intercepted::kNo;
  ++host_names.descriptors;
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  v8::Local<v8::Object> answer = v8::Object::New(context);
  if (answer.IsEmpty()) return v8::Intercepted::kNo;
  answer->Set(context, v8::String::NewFromUtf8Literal(isolate, "value"),
              v8::Integer::New(isolate, 42));
  answer->Set(context,
              v8::String::NewFromUtf8Literal(isolate, "enumerable"),
              v8::Boolean::New(isolate, true));
  answer->Set(context,
              v8::String::NewFromUtf8Literal(isolate, "configurable"),
              v8::Boolean::New(isolate, true));
  info.GetReturnValue().Set(answer);
  ++host_names.allocations;
  return v8::Intercepted::kYes;
}

// The definer receives the descriptor V8 built, so the facade has to marshal it
// back into the C++ shape.
v8::Intercepted HostDefiner(
    v8::Local<v8::Name> property, const v8::PropertyDescriptor& descriptor,
    const v8::PropertyCallbackInfo<v8::Boolean>& info) {
  v8::String::Utf8Value text(info.GetIsolate(), property);
  if (!*text || !IsHostName(*text)) return v8::Intercepted::kNo;
  ++host_names.definers;
  if (descriptor.has_value() && descriptor.has_writable()) {
    ++host_names.definer_descriptors;
  }
  info.GetReturnValue().Set(v8::Boolean::New(info.GetIsolate(), true));
  return v8::Intercepted::kYes;
}

void Bump(const v8::FunctionCallbackInfo<v8::Value>& info) {
  auto* counter = info.This().As<v8::Object>()->Wrappable<HostCounter>();
  if (counter == nullptr) {
    fprintf(stderr, "a wrappable method lost its object\n");
    info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), -1));
    return;
  }
  const int by = NumberArg(info, 0);
  counter->value += by == 0 ? 1 : by;
  info.GetReturnValue().Set(
      v8::Integer::New(info.GetIsolate(), counter->value));
}

void Print(const v8::FunctionCallbackInfo<v8::Value>& info) {
  for (int i = 0; i < info.Length(); ++i) {
    v8::HandleScope scope(info.GetIsolate());
    v8::String::Utf8Value text(info.GetIsolate(), info[i]);
    if (i != 0) printf(" ");
    printf("%s", *text ? *text : "<conversion failed>");
  }
  printf("\n");
  fflush(stdout);
}

void Sum(const v8::FunctionCallbackInfo<v8::Value>& info) {
  int total = 0;
  for (int i = 0; i < info.Length(); ++i) total += NumberArg(info, i);
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), total));
}

// A host function that builds and settles a capability from inside the call,
// which is what a binding's promise-returning accessor does: the resolver and
// the promise operations all run through the callback's window there.
void MakePromise(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  v8::Local<v8::Promise::Resolver> resolver;
  if (!v8::Promise::Resolver::New(context).ToLocal(&resolver)) return;
  const int32_t value = NumberArg(info, 0);
  if (!resolver->Resolve(context, v8::Integer::New(isolate, value))
           .FromMaybe(false)) {
    return;
  }
  info.GetReturnValue().Set(resolver->GetPromise());
}

// A call-as-function handler: the arguments arrive and the template's data value
// is on the info. `this` is the call's receiver, which for a plain call is
// undefined (as in V8); the instance's own internal fields are reached from the
// host side, on the callee value.
void CallableBody(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const int32_t a = NumberArg(info, 0);
  const int32_t b = NumberArg(info, 1);
  const int32_t base = info.Data()->Int32Value(context).FromMaybe(0);
  info.GetReturnValue().Set(v8::Integer::New(isolate, a + b + base));
}

void CallableMarker(const v8::FunctionCallbackInfo<v8::Value>& info) {
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 7));
}

// A method declared over a signature: it may only run on an instance of the
// template the signature names.
void GuardedBody(const v8::FunctionCallbackInfo<v8::Value>& info) {
  info.GetReturnValue().Set(v8::Integer::New(
      info.GetIsolate(), NumberArg(info, 0) * 2));
}

// An accessor declared over an `AccessorSignature`: its getter runs for an
// instance and is refused with `Illegal invocation` for anything else.
int guard_reads = 0;
void SignatureGuardGet(v8::Local<v8::Name>,
                       const v8::PropertyCallbackInfo<v8::Value>& info) {
  ++guard_reads;
  info.GetReturnValue().Set(v8::Integer::New(info.GetIsolate(), 77));
}

// The two answers a per-object access check can give
// (`ObjectTemplate::SetAccessCheckCallback`). The count is the host's proof
// that the check ran only where it should: a same-realm access never reaches
// it.
int access_checks = 0;
bool DenyAccess(v8::Local<v8::Context>, v8::Local<v8::Object>,
                v8::Local<v8::Value>) {
  ++access_checks;
  return false;
}
bool AllowAccess(v8::Local<v8::Context>, v8::Local<v8::Object>,
                 v8::Local<v8::Value> data) {
  ++access_checks;
  // The template's data arrives, which is what a host uses to name its origin.
  return data->IsInt32();
}

// The three helpers every block's `run`, `bind` and `number_at` delegate to.
//
// A harness must report a failing step, not die on it: `ToLocalChecked` and
// `ToChecked` abort the process (exit code 3, no message once stdout is gone),
// which turns a regression into a silent exit code. Each of these prints what
// failed and counts it, and answers a defined value so the rest of the block
// still runs and can report more.

// Compile and run `source` in `context`; *undefined* when it threw.
v8::Local<v8::Value> RunScript(v8::Isolate* isolate,
                               v8::Local<v8::Context> context,
                               const char* source, int* failures) {
  v8::TryCatch caught(isolate);
  v8::Local<v8::Script> script;
  v8::Local<v8::Value> result;
  if (v8::Script::Compile(context,
                          v8::String::NewFromUtf8Literal(isolate, source))
          .ToLocal(&script) &&
      script->Run(context).ToLocal(&result)) {
    return result;
  }
  v8::Local<v8::Value> thrown = caught.Exception();
  if (thrown.IsEmpty()) {
    fprintf(stderr, "script failed with no exception: %.90s\n", source);
  } else {
    v8::String::Utf8Value text(isolate, thrown);
    fprintf(stderr, "script threw %s: %.90s\n",
            *text == nullptr ? "(unrenderable)" : *text, source);
  }
  ++*failures;
  return v8::Local<v8::Value>();
}

// Define a global binding; a refused define is reported.
void BindGlobal(v8::Isolate* isolate, v8::Local<v8::Context> context,
                const char* name, v8::Local<v8::Value> value, int* failures) {
  if (context->Global()
          ->Set(context, v8::String::NewFromUtf8Literal(isolate, name), value)
          .FromMaybe(false)) {
    return;
  }
  fprintf(stderr, "could not bind the global '%s'\n", name);
  ++*failures;
}

// Set a named property on `object`; a refused write is reported.
void SetOrReport(v8::Isolate* isolate, v8::Local<v8::Context> context,
                 v8::Local<v8::Object> object, const char* name,
                 v8::Local<v8::Value> value, int* failures) {
  if (object->Set(context, v8::String::NewFromUtf8Literal(isolate, name), value)
          .FromMaybe(false)) {
    return;
  }
  fprintf(stderr, "could not set '%s' on the object\n", name);
  ++*failures;
}

// Read a global as an int, answering -1 and reporting when the read fails.
int IntGlobal(v8::Isolate* isolate, v8::Local<v8::Context> context,
              const char* name, int* failures) {
  v8::Local<v8::Value> value;
  if (!context->Global()
           ->Get(context, v8::String::NewFromUtf8Literal(isolate, name))
           .ToLocal(&value)) {
    fprintf(stderr, "could not read the global '%s'\n", name);
    ++*failures;
    return -1;
  }
  return value->Int32Value(context).FromMaybe(-1);
}

// A host allocator that counts what it lends, so the harness can prove the facade
// draws a backing store from it and gives the bytes back.
struct CountingAllocator : v8::ArrayBuffer::Allocator {
  size_t allocated = 0;
  size_t freed = 0;
  size_t allocations = 0;

  void* Allocate(size_t length) override {
    void* data = ::calloc(length, 1);
    if (data == nullptr) return nullptr;
    allocated += length;
    ++allocations;
    return data;
  }

  void Free(void* data, size_t length) override {
    freed += length;
    ::free(data);
  }
};

}  // namespace

int main() {
  // Unbuffered, so a run that dies mid-way still shows how far it got: a
  // block-buffered stdout loses everything when the process aborts.
  setvbuf(stdout, nullptr, _IONBF, 0);
  v8::V8::InitializeICUDefaultLocation(nullptr);
  v8::V8::Initialize();

  CountingAllocator allocator;
  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = &allocator;
  v8::Isolate* isolate = v8::Isolate::New(params);
  // A property callback reports this isolate, and the facade resolves the
  // context an allocation goes through from it, so the host enters it the way
  // V8's samples do.
  v8::Isolate::Scope isolate_scope(isolate);

  v8::Global<v8::Context> context;
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::ObjectTemplate> global = v8::ObjectTemplate::New(isolate);
    global->Set(isolate, "print", v8::FunctionTemplate::New(isolate, Print));
    global->Set(isolate, "sum", v8::FunctionTemplate::New(isolate, Sum));
    context = v8::Global<v8::Context>(
        isolate, v8::Context::New(isolate, nullptr, global));
  }

  int failures = 0;
  {
    v8::HandleScope scope(isolate);
    v8::Context::Scope context_scope(context.Get(isolate));

    // Nested scopes: each round allocates locals inside an inner scope and
    // releases them. The outer scope's source handle stays valid.
    v8::Local<v8::String> source =
        v8::String::NewFromUtf8Literal(isolate, "sum(1, 2, 3) + sum(4, 5)");
    for (int round = 0; round < 100; ++round) {
      v8::HandleScope inner(isolate);
      v8::Local<v8::Script> script =
          v8::Script::Compile(context.Get(isolate), source).ToLocalChecked();
      v8::Local<v8::Value> result =
          script->Run(context.Get(isolate)).ToLocalChecked();
      int value = result->Int32Value(context.Get(isolate)).FromMaybe(-1);
      if (value != 15) {
        fprintf(stderr, "round %d: expected 15, got %d\n", round, value);
        ++failures;
      }
      // `As` keeps the same slot, not a copy of the value.
      v8::Local<v8::Value> as_value = result.As<v8::Value>();
      if (as_value.raw() != result.raw()) {
        fprintf(stderr, "round %d: As changed the handle\n", round);
        ++failures;
      }
    }

    v8::Local<v8::Script> print_script =
        v8::Script::Compile(
            context.Get(isolate),
            v8::String::NewFromUtf8Literal(isolate, "print(sum(10, 20))"))
            .ToLocalChecked();
    print_script->Run(context.Get(isolate)).ToLocalChecked();

    {
      // The value below lives only in a handle slot (the C++ heap); the C++
      // stack holds the slot pointer. Force a collection and read it back.
      v8::Local<v8::String> held =
          v8::String::NewFromUtf8Literal(isolate, "survives");
      if (slag_collect(context.Get(isolate)->slag()) != 0) {
        fprintf(stderr, "collection failed\n");
        ++failures;
      }
      v8::String::Utf8Value text(isolate, held);
      if (!*text || strcmp(*text, "survives") != 0) {
        fprintf(stderr, "handle did not survive the collection\n");
        ++failures;
      }
    }
  }

  // A `Global` outlives every handle scope. The retention count is the
  // assertion rather than the read-back: a collection cannot show that the
  // retention is what kept the string alive, because a stale word in a dead
  // C++ frame sits inside the conservative scan's range and would do it alone.
  {
    const size_t before = slag_root_count();
    auto* pinned = new v8::Global<v8::String>();
    {
      v8::HandleScope scope(isolate);
      v8::Local<v8::String> text =
          v8::String::NewFromUtf8Literal(isolate, "pinned");
      *pinned = v8::Global<v8::String>(isolate, text);
    }
    if (slag_root_count() != before + 1) {
      fprintf(stderr, "a Global retained %lld values, expected 1\n",
              static_cast<long long>(slag_root_count()) -
                  static_cast<long long>(before));
      ++failures;
    }

    bool survived = false;
    {
      v8::HandleScope scope(isolate);
      if (slag_collect(context.Get(isolate)->slag()) != 0) {
        fprintf(stderr, "collection failed\n");
        ++failures;
      }
      v8::String::Utf8Value text(isolate, pinned->Get(isolate));
      survived = *text && strcmp(*text, "pinned") == 0;
    }
    if (!survived) {
      fprintf(stderr, "a Global did not survive the collection\n");
      ++failures;
    }

    pinned->Reset();
    if (!pinned->IsEmpty()) {
      fprintf(stderr, "Reset left the Global non-empty\n");
      ++failures;
    }
    if (slag_root_count() != before) {
      fprintf(stderr, "Reset retained %lld values, expected 0\n",
              static_cast<long long>(slag_root_count()) -
                  static_cast<long long>(before));
      ++failures;
    }
    delete pinned;
  }

  // A gin-shaped wrappable: a template with one internal field and a method,
  // an instance carrying a host pointer, and the pointer read back through
  // `this` after a collection has run.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    // The method below runs as a script callback, which asks the isolate for the
    // entered context the way gin does, so the harness enters one.
    v8::Context::Scope context_scope(current);

    v8::Local<v8::ObjectTemplate> tmpl = v8::ObjectTemplate::New(isolate);
    tmpl->SetInternalFieldCount(1);
    tmpl->Set(isolate, "bump", v8::FunctionTemplate::New(isolate, Bump));
    if (tmpl->InternalFieldCount() != 1) {
      fprintf(stderr, "the template forgot its internal field count\n");
      ++failures;
    }

    v8::Local<v8::Object> wrapper = tmpl->NewInstance(current).ToLocalChecked();
    if (wrapper->InternalFieldCount() != 1) {
      fprintf(stderr, "an instance has %d internal fields, expected 1\n",
              wrapper->InternalFieldCount());
      ++failures;
    }
    if (!wrapper->IsApiWrapper()) {
      fprintf(stderr, "an instance is not an API wrapper\n");
      ++failures;
    }
    if (wrapper->GetAlignedPointerFromInternalField(0) != nullptr) {
      fprintf(stderr, "a fresh internal field is not null\n");
      ++failures;
    }

    auto* counter = new HostCounter{5};
    wrapper->SetAlignedPointerInInternalField(
        0, counter, v8::CppHeapPointerTag::kEmbedderObjectWrapperTag);
    if (wrapper->Wrappable<HostCounter>() != counter) {
      fprintf(stderr, "Wrappable did not read the stored pointer\n");
      ++failures;
    }
    if (wrapper->GetAlignedPointerFromInternalField(current, 0) != counter ||
        wrapper->GetAlignedPointerFromInternalField(isolate, 0) != counter) {
      fprintf(stderr, "an internal field overload disagrees\n");
      ++failures;
    }

    // Past the end: V8 aborts, the facade reports the refusal and keeps the
    // object's real fields intact.
    wrapper->SetAlignedPointerInInternalField(3, counter);
    if (wrapper->GetAlignedPointerFromInternalField(3) != nullptr) {
      fprintf(stderr, "an out-of-range internal field was stored\n");
      ++failures;
    }

    if (slag_collect(current->slag()) != 0) {
      fprintf(stderr, "collection failed\n");
      ++failures;
    }
    if (wrapper->Wrappable<HostCounter>() != counter) {
      fprintf(stderr, "the wrappable did not survive the collection\n");
      ++failures;
    }

    // Reach the instance from JS so the method runs with `this`. Installing it
    // through `Object::Set`/`Get` also exercises the value-keyed property path.
    v8::Local<v8::Value> key =
        v8::String::NewFromUtf8Literal(isolate, "counter");
    v8::Local<v8::Object> global = current->Global();
    if (!global->Set(current, key, wrapper).FromMaybe(false)) {
      fprintf(stderr, "the wrapper could not be installed\n");
      ++failures;
    }
    v8::Local<v8::Value> installed;
    if (!global->Get(current, key).ToLocal(&installed) ||
        installed.raw() != wrapper.raw()) {
      fprintf(stderr, "Get did not return the wrapper Set stored\n");
      ++failures;
    }

    // A numeric key is the engine's ToPropertyKey, not the facade's.
    wrapper->Set(current, v8::Integer::New(isolate, 7),
                 v8::Integer::New(isolate, 3));
    v8::Local<v8::Value> by_index;
    if (!wrapper->Get(current, v8::String::NewFromUtf8Literal(isolate, "7"))
             .ToLocal(&by_index) ||
        by_index->Int32Value(current).FromMaybe(-1) != 3) {
      fprintf(stderr, "a numeric key did not reach the property it names\n");
      ++failures;
    }

    // The index forms are the key-value forms with the number built.
    if (!wrapper->Set(current, 0, v8::Integer::New(isolate, 5)).FromMaybe(
            false) ||
        wrapper->Get(current, 0).ToLocalChecked()->Int32Value(current)
                .FromMaybe(-1) != 5) {
      fprintf(stderr, "the index forms did not reach the element\n");
      ++failures;
    }

    // Has walks the chain: the own method, Object.prototype's, and a key that
    // is on neither.
    v8::Local<v8::String> mark_key =
        v8::String::NewFromUtf8Literal(isolate, "mark");
    if (!wrapper->Has(current, v8::String::NewFromUtf8Literal(isolate, "bump"))
             .FromMaybe(false) ||
        !wrapper->Has(current, v8::String::NewFromUtf8Literal(isolate,
                                                              "toString"))
             .FromMaybe(false) ||
        wrapper->Has(current, v8::String::NewFromUtf8Literal(isolate, "nope"))
            .FromMaybe(true) ||
        !wrapper->Has(current, 0).FromMaybe(false) ||
        wrapper->Has(current, 99).FromMaybe(true)) {
      fprintf(stderr, "Has answered the wrong way\n");
      ++failures;
    }

    // GetRealNamedProperty sees the own property and not the chain's.
    wrapper->Set(current, mark_key, v8::Integer::New(isolate, 3));
    v8::Local<v8::Value> own;
    if (!wrapper->GetRealNamedProperty(current, mark_key).ToLocal(&own) ||
        own->Int32Value(current).FromMaybe(-1) != 3) {
      fprintf(stderr, "GetRealNamedProperty missed the own property\n");
      ++failures;
    }
    if (!wrapper
             ->GetRealNamedProperty(
                 current, v8::String::NewFromUtf8Literal(isolate, "toString"))
             .IsEmpty()) {
      fprintf(stderr, "GetRealNamedProperty walked the chain\n");
      ++failures;
    }

    // The real-property predicates are own-only, and split named from indexed.
    if (!wrapper->HasRealNamedProperty(current, mark_key).FromMaybe(false) ||
        wrapper
            ->HasRealNamedProperty(
                current, v8::String::NewFromUtf8Literal(isolate, "toString"))
            .FromMaybe(true) ||
        wrapper->HasRealNamedProperty(
                   current, v8::String::NewFromUtf8Literal(isolate, "0"))
            .FromMaybe(true) ||
        !wrapper->HasRealIndexedProperty(current, 0).FromMaybe(false) ||
        wrapper->HasRealIndexedProperty(current, 1).FromMaybe(true) ||
        wrapper->HasRealNamedCallbackProperty(current, mark_key).FromMaybe(
            true)) {
      fprintf(stderr, "a real-property predicate answered the wrong way\n");
      ++failures;
    }

    // Delete removes the own property, and deleting an absent one is true
    // (there is nothing left to delete).
    if (!wrapper->Delete(current, mark_key) ||
        wrapper->Has(current, mark_key).FromMaybe(true) ||
        !wrapper->Delete(current, mark_key) || !wrapper->Delete(current, 1)) {
      fprintf(stderr, "Delete did not remove the property\n");
      ++failures;
    }

    v8::Local<v8::Value> result =
        v8::Script::Compile(current, v8::String::NewFromUtf8Literal(
                                         isolate, "counter.bump(10)"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocalChecked();
    if (result->Int32Value(current).FromMaybe(-1) != 15) {
      fprintf(stderr, "counter.bump(10) did not reach the wrappable\n");
      ++failures;
    }

    delete counter;
  }

  // A named-property handler on an instance template: the handler answers before
  // the engine's own lookup, and `kNo` runs the ordinary method.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    // A property callback runs with the host's context entered, which is how the
    // facade finds the context an allocation goes through.
    v8::Context::Scope context_scope(current);

    v8::NamedPropertyHandlerConfiguration configuration(
        HostGet, HostSet, HostQuery, HostDelete, HostEnumerate, HostDescriptor,
        HostDefiner, v8::Integer::New(isolate, 7));
    v8::Local<v8::ObjectTemplate> tmpl = v8::ObjectTemplate::New(isolate);
    tmpl->SetHandler(configuration);
    v8::Local<v8::Object> instance = tmpl->NewInstance(current).ToLocalChecked();

    v8::Local<v8::String> alpha =
        v8::String::NewFromUtf8Literal(isolate, "alpha");
    v8::Local<v8::String> beta =
        v8::String::NewFromUtf8Literal(isolate, "beta");

    // The getter answers `alpha`, and the query claims all three names.
    if (instance->Get(current, alpha)
            .ToLocalChecked()
            ->Int32Value(current)
            .FromMaybe(-1) != 42) {
      fprintf(stderr, "the handler getter did not answer\n");
      ++failures;
    }
    if (!instance->Has(current, beta).FromMaybe(false)) {
      fprintf(stderr, "the handler query did not claim a name\n");
      ++failures;
    }

    // `kNo` for a name the handler does not own: the engine's own property wins.
    v8::Local<v8::String> own =
        v8::String::NewFromUtf8Literal(isolate, "own");
    if (!instance->Set(current, own, v8::Integer::New(isolate, 9))
             .FromMaybe(false) ||
        instance->Get(current, own)
                .ToLocalChecked()
                ->Int32Value(current)
                .FromMaybe(-1) != 9) {
      fprintf(stderr, "a declined name did not reach the own property\n");
      ++failures;
    }

    // A set and a delete the handler claims are its own.
    if (!instance->Set(current, beta, v8::Integer::New(isolate, 1))
             .FromMaybe(false) ||
        !instance->Delete(current, beta) || host_names.setters == 0 ||
        host_names.deleters == 0) {
      fprintf(stderr, "the handler did not take the set or the delete\n");
      ++failures;
    }

    // The same answers through script: a read and the `in` operator.
    const char kHandled[] = "handled";
    if (slag_object_set(
            slag_global(current->slag()),
            reinterpret_cast<const uint8_t*>(kHandled), sizeof(kHandled) - 1,
            instance.raw()) != 0) {
      fprintf(stderr, "the handler instance could not be installed\n");
      ++failures;
    }
    v8::Local<v8::Value> from_script =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "handled.alpha + (('beta' in handled) ? 1 : 0)"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocalChecked();
    if (from_script->Int32Value(current).FromMaybe(-1) != 43) {
      fprintf(stderr, "script did not reach the handler (got %d)\n",
              from_script->Int32Value(current).FromMaybe(-1));
      ++failures;
    }
    if (host_names.data_misses != 0) {
      ++failures;
    }

    // The handler's own descriptor answers `Object.getOwnPropertyDescriptor`, and
    // a define reaches the definer with the descriptor marshalled back into C++.
    v8::Local<v8::Value> described;
    bool read_descriptor =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "Object.getOwnPropertyDescriptor(handled, 'alpha').value"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&described) &&
        described->Int32Value(current).FromMaybe(-1) == 42;
    if (!read_descriptor) {
      fprintf(stderr, "the handler's descriptor did not answer\n");
      ++failures;
    }
    v8::Local<v8::Value> define_result;
    bool defined =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "Object.defineProperty(handled, 'gamma', { value: 5, writable: true }); 1"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&define_result) &&
        define_result->Int32Value(current).FromMaybe(-1) == 1;
    if (!defined) {
      fprintf(stderr, "the define did not reach the handler's definer\n");
      ++failures;
    }
    if (host_names.descriptors == 0 || host_names.definers == 0 ||
        host_names.definer_descriptors == 0) {
      fprintf(stderr, "the descriptor or definer callback was never asked\n");
      ++failures;
    }

    // The getter and the descriptor answer with values built inside the callback.
    // A fresh object each read is the proof that allocation ran.
    v8::Local<v8::Value> allocated;
    bool built =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "(handled.fresh.value === 11 &&"
                " handled.fresh !== handled.fresh &&"
                " handled.list.length === 2 && handled.list[0] === 'a') ? 1 : 0"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&allocated) &&
        allocated->Int32Value(current).FromMaybe(-1) == 1;
    if (!built || host_names.allocations == 0) {
      fprintf(stderr, "a callback did not allocate the value it answered\n");
      ++failures;
    }

    // `Object.keys` walks the enumerator's keys and reads each one's own
    // descriptor, so it exercises both callbacks together.
    v8::Local<v8::Value> keys;
    bool enumerated =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate, "Object.keys(handled).join(',') === 'alpha,beta,gamma' ? 1 : 0"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&keys) &&
        keys->Int32Value(current).FromMaybe(-1) == 1;
    if (!enumerated || host_names.enumerators == 0) {
      fprintf(stderr, "the enumerator's keys did not reach Object.keys\n");
      ++failures;
    }

    // The indexed handler owns elements 0..2; a higher index falls back.
    v8::IndexedPropertyHandlerConfiguration indexed(HostGetIndex, HostSetIndex,
                                                   HostQueryIndex,
                                                   HostDeleteIndex,
                                                   HostEnumerateIndex);
    v8::Local<v8::ObjectTemplate> elements = v8::ObjectTemplate::New(isolate);
    elements->SetHandler(indexed);
    v8::Local<v8::Object> array_like =
        elements->NewInstance(current).ToLocalChecked();
    if (array_like->Get(current, 1).ToLocalChecked()->Int32Value(current).FromMaybe(
            -1) != 10 ||
        !array_like->Has(current, 2).FromMaybe(false) ||
        array_like->Has(current, 3).FromMaybe(true) ||
        !array_like->Set(current, 0, v8::Integer::New(isolate, 1))
             .FromMaybe(false) ||
        !array_like->Delete(current, 0) ||
        array_like->Get(current, 3).ToLocalChecked()->IsUndefined() == false) {
      fprintf(stderr, "an indexed handler answered the wrong way\n");
      ++failures;
    }
    if (host_names.indexed_getters == 0 || host_names.indexed_setters == 0 ||
        host_names.indexed_deleters == 0) {
      fprintf(stderr, "an indexed callback was never asked\n");
      ++failures;
    }

    // `Reflect.ownKeys` asks only for the keys, so it reaches the indexed
    // enumerator without needing a descriptor callback.
    const char kArrayLike[] = "array_like";
    if (slag_object_set(slag_global(current->slag()),
                       reinterpret_cast<const uint8_t*>(kArrayLike),
                       sizeof(kArrayLike) - 1, array_like.raw()) != 0) {
      fprintf(stderr, "the indexed instance could not be installed\n");
      ++failures;
    }
    v8::Local<v8::Value> indexed_keys;
    bool indexed_enumerated =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "Reflect.ownKeys(array_like).join(',') === '0,1,2' ? 1 : 0"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&indexed_keys) &&
        indexed_keys->Int32Value(current).FromMaybe(-1) == 1;
    if (!indexed_enumerated || host_names.indexed_enumerators == 0) {
      fprintf(stderr, "the indexed enumerator's keys did not reach the engine\n");
      ++failures;
    }
  }

  // An accessor installed on a template: a read runs the getter (which allocates),
  // a write runs the setter, and the accessor's data arrives on the info.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);

    v8::Local<v8::ObjectTemplate> tmpl = v8::ObjectTemplate::New(isolate);
    tmpl->SetAccessor(v8::String::NewFromUtf8Literal(isolate, "lazy"),
                      HostAccessGet, HostAccessSet,
                      v8::Integer::New(isolate, 3));
    v8::Local<v8::Object> object = tmpl->NewInstance(current).ToLocalChecked();
    object->Set(current, v8::String::NewFromUtf8Literal(isolate, "tag"),
                v8::Integer::New(isolate, 7));
    object->Set(current, v8::String::NewFromUtf8Literal(isolate, "scratch"),
                v8::Integer::New(isolate, 1));

    const char kAccessorObject[] = "accessor_obj";
    if (slag_object_set(slag_global(current->slag()),
                        reinterpret_cast<const uint8_t*>(kAccessorObject),
                        sizeof(kAccessorObject) - 1, object.raw()) != 0) {
      fprintf(stderr, "the accessor instance could not be installed\n");
      ++failures;
    }

    v8::Local<v8::Value> read;
    bool accessor_read =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "(accessor_obj.lazy.name === 'lazy' &&"
                " accessor_obj.lazy.data === 3 &&"
                " accessor_obj.lazy.tag === 7 &&"
                " Object.keys(accessor_obj).indexOf('lazy') >= 0) ? 1 : 0"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&read) &&
        read->Int32Value(current).FromMaybe(-1) == 1;
    if (!accessor_read || host_accessor.gets == 0 || host_accessor.reads == 0 ||
        host_accessor.hits == 0 || host_accessor.real_reads == 0 ||
        host_accessor.real_hits == 0) {
      fprintf(stderr, "the accessor getter did not answer\n");
      ++failures;
    }

    v8::Local<v8::Value> wrote;
    bool accessor_write =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(isolate, "accessor_obj.lazy = 9; 1"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocal(&wrote) &&
        wrote->Int32Value(current).FromMaybe(-1) == 1;
    if (!accessor_write || host_accessor.sets == 0 || host_accessor.last != 9) {
      fprintf(stderr, "the accessor setter did not take the write\n");
      ++failures;
    }
    if (host_accessor.deleted == 0) {
      fprintf(stderr, "the accessor setter could not delete through the window\n");
      ++failures;
    }

    // The object form installs the same accessor on an object that already
    // exists, and reads it back through C++.
    v8::Local<v8::Object> plain =
        v8::ObjectTemplate::New(isolate)->NewInstance(current).ToLocalChecked();
    plain->SetAccessor(current,
                       v8::String::NewFromUtf8Literal(isolate, "lazy"),
                       HostAccessGet, nullptr, v8::Integer::New(isolate, 5));
    v8::Local<v8::Value> direct =
        plain->Get(current, v8::String::NewFromUtf8Literal(isolate, "lazy"))
            .ToLocalChecked();
    v8::Local<v8::Value> direct_data =
        direct.As<v8::Object>()
            ->Get(current, v8::String::NewFromUtf8Literal(isolate, "data"))
            .ToLocalChecked();
    if (direct_data->Int32Value(current).FromMaybe(-1) != 5) {
      fprintf(stderr, "Object::SetAccessor did not answer with its data\n");
      ++failures;
    }
  }

  // Identity hashes and the thrown value: a host keys a table by the hash, and
  // TryCatch hands back the value a script threw.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);

    v8::Local<v8::Object> one =
        v8::ObjectTemplate::New(isolate)->NewInstance(current).ToLocalChecked();
    // The hash is stable and never zero, and neighbouring objects do not all
    // share one answer: a hash may collide, but a table must not be linear.
    const int one_hash = one->GetIdentityHash();
    int hashes[8] = {};
    for (int i = 0; i < 8; ++i) {
      hashes[i] = v8::ObjectTemplate::New(isolate)
                      ->NewInstance(current)
                      .ToLocalChecked()
                      ->GetIdentityHash();
    }
    int distinct = 0;
    for (int i = 0; i < 8; ++i) {
      bool first = hashes[i] != 0;
      for (int j = 0; j < i; ++j) {
        if (hashes[j] == hashes[i]) first = false;
      }
      distinct += first ? 1 : 0;
    }
    if (one_hash == 0 || one_hash != one->GetIdentityHash()) {
      fprintf(stderr, "the identity hash is zero or not stable\n");
      ++failures;
    }
    if (distinct < 4) {
      fprintf(stderr, "the identity hash does not vary between objects\n");
      ++failures;
    }

    // A clean run leaves nothing pending.
    {
      v8::TryCatch try_catch(isolate);
      v8::Script::Compile(current,
                          v8::String::NewFromUtf8Literal(isolate, "1 + 1"))
          .ToLocalChecked()
          ->Run(current);
      if (try_catch.HasCaught()) {
        fprintf(stderr, "a clean run was reported as caught\n");
        ++failures;
      }
    }

    // A throw carries its value: the object the script threw, not a message.
    v8::TryCatch try_catch(isolate);
    v8::Script::Compile(
        current,
        v8::String::NewFromUtf8Literal(isolate, "throw { code: 99 }"))
        .ToLocalChecked()
        ->Run(current);
    if (!try_catch.HasCaught()) {
      fprintf(stderr, "the throw was not caught\n");
      ++failures;
    }
    v8::Local<v8::Value> exception = try_catch.Exception();
    if (exception.IsEmpty() ||
        exception.As<v8::Object>()
                ->Get(current, v8::String::NewFromUtf8Literal(isolate, "code"))
                .ToLocalChecked()
                ->Int32Value(current)
                .FromMaybe(-1) != 99) {
      fprintf(stderr, "the thrown value did not come back\n");
      ++failures;
    }

    // A thrown Error carries the stack the engine captured, and StackTrace hands
    // it back as a string.
    {
      v8::TryCatch error_catch(isolate);
      v8::Script::Compile(
          current,
          v8::String::NewFromUtf8Literal(isolate, "throw new Error('boom')"))
          .ToLocalChecked()
          ->Run(current);
      v8::Local<v8::Value> stack;
      bool has_stack =
          error_catch.HasCaught() &&
          error_catch.StackTrace(current).ToLocal(&stack) &&
          stack->IsString();
      if (has_stack) {
        v8::String::Utf8Value text(isolate, stack);
        has_stack = *text != nullptr && strstr(*text, "boom") != nullptr;
      }
      if (!has_stack) {
        fprintf(stderr, "the thrown Error's stack did not come back\n");
        ++failures;
      }
    }

    // A compile error records a position, so Message answers the script's name,
    // the line, and the line's text.
    {
      v8::TryCatch compile_catch(isolate);
      v8::Local<v8::Value> name =
          v8::String::NewFromUtf8Literal(isolate, "broken.js");
      v8::ScriptOrigin origin(name);
      v8::Local<v8::Script> script;
      const bool compiled =
          v8::Script::Compile(
              current, v8::String::NewFromUtf8Literal(isolate, "let = ;"),
              &origin)
              .ToLocal(&script);
      if (compiled || !compile_catch.HasCaught()) {
        fprintf(stderr, "the syntax error compiled or was not caught\n");
        ++failures;
      }
      v8::Local<v8::Message> message = compile_catch.Message();
      if (message.IsEmpty()) {
        fprintf(stderr, "the compile error had no message\n");
        ++failures;
      } else {
        v8::String::Utf8Value filename(
            isolate, message->GetScriptOrigin().ResourceName());
        v8::String::Utf8Value source_line(
            isolate, message->GetSourceLine(current).ToLocalChecked());
        const int line = message->GetLineNumber(current).FromJust();
        const int start = message->GetStartColumn(current).FromJust();
        const int end = message->GetEndColumn(current).FromJust();
        if (*filename == nullptr || strcmp(*filename, "broken.js") != 0 ||
            line != 1 || *source_line == nullptr ||
            strcmp(*source_line, "let = ;") != 0 || start < 0 || end < start) {
          fprintf(stderr, "the compile error's message was wrong\n");
          ++failures;
        }
      }
    }
  }

  // Microtasks: the default policy drains a run's own jobs, and `kExplicit`
  // leaves them for the checkpoint or the platform's message pump.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    std::unique_ptr<v8::Platform> platform = v8::platform::NewDefaultPlatform();
    const auto read = [&](const char* name) {
      return current->Global()
          ->Get(current, v8::String::NewFromUtf8Literal(isolate, name))
          .ToLocalChecked()
          ->Int32Value(current)
          .FromMaybe(-1);
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };

    // `kAuto` (the default): the promise reaction has run when Run returns.
    run(
        "globalThis.settled = 0;"
        " Promise.resolve(1).then(function () { globalThis.settled = 1; });");
    if (read("settled") != 1) {
      fprintf(stderr, "the default policy did not drain the run's microtasks\n");
      ++failures;
    }

    // `kExplicit`: nothing runs until the host pumps or checkpoints.
    isolate->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);
    if (isolate->GetMicrotasksPolicy() != v8::MicrotasksPolicy::kExplicit) {
      fprintf(stderr, "the microtasks policy did not stick\n");
      ++failures;
    }
    run(
        "globalThis.pumped = 0;"
        " Promise.resolve(1).then(function () { globalThis.pumped = 1; });");
    if (read("pumped") != 0) {
      fprintf(stderr, "kExplicit drained without being asked\n");
      ++failures;
    }
    if (!v8::platform::PumpMessageLoop(platform.get(), isolate) ||
        read("pumped") != 1) {
      fprintf(stderr, "the message pump did not run the queued job\n");
      ++failures;
    }
    if (v8::platform::PumpMessageLoop(platform.get(), isolate)) {
      fprintf(stderr, "a second pump found work that was not there\n");
      ++failures;
    }

    // The explicit checkpoint drains the same way.
    run(
        "globalThis.checked = 0;"
        " Promise.resolve(1).then(function () { globalThis.checked = 1; });");
    isolate->PerformMicrotaskCheckpoint();
    if (read("checked") != 1) {
      fprintf(stderr, "the checkpoint did not run the queued job\n");
      ++failures;
    }
    isolate->SetMicrotasksPolicy(v8::MicrotasksPolicy::kAuto);
  }

  // Value predicates and conversions: a host that inspects a value the engine
  // returned asks the value, rather than guessing the type from its bits.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "value predicates: %s\n", what);
        ++failures;
      }
    };
    const auto render = [&](v8::Local<v8::String> string) {
      v8::String::Utf8Value utf8(isolate, string);
      return *utf8 == nullptr ? std::string() : std::string(*utf8);
    };

    // The shape bits, which need no agent.
    v8::Local<v8::Value> object = run("({ a: 1 })");
    expect(object->IsObject() && !object->IsArray() &&
               !object->IsFunction() && !object->IsString(),
           "a plain object's type bits");
    v8::Local<v8::Value> array = run("[1, 2]");
    expect(array->IsArray() && array->IsObject() && !array->IsFunction(),
           "an array's type bits");

    v8::Local<v8::Value> called = run("(function named() {})");
    expect(called->IsFunction() && called->IsObject() &&
               called.As<v8::Object>()->IsCallable() &&
               called.As<v8::Object>()->IsConstructor(),
           "a function is a callable, constructible object");
    v8::Local<v8::Value> arrow = run("() => 1");
    expect(arrow->IsFunction() && arrow.As<v8::Object>()->IsCallable() &&
               !arrow.As<v8::Object>()->IsConstructor(),
           "an arrow is callable but not a constructor");

    expect(run("7")->IsNumber() && run("7")->IsInt32() &&
               run("7")->IsUint32(),
           "a small integer is a number, an int32, and a uint32");
    expect(run("-1")->IsInt32() && !run("-1")->IsUint32(),
           "a negative integer is not a uint32");
    expect(run("3.5")->IsNumber() && !run("3.5")->IsInt32(),
           "a fraction is not an int32");
    expect(run("'abc'")->IsString() && !run("'abc'")->IsNumber(),
           "a string's type bits");
    expect(run("Symbol('s')")->IsSymbol() &&
               !run("Symbol('s')")->IsString(),
           "a symbol's type bits");
    expect(run("10n")->IsBigInt(), "a bigint's type bits");
    expect(run("undefined")->IsUndefined() &&
               run("undefined")->IsNullOrUndefined() &&
               !run("undefined")->IsNull(),
           "undefined's type bits");
    expect(run("null")->IsNull() && run("null")->IsNullOrUndefined() &&
               !run("null")->IsUndefined() && !run("null")->IsObject(),
           "null's type bits");

    // The brands, which the agent records per object and so need the context.
    expect(run("new Date(0)")->IsDate(), "a Date's brand");
    expect(run("/x/")->IsRegExp(), "a RegExp's brand");
    expect(run("Promise.resolve(1)")->IsPromise(), "a Promise's brand");
    expect(run("new Map()")->IsMap(), "a Map's brand");
    expect(run("new Set()")->IsSet(), "a Set's brand");
    expect(run("new Error('x')")->IsNativeError(), "an Error's brand");
    expect(run("new Uint8Array(2)")->IsTypedArray() &&
               !run("new Uint8Array(2)")->IsArray(),
           "a typed array's brand");
    expect(run("new ArrayBuffer(2)")->IsArrayBuffer(),
           "an ArrayBuffer's brand");
    expect(run("new SharedArrayBuffer(2)")->IsSharedArrayBuffer() &&
               !run("new SharedArrayBuffer(2)")->IsArrayBuffer(),
           "a SharedArrayBuffer's brand");
    expect(run("new DataView(new ArrayBuffer(2))")->IsDataView(),
           "a DataView's brand");
    expect(run("new Proxy({}, {})")->IsProxy(), "a Proxy's brand");
    expect(run("(function () { return arguments; })()")->IsArgumentsObject(),
           "an arguments object's brand");
    expect(!object->IsDate() && !object->IsMap() && !object->IsRegExp(),
           "a plain object has no brand");

    // StrictEquals and SameValue differ exactly where the spec says: NaN and
    // the two zeroes.
    v8::Local<v8::Value> nan = run("NaN");
    v8::Local<v8::Value> zero = run("0");
    v8::Local<v8::Value> negative_zero = run("-0");
    expect(!nan->StrictEquals(nan), "NaN is not strictly equal to itself");
    expect(nan->SameValue(nan), "NaN is SameValue to itself");
    expect(zero->StrictEquals(negative_zero),
           "+0 is strictly equal to -0");
    expect(!zero->SameValue(negative_zero), "+0 is not SameValue to -0");
    expect(!run("1")->StrictEquals(run("'1'")),
           "a number and its string are not strictly equal");
    expect(array->StrictEquals(array) && !array->StrictEquals(run("[1, 2]")),
           "objects compare by identity");

    // ToBoolean and typeof.
    expect(!run("''")->BooleanValue(isolate) &&
               !run("0")->BooleanValue(isolate) &&
               run("({})")->BooleanValue(isolate),
           "BooleanValue follows ToBoolean");
    expect(render(run("'x'")->TypeOf(current).ToLocalChecked()) == "string" &&
               render(run("() => 1")->TypeOf(current).ToLocalChecked()) ==
                   "function" &&
               render(run("null")->TypeOf(current).ToLocalChecked()) ==
                   "object",
           "TypeOf answers typeof");

    // The coercions run host code: a custom toString/valueOf takes part.
    v8::Local<v8::Value> described = run(
        "({ toString: function () { return 'custom'; } })");
    expect(render(described->ToString(current).ToLocalChecked()) == "custom",
           "ToString ran the object's toString");
    expect(run("({ valueOf: function () { return 7; } })")
                   ->ToNumber(current)
                   .FromMaybe(-1) == 7,
           "ToNumber ran the object's valueOf");
    v8::Local<v8::Value> boxed =
        run("'abc'")->ToObject(current).ToLocalChecked();
    expect(boxed->IsObject() && !boxed->IsString(),
           "ToObject boxes a primitive");
  }

  // Strings and the integer conversions: the Latin-1 and UTF-16 constructors,
  // the three write forms, and the ToInt32/ToUint32/ToInteger wrappers.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "strings: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };

    // NewFromOneByte takes Latin-1: 0xE9 is U+00E9, not the first byte of a
    // UTF-8 sequence.
    const uint8_t latin1[] = {'a', 'b', 0xE9};
    v8::Local<v8::String> one_byte =
        v8::String::NewFromOneByte(isolate, latin1, v8::NewStringType::kNormal,
                                   3)
            .ToLocalChecked();
    expect(one_byte->Length() == 3, "a Latin-1 string is three code units");
    expect(one_byte->ContainsOnlyOneByte(), "a Latin-1 string is one-byte");
    expect(one_byte->Utf8Length(isolate) == 4, "0xE9 takes two UTF-8 bytes");

    // Length is a code unit count, which for an astral character is two, and
    // its UTF-8 encoding is four bytes.
    expect(run("'\\u{1F600}'").As<v8::String>()->Length() == 2,
           "Length counts code units rather than bytes");
    expect(run("'\\u{1F600}'").As<v8::String>()->Utf8Length(isolate) == 4,
           "an astral character is four UTF-8 bytes");

    uint16_t units[8] = {};
    expect(one_byte->Write(units) == 3 && units[0] == 'a' && units[1] == 'b' &&
               units[2] == 0xE9,
           "Write copied the code units");
    expect(units[3] == 0, "Write NUL-terminated the buffer");

    uint16_t window[8] = {};
    expect(one_byte->Write(window, 1, 1) == 1 && window[0] == 'b' &&
               window[1] == 0,
           "Write honoured the start and length window");

    uint16_t tail[8] = {};
    expect(one_byte->Write(tail, -1) == 1 && tail[0] == 0xE9,
           "a negative start counts from the end");

    uint16_t unterminated[8];
    for (uint16_t& unit : unterminated) unit = 0xFFFF;
    expect(one_byte->Write(unterminated, 0, -1, v8::NO_NULL_TERMINATION) == 3 &&
               unterminated[3] == 0xFFFF,
           "NO_NULL_TERMINATION left the buffer unterminated");

    uint8_t bytes[8] = {};
    expect(one_byte->WriteOneByte(bytes) == 3 && bytes[2] == 0xE9 &&
               bytes[3] == 0,
           "WriteOneByte narrowed the code units");

    // A surrogate pair is two code units and one four-byte UTF-8 character.
    const uint16_t pair[] = {0xD83D, 0xDE00};
    v8::Local<v8::String> astral =
        v8::String::NewFromTwoByte(isolate, pair, v8::NewStringType::kNormal, 2)
            .ToLocalChecked();
    expect(astral->Length() == 2 && !astral->ContainsOnlyOneByte(),
           "a surrogate pair is two code units and not one-byte");
    char utf8[16] = {};
    int covered = 0;
    expect(astral->WriteUtf8(isolate, utf8, -1, &covered) == 5 && covered == 2,
           "WriteUtf8 wrote four bytes and a terminator for two code units");
    expect(static_cast<unsigned char>(utf8[0]) == 0xF0 &&
               static_cast<unsigned char>(utf8[2]) == 0x98 &&
               static_cast<unsigned char>(utf8[3]) == 0x80 && utf8[4] == '\0',
           "WriteUtf8 encoded the pair as one four-byte character");

    // A lone surrogate has no encoding of its own, so the default write is its
    // own three bytes and only REPLACE_INVALID_UTF8 substitutes U+FFFD.
    const uint16_t lone[] = {0xD800, 'A'};
    v8::Local<v8::String> surrogate =
        v8::String::NewFromTwoByte(isolate, lone, v8::NewStringType::kNormal, 2)
            .ToLocalChecked();
    expect(surrogate->Utf8Length(isolate) == 4,
           "a lone surrogate counts three UTF-8 bytes");
    char lone_utf8[16] = {};
    int lone_covered = 0;
    expect(surrogate->WriteUtf8(isolate, lone_utf8, -1, &lone_covered) == 5 &&
               lone_covered == 2 &&
               static_cast<unsigned char>(lone_utf8[0]) == 0xED &&
               static_cast<unsigned char>(lone_utf8[1]) == 0xA0 &&
               static_cast<unsigned char>(lone_utf8[2]) == 0x80 &&
               lone_utf8[3] == 'A',
           "WriteUtf8 wrote the lone surrogate as its own three bytes");

    char replaced[16] = {};
    expect(surrogate->WriteUtf8(isolate, replaced, -1, nullptr,
                               v8::REPLACE_INVALID_UTF8) == 5 &&
               static_cast<unsigned char>(replaced[0]) == 0xEF &&
               static_cast<unsigned char>(replaced[2]) == 0xBD,
           "REPLACE_INVALID_UTF8 substituted U+FFFD");

    // A buffer that cannot hold the whole character gets nothing of it, only
    // the terminator.
    char limited[4];
    for (char& byte : limited) byte = 0x7F;
    expect(astral->WriteUtf8(isolate, limited, 3) == 1 && limited[0] == '\0' &&
               limited[1] == 0x7F,
           "a buffer too small for the character was left alone");

    // The integer conversions convert with ToNumber first, so an object's own
    // valueOf takes part, and then wrap the way ToInt32/ToUint32 do.
    expect(run("4294967297")->Int32Value(current).FromMaybe(-99) == 1,
           "Int32Value wrapped past 2^32");
    expect(run("-1.9")->Int32Value(current).FromMaybe(-99) == -1,
           "Int32Value truncated toward zero");
    expect(run("'-1'")->Int32Value(current).FromMaybe(-99) == -1,
           "Int32Value ran ToNumber on a string");
    expect(run("({ valueOf: function () { return 7.9; } })")
                   ->Int32Value(current)
                   .FromMaybe(-99) == 7,
           "Int32Value ran the object's valueOf");
    expect(run("-1")->Uint32Value(current).FromMaybe(0) == 4294967295u,
           "Uint32Value wrapped a negative number");
    expect(run("2.9")->IntegerValue(current).FromMaybe(-99) == 2,
           "IntegerValue truncated");
    expect(run("-2.9")->IntegerValue(current).FromMaybe(-99) == -2,
           "IntegerValue truncated toward zero for a negative number");
    expect(run("Infinity")->IntegerValue(current).FromMaybe(0) ==
               9223372036854775807LL,
           "IntegerValue saturated at the int64 end");
    {
      v8::TryCatch throwing(isolate);
      expect(run("({ valueOf: function () { throw new RangeError('no'); } })")
                 ->IntegerValue(current)
                 .IsNothing(),
             "a throwing valueOf left the conversion empty");
      expect(throwing.HasCaught(), "the throw reached the host");
    }
  }

  // Promises: the resolver pair, the derived promise, and the state and result a
  // host reads.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "promises: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    v8::Local<v8::Promise::Resolver> resolver =
        v8::Promise::Resolver::New(current).ToLocalChecked();
    v8::Local<v8::Promise> promise = resolver->GetPromise();
    expect(!promise.IsEmpty() && promise->State() == v8::Promise::kPending,
           "a fresh resolver's promise is pending");
    expect(promise->Result()->IsUndefined(),
           "a pending promise's result is undefined");

    expect(resolver->Resolve(current, v8::Integer::New(isolate, 7))
               .FromMaybe(false),
           "Resolve settled the promise");
    expect(promise->State() == v8::Promise::kFulfilled,
           "the promise is fulfilled");
    expect(promise->Result()->Int32Value(current).FromMaybe(-1) == 7,
           "Result answered the settled value");

    // The pair shares one flag, so neither side settles it twice, and V8's
    // answer says which call did the work.
    expect(!resolver->Resolve(current, v8::Integer::New(isolate, 8))
                .FromMaybe(true),
           "a second Resolve did not settle it again");
    expect(!resolver->Reject(current, v8::String::NewFromUtf8Literal(isolate, "no"))
                .FromMaybe(true),
           "Reject after Resolve did not settle it either");
    expect(promise->Result()->Int32Value(current).FromMaybe(-1) == 7,
           "the first settlement is the one that stands");

    // A rejection keeps its reason, and marking it handled is what a host does
    // with a rejection it is not reacting to.
    v8::Local<v8::Promise::Resolver> rejecter =
        v8::Promise::Resolver::New(current).ToLocalChecked();
    expect(rejecter->Reject(current, v8::String::NewFromUtf8Literal(isolate, "no"))
               .FromMaybe(false),
           "Reject settled the promise");
    v8::Local<v8::Promise> rejected_promise = rejecter->GetPromise();
    expect(rejected_promise->State() == v8::Promise::kRejected,
           "the promise is rejected");
    expect(!rejected_promise->MarkAsHandled().IsNothing(),
           "MarkAsHandled answered");

    // Then2 derives a promise through the engine's own PerformPromiseThen.
    v8::Local<v8::Promise::Resolver> source =
        v8::Promise::Resolver::New(current).ToLocalChecked();
    v8::Local<v8::Function> on_fulfilled =
        run("(function (value) { globalThis.promiseSaw = value; return value + 1; })")
            .As<v8::Function>();
    v8::Local<v8::Promise> derived =
        source->GetPromise()
            ->Then2(current, on_fulfilled, v8::Local<v8::Function>())
            .ToLocalChecked();
    expect(source->Resolve(current, v8::Integer::New(isolate, 41))
               .FromMaybe(false),
           "the source resolved");
    isolate->PerformMicrotaskCheckpoint();
    expect(number_at("promiseSaw") == 41, "the host handler ran");
    expect(derived->State() == v8::Promise::kFulfilled,
           "the derived promise fulfilled");
    expect(derived->Result()->Int32Value(current).FromMaybe(-1) == 42,
           "the derived promise carries the handler's answer");

    // Catch is the same call with the fulfilling side left to the default.
    v8::Local<v8::Function> on_rejected =
        run("(function (reason) { return 5; })").As<v8::Function>();
    v8::Local<v8::Promise> caught =
        rejected_promise->Catch(current, on_rejected).ToLocalChecked();
    isolate->PerformMicrotaskCheckpoint();
    expect(caught->State() == v8::Promise::kFulfilled,
           "Catch settled the derived promise");
    expect(caught->Result()->Int32Value(current).FromMaybe(-1) == 5,
           "the rejection handler ran");

    // The statics: Resolve answers the promise it is given when it is one of
    // this realm's, and makes a new settled promise otherwise.
    v8::Local<v8::Promise> same =
        v8::Promise::Resolve(current, promise).ToLocalChecked();
    expect(same.raw() == promise.raw(),
           "Resolve answered the promise it was given");
    v8::Local<v8::Promise> fresh =
        v8::Promise::Resolve(current, v8::Integer::New(isolate, 3))
            .ToLocalChecked();
    expect(fresh->State() == v8::Promise::kFulfilled &&
               fresh->Result()->Int32Value(current).FromMaybe(-1) == 3,
           "Resolve fulfilled a new promise");
    v8::Local<v8::Promise> failed =
        v8::Promise::Reject(current,
                            v8::String::NewFromUtf8Literal(isolate, "no"))
            .ToLocalChecked();
    expect(failed->State() == v8::Promise::kRejected &&
               !failed->MarkAsHandled().IsNothing(),
           "Reject rejected a new promise");

    // A host function that builds and settles a capability inside the call, which
    // is what a promise-returning accessor does.
    v8::Local<v8::ObjectTemplate> tmpl = v8::ObjectTemplate::New(isolate);
    tmpl->Set(isolate, "makePromise",
              v8::FunctionTemplate::New(isolate, MakePromise));
    v8::Local<v8::Object> host = tmpl->NewInstance(current).ToLocalChecked();
    BindGlobal(isolate, current, "host", host, &failures);
    run("host.makePromise(99).then(function (value) {"
        " globalThis.callbackPromise = value; });");
    isolate->PerformMicrotaskCheckpoint();
    expect(number_at("callbackPromise") == 99,
           "a callback made and settled a promise");
  }

  // Maps and Sets: the internal-slot operations a host calls, and the script's
  // own view of the same object.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "containers: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    v8::Local<v8::Map> map = v8::Map::New(current);
    expect(!map.IsEmpty() && map->Size(current) == 0, "a fresh map is empty");

    v8::Local<v8::Value> key = v8::String::NewFromUtf8Literal(isolate, "a");
    expect(!map->Set(current, key, v8::Integer::New(isolate, 1)).IsEmpty(),
           "Set answered the map");
    map->Set(current, v8::Integer::New(isolate, 2), v8::Integer::New(isolate, 4));
    expect(map->Size(current) == 2, "the map holds two entries");
    expect(map->Get(current, key)
                   .ToLocalChecked()
                   ->Int32Value(current)
                   .FromMaybe(-1) == 1,
           "Get answered the stored value");
    expect(map->Get(current, v8::String::NewFromUtf8Literal(isolate, "none"))
               .ToLocalChecked()
               ->IsUndefined(),
           "a missing key answers undefined");
    expect(map->Has(current, key).FromMaybe(false), "Has sees the key");
    expect(!map->Has(current, v8::String::NewFromUtf8Literal(isolate, "none"))
                .FromMaybe(true),
           "Has does not see a missing key");

    // A host map is an ordinary JS Map, so a script reads the same entries.
    bind("hostMap", map);
    run("globalThis.mapSize = hostMap.size;"
        " globalThis.mapValue = hostMap.get('a');");
    expect(number_at("mapSize") == 2 && number_at("mapValue") == 1,
           "a script reads the host's map");

    // AsArray is the flat key-value form.
    v8::Local<v8::Array> pairs = map->AsArray(current).ToLocalChecked();
    v8::Local<v8::Value> flat;
    expect(pairs->Get(current, 1).ToLocal(&flat) &&
               flat->Int32Value(current).FromMaybe(-1) == 1,
           "AsArray flattened the entries");

    expect(map->Delete(current, key).FromMaybe(false), "Delete removed the key");
    expect(map->Size(current) == 1, "the map shrank");
    map->Clear();
    expect(map->Size(current) == 0, "Clear emptied the map");

    // A Set is the same surface with add instead of set.
    v8::Local<v8::Set> set = v8::Set::New(current);
    expect(!set.IsEmpty() && set->Size(current) == 0, "a fresh set is empty");
    expect(!set->Add(current, v8::Integer::New(isolate, 3)).IsEmpty(),
           "Add answered the set");
    set->Add(current, v8::String::NewFromUtf8Literal(isolate, "x"));
    expect(set->Size(current) == 2, "the set holds two elements");
    expect(set->Has(current, v8::Integer::New(isolate, 3)).FromMaybe(false),
           "Has sees the element");
    bind("hostSet", set);
    run("globalThis.setSize = hostSet.size;"
        " globalThis.setHas = hostSet.has('x');");
    expect(number_at("setSize") == 2 && number_at("setHas") == 1,
           "a script reads the host's set");

    v8::Local<v8::Array> elements = set->AsArray(current).ToLocalChecked();
    expect(elements->Get(current, 1).ToLocal(&flat),
           "AsArray answered an element");
    v8::String::Utf8Value listed(isolate, flat);
    expect(*listed != nullptr && strcmp(*listed, "x") == 0,
           "AsArray listed the elements");
    expect(set->Delete(current, v8::Integer::New(isolate, 3)).FromMaybe(false),
           "Delete removed the element");
    set->Clear();
    expect(set->Size(current) == 0, "Clear emptied the set");

    // A handle that is not a map answers -1 rather than guessing.
    v8::Local<v8::Map> not_a_map = v8::Object::New(isolate).As<v8::Map>();
    expect(not_a_map->Size(current) == -1, "Size refuses a non-map");
    slag_clear_error(current->slag());
  }

  // Errors, private names, and the data-property forms: the values a host
  // throws, the keys it hides, and the accessors a template installs.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "properties: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    // The error factories make the engine's own objects, and ThrowException
    // hands exactly that value to the host.
    {
      v8::TryCatch type_catch(isolate);
      isolate->ThrowException(v8::Exception::TypeError(
          v8::String::NewFromUtf8Literal(isolate, "bad shape")));
      expect(type_catch.HasCaught(), "ThrowException was caught");
      v8::Local<v8::Value> thrown = type_catch.Exception();
      expect(!thrown.IsEmpty() && thrown->IsNativeError(),
             "the thrown value is a native error");
      v8::Local<v8::Object> error = thrown.As<v8::Object>();
      v8::String::Utf8Value name(
          isolate, error->Get(current, v8::String::NewFromUtf8Literal(isolate,
                                                                   "name"))
                       .ToLocalChecked());
      v8::String::Utf8Value message(
          isolate,
          error->Get(current, v8::String::NewFromUtf8Literal(isolate, "message"))
              .ToLocalChecked());
      expect(*name != nullptr && strcmp(*name, "TypeError") == 0 &&
                 *message != nullptr && strcmp(*message, "bad shape") == 0,
             "a TypeError carries its name and message");
      expect(v8::Exception::CreateMessage(isolate, thrown).IsEmpty(),
             "a runtime error has no source message");
    }
    {
      v8::TryCatch error_catch(isolate);
      isolate->ThrowException(
          v8::Exception::RangeError(v8::String::NewFromUtf8Literal(isolate, "q")));
      v8::Local<v8::Value> thrown = error_catch.Exception();
      v8::String::Utf8Value name(
          isolate,
          thrown.As<v8::Object>()
              ->Get(current, v8::String::NewFromUtf8Literal(isolate, "name"))
              .ToLocalChecked());
      expect(error_catch.HasCaught() && *name != nullptr &&
                 strcmp(*name, "RangeError") == 0,
             "a RangeError carries its name");
    }

    // Private names: ForApi dedupes, New mints, and a private property round
    // trips by a key the ordinary walks do not list.
    v8::Local<v8::Private> api_key = v8::Private::ForApi(
        isolate, v8::String::NewFromUtf8Literal(isolate, "shapes.k"));
    v8::Local<v8::Private> same_key = v8::Private::ForApi(
        isolate, v8::String::NewFromUtf8Literal(isolate, "shapes.k"));
    v8::Local<v8::Private> fresh_a = v8::Private::New(
        isolate, v8::String::NewFromUtf8Literal(isolate, "shapes.k"));
    v8::Local<v8::Private> fresh_b = v8::Private::New(
        isolate, v8::String::NewFromUtf8Literal(isolate, "shapes.k"));
    expect(api_key.raw() == same_key.raw() &&
               fresh_a.raw() != fresh_b.raw(),
           "ForApi dedupes and New mints fresh names");
    v8::String::Utf8Value key_name(isolate, api_key->Name());
    expect(*key_name != nullptr && strcmp(*key_name, "shapes.k") == 0,
           "Private::Name answers the minted name");

    v8::Local<v8::Object> private_host =
        v8::ObjectTemplate::New(isolate)->NewInstance(current).ToLocalChecked();
    expect(private_host->SetPrivate(current, api_key,
                                    v8::Integer::New(isolate, 5))
                   .FromMaybe(false) &&
               private_host->HasPrivate(current, api_key).FromMaybe(false) &&
               private_host->GetPrivate(current, api_key)
                       .ToLocalChecked()
                       ->Int32Value(current)
                       .FromMaybe(-1) == 5,
           "a private property round trips");
    bind("privateHost", private_host);
    run("globalThis.privateKeys = Object.keys(privateHost).length;"
        " globalThis.privateSymbols ="
        " Object.getOwnPropertySymbols(privateHost).length;");
    expect(number_at("privateKeys") == 0, "a private key is not an object key");
    expect(private_host->DeletePrivate(current, api_key) &&
               !private_host->HasPrivate(current, api_key).FromMaybe(true),
           "DeletePrivate removed the property");

    // The template's data-property forms: a lazy getter runs on every read (the
    // engine has no one-shot slot), and a native setter takes the write.
    host_properties = HostProperties{};
    v8::Local<v8::ObjectTemplate> data_template = v8::ObjectTemplate::New(isolate);
    data_template->SetLazyDataProperty(
        v8::String::NewFromUtf8Literal(isolate, "lazy"), &HostLazyGet);
    data_template->SetNativeDataProperty(
        v8::String::NewFromUtf8Literal(isolate, "native"), &HostLazyGet,
        &HostNativeSet);
    data_template->SetNativeDataProperty(
        v8::String::NewFromUtf8Literal(isolate, "hidden"), &HostLazyGet, nullptr,
        v8::Local<v8::Value>(), v8::DontEnum);
    data_template->SetNativeDataProperty(
        v8::String::NewFromUtf8Literal(isolate, "frozen"), &HostLazyGet,
        &HostNativeSet, v8::Local<v8::Value>(), v8::ReadOnly);
    v8::Local<v8::Object> data_host =
        data_template->NewInstance(current).ToLocalChecked();
    bind("dataHost", data_host);
    run("globalThis.lazyValue = dataHost.lazy;"
        " globalThis.lazyAgain = dataHost.lazy;"
        " dataHost.native = 33;"
        " dataHost.frozen = 44;"
        " globalThis.enumerable = Object.keys(dataHost).join(',');");
    expect(number_at("lazyValue") == 41 && number_at("lazyAgain") == 41,
           "a lazy data property answered its getter");
    expect(host_properties.lazy_reads == 2,
           "the lazy getter ran on every read");
    expect(host_properties.native_writes == 1 &&
               host_properties.last_written == 33,
           "a native setter took the write");
    v8::String::Utf8Value enumerable(
        isolate, current->Global()
                     ->Get(current, v8::String::NewFromUtf8Literal(
                                                   isolate, "enumerable"))
                     .ToLocalChecked());
    expect(*enumerable != nullptr && strcmp(*enumerable, "lazy,native,frozen") == 0,
           "DontEnum hid one accessor and left the others");

    // An accessor whose get and set are FunctionTemplates, with the receiver as
    // this and the assigned value as the setter's first argument.
    host_properties = HostProperties{};
    v8::Local<v8::Object> paired_host =
        v8::ObjectTemplate::New(isolate)->NewInstance(current).ToLocalChecked();
    v8::Local<v8::FunctionTemplate> getter_template =
        v8::FunctionTemplate::New(isolate, &PropertyTemplateGet);
    v8::Local<v8::FunctionTemplate> setter_template =
        v8::FunctionTemplate::New(isolate, &PropertyTemplateSet);
    paired_host->SetAccessorProperty(
        v8::String::NewFromUtf8Literal(isolate, "paired"), getter_template,
        setter_template);
    bind("pairedHost", paired_host);
    run("globalThis.pairedValue = pairedHost.paired;"
        " pairedHost.paired = 12;");
    expect(number_at("pairedValue") == 77, "the template getter ran");
    expect(host_properties.template_reads == 1 &&
               host_properties.template_writes == 1 &&
               host_properties.last_written == 12,
           "the template setter took the write");
  }

  // Buffers and typed arrays: the bytes a host hands to JavaScript, the views
  // over them, and the detach protocol.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "buffers: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    // A new buffer is zeroed, and a host write is what the script reads.
    v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, 16);
    expect(!buffer.IsEmpty() && buffer->ByteLength() == 16 &&
               buffer->IsDetachable() && !buffer->WasDetached(),
           "a new ArrayBuffer's geometry");
    expect(buffer->Data() != nullptr, "a new ArrayBuffer has bytes");
    static_cast<uint8_t*>(buffer->Data())[0] = 7;
    bind("hostBuffer", buffer);
    run("globalThis.firstByte = new Uint8Array(hostBuffer)[0];");
    expect(number_at("firstByte") == 7, "the script read the host's byte");

    // A DataView over the buffer: the engine validates the geometry, so a range
    // past the end leaves the RangeError pending.
    v8::Local<v8::DataView> view = v8::DataView::New(buffer, 4, 8);
    expect(!view.IsEmpty() && view->IsDataView() && view->ByteOffset() == 4 &&
               view->ByteLength() == 8,
           "DataView::New answered the view");
    bind("hostView", view);
    run("hostView.setInt32(0, 1234, true);"
        " globalThis.viewByte = new Uint8Array(hostBuffer)[4] |"
        "   (new Uint8Array(hostBuffer)[5] << 8);");
    expect(number_at("viewByte") == (1234 & 0xffff),
           "a write through the view is in the buffer");
    expect(v8::DataView::New(buffer, 12, 8).IsEmpty(),
           "a view past the end is refused");
    slag_clear_error(current->slag());

    // A host-owned store: writing through it is what the script reads, and the
    // buffer and the store name the same bytes.
    std::unique_ptr<v8::BackingStore> fresh =
        v8::ArrayBuffer::NewBackingStore(isolate, 8);
    expect(!fresh->IsEmpty() && fresh->ByteLength() == 8 &&
               fresh->Data() != nullptr,
           "a new backing store's geometry");
    std::shared_ptr<v8::BackingStore> shared(std::move(fresh));
    v8::Local<v8::ArrayBuffer> over_store =
        v8::ArrayBuffer::New(isolate, shared);
    expect(!over_store.IsEmpty() && over_store->ByteLength() == 8,
           "an ArrayBuffer over a store");
    static_cast<uint8_t*>(shared->Data())[2] = 9;
    bind("storeBuffer", over_store);
    run("globalThis.thirdByte = new Uint8Array(storeBuffer)[2];");
    expect(number_at("thirdByte") == 9, "the store shares the buffer's bytes");

    // A view's bytes are the buffer's bytes, in both directions, and the view
    // reports the geometry it was built with.
    v8::Local<v8::Uint8Array> bytes =
        v8::Uint8Array::New(buffer, 0, 16);
    expect(bytes->Length() == 16 && bytes->ByteLength() == 16 &&
               bytes->ByteOffset() == 0 && bytes->HasBuffer(),
           "a Uint8Array's geometry");
    expect(bytes->Buffer().raw() == buffer.raw(),
           "a view answers the buffer it was built over");
    bind("hostView", bytes);
    run("hostView[1] = 11;");
    expect(static_cast<uint8_t*>(buffer->Data())[1] == 11,
           "a script write reached the buffer");

    v8::Local<v8::Uint32Array> words = v8::Uint32Array::New(buffer, 4, 2);
    expect(words->ByteOffset() == 4 && words->ByteLength() == 8 &&
               words->Length() == 2,
           "a Uint32Array's geometry");
    v8::Local<v8::Float64Array> doubles = v8::Float64Array::New(buffer, 8, 1);
    expect(doubles->Length() == 1 && doubles->ByteLength() == 8,
           "a Float64Array's geometry");
    v8::Local<v8::BigInt64Array> big = v8::BigInt64Array::New(buffer, 8, 1);
    expect(big->Length() == 1, "a BigInt64Array's geometry");

    // A range that does not fit is the engine's RangeError, not a view.
    {
      v8::TryCatch range_catch(isolate);
      v8::Local<v8::Uint32Array> bad = v8::Uint32Array::New(buffer, 15, 1);
      expect(bad.IsEmpty() && range_catch.HasCaught(),
             "an out-of-range view left an error pending");
    }

    // GetBackingStore names the same bytes the buffer has.
    std::shared_ptr<v8::BackingStore> read_back = buffer->GetBackingStore();
    expect(read_back && read_back->ByteLength() == 16 &&
               read_back->Data() == buffer->Data(),
           "GetBackingStore names the buffer's bytes");

    // Detach: the bytes go away, views report empty, and a second detach is
    // refused.
    expect(buffer->Detach(), "a detachable buffer detached");
    expect(buffer->WasDetached() && buffer->ByteLength() == 0 &&
               buffer->Data() == nullptr && !buffer->Detach(),
           "a detached buffer's state");
    run("globalThis.viewLength = hostView.length;");
    expect(number_at("viewLength") == 0, "a detached buffer's view is empty");

    // A detach key is only satisfied by the key itself.
    {
      v8::Local<v8::ArrayBuffer> keyed = v8::ArrayBuffer::New(isolate, 4);
      v8::Local<v8::Value> key = v8::String::NewFromUtf8Literal(isolate, "k");
      keyed->SetDetachKey(key);
      expect(!keyed->Detach(), "a keyed buffer detached without its key");
      expect(keyed->Detach(key) && keyed->WasDetached(),
             "a keyed buffer detached with its key");
    }

    // A SharedArrayBuffer's store is shared, and its bytes are writable.
    v8::Local<v8::SharedArrayBuffer> shared_buffer =
        v8::SharedArrayBuffer::New(isolate, 8);
    std::shared_ptr<v8::BackingStore> shared_store =
        shared_buffer->GetBackingStore();
    expect(shared_buffer->ByteLength() == 8 && shared_store &&
               shared_store->IsShared() &&
               shared_store->ByteLength() == 8,
           "a SharedArrayBuffer and its store");

    // The external-memory account moves by what the host records and comes back
    // when the host takes it away.
    const int64_t account_before =
        isolate->AdjustAmountOfExternalAllocatedMemory(0);
    expect(isolate->AdjustAmountOfExternalAllocatedMemory(4096) ==
               account_before + 4096,
           "the external-memory account moved by the host's change");
    expect(isolate->AdjustAmountOfExternalAllocatedMemory(-4096) ==
               account_before,
           "the external-memory account came back");
    {
      v8::ExternalMemoryAccounter accounter;
      accounter.Update(isolate, 1024);
      expect(isolate->AdjustAmountOfExternalAllocatedMemory(0) ==
                 account_before + 1024,
             "ExternalMemoryAccounter recorded its size");
    }
    expect(isolate->AdjustAmountOfExternalAllocatedMemory(0) == account_before,
           "ExternalMemoryAccounter gave its size back");
  }

  // The host's allocator: a backing store's bytes come from it, and go back when
  // the last buffer and store over them are gone.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "allocator: %s\n", what);
        ++failures;
      }
    };

    const size_t allocated_before = allocator.allocated;
    v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, 64);
    expect(!buffer.IsEmpty() && buffer->ByteLength() == 64,
           "ArrayBuffer::New answered a buffer");
    expect(allocator.allocated >= allocated_before + 64,
           "the buffer's bytes came from the host's allocator");

    // A store the host drops hands its bytes back at once: no buffer over it
    // keeps the block alive.
    const size_t freed_before = allocator.freed;
    {
      std::unique_ptr<v8::BackingStore> store =
          v8::ArrayBuffer::NewBackingStore(isolate, 32);
      expect(store != nullptr && store->ByteLength() == 32,
             "NewBackingStore answered a store");
      expect(allocator.allocated >= allocated_before + 96,
             "the store's bytes came from the allocator too");
    }
    expect(allocator.freed >= freed_before + 32,
           "Free ran when the store went");

    // The bytes are still the isolate's external memory while the buffer is
    // alive, which is the number HeapStatistics reports.
    v8::HeapStatistics stats;
    isolate->GetHeapStatistics(&stats);
    expect(stats.external_memory >= 64,
           "the allocator's bytes are external memory");
  }

  // Embedder data, security tokens, and the creation context: the slots and
  // identity a host attaches to a realm.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "embedder data: %s\n", what);
        ++failures;
      }
    };

    int marker = 0;
    current->SetAlignedPointerInEmbedderData(1, &marker);
    expect(current->GetAlignedPointerFromEmbedderData(1) == &marker &&
               current->GetAlignedPointerFromEmbedderData(0) == nullptr &&
               current->GetAlignedPointerFromEmbedderData(7) == nullptr,
           "an aligned pointer slot round trips");

    current->SetEmbedderData(0, v8::Integer::New(isolate, 5));
    current->SetEmbedderData(0, v8::Integer::New(isolate, 6));
    expect(current->GetEmbedderData(0)->Int32Value(current).FromMaybe(-1) == 6,
           "an embedder value round trips");
    expect(current->GetEmbedderData(3)->IsUndefined(),
           "an unwritten embedder value is undefined");
    expect(current->GetNumberOfEmbedderDataFields() >= 2,
           "the field count covers both stores");

    // The slot is what names a value set inside a scope that has since popped.
    {
      v8::HandleScope inner(isolate);
      current->SetEmbedderData(2, v8::Integer::New(isolate, 9));
    }
    slag_collect(current->slag());
    expect(current->GetEmbedderData(2)->Int32Value(current).FromMaybe(-1) == 9,
           "an embedder value outlived the scope that made it");

    const size_t roots_before_clear = slag_root_count();
    current->ClearAllSlots();
    const size_t released = roots_before_clear - slag_root_count();
    expect(released == 2 &&
               current->GetEmbedderData(0)->IsUndefined() &&
               current->GetAlignedPointerFromEmbedderData(1) == nullptr &&
               current->GetNumberOfEmbedderDataFields() == 0,
           "ClearAllSlots released both slots and forgot them");

    // One default token per isolate, and a set token replaces it until the
    // context goes back to the default.
    v8::Local<v8::Value> default_token = current->GetSecurityToken();
    expect(!default_token.IsEmpty() && default_token->IsObject() &&
               default_token.raw() == current->GetSecurityToken().raw(),
           "the default security token is one shared object");
    v8::Local<v8::Value> custom = v8::Object::New(isolate);
    current->SetSecurityToken(custom);
    expect(current->GetSecurityToken().raw() == custom.raw(),
           "a set security token reads back");
    current->UseDefaultSecurityToken();
    expect(current->GetSecurityToken().raw() == default_token.raw(),
           "UseDefaultSecurityToken went back to the default");

    v8::Local<v8::Object> made = v8::Object::New(isolate);
    v8::Local<v8::Context> creation;
    expect(made->GetCreationContext().ToLocal(&creation) &&
               creation.raw() == current.raw() &&
               made->GetCreationContextChecked().raw() == current.raw(),
           "an object reports the entered context");
  }

  // Weak handles, GC callbacks, and heap statistics: the lifetime hooks an
  // embedding needs around a persistent and around a requested collection.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "weak handles: %s\n", what);
        ++failures;
      }
    };

    // The value is made in a frame that returns only the handle, and the stack
    // is scrubbed, so the conservative scan cannot keep it alive.
    weak_state = WeakState{};
    v8::Global<v8::Object> weak;
    int parameter = 0;
    MakeUnrootedWeak(isolate, &weak, &parameter, &WeakGone);
    expect(weak.IsWeak(), "SetWeak made the handle weak");
    ScrubStack(16);
    isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
    expect(weak_state.first == 1 && weak_state.second == 1 &&
               weak_state.parameter == &parameter,
           "a weak callback and its second pass ran");
    expect(weak.Get(isolate).IsEmpty(), "a dead weak handle reads empty");
    weak.Reset();

    // ClearWeak before the death keeps the value alive and cancels the callback.
    weak_state = WeakState{};
    v8::Global<v8::Object> kept;
    int cleared_parameter = 0;
    MakeUnrootedWeak(isolate, &kept, &cleared_parameter, &WeakClearedSurvived);
    kept.ClearWeak();
    ScrubStack(16);
    isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
    expect(weak_state.cleared_survived == 0 && !kept.IsWeak() &&
               !kept.Get(isolate).IsEmpty(),
           "ClearWeak made the handle strong again");
    kept.Reset();

    // The hooks run around an explicit collection, and removal takes effect.
    gc_prologue = 0;
    gc_epilogue = 0;
    isolate->AddGCPrologueCallback(&GCCallbackPrologue);
    isolate->AddGCEpilogueCallback(&GCCallbackEpilogue);
    isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
    expect(gc_prologue == 1 && gc_epilogue == 1,
           "the GC callbacks ran around the request");
    isolate->RemoveGCEpilogueCallback(&GCCallbackEpilogue);
    isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
    expect(gc_prologue == 2 && gc_epilogue == 1,
           "RemoveGCEpilogueCallback removed the hook");
    isolate->LowMemoryNotification();
    expect(gc_prologue == 3, "LowMemoryNotification collected");
    isolate->RemoveGCPrologueCallback(&GCCallbackPrologue);

    // The heap statistics report the arena and the live buffers.
    v8::Local<v8::ArrayBuffer> measured = v8::ArrayBuffer::New(isolate, 64);
    v8::HeapStatistics stats;
    isolate->GetHeapStatistics(&stats);
    expect(!measured.IsEmpty() && stats.total_heap_size > 0 &&
               stats.used_heap_size > 0 &&
               stats.used_heap_size <= stats.total_heap_size &&
               stats.heap_size_limit > stats.total_heap_size &&
               stats.external_memory >= 64 &&
               stats.number_of_native_contexts >= 1 && !stats.does_zap_garbage,
           "the heap statistics report the arena");
  }

  // Modules: compile, resolve a dependency through the host callback, link,
  // evaluate, and read the namespace.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "modules: %s\n", what);
        ++failures;
      }
    };

    {
      v8::HandleScope inner(isolate);
      v8::Local<v8::String> source = v8::String::NewFromUtf8Literal(
          isolate, "export const answer = 42;");
      v8::Local<v8::Value> name =
          v8::String::NewFromUtf8Literal(isolate, "dep.js");
      v8::ScriptOrigin origin(name);
      v8::Source origin_source(source, origin);
      v8::Local<v8::Module> module;
      expect(v8::ScriptCompiler::CompileModule(isolate, &origin_source)
                 .ToLocal(&module),
             "a dependency compiled");
      expect(module->GetStatus() == v8::ModuleStatus::kUninstantiated,
             "a fresh module is uninstantiated");
      expect(module->IsSourceTextModule().FromMaybe(false) &&
                 !module->IsSyntheticModule().FromMaybe(true) &&
                 module->GetIdentityHash()->Int32Value(current).FromMaybe(0) !=
                     0,
             "a source text module reports its kind and identity");
      module_dependency.Reset(isolate, module);
    }

    v8::Local<v8::String> source = v8::String::NewFromUtf8Literal(
        isolate,
        "import { answer } from 'dep.js';\nexport const doubled = answer * 2;");
    v8::Local<v8::Value> name =
        v8::String::NewFromUtf8Literal(isolate, "main.js");
    v8::ScriptOrigin origin(name);
    v8::Source main_source(source, origin);
    v8::Local<v8::Module> main;
    expect(v8::ScriptCompiler::CompileModule(isolate, &main_source)
               .ToLocal(&main),
           "the main module compiled");
    expect(main->GetModuleRequestsLength() == 1,
           "the main module declares one import");
    v8::String::Utf8Value request(isolate, main->GetModuleRequest(0));
    expect(*request != nullptr && strcmp(*request, "dep.js") == 0,
           "GetModuleRequest answers the specifier");

    expect(main->InstantiateModule(current, &ResolveModule).FromMaybe(false),
           "the module graph linked");
    expect(main->GetStatus() == v8::ModuleStatus::kInstantiated,
           "a linked module is instantiated");

    v8::TryCatch module_catch(isolate);
    v8::Local<v8::Value> evaluated;
    expect(main->Evaluate(current).ToLocal(&evaluated) &&
               evaluated->IsPromise() && !module_catch.HasCaught(),
           "Evaluate answered the evaluation promise");
    expect(main->GetStatus() == v8::ModuleStatus::kEvaluated,
           "the module evaluated");

    v8::Local<v8::Value> namespace_object = main->GetModuleNamespace();
    expect(!namespace_object.IsEmpty() && namespace_object->IsObject() &&
               namespace_object.As<v8::Object>()
                       ->Get(current,
                             v8::String::NewFromUtf8Literal(isolate,
                                                            "doubled"))
                       .ToLocalChecked()
                       ->Int32Value(current)
                       .FromMaybe(-1) == 84,
           "the namespace exposes the export");
    expect(main->SourceOffsetToLocation(0).GetLineNumber() == 0,
           "an offset maps to a 0-based location");
  }

  // WebAssembly: compile wire bytes into a module object, read its decoded
  // module back, and instantiate it from a script.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "wasm: %s\n", what);
        ++failures;
      }
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    // (func (export "add") (param i32 i32) (result i32) local.get 0
    //   local.get 1 i32.add), the module V8's own sample carries.
    const uint8_t wasm[] = {
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01,
        0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07,
        0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x0a, 0x09, 0x01,
        0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b};
    v8::Local<v8::WasmModuleObject> module;
    expect(v8::WasmModuleObject::Compile(
               isolate, v8::MemorySpan<const uint8_t>(wasm, sizeof(wasm)))
               .ToLocal(&module),
           "a wasm module compiled");
    expect(module->IsObject() &&
               module.As<v8::Object>()
                   ->Get(current,
                         v8::String::NewFromUtf8Literal(isolate, "toString"))
                   .ToLocalChecked()
                   ->IsFunction(),
           "a wasm module object is an object");

    v8::CompiledWasmModule compiled = module->GetCompiledModule();
    expect(!compiled.is_null(), "a module object answers its compiled module");
    v8::Local<v8::WasmModuleObject> again;
    expect(v8::WasmModuleObject::FromCompiledModule(isolate, compiled)
                   .ToLocal(&again) &&
               again->IsObject(),
           "a decoded module makes another module object");

    bind("wasmModule", module);
    v8::Script::Compile(
        current, v8::String::NewFromUtf8Literal(
                     isolate,
                     "globalThis.wasmAnswer ="
                     " new WebAssembly.Instance(wasmModule).exports.add(3, 4);"))
        .ToLocalChecked()
        ->Run(current);
    expect(number_at("wasmAnswer") == 7, "the instantiated export ran");

    // The instance surface: the engine's exports record is the brand and the
    // lookup table, so a host reaches the same export the script did.
    v8::Local<v8::Value> instance =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "globalThis.wasmInstance ="
                " new WebAssembly.Instance(wasmModule); globalThis.wasmInstance"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocalChecked();
    expect(v8::WasmInstanceObject::IsWasmInstanceObject(instance),
           "an instance names itself");
    expect(!v8::WasmInstanceObject::IsWasmInstanceObject(
               v8::Object::New(isolate).As<v8::Value>()),
           "an ordinary object is not an instance");

    v8::Local<v8::WasmInstanceObject> as_instance =
        instance.As<v8::WasmInstanceObject>();
    v8::Local<v8::Value> add = as_instance
                                   ->GetExportedFunction(
                                       current,
                                       v8::String::NewFromUtf8Literal(isolate, "add"))
                                   .ToLocalChecked();
    expect(add->IsFunction(), "the named export is a function");
    expect(as_instance->GetFunctionIndex(
               v8::String::NewFromUtf8Literal(isolate, "add")) == 0,
           "the export's function index came back");
    expect(as_instance
               ->GetExportedFunction(current, 0)
               .ToLocalChecked()
               ->IsFunction(),
           "the indexed export is the same function");
    expect(as_instance
               ->GetExportedFunction(
                   current, v8::String::NewFromUtf8Literal(isolate, "nope"))
               .IsEmpty(),
           "a name that is not exported answers empty");
    expect(as_instance->GetMemoryObject().IsEmpty(),
           "a module without a memory has none");

    // A memory is an object too, and its buffer is the engine's ArrayBuffer.
    v8::Local<v8::Value> memory =
        v8::Script::Compile(
            current,
            v8::String::NewFromUtf8Literal(
                isolate,
                "new WebAssembly.Memory({ initial: 1 })"))
            .ToLocalChecked()
            ->Run(current)
            .ToLocalChecked();
    expect(v8::WasmMemoryObject::IsWasmMemoryObject(memory),
           "a memory names itself");
    expect(!v8::WasmMemoryObject::IsWasmMemoryObject(instance),
           "an instance is not a memory");
    v8::Local<v8::WasmMemoryObject> as_memory = memory.As<v8::WasmMemoryObject>();
    v8::Local<v8::ArrayBuffer> pages = as_memory->Buffer();
    expect(!pages.IsEmpty() && pages->ByteLength() == 65536 &&
               as_memory->Size() == 65536,
           "a one-page memory answers its buffer");
  }

  // Snapshots: write a realm's globals into a blob, then restore the blob in a
  // fresh isolate's first context.
  {
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "snapshots: %s\n", what);
        ++failures;
      }
    };

    v8::StartupData blob;
    {
      v8::SnapshotCreator creator;
      v8::Isolate* snapshot_isolate = creator.GetIsolate();
      {
        v8::Isolate::Scope isolate_scope(snapshot_isolate);
        v8::HandleScope scope(snapshot_isolate);
        v8::Local<v8::Context> snapshot_context =
            v8::Context::New(snapshot_isolate);
        v8::Context::Scope context_scope(snapshot_context);
        // `write_snapshot` carries no host-callback table, so the console and
        // timers `Context::New` installs have to be out of the realm first (the
        // reading context puts its own back).
        v8::Script::Compile(
            snapshot_context,
            v8::String::NewFromUtf8Literal(
                snapshot_isolate,
                "delete globalThis.console; delete globalThis.setTimeout;"
                " delete globalThis.setInterval; delete globalThis.clearTimeout;"
                " delete globalThis.clearInterval;"))
            .ToLocalChecked()
            ->Run(snapshot_context);
        v8::Script::Compile(
            snapshot_context,
            v8::String::NewFromUtf8Literal(snapshot_isolate,
                                           "var snapshotValue = 123;"))
            .ToLocalChecked()
            ->Run(snapshot_context);
        creator.SetDefaultContext(snapshot_context);
      }
      blob = creator.CreateBlob();
      expect(blob.is_valid(), "the snapshot blob was written");
    }

    {
      v8::Isolate::CreateParams params;
      params.array_buffer_allocator =
          v8::ArrayBuffer::Allocator::NewDefaultAllocator();
      params.snapshot_blob = &blob;
      v8::Isolate* restored_isolate = v8::Isolate::New(params);
      {
        v8::Isolate::Scope isolate_scope(restored_isolate);
        v8::HandleScope scope(restored_isolate);
        v8::Local<v8::Context> restored_context =
            v8::Context::New(restored_isolate);
        v8::Context::Scope context_scope(restored_context);
        v8::Local<v8::Value> value =
            v8::Script::Compile(
                restored_context,
                v8::String::NewFromUtf8Literal(restored_isolate,
                                               "snapshotValue"))
                .ToLocalChecked()
                ->Run(restored_context)
                .ToLocalChecked();
        expect(value->Int32Value(restored_context).FromMaybe(-1) == 123,
               "the restored realm carries the global");
      }
      restored_isolate->Dispose();
      delete params.array_buffer_allocator;
    }
    delete[] blob.data;
  }

  // Function templates and prototypes: materialize a template, inherit from
  // another, put a method on the prototype template, and read/set prototypes.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "templates: %s\n", what);
        ++failures;
      }
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto run = [&](const char* source) {
      v8::TryCatch caught(isolate);
      v8::Local<v8::Value> result;
      v8::Local<v8::Script> script;
      if (!v8::Script::Compile(
               current, v8::String::NewFromUtf8Literal(isolate, source))
               .ToLocal(&script) ||
          !script->Run(current).ToLocal(&result)) {
        v8::String::Utf8Value text(isolate, caught.Exception());
        fprintf(stderr, "templates: script failed: %s\n",
                *text == nullptr ? "" : *text);
      }
      return result;
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    v8::Local<v8::Value> data = v8::Integer::New(isolate, 7);
    v8::Local<v8::FunctionTemplate> parent =
        v8::FunctionTemplate::New(isolate, &TemplateConstructor);
    parent->SetClassName("Parent");
    parent->PrototypeTemplate()->Set(
        isolate, "kind", v8::FunctionTemplate::New(isolate, &ParentKind));
    v8::Local<v8::Signature> signature = v8::Signature::New(isolate, parent);

    v8::Local<v8::FunctionTemplate> child = v8::FunctionTemplate::New(
        isolate, &TemplateConstructor, data, signature, 1,
        v8::ConstructorBehavior::kAllow);
    child->SetClassName("Child");
    child->Inherit(parent);
    child->InstanceTemplate()->SetInternalFieldCount(1);
    // An accessor declared over an `AccessorSignature`: it lands on the built
    // prototype, so it must be installed before the template is materialized
    // (the prototype object is built once and kept).
    v8::Local<v8::AccessorSignature> guard_signature =
        v8::AccessorSignature::New(isolate, parent);
    child->PrototypeTemplate()->SetAccessor(
        v8::String::NewFromUtf8Literal(isolate, "guard"), &SignatureGuardGet,
        nullptr, v8::Local<v8::Value>(), guard_signature);

    v8::Local<v8::Function> constructor;
    expect(child->GetFunction(current).ToLocal(&constructor),
           "the template materialized a function");
    expect(v8::Isolate::GetCurrent() == isolate &&
               v8::Isolate::TryGetCurrent() == isolate,
           "GetCurrent answers the entered isolate");
    expect(current->GetIsolate() == isolate,
           "a context names its isolate back");
    expect(isolate->GetEnteredContext().raw() == current.raw() &&
               isolate->GetEnteredOrMicrotaskContext().raw() == current.raw(),
           "the entered-context accessors answer the scope's context");
    bind("Child", constructor);
    run("var made = new Child(5);"
        " globalThis.madeArg = made.arg0;"
        " globalThis.madeData = made.data;"
        " globalThis.madeKind = made.kind();"
        " globalThis.madeIsChild = made instanceof Child;");
    expect(number_at("madeArg") == 5, "the constructor ran with the argument");
    expect(number_at("madeData") == 7,
           "the callback read the template's data");
    expect(number_at("madeKind") == 42,
           "an instance reached the parent's prototype method");
    expect(number_at("madeIsChild") == 1, "the instance is an instance");

    // An accessor declared over an `AccessorSignature` runs for an instance and
    // is refused for any other `this`, through the same receiver check a
    // method's signature makes.
    {
      run("var guarded = new Child(1);"
          " globalThis.guardOwn = guarded.guard;"
          " var descriptor = Object.getOwnPropertyDescriptor("
          "   Object.getPrototypeOf(guarded), 'guard');"
          " try {"
          "   descriptor.get.call({});"
          "   globalThis.guardForeign = 1;"
          " } catch (e) {"
          "   globalThis.guardForeign = 0;"
          "   globalThis.guardMessage = e.message;"
          " }");
      expect(number_at("guardOwn") == 77 && guard_reads == 1,
             "an accessor declared over a signature ran for its instance");
      expect(number_at("guardForeign") == 0,
             "the accessor refused a receiver outside the signature");
      expect(guard_reads == 1,
             "the refused accessor did not run its getter");
      run("globalThis.guardRefused = globalThis.guardMessage === "
          "'Illegal invocation';");
      expect(number_at("guardRefused") == 1,
             "the refusal is Illegal invocation");
    }

    // The same check reaches `SetAccessorProperty`, whose getter and setter are
    // function templates: a template carrying a signature is enforced there
    // too, so an accessor declared over a type refuses a foreign receiver.
    {
      v8::Local<v8::FunctionTemplate> signed_getter = v8::FunctionTemplate::New(
          isolate, &ParentKind, v8::Local<v8::Value>(), signature);
      v8::Local<v8::Object> holder = v8::Object::New(isolate);
      holder->SetAccessorProperty(
          v8::String::NewFromUtf8Literal(isolate, "signed"), signed_getter,
          v8::Local<v8::FunctionTemplate>());
      bind("signedHolder", holder);
      run("try {"
          "  signedHolder.signed;"
          "  globalThis.signedRefused = 0;"
          "} catch (e) {"
          "  globalThis.signedRefused = e.message === 'Illegal invocation';"
          "}");
      expect(number_at("signedRefused") == 1,
             "a template accessor over a signature refused a foreign receiver");
    }

    // ConstructorBehavior::kThrow materializes a function with no [[Construct]].
    v8::Local<v8::FunctionTemplate> plain_template = v8::FunctionTemplate::New(
        isolate, &TemplateConstructor, v8::Local<v8::Value>(),
        v8::Local<v8::Signature>(), 0, v8::ConstructorBehavior::kThrow);
    v8::Local<v8::Function> plain_function;
    expect(plain_template->GetFunction(current).ToLocal(&plain_function),
           "a kThrow template materialized");
    bind("Plain", plain_function);
    run("try { new Plain(); globalThis.plainConstructed = 1; }"
        " catch (e) { globalThis.plainConstructed = 0; }");
    expect(number_at("plainConstructed") == 0,
           "kThrow materialized a non-constructible function");

    v8::Local<v8::Object> plain = v8::Object::New(isolate);
    v8::Local<v8::Object> prototype = v8::Object::New(isolate);
    prototype->Set(current, v8::String::NewFromUtf8Literal(isolate, "mark"),
                   v8::Integer::New(isolate, 9));
    expect(plain->SetPrototype(current, prototype) &&
               plain->GetPrototype().raw() == prototype.raw(),
           "SetPrototype and GetPrototype agree");
    bind("plain", plain);
    run("globalThis.inheritedMark = plain.mark;");
    expect(number_at("inheritedMark") == 9,
           "the prototype's property is inherited");

    // A per-object access check: an instance of a template that installs one is
    // readable in its own realm and refused from another
    // (`ObjectTemplate::SetAccessCheckCallback`).
    {
      v8::Local<v8::ObjectTemplate> checked = v8::ObjectTemplate::New(isolate);
      checked->SetAccessCheckCallback(isolate, &DenyAccess,
                                      v8::Integer::New(isolate, 3));
      v8::Local<v8::Object> guarded =
          checked->NewInstance(current).ToLocalChecked();
      SetOrReport(isolate, current, guarded, "value",
                  v8::Integer::New(isolate, 11), &failures);
      bind("guarded", guarded);
      run("globalThis.sameRealm = guarded.value;");
      expect(number_at("sameRealm") == 11,
             "an access-checked object is readable in its own realm");
      expect(access_checks == 0,
             "a same-realm access does not consult the check");

      // A second realm, which is where the check fires. Reading the object from
      // it is refused, and the refusal is the callback's answer.
      v8::Local<v8::Context> other = v8::Context::New(isolate);
      {
        v8::Context::Scope other_scope(other);
        SetOrReport(isolate, other, other->Global(), "guarded", guarded,
                    &failures);
        bool refused = false;
        {
          v8::TryCatch caught(isolate);
          v8::Local<v8::Script> script;
          v8::Local<v8::Value> result;
          if (!v8::Script::Compile(
                   other,
                   v8::String::NewFromUtf8Literal(isolate, "guarded.value"))
                   .ToLocal(&script) ||
              !script->Run(other).ToLocal(&result)) {
            refused = caught.HasCaught();
          }
        }
        expect(refused,
               "a cross-realm read of an access-checked object is refused");
        expect(access_checks == 1,
               "the check ran once, for the cross-realm read");

        // The callback's answer is the decision: a template that allows the
        // access is read across realms like any other object.
        v8::Local<v8::ObjectTemplate> shared = v8::ObjectTemplate::New(isolate);
        shared->SetAccessCheckCallback(isolate, &AllowAccess,
                                       v8::Integer::New(isolate, 4));
        v8::Local<v8::Object> open =
            shared->NewInstance(current).ToLocalChecked();
        SetOrReport(isolate, current, open, "value",
                    v8::Integer::New(isolate, 21), &failures);
        SetOrReport(isolate, other, other->Global(), "open", open, &failures);
        v8::TryCatch caught(isolate);
        v8::Local<v8::Script> script;
        v8::Local<v8::Value> result;
        expect(v8::Script::Compile(
                   other, v8::String::NewFromUtf8Literal(isolate, "open.value"))
                       .ToLocal(&script) &&
                   script->Run(other).ToLocal(&result) &&
                   result->Int32Value(other).FromMaybe(-1) == 21,
               "the check's allow answer admits the cross-realm read");
      }
    }

    // A MicrotasksScope drains the queue on its outermost exit.
    isolate->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);
    run("globalThis.scopeSettled = 0;"
        " Promise.resolve(1).then(function () { globalThis.scopeSettled = 1; });");
    expect(number_at("scopeSettled") == 0, "kExplicit left the job queued");
    {
      v8::MicrotasksScope microtasks(isolate,
                                     v8::MicrotasksScope::kRunMicrotasks);
      expect(v8::MicrotasksScope::GetCurrentDepth(isolate) == 1,
             "the scope reports its depth");
    }
    expect(number_at("scopeSettled") == 1, "the scope drained on exit");
    isolate->SetMicrotasksPolicy(v8::MicrotasksPolicy::kAuto);
  }

  // A call-as-function handler: the instance a template builds is callable, and a
  // call runs the host's handler with the instance as `this`.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "callable: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    v8::Local<v8::ObjectTemplate> tmpl = v8::ObjectTemplate::New(isolate);
    tmpl->SetInternalFieldCount(1);
    tmpl->Set(isolate, "mark",
              v8::FunctionTemplate::New(isolate, CallableMarker));
    tmpl->SetCallAsFunctionHandler(CallableBody, v8::Integer::New(isolate, 100));

    v8::Local<v8::Object> callable = tmpl->NewInstance(current).ToLocalChecked();
    expect(callable.As<v8::Value>()->IsFunction(),
           "a call handler makes the instance callable");
    expect(callable->InternalFieldCount() == 1,
           "the callable instance keeps its internal fields");

    auto* counter = new HostCounter{5};
    callable->SetAlignedPointerInInternalField(0, counter);
    expect(callable->Wrappable<HostCounter>() == counter,
           "the callable instance carries its internal field");
    bind("callable", callable);
    run("globalThis.callableAnswer = callable(2, 3);"
        " globalThis.callableMark = callable.mark();"
        " globalThis.callableIsFunction = typeof callable === 'function' ? 1 : 0;");
    expect(number_at("callableAnswer") == 105,
           "the handler ran with its arguments and data");
    expect(number_at("callableMark") == 7,
           "the callable carries the template's method");
    expect(number_at("callableIsFunction") == 1,
           "a script sees the instance as a function");

    // A template with a property handler keeps the handler object instead, since
    // the engine's host objects have no [[Call]]; the instance is then not
    // callable, which is the narrowing the README records.
    v8::Local<v8::ObjectTemplate> handled = v8::ObjectTemplate::New(isolate);
    v8::NamedPropertyHandlerConfiguration configuration(
        HostGet, HostSet, HostQuery, HostDelete, HostEnumerate, HostDescriptor,
        HostDefiner);
    handled->SetHandler(configuration);
    handled->SetCallAsFunctionHandler(CallableBody);
    v8::Local<v8::Object> plain = handled->NewInstance(current).ToLocalChecked();
    expect(!plain.As<v8::Value>()->IsFunction(),
           "a template with a handler keeps a handler object");
    delete counter;
  }

  // A signature's access check: the method only runs on an instance of the type
  // the signature names, and anything else fails the call with a TypeError.
  {
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> current = context.Get(isolate);
    v8::Context::Scope context_scope(current);
    const auto expect = [&](bool condition, const char* what) {
      if (!condition) {
        fprintf(stderr, "signature: %s\n", what);
        ++failures;
      }
    };
    const auto run = [&](const char* source) {
      return RunScript(isolate, current, source, &failures);
    };
    const auto bind = [&](const char* name, v8::Local<v8::Value> value) {
      BindGlobal(isolate, current, name, value, &failures);
    };
    const auto number_at = [&](const char* name) {
      return IntGlobal(isolate, current, name, &failures);
    };

    v8::Local<v8::FunctionTemplate> guarded =
        v8::FunctionTemplate::New(isolate, &TemplateConstructor);
    guarded->SetClassName("Guarded");
    v8::Local<v8::Signature> signature = v8::Signature::New(isolate, guarded);
    guarded->PrototypeTemplate()->Set(
        isolate, "guarded",
        v8::FunctionTemplate::New(isolate, &GuardedBody, v8::Local<v8::Value>(),
                                  signature));
    v8::Local<v8::Function> constructor;
    expect(guarded->GetFunction(current).ToLocal(&constructor),
           "the guarded template materialized");
    bind("Guarded", constructor);

    run("globalThis.guardedInstance = new Guarded();"
        " globalThis.guardedAllowed = guardedInstance.guarded(4);"
        " try {"
        "   guardedInstance.guarded.call({}, 4);"
        "   globalThis.guardedRefused = 0;"
        " } catch (e) {"
        "   globalThis.guardedRefused = e instanceof TypeError ? 1 : 2;"
        "   globalThis.guardedMessage = e.message;"
        " }");
    expect(number_at("guardedAllowed") == 8,
           "an instance reaches the guarded method");
    expect(number_at("guardedRefused") == 1,
           "a receiver that is not an instance fails with a TypeError");
    v8::String::Utf8Value message(
        isolate, current->Global()
                     ->Get(current, v8::String::NewFromUtf8Literal(
                                                   isolate, "guardedMessage"))
                     .ToLocalChecked());
    expect(*message != nullptr && strcmp(*message, "Illegal invocation") == 0,
           "the refusal names the access check");
  }

  // A host-traced reference: a root the C++ side owns and releases. Once it
  // handle scope pops, the object is reachable only through the reference.
  {
    v8::TracedReference<v8::Object> traced;
    v8::TracedReference<v8::Object> copy;
    {
      v8::HandleScope scope(isolate);
      v8::Local<v8::Context> current = context.Get(isolate);
      v8::Local<v8::ObjectTemplate> plain = v8::ObjectTemplate::New(isolate);
      v8::Local<v8::Object> object =
          plain->NewInstance(current).ToLocalChecked();
      if (!object
               ->Set(current, v8::String::NewFromUtf8Literal(isolate, "mark"),
                     v8::Integer::New(isolate, 41))
               .FromMaybe(false)) {
        fprintf(stderr, "the traced object could not be marked\n");
        ++failures;
      }
      traced.Reset(isolate, object);
      copy = traced;
      if (traced.IsEmpty() || copy.IsEmpty()) {
        fprintf(stderr, "a traced reference started empty\n");
        ++failures;
      }
    }

    bool survived = false;
    {
      v8::HandleScope scope(isolate);
      v8::Local<v8::Context> current = context.Get(isolate);
      if (slag_collect(current->slag()) != 0) {
        fprintf(stderr, "collection failed\n");
        ++failures;
      }
      v8::Local<v8::Value> mark;
      if (traced.Get(isolate)
              ->Get(current, v8::String::NewFromUtf8Literal(isolate, "mark"))
              .ToLocal(&mark) &&
          mark->Int32Value(current).FromMaybe(-1) == 41) {
        survived = true;
      }
    }
    if (!survived) {
      fprintf(stderr, "a traced reference lost its value\n");
      ++failures;
    }

    // The copy is a second reference, so releasing the first must not clear it.
    traced.Reset();
    if (!traced.IsEmpty()) {
      fprintf(stderr, "Reset left a traced reference non-empty\n");
      ++failures;
    }
    {
      v8::HandleScope scope(isolate);
      v8::Local<v8::Context> current = context.Get(isolate);
      slag_collect(current->slag());
      v8::Local<v8::Value> mark;
      if (!copy.Get(isolate)
               ->Get(current, v8::String::NewFromUtf8Literal(isolate, "mark"))
               .ToLocal(&mark) ||
          mark->Int32Value(current).FromMaybe(-1) != 41) {
        fprintf(stderr, "the copy did not keep the value alive\n");
        ++failures;
      }
    }

    copy.Reset();
    if (!copy.IsEmpty()) {
      fprintf(stderr, "Reset left the copy non-empty\n");
      ++failures;
    }
  }

  isolate->Dispose();
  return failures == 0 ? 0 : 1;
}
