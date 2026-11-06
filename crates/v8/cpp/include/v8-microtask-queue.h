#ifndef INCLUDE_V8_MICROTASK_QUEUE_H_
#define INCLUDE_V8_MICROTASK_QUEUE_H_
/* `MicrotaskQueue` and `MicrotasksPolicy` live in `v8.h`; this file exists
 * because a host includes it by name (Blink's `Context::GetMicrotaskQueue()`
 * callers do). `MicrotasksScope` is here too, upstream. */
#include "v8.h"
#endif
