;; Pairwise-add and dot-product lowering probe: the four extadd_pairwise forms
;; and i32x4.dot_i16x8_s, the last integer group without a lowering.
;;
;; Every lane stays bounded (each stage is narrowed back with saturation), so
;; the expected value is exact in any engine.
;;
;; Measure with one binary, medians of several interleaved runs:
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-pairwise.wast
;;   SLAG_WASM_SIMD_NATIVE=0 target/release/wasmtest run --compiled <same>
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a v128)
    (local $w v128)
    (local $z v128)
    (local $k v128)
    (local $one v128)
    (local.set $a (v128.const i8x16 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16))
    (local.set $w (v128.const i16x8 0 0 0 0 0 0 0 0))
    (local.set $z (v128.const i32x4 0 0 0 0))
    (local.set $k (v128.const i16x8 1 2 3 4 5 6 7 8))
    (local.set $one (v128.const i8x16 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1))
    (loop $l
      (local.set $w (i16x8.extadd_pairwise_i8x16_s (local.get $a)))
      (local.set $w
        (i16x8.add (local.get $w) (i16x8.extadd_pairwise_i8x16_u (local.get $a))))
      (local.set $a (i8x16.narrow_i16x8_u (local.get $w) (local.get $w)))
      (local.set $z (i32x4.extadd_pairwise_i16x8_s (local.get $w)))
      (local.set $z
        (i32x4.add (local.get $z) (i32x4.extadd_pairwise_i16x8_u (local.get $w))))
      (local.set $z
        (i32x4.add (local.get $z) (i32x4.dot_i16x8_s (local.get $w) (local.get $k))))
      (local.set $w (i16x8.narrow_i32x4_s (local.get $z) (local.get $z)))
      (local.set $a (i8x16.add (local.get $a) (local.get $one)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32.add
        (i32x4.extract_lane 0 (local.get $a))
        (i32x4.extract_lane 1 (local.get $w)))
      (i32.add
        (i32x4.extract_lane 0 (local.get $z))
        (i32x4.extract_lane 2 (local.get $z)))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const 89195929))
