;; `i16x8.q15mulr_sat_s` lowering probe, the last op that needed a hand-built
;; sequence (widen, multiply, round, shift, saturating narrow).
;;
;; The multiply-and-add pair converges to a small fixed point, so the lanes stay
;; bounded and the expected value is exact in any engine.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-q15.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $k v128)
    (local $c v128)
    (local.set $a (v128.const i16x8 1 2 3 4 5 6 7 8))
    (local.set $k (v128.const i16x8 16384 -16384 16384 -16384 16384 -16384 16384 -16384))
    (local.set $c (v128.const i16x8 1000 -1000 500 -500 100 -100 10 -10))
    (loop $l
      (local.set $a (i16x8.q15mulr_sat_s (local.get $a) (local.get $k)))
      (local.set $a (i16x8.add (local.get $a) (local.get $c)))
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
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -70185836))
