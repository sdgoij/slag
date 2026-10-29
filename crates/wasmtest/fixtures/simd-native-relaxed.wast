;; Relaxed SIMD lowering probe for the ops whose deterministic interpreter
;; choice is one of the native lowerings: relaxed swizzle, the four relaxed
;; trunc forms, relaxed laneselect, and relaxed min/max.
;;
;; The inputs are chosen so that every allowed behaviour of each relaxed op
;; agrees: swizzle indices stay in 0..15, laneselect masks are whole-lane
;; all-ones or all-zeros, and the floats are non-negative and bounded, so
;; trunc is exact and min/max never sees a NaN or a signed zero. That keeps the
;; expected value comparable with V8 as well as between the two Slag paths.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $f v128)
    (local $d v128)
    (local $s v128)
    (local $perm v128)
    (local $lanemask v128)
    (local $kf v128)
    (local $hi v128)
    (local $kd v128)
    (local $dhi v128)
    (local.set $f (v128.const f32x4 1.5 2.5 3.5 4.5))
    (local.set $d (v128.const f64x2 1.5 2.5))
    (local.set $s (v128.const i8x16 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15))
    (local.set $perm (v128.const i8x16 15 0 14 1 13 2 12 3 11 4 10 5 9 6 8 7))
    (local.set $lanemask (v128.const i32x4 -1 0 -1 0))
    (local.set $kf (v128.const f32x4 1.0 2.0 3.0 4.0))
    (local.set $hi (v128.const f32x4 1024.0 1024.0 1024.0 1024.0))
    (local.set $kd (v128.const f64x2 1.0 2.0))
    (local.set $dhi (v128.const f64x2 1024.0 1024.0))
    (loop $l
      (local.set $f
        (f32x4.add (local.get $f) (f32x4.splat (f32.const 0.5))))
      (local.set $f (f32x4.min (local.get $f) (local.get $hi)))
      (local.set $f (i32x4.relaxed_trunc_f32x4_s (local.get $f)))
      (local.set $f (f32x4.convert_i32x4_s (local.get $f)))
      (local.set $f (i32x4.relaxed_trunc_f32x4_u (local.get $f)))
      (local.set $f (f32x4.convert_i32x4_u (local.get $f)))
      (local.set $f (f32x4.relaxed_min (local.get $f) (local.get $kf)))
      (local.set $f (f32x4.relaxed_max (local.get $f) (local.get $kf)))
      (local.set $f
        (i32x4.relaxed_laneselect (local.get $f) (local.get $kf) (local.get $lanemask)))
      (local.set $d
        (f64x2.add (local.get $d) (f64x2.splat (f64.const 0.5))))
      (local.set $d (f64x2.min (local.get $d) (local.get $dhi)))
      (local.set $d (i32x4.relaxed_trunc_f64x2_s_zero (local.get $d)))
      (local.set $d (f64x2.convert_low_i32x4_s (local.get $d)))
      (local.set $d (i32x4.relaxed_trunc_f64x2_u_zero (local.get $d)))
      (local.set $d (f64x2.convert_low_i32x4_u (local.get $d)))
      (local.set $d (f64x2.relaxed_min (local.get $d) (local.get $kd)))
      (local.set $d (f64x2.relaxed_max (local.get $d) (local.get $kd)))
      (local.set $s (i8x16.relaxed_swizzle (local.get $s) (local.get $perm)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $f))
        (i32x4.extract_lane 3 (local.get $f)))
      (i32.add
        (i32.wrap_i64 (i64x2.extract_lane 0 (local.get $d)))
        (i32.add
          (i8x16.extract_lane_u 0 (local.get $s))
          (i8x16.extract_lane_u 15 (local.get $s))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -2147483633))
