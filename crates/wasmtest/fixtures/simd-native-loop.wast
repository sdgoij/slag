;; v128 lowering probe for the ops that still round-trip through the runtime
;; helper (mode 70): i32x4.mul, i16x8.splat/add, i32x4.shl, i32x4.gt_s,
;; v128.bitselect, i32x4.replace_lane, and i8x16.bitmask. `simd-loop.wast`
;; covers only i32x4.add and v128.xor, which are already native, so it cannot
;; see this work.
;;
;; Measure with one binary, medians of several interleaved runs (without
;; `--compiled` the store forces the interpreter, which is what makes the A/B
;; meaningful):
;;   target/release/wasmtest run           crates/wasmtest/fixtures/simd-native-loop.wast
;;   target/release/wasmtest run --compiled crates/wasmtest/fixtures/simd-native-loop.wast
;; The same-binary switch for the native lowering is SLAG_WASM_SIMD_NATIVE=0.
;;
;; The `assert_return` value is the one V8 (node v24.12.0) computes for this
;; module, so both Slag paths are checked against a third opinion rather than
;; only against each other.
(module
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $acc v128)
    (local $key v128)
    (local.set $acc (v128.const i32x4 1 2 3 4))
    (local.set $key
      (v128.const i32x4 2654435761 40503 2246822519 3266489917))
    (loop $l
      (local.set $acc
        (i32x4.mul (local.get $acc) (local.get $key)))
      (local.set $acc
        (i16x8.add (local.get $acc) (i16x8.splat (local.get $i))))
      (local.set $acc
        (i32x4.shl
          (local.get $acc)
          (i32.and (local.get $i) (i32.const 15))))
      (local.set $acc
        (v128.bitselect
          (local.get $acc)
          (local.get $key)
          (i32x4.gt_s (local.get $acc) (local.get $key))))
      (local.set $acc
        (i32x4.replace_lane 0
          (local.get $acc)
          (i32.add
            (i32x4.extract_lane 0 (local.get $acc))
            (i8x16.bitmask (local.get $acc)))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (i32.add
      (i32x4.extract_lane 0 (local.get $acc))
      (i32x4.extract_lane 2 (local.get $acc))))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -1857128295))
