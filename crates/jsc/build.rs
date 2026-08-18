// Compile the C compat tests into a static lib the integration test links.
// The tests exercise the public JSC C API surface from C, exactly the
// drop-in contract: `#include <JavaScriptCore/JSContextRef.h>` etc.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=tests/c");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let mut build = cc::Build::new();
    build
        .include(Path::new("include"))
        .include(Path::new("include/JavaScriptCore"))
        .warnings(true);
    for entry in std::fs::read_dir("tests/c").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "c") {
            build.file(path);
        }
    }
    build.compile("jsc_compat_tests");
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=jsc_compat_tests");
}
