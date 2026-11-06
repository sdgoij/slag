// `v8::MicrotaskQueue` against the engine's host-drained queues: a context
// bound to a queue sends its realm's promise jobs there, so nothing runs until
// the host asks, and `PerformCheckpoint` runs exactly that queue.
//
// V8 15 makes the queue a `cppgc::GarbageCollected`, so this facade's queue is
// an ordinary object the embedder owns (see the class comment). The checks here
// are the parts that are the same either way: the policy round-trips, the
// checkpoint is what runs the jobs, and the completed callback fires even when
// the queue was empty.

#include <stdio.h>

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

#include "include/v8-context.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-microtask-queue.h"
#include "include/v8-object.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"
#include "include/v8-value.h"

namespace {

int g_failures = 0;

void Check(bool ok, const char* what) {
  if (!ok) {
    fprintf(stderr, "FAILED: %s\n", what);
    ++g_failures;
  }
}

void OnMicrotasksCompleted(v8::Isolate*, void* data) {
  ++*static_cast<int*>(data);
}

bool Evaluate(v8::Isolate* isolate, v8::Local<v8::Context> context,
              const char* source) {
  v8::Local<v8::String> text = v8::String::NewFromUtf8Literal(isolate, source);
  v8::Local<v8::Value> result =
      v8::Script::Compile(context, text).ToLocalChecked()
          ->Run(context)
          .ToLocalChecked();
  return result->Int32Value(context).FromMaybe(0) == 1;
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

    // A queue the host owns, bound to a context that sends its promise jobs
    // there. `kExplicit` is what makes the test meaningful: nothing drains the
    // queue except the checkpoint below.
    v8::MicrotaskQueue* queue =
        v8::MicrotaskQueue::New(isolate, v8::MicrotasksPolicy::kExplicit);
    Check(queue != nullptr, "MicrotaskQueue::New answers a queue");
    Check(queue->GetMicrotasksPolicy() == v8::MicrotasksPolicy::kExplicit,
          "the policy the queue was made with reads back");
    queue->SetMicrotasksPolicy(v8::MicrotasksPolicy::kAuto);
    Check(queue->GetMicrotasksPolicy() == v8::MicrotasksPolicy::kAuto,
          "SetMicrotasksPolicy round-trips");
    queue->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);

    int completed = 0;
    queue->AddMicrotasksCompletedCallback(&OnMicrotasksCompleted, &completed);

    v8::Local<v8::Context> context =
        v8::Context::New(isolate, nullptr, v8::Local<v8::ObjectTemplate>(), queue);
    v8::Context::Scope context_scope(context);
    Check(context->GetMicrotaskQueue() == queue,
          "the context answers the queue it was created with");

    // A promise reaction queued by the script stays queued: the realm's jobs go
    // to the host queue, and nothing drains it here.
    Check(Evaluate(isolate, context,
                   "globalThis.ran = 0;"
                   "Promise.resolve().then(function () { globalThis.ran = 1; });"
                   "1"),
          "the script ran");
    Check(!Evaluate(isolate, context, "globalThis.ran ? 1 : 0"),
          "an explicit host queue is not drained by the run");

    Check(!queue->IsRunningMicrotasks(), "not running before a checkpoint");
    queue->PerformCheckpoint(isolate);
    Check(Evaluate(isolate, context, "globalThis.ran ? 1 : 0"),
          "PerformCheckpoint runs the queued reaction");
    Check(completed == 1, "the completed callback fired once");
    Check(!queue->IsRunningMicrotasks(), "not running after a checkpoint");

    // The callback fires on an empty queue too, which is V8's contract and what
    // a host uses to notice the queue is quiet.
    queue->PerformCheckpoint(isolate);
    Check(completed == 2, "the completed callback fires on an empty queue");
    queue->RemoveMicrotasksCompletedCallback(&OnMicrotasksCompleted, &completed);
    queue->PerformCheckpoint(isolate);
    Check(completed == 2, "a removed callback does not fire");

    // A context made without a queue owns one of its own, and it is not null.
    v8::Local<v8::Context> plain = v8::Context::New(isolate);
    Check(plain->GetMicrotaskQueue() != nullptr,
          "a default context has its own queue");

    delete queue;
  }

  v8::V8::Dispose();
  if (g_failures != 0) {
    fprintf(stderr, "microtasks: %d failure(s)\n", g_failures);
    return 1;
  }
  printf("microtasks: ok\n");
  return 0;
}
