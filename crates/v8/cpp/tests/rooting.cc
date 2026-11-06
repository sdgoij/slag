// E2 of `.notes/browser.md` §9.5, in the form the facade can answer it: a
// `Local` that exists only in an outer frame while an out-of-line call
// allocates and collects.
//
// The design claim is that `Local<T>` is an indirect handle — a pointer to a
// slot in a thread-local handle stack that `slagc` registers as a root — so the
// value survives without the conservative stack scan seeing anything. At /O2
// the compiler may keep the handle's slot pointer in a callee-saved register
// across the call, which is exactly the case a scan cannot cover and a slot
// does not need. Run this in the Release configuration: a Debug build can leave
// the value in a stack slot and the experiment proves nothing.
//
// The last two blocks are the control. An open `HandleScope` roots exactly one
// value and closing it releases that root, so the survival above is the slot's
// doing rather than luck; and a bare tagged word roots nothing, which is the
// aliasing hazard `crates/crux/src/heap.rs` names. The bare-word block asserts
// the root count rather than reading the arena through the word, because that
// read would be undefined behaviour rather than a failed check.

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <memory>

#ifdef _WIN32
#include <crtdbg.h>

namespace {
// Armed before `main`, so an abort raised inside the engine is reported on
// stderr instead of waiting for a click on a modal box.
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

#if defined(_MSC_VER)
#define SLAG_NOINLINE __declspec(noinline)
#else
#define SLAG_NOINLINE __attribute__((noinline))
#endif

namespace {

// Allocates enough to sweep, then collects. No parameter names the caller's
// string: the only edge to it is the handle-scope slot.
SLAG_NOINLINE void ChurnAndCollect(v8::Isolate* isolate, SlagContext* slag) {
  for (int round = 0; round < 2000; ++round) {
    v8::HandleScope scope(isolate);
    v8::String::NewFromUtf8Literal(isolate, "noise that fills the arena");
  }
  if (slag_collect(slag) != 0) {
    fprintf(stderr, "collection failed\n");
  }
}

// A weak `Persistent` has to be made in a frame that returns, and the stack
// scrubbed, or the conservative scan keeps the value alive — the same shape
// `gin-shapes` uses for its weak-callback test.
void IgnoredWeakCallback(const v8::WeakCallbackInfo<int>&) {}

SLAG_NOINLINE void MakeWeakPersistentWithCallback(
    v8::Isolate* isolate, v8::Persistent<v8::Object>* weak, int* parameter) {
  v8::HandleScope scope(isolate);
  weak->Reset(isolate, v8::Object::New(isolate));
  weak->SetWeak(parameter, &IgnoredWeakCallback,
                v8::WeakCallbackType::kParameter);
}

SLAG_NOINLINE void ScrubStack(int depth) {
  volatile uint64_t pad[16] = {};
  if (depth > 0) ScrubStack(depth - 1);
  (void)pad[0];
}

}  // namespace

int main() {
  setvbuf(stdout, nullptr, _IONBF, 0);
  v8::V8::InitializeICUDefaultLocation(nullptr);
  v8::V8::Initialize();

#ifdef NDEBUG
  printf("rooting: configured optimised (NDEBUG)\n");
#else
  printf("rooting: configured UNOPTIMISED — the survival check proves nothing\n");
#endif

  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator(
      v8::ArrayBuffer::Allocator::NewDefaultAllocator());
  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = allocator.get();
  v8::Isolate* isolate = v8::Isolate::New(params);

  int failures = 0;
  {
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> context = v8::Context::New(isolate);
    v8::Context::Scope context_scope(context);
    SlagContext* slag = context->slag();

    const size_t base = slag_root_count();
    const char kText[] = "survives an out-of-line collection";

    // The experiment: the string is named by nothing but this frame's handle.
    {
      v8::HandleScope outer(isolate);
      v8::Local<v8::String> held = v8::String::NewFromUtf8Literal(isolate, kText);
      ChurnAndCollect(isolate, slag);
      v8::String::Utf8Value text(isolate, held);
      if (*text == nullptr || strcmp(*text, kText) != 0) {
        fprintf(stderr, "a handle-scope Local did not survive the collection\n");
        ++failures;
      }
    }

    // The slot is what rooted it: one value while the scope is open, none
    // after it closes.
    {
      v8::HandleScope scoped(isolate);
      v8::Local<v8::String> temp =
          v8::String::NewFromUtf8Literal(isolate, "scoped");
      const long long rooted = static_cast<long long>(slag_root_count()) -
                               static_cast<long long>(base);
      if (rooted != 1) {
        fprintf(stderr, "an open scope rooted %lld values, expected 1\n", rooted);
        ++failures;
      }
    }
    if (slag_root_count() != base) {
      const long long leaked = static_cast<long long>(slag_root_count()) -
                               static_cast<long long>(base);
      fprintf(stderr, "a closed scope left %lld roots, expected 0\n", leaked);
      ++failures;
    }

    // The control: a bare word in a C++ local roots nothing.
    {
      uint64_t word = 0;
      {
        v8::HandleScope scoped(isolate);
        v8::Local<v8::String> temp =
            v8::String::NewFromUtf8Literal(isolate, "bare");
        word = temp.raw();
      }
      ChurnAndCollect(isolate, slag);
      if (slag_root_count() != base) {
        fprintf(stderr, "a bare tagged word rooted a value\n");
        ++failures;
      }
      if (word == 0) {
        fprintf(stderr, "the bare word was empty, so the control proved nothing\n");
        ++failures;
      }
    }

    // `Persistent`: the deprecated handle V8 keeps for pre-`Global` code. It is
    // a `Global` here, so the same retention has to hold it up — an object made
    // in a frame that returns, with the stack scrubbed, so nothing else does.
    {
      v8::Persistent<v8::Object> persistent;
      if (!persistent.IsEmpty()) {
        fprintf(stderr, "a default Persistent is not empty\n");
        ++failures;
      }
      {
        v8::HandleScope scoped(isolate);
        persistent.Reset(isolate, v8::Object::New(isolate));
      }
      ScrubStack(16);
      isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
      if (persistent.IsEmpty()) {
        fprintf(stderr, "a Persistent did not survive the collection\n");
        ++failures;
      }
      persistent.Reset();
      if (!persistent.IsEmpty()) {
        fprintf(stderr, "Reset() left the Persistent non-empty\n");
        ++failures;
      }
    }

    // A weak `Persistent` empties when the collector takes its value, which is
    // what Blink asserts of the persistents it hands to `SetWeak`. The
    // callback form is the shape `gin-shapes` proves; the zero-argument
    // `SetWeak()` is the same call without a listener, and whether the engine
    // empties that one is still open — recorded rather than asserted.
    {
      v8::Persistent<v8::Object> weak;
      int parameter = 0;
      MakeWeakPersistentWithCallback(isolate, &weak, &parameter);
      if (!weak.IsWeak()) {
        fprintf(stderr, "SetWeak() left the Persistent strong\n");
        ++failures;
      }
      ScrubStack(16);
      isolate->RequestGarbageCollectionForTesting(v8::kFullGarbageCollection);
      if (!weak.IsEmpty()) {
        fprintf(stderr, "a weak Persistent kept its value alive\n");
        ++failures;
      }
    }
  }

  v8::V8::Dispose();
  if (failures != 0) {
    fprintf(stderr, "rooting: %d failure(s)\n", failures);
    return 1;
  }
  printf("rooting: ok\n");
  return 0;
}
