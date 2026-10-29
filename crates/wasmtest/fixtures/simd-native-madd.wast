;; Relaxed madd/nmadd lowering probe, isolated from the rest of relaxed SIMD so
;; the two-rounding choice has its own number.
;;
;; Every product here is exact (a factor of two, or a half) and every sum is
;; representable, so the fused and two-rounding behaviours coincide: the value
;; is the same in V8, in the interpreter and in the compiled path.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $f v128)
    (local $d v128)
    (local $m2 v128)
    (local $mh v128)
    (local $mk v128)
    (local $hi v128)
    (local $m2d v128)
    (local $mhd v128)
    (local $mkd v128)
    (local $dhi v128)
    (local.set $f (v128.const f32x4 1.5 2.5 3.5 4.5))
    (local.set $d (v128.const f64x2 1.5 2.5))
    (local.set $m2 (v128.const f32x4 2.0 2.0 2.0 2.0))
    (local.set $mh (v128.const f32x4 0.5 0.5 0.5 0.5))
    (local.set $mk (v128.const f32x4 1024.0 1024.0 1024.0 1024.0))
    (local.set $hi (v128.const f32x4 1024.0 1024.0 1024.0 1024.0))
    (local.set $m2d (v128.const f64x2 2.0 2.0))
    (local.set $mhd (v128.const f64x2 0.5 0.5))
    (local.set $mkd (v128.const f64x2 1024.0 1024.0))
    (local.set $dhi (v128.const f64x2 1024.0 1024.0))
    (loop $l
      (local.set $f
        (f32x4.relaxed_madd (local.get $f) (local.get $m2) (local.get $mh)))
      (local.set $f
        (f32x4.relaxed_nmadd (local.get $f) (local.get $mh) (local.get $mk)))
      (local.set $f (f32x4.min (local.get $f) (local.get $hi)))
      (local.set $d
        (f64x2.relaxed_madd (local.get $d) (local.get $m2d) (local.get $mhd)))
      (local.set $d
        (f64x2.relaxed_nmadd (local.get $d) (local.get $mhd) (local.get $mkd)))
      (local.set $d (f64x2.min (local.get $d) (local.get $dhi)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $f))
        (i32x4.extract_lane 3 (local.get $f)))
      (i32.wrap_i64
        (i64.add
          (i64x2.extract_lane 0 (local.get $d))
          (i64x2.extract_lane 1 (local.get $d))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -2142240768))
