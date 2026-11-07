#ifndef INCLUDE_V8_CALLBACKS_H_
#define INCLUDE_V8_CALLBACKS_H_

/*
 * V8's callback declarations. The one this facade provides is
 * `ModuleImportPhase`, which Blink's module machinery names; it is defined in
 * `v8.h`, the single implementation header every facade header redirects to.
 *
 * The rest of V8 15's `v8-callbacks.h` — the OOM, JIT-code-event, fatal-error,
 * deserialization, and internal-field callbacks — arrives with the `Isolate`
 * handler cluster, which the facade does not expose yet.
 */
#include "v8.h"

#endif  // INCLUDE_V8_CALLBACKS_H_
