# quickjs

A QuickJS facade over Slag's engine. It exposes two surfaces:

- A **Rust API** at the crate root, shaped after `rquickjs` the way
  `crates/v8` mirrors `rusty_v8`.
- A **C drop-in** (`quickjs.h` symbols) under `src/c/` and `c/`, matching
  either the QuickJS-NG or the Bellard ABI, selected by feature.

The crate is a workspace member but not a default member (like `crates/v8` and
`crates/jsc`), so it is built explicitly.

## Rust facade

The public API is `rquickjs`-shaped and carries no `'js` lifetime: values are
copyable handles and the context is passed explicitly.

```rust
use quickjs::{Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    runtime.add_function(&context, "double", |args| {
        let n = args.get(0).and_then(|v| v.as_number()).unwrap_or(0.0);
        args.set_return(Value::number(n * 2.0));
    })?;

    let value = context.eval("double(21)")?;
    println!("{}", value.as_number().unwrap_or_default());
    Ok(())
}
```

`Runtime` owns the isolate; `Context` is a realm on it. `Value`, `Object`,
`Array`, `Function`, `String`, `Symbol`, `BigInt`, `Class`, `Module`, `Promise`,
`ArrayBuffer`, `TypedArray`, and `DataView` are all available at the root, along
with `Runtime::run_microtasks`, `Runtime::set_heap_limits`,
`Runtime::terminate_execution`, and `Runtime::set_gc_stress`.

The fidelity checklist is in `src/lib.rs`; known divergences are in
`DIVERGENCES.md`. Runnable examples live in `examples/`:

```
cargo run -p quickjs --example eval
cargo run -p quickjs --example host_function
```

## C drop-in

Two mutually-exclusive features select the ABI; neither is default:

```
cargo build -p quickjs               # Rust facade only
cargo build -p quickjs --features ng      # QuickJS-NG 0.18 ABI
cargo build -p quickjs --features bellard # Bellard 2025-09-13 ABI
```

Enabling both is a `compile_error!`; clippy must be run per feature set, never
`--all-features`.

The build emits a staticlib and a cdylib, plus a small C shim (the variadic
`JS_New*Error` / `JS_Throw*Error` symbols). The staticlib is not
self-contained: a C host also links the shim and the platform system libraries
the Rust std uses. `tools/link-test-quickjs.sh` demonstrates the exact link
line and runs a tiny C host against both artifacts:

```
bash tools/link-test-quickjs.sh ng
bash tools/link-test-quickjs.sh bellard
```

The C tests (`c/tests/{ng,bellard}/*.c`) are compiled by `build.rs` and driven
from `tests/compat_ng.rs` / `tests/compat_bellard.rs`; they are the drop-in
proof that a C program written against the vendored `quickjs.h` runs unmodified.

## Verification

```
cargo test  -p quickjs
cargo test  -p quickjs --features ng
cargo test  -p quickjs --features bellard
cargo clippy -p quickjs --all-targets -- -D warnings          # per feature set
bash tools/check-quickjs-headers.sh     # vendored-header drift
bash tools/check-quickjs-surface.sh     # C symbol surface ledger
```
