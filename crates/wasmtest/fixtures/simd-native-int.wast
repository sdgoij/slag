;; Integer v128 lowering probe: saturating add/sub, min/max, the unsigned
;; rounding average, the two narrow forms, the widening extends, and extmul at
;; all three widths.
;;
;; Integer lanes cannot become NaN or infinite, so the expected value is exact
;; in any engine; the i64x2 chain is allowed to wrap.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-int.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $w v128)
    (local $x v128)
    (local $y v128)
    (local $k8 v128)
    (local $one8 v128)
    (local $k16 v128)
    (local $k32 v128)
    (local $one32 v128)
    (local $one64 v128)
    (local.set $a (v128.const i8x16 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16))
    (local.set $w (v128.const i16x8 1 2 3 4 5 6 7 8))
    (local.set $x (v128.const i32x4 1 2 3 4))
    (local.set $y (v128.const i64x2 1 2))
    (local.set $k8 (v128.const i8x16 1 2 3 4 5 6 7 8 -1 -2 -3 -4 -5 -6 -7 -8))
    (local.set $one8 (v128.const i8x16 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1))
    (local.set $k16 (v128.const i16x8 100 200 300 400 500 600 700 800))
    (local.set $k32 (v128.const i32x4 65536 100000 123456 65535))
    (local.set $one32 (v128.const i32x4 1 1 1 1))
    (local.set $one64 (v128.const i64x2 1 1))
    (loop $l
      (local.set $a (i8x16.add_sat_s (local.get $a) (local.get $k8)))
      (local.set $a (i8x16.sub_sat_u (local.get $a) (local.get $k8)))
      (local.set $a (i8x16.avgr_u (local.get $a) (local.get $k8)))
      (local.set $a (i8x16.min_s (local.get $a) (local.get $k8)))
      (local.set $a (i8x16.max_u (local.get $a) (local.get $k8)))
      (local.set $a (i8x16.add_sat_u (local.get $a) (local.get $one8)))
      (local.set $a (i8x16.sub_sat_s (local.get $a) (local.get $one8)))
      (local.set $w (i16x8.extend_low_i8x16_s (local.get $a)))
      (local.set $w
        (i16x8.add (local.get $w) (i16x8.extend_high_i8x16_u (local.get $a))))
      (local.set $w (i16x8.extmul_low_i8x16_s (local.get $a) (local.get $k8)))
      (local.set $w (i16x8.add_sat_s (local.get $w) (local.get $k16)))
      (local.set $w (i16x8.avgr_u (local.get $w) (local.get $k16)))
      (local.set $w (i16x8.min_u (local.get $w) (local.get $k16)))
      (local.set $w (i16x8.max_s (local.get $w) (local.get $k16)))
      (local.set $x (i32x4.extmul_low_i16x8_s (local.get $w) (local.get $k16)))
      (local.set $x (i32x4.min_s (local.get $x) (local.get $k32)))
      (local.set $x (i32x4.max_u (local.get $x) (local.get $k32)))
      (local.set $y (i64x2.extmul_low_i32x4_s (local.get $x) (local.get $one32)))
      (local.set $y (i64x2.add (local.get $y) (local.get $one64)))
      (local.set $a (i8x16.narrow_i16x8_s (local.get $w) (local.get $k16)))
      (local.set $a (i8x16.narrow_i16x8_u (local.get $w) (local.get $k16)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 2 (local.get $a)))
      (i32.add
        (i32.add
          (i32x4.extract_lane 0 (local.get $w))
          (i32x4.extract_lane 1 (local.get $x)))
        (i32.wrap_i64 (i64x2.extract_lane 0 (local.get $y))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const 13244365))
