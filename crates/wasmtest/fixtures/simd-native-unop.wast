;; Unary float v128 lowering probe: f32x4 and f64x2 abs/neg/sqrt and the four
;; rounding forms (ceil, floor, trunc, nearest), plus i8x16.swizzle. The float
;; chain stays non-negative and finite (abs before every sqrt, clamp after), so
;; it never produces a NaN: the final bit pattern is then identical in every
;; engine, and the `assert_return` can be an exact comparison.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-unop.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $d v128)
    (local $s v128)
    (local $mask v128)
    (local $lo v128)
    (local $hi v128)
    (local $dlo v128)
    (local $dhi v128)
    (local.set $a (v128.const f32x4 1.5 2.5 3.5 -4.5))
    (local.set $d (v128.const f64x2 1.5 -2.5))
    (local.set $s (v128.const i8x16 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15))
    (local.set $mask (v128.const i8x16 15 0 14 1 13 2 12 3 11 4 10 5 9 6 8 7))
    (local.set $lo (v128.const f32x4 1.0 1.0 1.0 1.0))
    (local.set $hi (v128.const f32x4 4096.0 4096.0 4096.0 4096.0))
    (local.set $dlo (v128.const f64x2 1.0 1.0))
    (local.set $dhi (v128.const f64x2 4096.0 4096.0))
    (loop $l
      (local.set $a
        (f32x4.add (local.get $a) (f32x4.splat (f32.const 0.25))))
      (local.set $a (f32x4.abs (local.get $a)))
      (local.set $a (f32x4.sqrt (local.get $a)))
      (local.set $a (f32x4.nearest (local.get $a)))
      (local.set $a (f32x4.floor (local.get $a)))
      (local.set $a (f32x4.ceil (local.get $a)))
      (local.set $a (f32x4.trunc (local.get $a)))
      (local.set $a (f32x4.neg (local.get $a)))
      (local.set $a (f32x4.abs (local.get $a)))
      (local.set $a (f32x4.max (local.get $a) (local.get $lo)))
      (local.set $a (f32x4.min (local.get $a) (local.get $hi)))
      (local.set $d
        (f64x2.add (local.get $d) (f64x2.splat (f64.const 0.25))))
      (local.set $d (f64x2.abs (local.get $d)))
      (local.set $d (f64x2.sqrt (local.get $d)))
      (local.set $d (f64x2.nearest (local.get $d)))
      (local.set $d (f64x2.floor (local.get $d)))
      (local.set $d (f64x2.ceil (local.get $d)))
      (local.set $d (f64x2.trunc (local.get $d)))
      (local.set $d (f64x2.neg (local.get $d)))
      (local.set $d (f64x2.abs (local.get $d)))
      (local.set $d (f64x2.max (local.get $d) (local.get $dlo)))
      (local.set $d (f64x2.min (local.get $d) (local.get $dhi)))
      (local.set $s (i8x16.swizzle (local.get $s) (local.get $mask)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 3 (local.get $a)))
      (i32.add
        (i32.wrap_i64
          (i64.add
            (i64x2.extract_lane 0 (local.get $d))
            (i64x2.extract_lane 1 (local.get $d))))
        (i32.add
          (i8x16.extract_lane_u 0 (local.get $s))
          (i8x16.extract_lane_u 15 (local.get $s))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const 2139095055))
