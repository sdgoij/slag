// A gin-shaped compile/run target for A1.
//
// `gin` itself cannot be built standalone: gin/BUILD.gn depends on //base,
// //base/allocator:buildflags, //third_party/abseil-cpp:absl, and
// gin/wrappable.h needs v8::Object::Wrappable, v8::CppHeapPointerTag, and
// Object::Unwrap. This harness exercises the V8 usage patterns gin's
// arguments/converter/object_template_builder paths rely on, without
// Chromium's base library.

#include <stdio.h>
#include <string.h>

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

}  // namespace

int main() {
  v8::V8::InitializeICUDefaultLocation(nullptr);
  v8::V8::Initialize();

  v8::Isolate::CreateParams params;
  params.array_buffer_allocator =
      v8::ArrayBuffer::Allocator::NewDefaultAllocator();
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
        HostGet, HostSet, HostQuery, HostDelete, HostDescriptor, HostDefiner,
        v8::Integer::New(isolate, 7));
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

    // The indexed handler owns elements 0..2; a higher index falls back.
    v8::IndexedPropertyHandlerConfiguration indexed(HostGetIndex, HostSetIndex,
                                                   HostQueryIndex,
                                                   HostDeleteIndex);
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
  }

  // A host-traced reference: a root the C++ side owns and releases. Once the
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
  delete params.array_buffer_allocator;
  return failures == 0 ? 0 : 1;
}
