;; Engine-owned decoder-classification fixtures. The vendored corpus does not
;; reach any of these encodings, so on its own it cannot tell a reserved opcode
;; reported as Malformed from one parked as Unsupported — the flip that mattered
;; turned a `pending` into a `pass` without moving the total.
;;
;; The runner maps Malformed -> pass for `assert_malformed` and Unsupported ->
;; pending, so reclassifying any case below as a feature a later cut owns would
;; move a test from pass to an invisible pending. `tests/decoder_classification.rs`
;; runs this file and fails on any pending, which is what makes the wasm sweep's
;; "0 pendings" a gate instead of a claim.
;;
;; Adding a case here means bumping the pinned count in that test.

;; A reserved 0xfd subopcode (4096). Every SIMD subopcode the pinned spec
;; defines is in the dispatch, relaxed SIMD 0x100..=0x113 included.
(assert_malformed
  (module binary "\00asm\01\00\00\00\01\04\01\60\00\00\03\02\01\00\0a\07\01\05\00\fd\80\20\0b")
  "malformed simd opcode")

;; A reserved 0xfc subopcode (100): the bulk-memory and table forms stop at 17.
(assert_malformed
  (module binary "\00asm\01\00\00\00\01\04\01\60\00\00\03\02\01\00\0a\06\01\04\00\fc\64\0b")
  "malformed 0xfc subopcode")

;; A reserved 0xfb (GC) subopcode (100): the aggregate and cast forms stop at 30.
(assert_malformed
  (module binary "\00asm\01\00\00\00\01\04\01\60\00\00\03\02\01\00\0a\06\01\04\00\fb\64\0b")
  "malformed GC opcode")

;; The 0xfe atomics prefix: the pinned spec carries no threads proposal, so this
;; stays malformed rather than becoming a later-cut pending.
(assert_malformed
  (module binary "\00asm\01\00\00\00\01\04\01\60\00\00\03\02\01\00\0a\05\01\03\00\fe\0b")
  "illegal opcode")

;; A reserved data-segment flag (3): 0/1/2 are the defined forms.
(assert_malformed
  (module binary "\00asm\01\00\00\00\0b\02\01\03")
  "malformed data segment flags")

;; A function body whose declared size outlives its expression. The section
;; payload is consumed exactly, so only the body-level check can reject this.
(assert_malformed
  (module binary "\00asm\01\00\00\00\01\04\01\60\00\00\03\02\01\00\0a\05\01\03\00\0b\00")
  "trailing bytes in function body")
