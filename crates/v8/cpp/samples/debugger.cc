// The facade's debugger surface, end to end: a session, a breakpoint resolved
// from the script text, the stop, and the resume.
//
// Not a V8 sample. V8 exposes this machinery through the inspector protocol,
// which is A5's cost (see the README's Debugger section); the facade exposes the
// engine's own protocol so the path can be demonstrated before the inspector
// exists.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <crtdbg.h>

namespace {
// Armed before `main`, so an abort raised while the first globals are built is
// reported on stderr instead of a modal box.
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

#include "include/libplatform/libplatform.h"
#include "include/v8-context.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"

namespace {

// A call-free script is the case where the engine keeps its `var`s in the
// halted frame rather than on the global object, so the evaluation below shows
// both halves of `EvaluateOnPausedFrame`: it reads the slot, and an assignment
// reaches the slot the resumed script then reads.
const char kSource[] =
    "var counter = 41;\n"
    "counter = counter + 1;\n"
    "counter;";

const char kBreakpoint[] = "counter = counter + 1;";

// Answers null on success, or the reason the demo failed.
const char* RunDemo(v8::Isolate* isolate, v8::Local<v8::Context> context) {
  context->SetDebugAttached(true);
  const char* at = strstr(kSource, kBreakpoint);
  if (at == nullptr) return "the breakpoint text moved";
  uint32_t offset = static_cast<uint32_t>(at - kSource);
  context->SetBreakpoints(&offset, 1);

  v8::Local<v8::String> source =
      v8::String::NewFromUtf8Literal(isolate, kSource);
  v8::Local<v8::Script> script;
  if (!v8::Script::Compile(context, source).ToLocal(&script)) return "compile";

  {
    (void)script->Run(context);
    v8::Context::PauseInfo pause;
    if (!context->Paused(&pause)) return "the breakpoint did not stop the run";
    if (pause.reason != v8::Context::PauseInfo::kBreakpoint) {
      return "the stop was not a breakpoint";
    }
    if (pause.start != offset) return "the stop named the wrong position";
    printf("paused: breakpoint at offset %u\n", pause.start);

    /* Evaluate at the stop: `counter` is a frame slot of the halted script,
     * so the expression reads it and the assignment writes the slot the script
     * will use when it continues. */
    v8::Local<v8::Value> probed;
    v8::Local<v8::String> question =
        v8::String::NewFromUtf8Literal(isolate, "counter + 1");
    if (!context->EvaluateOnPausedFrame(question).ToLocal(&probed)) {
      return "evaluate on the paused frame";
    }
    int32_t answer = probed->Int32Value(context).FromMaybe(0);
    printf("evaluated: counter + 1 = %d\n", answer);
    if (answer != 42) return "the halted frame's slot was not in view";

    v8::Local<v8::String> assignment =
        v8::String::NewFromUtf8Literal(isolate, "(counter = 100, counter)");
    if (!context->EvaluateOnPausedFrame(assignment).ToLocal(&probed)) {
      return "assign on the paused frame";
    }
    printf("evaluated: counter = %d\n", probed->Int32Value(context).FromMaybe(0));
  }

  v8::Local<v8::Value> value;
  v8::MaybeLocal<v8::Value> resumed = context->Resume();
  if (!resumed.ToLocal(&value)) return "resume";
  int32_t completed = value->Int32Value(context).FromMaybe(0);
  printf("resumed: the script completed with %d\n", completed);
  if (completed != 101) {
    return "the resumed run did not read the slot the evaluation wrote";
  }
  if (context->Paused(nullptr)) return "a finished run must not stay parked";
  return nullptr;
}

}  // namespace

int main(int argc, char* argv[]) {
  if (!v8::V8::InitializeICUDefaultLocation(argv[0])) {
    fprintf(stderr, "Failed to initialize ICU\n");
    return 1;
  }
  v8::V8::InitializeExternalStartupData(argv[0]);
  std::unique_ptr<v8::Platform> platform = v8::platform::NewDefaultPlatform();
  v8::V8::InitializePlatform(platform.get());
  v8::V8::Initialize();

  v8::Isolate::CreateParams create_params;
  create_params.array_buffer_allocator =
      v8::ArrayBuffer::Allocator::NewDefaultAllocator();
  v8::Isolate* isolate = v8::Isolate::New(create_params);
  int status = 0;
  {
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope handle_scope(isolate);
    v8::Local<v8::Context> context = v8::Context::New(isolate);
    v8::Context::Scope context_scope(context);

    if (const char* error = RunDemo(isolate, context)) {
      fprintf(stderr, "debugger: %s\n", error);
      status = 1;
    }
  }

  isolate->Dispose();
  v8::V8::Dispose();
  v8::V8::DisposePlatform();
  delete create_params.array_buffer_allocator;
  return status;
}
