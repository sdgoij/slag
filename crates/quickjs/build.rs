// Compile the C shim and the C compat tests into static libs the crate links.
// The shim supplies the variadic `JS_New*Error` / `JS_Throw*Error` symbols;
// the compat tests exercise the public QuickJS C API from C.

use std::path::Path;

fn main() {
    let ng = std::env::var("CARGO_FEATURE_NG").is_ok();
    let bellard = std::env::var("CARGO_FEATURE_BELLARD").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NG");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BELLARD");

    if ng {
        compile_shim("ng");
        compile_c_tests("ng");
    }
    if bellard {
        compile_shim("bellard");
        compile_c_tests("bellard");
    }
}

fn compile_shim(target: &str) {
    println!("cargo:rerun-if-changed=c/shim");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let mut build = cc::Build::new();
    build
        .include(Path::new("c/include").join(target))
        .warnings(true)
        .file("c/shim/variadic.c");
    if target == "ng" {
        build.define("QJS_NG", None);
    }
    build.compile("qjs_shim");
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=qjs_shim");
}

fn compile_c_tests(target: &str) {
    println!("cargo:rerun-if-changed=c/tests/{target}");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let mut build = cc::Build::new();
    build
        .include(Path::new("c/include").join(target))
        .warnings(true);
    for entry in std::fs::read_dir(format!("c/tests/{target}")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "c") {
            build.file(path);
        }
    }
    build.compile("qjs_compat_tests");
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=qjs_compat_tests");
}
