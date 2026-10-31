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
