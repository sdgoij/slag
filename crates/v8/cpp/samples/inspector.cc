// A DevTools session over the inspector facade, end to end: an inspector, a
// context group, a session with a channel, the protocol's own commands and the
// events it answers with.
//
// This is the path a browser drives. Blink does not talk to the engine's debug
// points (see `debugger.cc`); it connects a `V8InspectorSession`, sends Chrome
// DevTools Protocol messages, and reads the events back. Everything below the
// `v8_inspector` calls is `crates/inspector`, which is also what a Rust host
// uses.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <memory>
#include <string>
#include <utility>
#include <vector>

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
#include "include/v8-inspector.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"

namespace {

const char kUrl[] = "app.js";

// `marker` is a script-level `const`, so the stop's frame is the script's own
// and the evaluation below reads a slot of it.
const char kSource[] =
    "const marker = 41;\n"
    "globalThis.hit = marker + 1;\n"
    "globalThis.hit;";

v8_inspector::StringView View(const char* text) {
  return v8_inspector::StringView(
      reinterpret_cast<const uint8_t*>(text), strlen(text));
}

std::string Utf8(v8_inspector::StringView text) {
  // `StringBuffer::create` answers UTF-8, so only that form reaches a channel.
  if (!text.is8Bit()) return std::string();
  return std::string(reinterpret_cast<const char*>(text.characters8()),
                     text.length());
}

// A channel that records what the session sends.
//
// It answers nothing from inside a callback. A callback runs inside the
// session's own call, where the session's state is already borrowed, so calling
// back into it there is refused; the loop dispatches after the call returns.
// A browser's channel forwards to its frontend before the frontend answers for
// the same reason.
class RecordingChannel final : public v8_inspector::V8Inspector::Channel {
 public:
  void sendResponse(int call_id,
                    std::unique_ptr<v8_inspector::StringBuffer> message) override {
    responses.emplace_back(call_id, Utf8(message->string()));
  }

  void sendNotification(
      std::unique_ptr<v8_inspector::StringBuffer> message) override {
    notifications.push_back(Utf8(message->string()));
  }

  void flushProtocolNotifications() override {}

  std::vector<std::pair<int, std::string>> TakeResponses() {
    return std::exchange(responses, {});
  }

  std::vector<std::string> TakeNotifications() {
    return std::exchange(notifications, {});
  }

  std::vector<std::pair<int, std::string>> responses;
  std::vector<std::string> notifications;
};

class Client final : public v8_inspector::V8InspectorClient {};

// Answers null on success, or the reason the demo failed.
const char* RunDemo(v8::Isolate* isolate, v8::Local<v8::Context> context) {
  Client client;
  std::unique_ptr<v8_inspector::V8Inspector> inspector =
      v8_inspector::V8Inspector::create(isolate, &client);

  const int kGroup = 7;
  v8_inspector::V8ContextInfo info(context, kGroup, View("app"));
  info.origin = View("http://example.test");
  inspector->contextCreated(info);

  if (!v8_inspector::V8InspectorSession::canDispatchMethod(View("Debugger.enable"))) {
    return "the session does not claim Debugger.enable";
  }
  if (v8_inspector::V8InspectorSession::canDispatchMethod(View("Page.reload"))) {
    return "the session claimed a domain it does not have";
  }

  RecordingChannel channel;
  std::unique_ptr<v8_inspector::V8InspectorSession> session =
      inspector->connect(kGroup, &channel);
  if (!session) return "connect";
  if (inspector->connect(kGroup, &channel)) {
    return "a second session on one context was accepted";
  }

  // Enabling the debugger is what attaches the engine: until then a run
  // carries no debug points at all.
  session->dispatchProtocolMessage(
      View("{\"id\":1,\"method\":\"Debugger.enable\"}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1 || answers[0].first != 1) return "enable answered nothing";
    printf("enabled: %s\n", answers[0].second.c_str());
  }

  v8::Local<v8::String> source = v8::String::NewFromUtf8Literal(isolate, kSource);
  v8::Local<v8::String> name = v8::String::NewFromUtf8Literal(isolate, kUrl);
  v8::ScriptOrigin origin(name);
  v8::Local<v8::Script> script;
  if (!v8::Script::Compile(context, source, &origin).ToLocal(&script)) {
    return "compile";
  }

  // The compile registered the script with the session, so a URL breakpoint
  // now has something to resolve against.
  session->dispatchProtocolMessage(View(
      "{\"id\":2,\"method\":\"Debugger.setBreakpointByUrl\",\"params\":"
      "{\"url\":\"app.js\",\"lineNumber\":1,\"columnNumber\":0}}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1) return "the breakpoint answered nothing";
    if (answers[0].second.find("breakpointId") == std::string::npos) {
      return "the breakpoint resolved to no id";
    }
    printf("breakpoint: %s\n", answers[0].second.c_str());
  }
  // The script announcement was queued by the compile and reached the channel
  // with that dispatch's poll.
  for (const std::string& event : channel.TakeNotifications()) {
    printf("event: %s\n", event.c_str());
  }

  v8::MaybeLocal<v8::Value> ran = script->Run(context);
  v8::Local<v8::Value> completed;
  if (ran.ToLocal(&completed)) return "the breakpoint did not stop the run";
  v8::Context::PauseInfo pause;
  if (!context->Paused(&pause)) return "the run is not parked";

  // The drain a host's pause loop makes: V8 announces the stop from inside the
  // run, this facade leaves the loop to the host.
  session->pollEvents();
  {
    auto events = channel.TakeNotifications();
    bool announced = false;
    for (const std::string& event : events) {
      printf("event: %s\n", event.c_str());
      if (event.find("Debugger.paused") != std::string::npos) {
        if (event.find("callFrames") == std::string::npos) {
          return "the stop announced no call frames";
        }
        announced = true;
      }
    }
    if (!announced) return "the stop was not announced";
  }

  // `callFrameId` names one of the frames `Debugger.paused` listed: the
  // innermost parked activation.
  session->dispatchProtocolMessage(View(
      "{\"id\":3,\"method\":\"Debugger.evaluateOnCallFrame\",\"params\":"
      "{\"callFrameId\":\"frame-1\",\"expression\":\"marker + 1\"}}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1) return "the evaluation answered nothing";
    if (answers[0].second.find("\"value\":42") == std::string::npos) {
      return "the halted frame's binding was not in view";
    }
    printf("evaluated: %s\n", answers[0].second.c_str());
  }

  session->dispatchProtocolMessage(
      View("{\"id\":4,\"method\":\"Debugger.resume\"}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1) return "the resume answered nothing";
  }
  {
    auto events = channel.TakeNotifications();
    bool resumed = false;
    for (const std::string& event : events) {
      printf("event: %s\n", event.c_str());
      if (event.find("Debugger.resumed") != std::string::npos) resumed = true;
    }
    if (!resumed) return "the resume announced nothing";
  }
  if (context->Paused(nullptr)) return "a resumed run must not stay parked";

  // A stop inside the session's own lifetime is the case the close has to
  // clear: the run continues and the engine detaches.
  session->stop();
  inspector->contextDestroyed(context);
  if (context->Paused(nullptr)) return "closing a session left the run parked";

  v8::Local<v8::Value> hit;
  v8::Local<v8::String> question =
      v8::String::NewFromUtf8Literal(isolate, "globalThis.hit");
  if (!v8::Script::Compile(context, question).ToLocal(&script)) {
    return "probe compile";
  }
  if (!script->Run(context).ToLocal(&hit)) return "probe run";
  const int32_t produced = hit->Int32Value(context).FromMaybe(0);
  printf("after resuming, globalThis.hit = %d\n", produced);
  if (produced != 42) return "the resumed run did not finish the statement";
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
      fprintf(stderr, "inspector: %s\n", error);
      status = 1;
    }
  }

  isolate->Dispose();
  v8::V8::Dispose();
  v8::V8::DisposePlatform();
  delete create_params.array_buffer_allocator;
  return status;
}
