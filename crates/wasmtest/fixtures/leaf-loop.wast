;; Call-free leaf-body probe: the compiled path's best case, where a body has no
;; call for the Cranelift backend to route back through the interpreter
;; (`.notes/wasm-analysis.md` §5). `interp-hot-loop.wast` is the other regime —
;; a call per iteration — and the two together show how much of the backend's
;; value the call helper currently holds back.
;;
;; Compare the interpreter against the compiled path with one binary built with
;; the feature (without `--compiled` the store forces the interpreter, which is
;; what makes the A/B meaningful):
;;   cargo build --release -p wasmtest --features compile
;;   time target/release/wasmtest run          crates/wasmtest/fixtures/leaf-loop.wast
;;   time target/release/wasmtest run --compiled crates/wasmtest/fixtures/leaf-loop.wast
;;
;; The `assert_return` value is the one V8 (node v24.12.0) computes for the same
;; module, so both Slag paths are checked against a third opinion rather than
;; only against each other.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $acc i32)
    (loop $l
      (local.set $acc
        (i32.rotl
          (i32.xor
            (i32.add (local.get $acc) (local.get $i))
            (i32.const 2654435761))
          (i32.const 13)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (local.get $acc))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 10000000)) (i32.const 2115478933))
