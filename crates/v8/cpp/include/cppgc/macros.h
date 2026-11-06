#ifndef INCLUDE_CPPGC_MACROS_H_
#define INCLUDE_CPPGC_MACROS_H_

/*
 * cppgc's allocation-discipline macros. V8's header pulls a compiler-specific
 * helper for `CPPGC_UNUSED`; this one is self-contained, because the helper
 * lives under `cppgc/internal/`, which is not part of the public surface a host
 * includes.
 *
 * The facade has no collector yet, so these are declarations a host's class
 * bodies compile against: they enforce what the declaration says (an
 * unused-attribute alias and deleted `operator new`) and nothing more.
 * `cppgc::GarbageCollected` itself is the item the port has not reached
 * (`.notes/browser.md` §5).
 */

#include <cstddef>

namespace cppgc {

#if defined(__clang__) || defined(__GNUC__)
#define CPPGC_UNUSED __attribute__((unused))
#else
#define CPPGC_UNUSED
#endif

#define CPPGC_DISALLOW_NEW()                                      \
 public:                                                          \
  using IsDisallowNewMarker CPPGC_UNUSED = int;                   \
  void* operator new(size_t, void* location) { return location; } \
  void* operator new(size_t) = delete;                            \
  static_assert(true, "Force semicolon.")

#define CPPGC_STACK_ALLOCATED()                        \
 public:                                               \
  using IsStackAllocatedTypeMarker CPPGC_UNUSED = int; \
                                                       \
 private:                                              \
  void* operator new(size_t) = delete;                 \
  void* operator new(size_t, void*) = delete;          \
  static_assert(true, "Force semicolon.")

}  // namespace cppgc

#endif
