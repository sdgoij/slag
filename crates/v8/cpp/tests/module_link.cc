// Module linking with V8 15's resolver shapes: `ResolveModuleCallback` takes the
// request's import attributes as a `FixedArray`, and `InstantiateModule` also
// accepts a `ResolveSourceCallback` whose answer is what a source-phase import
// binds.
//
// Two graphs. The first links `import source src from "src.js"` and checks that
// the host's source object is the binding — without one the engine has no
// `%AbstractModuleSource%` for a source text module and the import throws. The
// second links `import "dep.js" with { type: "js" }` and checks the attributes
// arrive at the resolver, which is the only way a host learns them.

#include <stdio.h>
#include <string.h>

#include <memory>
#include <string>

#ifdef _WIN32
#include <crtdbg.h>

namespace {
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

#include "include/v8-context.h"
#include "include/v8-data.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-object.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"

namespace {

int g_failures = 0;

void Check(bool ok, const char* what) {
  if (!ok) {
    fprintf(stderr, "FAILED: %s\n", what);
    ++g_failures;
  }
}

// The graph the resolvers answer for and what they were asked. V8's callbacks
// are plain function pointers, so the state lives at file scope.
struct LinkState {
  v8::Global<v8::Module> dep;
  v8::Global<v8::Module> src;
  v8::Global<v8::Module> src2;
  v8::Global<v8::Object> source;
  int source_calls = 0;
  int dep_attribute_length = -1;
  std::string dep_type;
};

LinkState g_link;

std::string TextOf(v8::Local<v8::Value> value) {
  v8::String::Utf8Value utf8(v8::g_current_isolate, value);
  return *utf8 == nullptr ? std::string() : std::string(*utf8);
}

v8::Local<v8::String> Key(v8::Isolate* isolate, const char* text) {
  return v8::String::NewFromUtf8Literal(isolate, text);
}

// The four-argument resolver: `import_attributes` is V8 15's
// `[key, value, offset, …]` array, empty for a request with no `with` clause.
v8::MaybeLocal<v8::Module> Resolve(v8::Local<v8::Context>,
                                   v8::Local<v8::String> specifier,
                                   v8::Local<v8::FixedArray> import_attributes,
                                   v8::Local<v8::Module>) {
  const std::string text = TextOf(specifier);
  if (text == "dep.js") {
    g_link.dep_attribute_length = import_attributes->Length();
    if (import_attributes->Length() >= 2) {
      g_link.dep_type =
          TextOf(import_attributes->Get(1).As<v8::String>());
    }
    return v8::MaybeLocal<v8::Module>(g_link.dep.Get(v8::g_current_isolate));
  }
  if (text == "src.js") {
    if (import_attributes->Length() != 0) {
      return v8::MaybeLocal<v8::Module>();
    }
    return v8::MaybeLocal<v8::Module>(g_link.src.Get(v8::g_current_isolate));
  }
  if (text == "src2.js") {
    return v8::MaybeLocal<v8::Module>(g_link.src2.Get(v8::g_current_isolate));
  }
  return v8::MaybeLocal<v8::Module>();
}

// The source-phase answer: the object the `import source` binding takes.
v8::MaybeLocal<v8::Object> ResolveSource(v8::Local<v8::Context>,
                                         v8::Local<v8::String> specifier,
                                         v8::Local<v8::FixedArray>,
                                         v8::Local<v8::Module>) {
  if (TextOf(specifier) != "src.js") return v8::MaybeLocal<v8::Object>();
  ++g_link.source_calls;
  return v8::MaybeLocal<v8::Object>(g_link.source.Get(v8::g_current_isolate));
}

bool Compile(v8::Isolate* isolate, const char* name, const char* text,
             v8::Local<v8::Module>* out) {
  v8::Local<v8::Value> resource = Key(isolate, name);
  v8::ScriptOrigin origin(resource);
  v8::Source source(Key(isolate, text), origin);
  return v8::ScriptCompiler::CompileModule(isolate, &source).ToLocal(out);
}

}  // namespace

int main() {
  v8::V8::InitializeICUDefaultLocation(nullptr);
  v8::V8::Initialize();

  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator(
      v8::ArrayBuffer::Allocator::NewDefaultAllocator());
  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = allocator.get();
  v8::Isolate* isolate = v8::Isolate::New(params);

  {
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> context = v8::Context::New(isolate);
    v8::Context::Scope context_scope(context);

    v8::Local<v8::Module> dep;
    Check(Compile(isolate, "dep.js", "export const unused = 1;", &dep),
          "dep.js compiled");
    g_link.dep.Reset(isolate, dep);

    v8::Local<v8::Module> src;
    Check(Compile(isolate, "src.js", "export const unused = 2;", &src),
          "src.js compiled");
    g_link.src.Reset(isolate, src);

    // The host's Module Source Object: a plain object the test can recognise.
    v8::Local<v8::Object> host_source = v8::Object::New(isolate);
    Check(host_source
              ->Set(context, Key(isolate, "marker"),
                    v8::Integer::New(isolate, 7))
              .FromMaybe(false),
          "the host source object is writable");
    g_link.source.Reset(isolate, host_source);

    // A source-phase import, which a source text module cannot answer on its
    // own: the host's object is the only thing that can make this link.
    g_link.source_calls = 0;
    v8::Local<v8::Module> main;
    Check(Compile(isolate, "main.js",
                  "import source src from \"src.js\"; export const got = src;",
                  &main),
          "the source-phase import compiled");
    Check(main->InstantiateModule(context, &Resolve, &ResolveSource)
              .FromMaybe(false),
          "the source-phase graph linked");
    Check(g_link.source_calls == 1,
          "the source callback ran once for the source-phase request");

    v8::Local<v8::Value> evaluated;
    Check(main->Evaluate(context).ToLocal(&evaluated),
          "the source-phase module evaluated");
    v8::Local<v8::Object> namespace_object =
        main->GetModuleNamespace().As<v8::Object>();
    v8::Local<v8::Value> got;
    Check(namespace_object->Get(context, Key(isolate, "got")).ToLocal(&got),
          "the namespace carries got");
    Check(!got.IsEmpty() && got.raw() == host_source.raw(),
          "the source-phase binding is the host's object");
    Check(got->IsObject() &&
              got.As<v8::Object>()
                      ->Get(context, Key(isolate, "marker"))
                      .ToLocalChecked()
                      ->Int32Value(context)
                      .FromMaybe(0) == 7,
          "the bound object is the one the source callback answered");

    // The control: the same import without the source callback fails. A source
    // text module has no `%AbstractModuleSource%`, so the engine has nothing to
    // bind, which is exactly the gap the callback fills. `src2.js` is a
    // separate record, so the source registered above cannot answer it.
    v8::Local<v8::Module> src2;
    Check(Compile(isolate, "src2.js", "export const unused = 3;", &src2),
          "src2.js compiled");
    g_link.src2.Reset(isolate, src2);
    v8::Local<v8::Module> unsourced;
    Check(Compile(isolate, "unsourced.js",
                  "import source src from \"src2.js\"; export const got = src;",
                  &unsourced),
          "the unsourced import compiled");
    {
      v8::TryCatch caught(isolate);
      Check(!unsourced->InstantiateModule(context, &Resolve).FromMaybe(true),
            "a source-phase import without a source callback fails to link");
    }

    // An ordinary import with a `with` clause: the resolver learns the
    // attributes, and calling `InstantiateModule` with one argument uses the
    // default (no source callback).
    g_link.dep_attribute_length = -1;
    g_link.dep_type.clear();
    v8::Local<v8::Module> with_attributes;
    Check(Compile(isolate, "with-attributes.js",
                  "import \"dep.js\" with { type: \"js\" };\n"
                  "export const two = 2;",
                  &with_attributes),
          "the attributed import compiled");
    Check(with_attributes->InstantiateModule(context, &Resolve)
              .FromMaybe(false),
          "the attributed graph linked without a source callback");
    Check(g_link.dep_attribute_length == 3,
          "the resolver saw three words for one attribute");
    Check(g_link.dep_type == "js",
          "the resolver saw the attribute's value");
  }

  v8::V8::Dispose();
  if (g_failures != 0) {
    fprintf(stderr, "module-link: %d failure(s)\n", g_failures);
    return 1;
  }
  printf("module-link: ok\n");
  return 0;
}
