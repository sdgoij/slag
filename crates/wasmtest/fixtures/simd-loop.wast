;; Register-SIMD probe: every iteration does two v128 ops, so it measures what
;; `.notes/wasm-analysis.md` §5 calls "pure-register SIMD … through runtime
;; helpers mirroring the interpreter" (§7 item 10 is lowering these natively).
;; Unlike `mem-loop.wast` the loop touches no memory: the only question is
;; whether a helper round-trip per vector op costs more than the interpreter's
;; own `simd_exec`.
;;
;; Compare the interpreter against the compiled path with one binary built with
;; the feature (without `--compiled` the store forces the interpreter, which is
;; what makes the A/B meaningful):
;;   cargo build --release -p wasmtest --features compile
;;   time target/release/wasmtest run          crates/wasmtest/fixtures/simd-loop.wast
;;   time target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-loop.wast
;;
;; The `assert_return` value is the one V8 (node v24.12.0) computes for the
;; module the in-process converter writes next to this file, so both Slag paths
;; are checked against a third opinion rather than only against each other.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $acc v128)
    (local.set $acc (v128.const i32x4 1 2 3 4))
    (loop $l
      (local.set $acc
        (i32x4.add
          (local.get $acc)
          (v128.const i32x4 2654435761 40503 2246822519 3266489917)))
      (local.set $acc
        (v128.xor (local.get $acc) (v128.const i32x4 13 31 61 127)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32x4.extract_lane 0 (local.get $acc)))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -111632127))
