;; Memory-heavy probe: every iteration does an `i32.store` then loads the same
;; slot back, so the accumulator depends on the round-trip and neither access is
;; dead. This is the regime `.notes/wasm-analysis.md` §5 flags as the open
;; question for a default-on decision — the compiled path emits a bounds check
;; and reloads the memory descriptor on every access (§7 item 6), while the
;; interpreter resolves the memory cell once and collapses the bound to a single
;; compare (§7 item 12).
;;
;; Compare the interpreter against the compiled path with one binary built with
;; the feature (without `--compiled` the store forces the interpreter, which is
;; what makes the A/B meaningful):
;;   cargo build --release -p wasmtest --features compile
;;   time target/release/wasmtest run          crates/wasmtest/fixtures/mem-loop.wast
;;   time target/release/wasmtest run --compiled crates/wasmtest/fixtures/mem-loop.wast
;;
;; The `assert_return` value is the one V8 (node v24.12.0) computes for the
;; module the in-process converter writes next to this file, so both Slag paths
;; are checked against a third opinion rather than only against each other.
;;
;; Control: the same loop with the store/load pair removed (`br $l` only),
;; timed on the same machine, is 0.020 s compiled against 0.415 s interpreted
;; (2M iterations). The round-trip is therefore ~1 ms over 2M iterations on the
;; compiled side — a few cycles per access against the interpreter's heavy
;; per-access cost — which is why §7 item 6 (bounds-check hoisting) is polish
;; rather than a default-on blocker. A variant that advances the address before
;; loading, so the load reads the *previous* iteration's store and no backend
;; can forward it, measures the same 0.020 s: the accesses are real traffic, and
;; still cheap.
(module
  (memory 1)
  (func $spin (param $n i32) (result i32)
    (local $i i32)
    (local $p i32)
    (local $acc i32)
    (loop $l
      (local.set $acc
        (i32.rotl
          (i32.xor
            (i32.add (local.get $acc) (local.get $i))
            (i32.const 2654435761))
          (i32.const 13)))
      (i32.store (local.get $p) (local.get $acc))
      (local.set $acc (i32.add (local.get $acc) (i32.load (local.get $p))))
      ;; Advance the byte address, wrapping inside the page at a 4-byte stride
      ;; so every access stays aligned and in bounds.
      (local.set $p
        (i32.and (i32.add (local.get $p) (i32.const 4)) (i32.const 0xFFFC)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get $n))))
    (local.get $acc))
  (export "spin" (func $spin)))
(assert_return (invoke "spin" (i32.const 2000000)) (i32.const -24673316))
