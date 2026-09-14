;; Interpreter hot-loop probe: exists so the perf numbers in
;; `.notes/wasm-analysis.md` §7 can be reproduced, not as a correctness
;; fixture (no pass/fail threshold depends on it, and no gate runs it).
;;
;; It stresses the three paths that analysis §7 items 1, 3, and 4 removed
;; allocations from: a `br_if` back-edge per iteration (the per-branch
;; BodyMap rescan), three numeric ops per iteration (the per-op Vec<Value>),
;; and a call returning a value per iteration (the allocating take_top).
;;
;; Time it (release; the conversion is cached after the first run, so repeat
;; runs measure execution — compare medians of three, the first reads cold):
;;   cargo build --release -p wasmtest
;;   time target/release/wasmtest run crates/wasmtest/fixtures/interp-hot-loop.wast
(module
  (func $step (param $i i32) (param $acc i32) (result i32)
    (i32.rotl
      (i32.xor
        (i32.add (local.get $acc) (local.get $i))
        (i32.const 2654435761))
      (i32.const 13)))
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $acc i32)
    (loop $l
      (local.set $acc (call $step (local.get $i) (local.get $acc)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (local.get $acc))
  (export "spin" (func $spin)))
(invoke "spin" (i32.const 1000000))
