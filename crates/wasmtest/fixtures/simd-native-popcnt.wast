;; `i8x16.popcnt` lowering probe, isolated so the one remaining op with a
;; Cranelift instruction gets its own number. Three popcounts per iteration
;; keep the loop dominated by the op under test; the bytes stay bounded
;; (popcount of any byte is 0..8) so the expected value is exact.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-popcnt.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $k v128)
    (local.set $a (v128.const i8x16 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15))
    (local.set $k (v128.const i8x16 -16 120 15 -1 90 -90 3 127 -128 60 17 -17 5 100 -100 42))
    (loop $l
      (local.set $a (i8x16.popcnt (local.get $a)))
      (local.set $a (i8x16.shl (local.get $a) (i32.const 2)))
      (local.set $a (i8x16.add (local.get $a) (local.get $k)))
      (local.set $a (i8x16.popcnt (local.get $a)))
      (local.set $a
        (i8x16.add (local.get $a) (i8x16.splat (local.get $i))))
      (local.set $a (i8x16.popcnt (local.get $a)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 1 (local.get $a)))
      (i32.add
        (i32x4.extract_lane 2 (local.get $a))
        (i32x4.extract_lane 3 (local.get $a)))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const 168298504))
