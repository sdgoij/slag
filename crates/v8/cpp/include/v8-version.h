#ifndef INCLUDE_V8_VERSION_H_
#define INCLUDE_V8_VERSION_H_

/*
 * The V8 API milestone this facade stands in for: Chromium `main`.
 *
 * Measured, not guessed — the numbers below are what the V8 checkout at
 * `third_party`-sibling `v8/` reports (`include/v8-version.h`), taken
 * 2026-11-06 from https://github.com/v8/v8 commit
 * 3213480130bf1e664ed252c648d51e5bf196c6b6. Moving the pin means editing this
 * file and regenerating `tests/v8-headers.txt`, and treating the API delta as a
 * port (`.notes/chromium-facade.md` §14, decision 1).
 *
 * A version query and the facade's actual shape are two different statements.
 * The platform entry points in `v8.h` are still the pre-11 shape — V8 15 has
 * `v8-locker.h`, a `Platform` with worker-thread and job interfaces, and
 * `PumpMessageLoop(Isolate*, MessageLoopBehavior)` — so pinning to `main` is
 * what makes that gap measurable rather than absent from the record.
 */
#define V8_MAJOR_VERSION 15
#define V8_MINOR_VERSION 9
#define V8_BUILD_NUMBER 0
#define V8_PATCH_LEVEL 0
#define V8_IS_CANDIDATE_VERSION 1

#define V8_VERSION_STRING "15.9.0"

#endif
