;; GC-aggregate probe: every iteration does an `array.set` then an `array.get`
;; on a one-element array, so it measures what `.notes/wasm-analysis.md` §5 calls
;; "helper work (GC allocation/casts)" — the compiled path routes each access
;; through the `gc_op` runtime helper, mirroring the interpreter's object pool,
;; rather than lowering it natively (the interpreter resolves the object itself).
;;
;; Compare the interpreter against the compiled path with one binary (without
;; `--compiled` the store forces the interpreter, which is what makes the A/B
;; meaningful):
;;   cargo build --release -p wasmtest
;;   time target/release/wasmtest run          crates/wasmtest/fixtures/gc-loop.wast
;;   time target/release/wasmtest run --compiled crates/wasmtest/fixtures/gc-loop.wast
;;
;; The `assert_return` value is the one V8 (node v24.12.0) computes for the
;; module the in-process converter writes next to this file, so both Slag paths
;; are checked against a third opinion rather than only against each other.
(module
  (type $arr (array (mut i32)))
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $a (ref $arr))
    (local.set $a (array.new_default $arr (i32.const 1)))
    (loop $l
      (array.set $arr (local.get $a) (i32.const 0) (local.get $i))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (array.get $arr (local.get $a) (i32.const 0)))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const 1999999))
