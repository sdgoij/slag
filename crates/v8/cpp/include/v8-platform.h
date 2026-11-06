#ifndef INCLUDE_V8_PLATFORM_H_
#define INCLUDE_V8_PLATFORM_H_

/*
 * The version-independent half of V8's platform surface: the abstractions a
 * host or `libplatform` implements, which Blink's `gin/v8_platform.cc` and
 * `v8_initializer.cc` include by name.
 *
 * What is deliberately not here: `Platform`'s own virtual set. It moves between
 * milestones — `PumpMessageLoop` lost its `Platform*` in V8 11, and
 * worker-thread posting, `SourceLocation` and `TaskPriority` moved with it — and
 * the facade follows the pre-11 shape (see `v8-version.h`). Declaring a guessed
 * set would make a host's `override` fail with a misleading error; the pin
 * (`.notes/browser.md` §14.1, item 1) is what settles it.
 */

#include <cstddef>
#include <cstdint>
#include <memory>
#include <utility>

#include "v8.h"

namespace v8 {

/* Scheduling priority of a task, V8 10's two-level set. */
enum class TaskPriority { kBestEffort, kUserBlocking };

/* Whether `PumpMessageLoop` may block waiting for work. */
enum class MessageLoopBehavior : bool { kDoNotWait = false, kWaitForWork = true };

class IdleTask {
 public:
  virtual ~IdleTask() = default;
  /* `deadline_in_seconds` is the monotonic time by which the task must yield. */
  virtual void Run(double deadline_in_seconds) = 0;
};

class Task {
 public:
  virtual ~Task() = default;
  virtual void Run() = 0;
};

class TaskRunner {
 public:
  virtual ~TaskRunner() = default;
  virtual void PostTask(std::unique_ptr<Task> task) = 0;
  virtual void PostDelayedTask(std::unique_ptr<Task> task,
                               double delay_in_seconds) = 0;
  virtual void PostIdleTask(std::unique_ptr<IdleTask> task) = 0;
  virtual bool IdleTasksEnabled() = 0;
};

class JobHandle {
 public:
  virtual ~JobHandle() = default;
  virtual void Join() = 0;
  virtual void Cancel() = 0;
};

/*
 * The page granularity the engine's code space and heap reservations ask a host
 * for. Blink supplies this from PartitionAlloc; the facade does not allocate
 * executable or reserved pages itself yet.
 */
class PageAllocator {
 public:
  static const size_t kSystemPageSize = 4096;
  static const size_t kPageAllocationGranularityShift = 16;
  static const size_t kPageAllocationGranularity = 1u << kPageAllocationGranularityShift;
  static const size_t kPageAllocationGranularityOffsetMask =
      kPageAllocationGranularity - 1;
  static const size_t kPageAllocationGranularityBaseMask =
      ~kPageAllocationGranularityOffsetMask;

  enum Permission {
    kNoAccess,
    kRead,
    kReadWrite,
    kReadWriteExecute,
    kReadExecute,
  };

  virtual ~PageAllocator() = default;
  virtual void* AllocatePages(void* address, size_t length, size_t alignment,
                              Permission permissions) = 0;
  virtual bool FreePages(void* address, size_t length) = 0;
  virtual bool ReleasePages(void* address, size_t length, size_t new_length) = 0;
  virtual bool SetPermissions(void* address, size_t length,
                              Permission permissions) = 0;
  virtual bool DiscardSystemPages(void* address, size_t size) { return false; }
};

/*
 * The trace-event sink `libplatform` owns by default and a host may replace.
 * Only the two calls every embedder makes are declared: a host that overrides
 * more will not compile until the pin settles the rest, which is the honest
 * failure rather than a silently mismatched vtable.
 */
class TracingController {
 public:
  virtual ~TracingController() = default;
  virtual const uint8_t* GetCategoryGroupEnabled(const char* name);
  virtual const char* GetCategoryGroupName(const uint8_t* category_group_enabled);
};

}  // namespace v8

#endif
