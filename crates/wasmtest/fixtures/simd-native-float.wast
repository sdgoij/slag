;; Float v128 lowering probe: f32x4 and f64x2 add/sub/mul/div/min/max/pmin/pmax,
;; which still round-trip through the runtime helper (mode 70) until the native
;; float lowering lands. The accumulators are kept bounded by the max/min clamp
;; so the loop never reaches a NaN or an infinity: both paths must agree on the
;; final bit pattern, and a NaN would make the expected value engine-specific.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-float.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion rather than
;; only against each other.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $d v128)
    (local $f v128)
    (local $g v128)
    (local $lo v128)
    (local $hi v128)
    (local $dlo v128)
    (local $dhi v128)
    (local.set $a (v128.const f32x4 1.0 2.0 3.0 4.0))
    (local.set $d (v128.const f64x2 1.0 2.0))
    (local.set $f (v128.const f32x4 1.5 0.75 2.25 0.5))
    (local.set $g (v128.const f64x2 1.5 0.75))
    (local.set $lo (v128.const f32x4 1.0 1.0 1.0 1.0))
    (local.set $hi (v128.const f32x4 1024.0 1024.0 1024.0 1024.0))
    (local.set $dlo (v128.const f64x2 1.0 1.0))
    (local.set $dhi (v128.const f64x2 1024.0 1024.0))
    (loop $l
      (local.set $a (f32x4.mul (local.get $a) (local.get $f)))
      (local.set $a
        (f32x4.add (local.get $a) (f32x4.splat (f32.const 0.5))))
      (local.set $a
        (f32x4.sub (local.get $a) (f32x4.splat (f32.const 0.25))))
      (local.set $a (f32x4.div (local.get $a) (local.get $f)))
      (local.set $a (f32x4.max (local.get $a) (local.get $lo)))
      (local.set $a (f32x4.min (local.get $a) (local.get $hi)))
      (local.set $a (f32x4.pmax (local.get $a) (local.get $lo)))
      (local.set $a (f32x4.pmin (local.get $a) (local.get $hi)))
      (local.set $d (f64x2.mul (local.get $d) (local.get $g)))
      (local.set $d
        (f64x2.add (local.get $d) (f64x2.splat (f64.const 0.5))))
      (local.set $d
        (f64x2.sub (local.get $d) (f64x2.splat (f64.const 0.25))))
      (local.set $d (f64x2.div (local.get $d) (local.get $g)))
      (local.set $d (f64x2.max (local.get $d) (local.get $dlo)))
      (local.set $d (f64x2.min (local.get $d) (local.get $dhi)))
      (local.set $d (f64x2.pmax (local.get $d) (local.get $dlo)))
      (local.set $d (f64x2.pmin (local.get $d) (local.get $dhi)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 3 (local.get $a)))
      (i32.wrap_i64
        (i64.add
          (i64x2.extract_lane 0 (local.get $d))
          (i64x2.extract_lane 1 (local.get $d))))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -1996488704))
