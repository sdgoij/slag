;; Conversion v128 lowering probe: i32x4.trunc_sat_f32x4_s/u,
;; f32x4.convert_i32x4_s/u, i32x4.trunc_sat_f64x2_s/u_zero,
;; f64x2.convert_low_i32x4_s/u, f32x4.demote_f64x2_zero and
;; f64x2.promote_low_f32x4.
;;
;; Every accumulator stays integral and bounded, so no lane can become a NaN or
;; an infinity and the expected value is exact in any engine.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-convert.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $d v128)
    (local $q v128)
    (local $hi v128)
    (local $dhi v128)
    (local.set $a (v128.const f32x4 1.0 2.0 3.0 4.0))
    (local.set $d (v128.const f64x2 1.0 2.0))
    (local.set $q (v128.const f64x2 1.0 2.0))
    (local.set $hi (v128.const f32x4 1024.0 1024.0 1024.0 1024.0))
    (local.set $dhi (v128.const f64x2 1024.0 1024.0))
    (loop $l
      (local.set $a
        (f32x4.convert_i32x4_s
          (i32x4.trunc_sat_f32x4_s (local.get $a))))
      (local.set $a
        (f32x4.add (local.get $a) (f32x4.splat (f32.const 1.0))))
      (local.set $a (f32x4.min (local.get $a) (local.get $hi)))
      (local.set $a
        (f32x4.convert_i32x4_u
          (i32x4.trunc_sat_f32x4_u (local.get $a))))
      (local.set $d
        (f64x2.convert_low_i32x4_s
          (i32x4.trunc_sat_f64x2_s_zero (local.get $d))))
      (local.set $d
        (f64x2.add (local.get $d) (f64x2.splat (f64.const 1.0))))
      (local.set $d (f64x2.min (local.get $d) (local.get $dhi)))
      (local.set $d
        (f64x2.convert_low_i32x4_u
          (i32x4.trunc_sat_f64x2_u_zero (local.get $d))))
      (local.set $q
        (f64x2.add (local.get $q) (f64x2.splat (f64.const 0.5))))
      (local.set $q (f64x2.min (local.get $q) (local.get $dhi)))
      (local.set $q
        (f64x2.promote_low_f32x4
          (f32x4.demote_f64x2_zero (local.get $q))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 3 (local.get $a)))
      (i32.add
        (i32x4.extract_lane 0 (local.get $d))
        (i32.add
          (i32x4.extract_lane 1 (local.get $d))
          (i32x4.extract_lane 0 (local.get $q))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -913309696))
