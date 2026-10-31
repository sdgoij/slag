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
#include <string>

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
      return v8::Script::Compile(
                 current, v8::String::NewFromUtf8Literal(isolate, source))
          .ToLocalChecked()
          ->Run(current);
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
      return v8::Script::Compile(
                 current, v8::String::NewFromUtf8Literal(isolate, source))
          .ToLocalChecked()
          ->Run(current)
          .ToLocalChecked();
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
  delete params.array_buffer_allocator;
  return failures == 0 ? 0 : 1;
}
