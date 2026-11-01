// A DevTools session over a context group, end to end: an inspector, a frame's
// two worlds, a session with a channel, the protocol's own commands and the
// events it answers with.
//
// This is the path a browser drives. Blink does not talk to the engine's debug
// points (see `debugger.cc`); it connects a `V8InspectorSession` for a frame,
// sends Chrome DevTools Protocol messages, and reads the events back. A frame
// is a context group and each of its worlds is a context, so the same URL
// compiles twice and one breakpoint covers both. Everything below the
// `v8_inspector` calls is `crates/inspector`.

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

// `marker` is a script-level `const`, so each world's stop is in its own
// script frame and the evaluation below reads a slot of it.
const char kSource[] =
    "const marker = 41;\n"
    "globalThis.hit = marker + 1;\n"
    "globalThis.hit;";

const int kGroup = 7;

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

// Print the events a call produced, answering whether one contains `wanted`.
bool Drain(RecordingChannel& channel, const char* wanted) {
  bool found = false;
  for (const std::string& event : channel.TakeNotifications()) {
    printf("  event: %s\n", event.c_str());
    if (event.find(wanted) != std::string::npos) found = true;
  }
  return found;
}

bool Compile(v8::Isolate* isolate, v8::Local<v8::Context> context,
             v8::Local<v8::Script>* out) {
  v8::Context::Scope scope(context);
  v8::Local<v8::String> source = v8::String::NewFromUtf8Literal(isolate, kSource);
  v8::Local<v8::String> name = v8::String::NewFromUtf8Literal(isolate, kUrl);
  v8::ScriptOrigin origin(name);
  return v8::Script::Compile(context, source, &origin).ToLocal(out);
}

// Answers null when the run completed, else the reason it did not.
const char* RunToStop(v8::Local<v8::Context> context,
                      v8::Local<v8::Script> script) {
  v8::Context::Scope scope(context);
  v8::Local<v8::Value> value;
  if (script->Run(context).ToLocal(&value)) return "the run did not stop";
  return context->Paused(nullptr) ? nullptr : "the run failed";
}

bool Contains(const std::vector<std::pair<int, std::string>>& responses,
              int id, const char* text) {
  for (const auto& response : responses) {
    if (response.first == id) {
      printf("  response %d: %s\n", id, response.second.c_str());
      return response.second.find(text) != std::string::npos;
    }
  }
  return false;
}

// Answers null on success, or the reason the demo failed.
const char* RunDemo(v8::Isolate* isolate, v8::Local<v8::Context> main_world,
                    v8::Local<v8::Context> isolated_world) {
  Client client;
  std::unique_ptr<v8_inspector::V8Inspector> inspector =
      v8_inspector::V8Inspector::create(isolate, &client);

  v8_inspector::V8ContextInfo main_info(main_world, kGroup, View("main"));
  main_info.origin = View("http://example.test");
  inspector->contextCreated(main_info);
  v8_inspector::V8ContextInfo world_info(isolated_world, kGroup, View("isolated"));
  world_info.origin = View("http://example.test");
  inspector->contextCreated(world_info);

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
    return "a second client on one group was accepted";
  }

  // Enabling the debugger is what attaches the engine, and it attaches every
  // context of the group: until then a run carries no debug points at all.
  session->dispatchProtocolMessage(
      View("{\"id\":1,\"method\":\"Debugger.enable\"}"));
  if (!Contains(channel.TakeResponses(), 1, "\"result\"")) {
    return "enable answered nothing";
  }

  v8::Local<v8::Script> main_script;
  v8::Local<v8::Script> world_script;
  if (!Compile(isolate, main_world, &main_script)) return "compile (main)";
  if (!Compile(isolate, isolated_world, &world_script)) return "compile (isolated)";

  // Each compile registered its script, and each announcement names the
  // context it was compiled in; the ids are the group's, so they are 1 and 2.
  session->pollEvents();
  printf("compiled the same URL in both worlds:\n");
  {
    bool first = false;
    bool second = false;
    for (const std::string& event : channel.TakeNotifications()) {
      printf("  event: %s\n", event.c_str());
      if (event.find("\"scriptId\":\"1\"") != std::string::npos &&
          event.find("\"executionContextId\":1") != std::string::npos) {
        first = true;
      }
      if (event.find("\"scriptId\":\"2\"") != std::string::npos &&
          event.find("\"executionContextId\":2") != std::string::npos) {
        second = true;
      }
    }
    if (!first || !second) return "a script was announced for the wrong world";
  }

  // One URL, compiled in both worlds: one breakpoint, one location per world.
  session->dispatchProtocolMessage(View(
      "{\"id\":2,\"method\":\"Debugger.setBreakpointByUrl\",\"params\":"
      "{\"url\":\"app.js\",\"lineNumber\":1,\"columnNumber\":0}}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1) return "the breakpoint answered nothing";
    printf("breakpoint: %s\n", answers[0].second.c_str());
    const size_t locations = answers[0].second.find("locations");
    if (locations == std::string::npos) return "the breakpoint resolved to nothing";
    // Two locations: one per world.
    const std::string after = answers[0].second.substr(locations);
    if (after.find("\"scriptId\":\"2\"") == std::string::npos) {
      return "the breakpoint did not resolve in the second world";
    }
  }

  // The main world runs and stops on its own script.
  if (const char* error = RunToStop(main_world, main_script)) return error;
  session->pollEvents();
  {
    auto events = channel.TakeNotifications();
    bool stopped = false;
    for (const std::string& event : events) {
      if (event.find("Debugger.paused") == std::string::npos) continue;
      printf("paused (main): %s\n", event.c_str());
      if (event.find("\"scriptId\":\"1\"") == std::string::npos) {
        return "the stop named the other world's script";
      }
      stopped = true;
    }
    if (!stopped) return "the main world's stop was not announced";
  }
  session->dispatchProtocolMessage(View(
      "{\"id\":3,\"method\":\"Debugger.evaluateOnCallFrame\",\"params\":"
      "{\"callFrameId\":\"frame-1\",\"expression\":\"marker + 1\"}}"));
  if (!Contains(channel.TakeResponses(), 3, "\"value\":42")) {
    return "the halted frame's binding was not in view";
  }
  session->dispatchProtocolMessage(View("{\"id\":4,\"method\":\"Debugger.resume\"}"));
  {
    auto answers = channel.TakeResponses();
    if (answers.size() != 1) return "the resume answered nothing";
    Drain(channel, "Debugger.resumed");
  }

  // The isolated world runs the same text and stops on the second script.
  if (const char* error = RunToStop(isolated_world, world_script)) return error;
  session->pollEvents();
  {
    auto events = channel.TakeNotifications();
    bool stopped = false;
    for (const std::string& event : events) {
      if (event.find("Debugger.paused") == std::string::npos) continue;
      printf("paused (isolated): %s\n", event.c_str());
      if (event.find("\"scriptId\":\"2\"") == std::string::npos) {
        return "the isolated world's stop named the first script";
      }
      stopped = true;
    }
    if (!stopped) return "the isolated world's stop was not announced";
  }
  session->dispatchProtocolMessage(View(
      "{\"id\":5,\"method\":\"Debugger.evaluateOnCallFrame\",\"params\":"
      "{\"callFrameId\":\"frame-1\",\"expression\":\"marker + 1\"}}"));
  if (!Contains(channel.TakeResponses(), 5, "\"value\":42")) {
    return "the isolated world's frame was not in view";
  }
  session->dispatchProtocolMessage(View("{\"id\":6,\"method\":\"Debugger.resume\"}"));
  channel.TakeResponses();
  Drain(channel, "Debugger.resumed");
  if (main_world->Paused(nullptr) || isolated_world->Paused(nullptr)) {
    return "a resumed run must not stay parked";
  }

  // The other half of the surface: running a host's own script in a context it
  // names, which is how a client installs what it exposes on the command line.
  auto injected = session->evaluate(main_world, View("globalThis.hit"));
  if (injected.type !=
      v8_inspector::V8InspectorSession::EvaluateResult::ResultType::kSuccess) {
    return "the injected evaluation did not run";
  }
  printf("evaluated in the main world: globalThis.hit = %d\n",
         injected.value->Int32Value(main_world).FromMaybe(-1));

  // A stop inside one session's lifetime is the case the close has to clear.
  session->stop();
  if (main_world->Paused(nullptr) || isolated_world->Paused(nullptr)) {
    return "closing a session left a run parked";
  }
  inspector->contextDestroyed(isolated_world);
  inspector->contextDestroyed(main_world);
  inspector->resetContextGroup(kGroup);
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
    v8::Local<v8::Context> main_world = v8::Context::New(isolate);
    // The isolated world a browser makes for an extension or for DevTools
    // itself: a context of the same frame, so of the same group.
    v8::Local<v8::Context> isolated_world = v8::Context::New(isolate);

    if (const char* error = RunDemo(isolate, main_world, isolated_world)) {
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
