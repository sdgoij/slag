// The module-request surface Chromium reaches for: `Module::GetModuleRequests`
// answering a `v8::FixedArray` of `v8::ModuleRequest`, and each request's
// specifier, phase, source offset, and import attributes (Blink's
// `ModuleRecord::ModuleRequests` and `ToBlinkImportAttributes` read exactly
// these).
//
// The `Data` half is a compile-time and run-time statement: a `Module`, a
// `FixedArray`, and a `ModuleRequest` are `v8::Data`, and `Local<Data>` over
// them reports what they are — which the facade answers from its own record of
// the objects it minted, not from the handle word.

#include <stdio.h>
#include <string.h>

#include <memory>

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

#include "include/v8-callbacks.h"
#include "include/v8-context.h"
#include "include/v8-data.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
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

bool Equals(v8::Isolate* isolate, v8::Local<v8::String> text, const char* want) {
  v8::String::Utf8Value utf8(isolate, text);
  return *utf8 != nullptr && strcmp(*utf8, want) == 0;
}

// Two side-effect imports, the second with an import attribute. The offset of
// the second specifier is what `GetSourceOffset` must report, and the quotes
// are part of the literal's span.
const char kSource[] =
    "import \"a.js\";\n"
    "import \"b.js\" with { type: \"json\" };\n";

}  // namespace

int main() {
  // V8 15's ordering: source, defer, evaluation.
  static_assert(static_cast<int>(v8::ModuleImportPhase::kSource) == 0);
  static_assert(static_cast<int>(v8::ModuleImportPhase::kDefer) == 1);
  static_assert(static_cast<int>(v8::ModuleImportPhase::kEvaluation) == 2);

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

    v8::Local<v8::Value> name =
        v8::String::NewFromUtf8Literal(isolate, "main.js");
    v8::ScriptOrigin origin(name);
    v8::Local<v8::String> text = v8::String::NewFromUtf8Literal(isolate, kSource);
    v8::Source source(text, origin);
    v8::Local<v8::Module> module;
    Check(v8::ScriptCompiler::CompileModule(isolate, &source).ToLocal(&module),
          "the module compiled");
    if (module.IsEmpty()) {
      fprintf(stderr, "module-requests: nothing to inspect\n");
      return 1;
    }

    // `Data`'s identity predicates, over the module and its requests array.
    v8::Local<v8::Data> module_data = module;
    Check(module_data->IsModule(), "a Module reports IsModule");
    Check(!module_data->IsValue(), "a Module is data, not a value");
    Check(!module_data->IsFixedArray(), "a Module is not a FixedArray");

    v8::Local<v8::FixedArray> requests = module->GetModuleRequests();
    Check(!requests.IsEmpty(), "GetModuleRequests answered a handle");
    Check(requests->Length() == 2, "the module declares two requests");
    Check(module->GetModuleRequests().raw() == requests.raw(),
          "GetModuleRequests answers the same array on every call");

    v8::Local<v8::Data> requests_data = requests;
    Check(requests_data->IsFixedArray(), "a FixedArray reports IsFixedArray");
    Check(!requests_data->IsValue(), "a FixedArray is data, not a value");
    Check(!requests_data->IsCppHeapExternal(),
          "nothing here is a cppgc external");
    Check(!requests_data->IsDictionaryTemplate(),
          "the facade has no DictionaryTemplate");

    // A request element is a `ModuleRequest`, not a value and not an array.
    v8::Local<v8::Data> first_data = requests->Get(0);
    Check(first_data->IsModuleRequest(), "Get(0) is a ModuleRequest");
    Check(!first_data->IsFixedArray(), "Get(0) is not a FixedArray");
    Check(!first_data->IsValue(), "a ModuleRequest is data, not a value");

    v8::Local<v8::ModuleRequest> first = first_data.As<v8::ModuleRequest>();
    Check(Equals(isolate, first->GetSpecifier(), "a.js"),
          "the first specifier is a.js");
    Check(first->GetPhase() == v8::ModuleImportPhase::kEvaluation,
          "an ordinary import is the evaluation phase");
    Check(first->GetImportAttributes()->Length() == 0,
          "a.js declares no attributes");
    const char* first_offset = strstr(kSource, "\"a.js\"");
    Check(first_offset != nullptr &&
              first->GetSourceOffset() ==
                  static_cast<int>(first_offset - kSource),
          "the first offset is the specifier literal");

    // The attributed import: specifier, offset, position, and the three-word
    // attribute encoding.
    v8::Local<v8::ModuleRequest> second =
        requests->Get(1).As<v8::ModuleRequest>();
    Check(Equals(isolate, second->GetSpecifier(), "b.js"),
          "the second specifier is b.js");
    Check(second->GetPhase() == v8::ModuleImportPhase::kEvaluation,
          "the attributed import is still the evaluation phase");

    const char* second_offset = strstr(kSource, "\"b.js\"");
    Check(second_offset != nullptr &&
              second->GetSourceOffset() ==
                  static_cast<int>(second_offset - kSource),
          "GetSourceOffset is the specifier literal, quotes included");
    v8::Location location =
        module->SourceOffsetToLocation(second->GetSourceOffset());
    Check(location.GetLineNumber() == 1 && location.GetColumnNumber() == 7,
          "SourceOffsetToLocation maps the offset to line 1, column 7");

    v8::Local<v8::FixedArray> attributes = second->GetImportAttributes();
    Check(attributes->Length() == 3, "one attribute is three array elements");
    Check(Equals(isolate, attributes->Get(0).As<v8::String>(), "type"),
          "the attribute key is type");
    Check(Equals(isolate, attributes->Get(1).As<v8::String>(), "json"),
          "the attribute value is json");
    Check(attributes->Get(2).As<v8::Int32>()->Value() ==
              second->GetSourceOffset(),
          "the attribute offset is the request's");
    Check(second->GetImportAssertions().raw() == attributes.raw(),
          "GetImportAssertions aliases GetImportAttributes");
  }

  v8::V8::Dispose();
  if (g_failures != 0) {
    fprintf(stderr, "module-requests: %d failure(s)\n", g_failures);
    return 1;
  }
  printf("module-requests: ok\n");
  return 0;
}
