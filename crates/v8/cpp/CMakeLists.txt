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

add_library(v8_facade STATIC src/facade.cc)
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

# A gin-shaped harness, not gin itself: gin is a Chromium component (see the
# README's A1 section).
add_executable(gin-shapes tests/gin_shapes.cc)
target_link_libraries(gin-shapes PRIVATE v8_facade)
