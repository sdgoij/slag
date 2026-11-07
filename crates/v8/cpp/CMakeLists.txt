cmake_minimum_required(VERSION 3.20)
project(slag_v8_facade CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# The Rust C ABI, built by `cargo build -p slagc --release`. Override with
# -DSLAGC_LIB=... to use target/debug during a fast cycle.
set(SLAGC_LIB
    "${CMAKE_CURRENT_SOURCE_DIR}/../../../target/release/slagc.lib"
    CACHE FILEPATH "The Rust static library backing the facade")
if(NOT EXISTS "${SLAGC_LIB}")
  message(FATAL_ERROR
    "Build the C ABI first (cargo build -p slagc --release); expected "
    "${SLAGC_LIB}. Pass -DSLAGC_LIB=<path> for a debug build.")
endif()

add_library(v8_facade STATIC src/facade.cc src/inspector.cc)
target_include_directories(v8_facade PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(v8_facade PUBLIC "${SLAGC_LIB}")

if(MSVC)
  # A Rust staticlib on MSVC leaves these to its consumer.
  target_link_libraries(v8_facade PUBLIC
    ws2_32 userenv advapi32 bcrypt ntdll ole32 oleaut32 shell32)
endif()

add_executable(hello-world samples/hello-world.cc)
target_link_libraries(hello-world PRIVATE v8_facade)

add_executable(shell samples/shell.cc)
target_link_libraries(shell PRIVATE v8_facade)

# The facade's debugger surface end to end (a session, a breakpoint, the stop,
# the resume). Not a V8 sample: V8 exposes this through the inspector.
add_executable(debugger samples/debugger.cc)
target_link_libraries(debugger PRIVATE v8_facade)

# A DevTools session over `v8-inspector.h`: the protocol's own commands, the
# context group, the channel, and the events a stop produces.
add_executable(inspector samples/inspector.cc)
target_link_libraries(inspector PRIVATE v8_facade)

# A gin-shaped harness, not gin itself: gin is a Chromium component (see the
# README's A1 section).
add_executable(gin-shapes tests/gin_shapes.cc)
target_link_libraries(gin-shapes PRIVATE v8_facade)

# Stage 0 of the Chromium port: which of V8's public headers the facade
# provides. No engine symbols, so it builds and runs before `slagc` exists.
add_executable(surface-census tests/surface_census.cc)
target_include_directories(surface-census PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_compile_definitions(surface-census PRIVATE
  SLAG_FACADE_INCLUDE_ROOT="${CMAKE_CURRENT_SOURCE_DIR}/include"
  SLAG_V8_HEADER_LIST="${CMAKE_CURRENT_SOURCE_DIR}/tests/v8-headers.txt")

# E2 of `.notes/browser.md` §9.5: a handle-scope `Local` across an out-of-line
# collection, with the root-count and bare-word controls.
add_executable(rooting tests/rooting.cc)
target_link_libraries(rooting PRIVATE v8_facade)

# Compile-only: every header the facade provides, included by the name a host
# uses. No `main`, never runs.
add_library(header-surface OBJECT tests/header_surface.cc)
target_include_directories(header-surface PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")

# The value basics Blink's converters reach for first: the four singletons, the
# `Primitive`/`Numeric` node, and `Maybe`'s `Just`/`Nothing`.
add_executable(value-basics tests/value_basics.cc)
target_link_libraries(value-basics PRIVATE v8_facade)

# `v8::MicrotaskQueue`: a host queue a context's realm sends its jobs to.
add_executable(microtasks tests/microtasks.cc)
target_link_libraries(microtasks PRIVATE v8_facade)

# `Module::GetModuleRequests`: a `FixedArray` of `ModuleRequest` with the
# specifier, phase, source offset, and attributes Blink's module machinery reads.
add_executable(module-requests tests/module_requests.cc)
target_link_libraries(module-requests PRIVATE v8_facade)
