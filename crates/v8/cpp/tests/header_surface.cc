// The compile-only half of Stage 0 (`.notes/browser.md` §9.2): every header the
// facade provides, included by the name a host uses. `surface_census` counts
// files; this target is what proves they compile — a redirect with a wrong
// guard, a header that needs something it does not include, or a declaration
// that collides with `v8.h` fails here rather than in a host's build.
//
// It has no `main`: it is an object target, so it costs a compile and never
// runs.

#include "include/v8-array-buffer.h"
#include "include/v8-container.h"
#include "include/v8-context.h"
#include "include/v8-data.h"
#include "include/v8-exception.h"
#include "include/v8-external.h"
#include "include/v8-forward.h"
#include "include/v8-function-callback.h"
#include "include/v8-function.h"
#include "include/v8-initialization.h"
#include "include/v8-inspector.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-message.h"
#include "include/v8-microtask-queue.h"
#include "include/v8-object.h"
#include "include/v8-platform.h"
#include "include/v8-primitive.h"
#include "include/v8-promise.h"
#include "include/v8-script.h"
#include "include/v8-template.h"
#include "include/v8-typed-array.h"
#include "include/v8-value.h"
#include "include/v8-version.h"
#include "include/v8.h"
#include "include/cppgc/macros.h"
#include "include/libplatform/libplatform.h"

// The version macros a host's preprocessor checks read, so their presence is a
// build failure rather than a silent zero.
#if V8_MAJOR_VERSION < 10
#error "v8-version.h must name the milestone the facade stands in for"
#endif
#if !defined(V8_VERSION_STRING)
#error "v8-version.h must define V8_VERSION_STRING"
#endif
