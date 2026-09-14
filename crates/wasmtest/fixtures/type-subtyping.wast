;; Subtype-declaration well-formedness (spec K-sub / S-comp), the rules
;; `waspec/test/core/gc/type-subtyping.wast` covers but which cannot be swept:
;; that file's `multiple supertypes` case uses `(sub $a $b …)` text the pinned
;; `wast` crate's grammar rejects, so the whole file is excluded from the
;; corpus. This fixture pins the same rules in a form the converter accepts —
;; the multi-supertype case is encoded as bytes, the rest as text — so
;; `cargo test -p wasmtest` gates them. Each case is checked against V8 (node
;; v24.12.0), which rejects every `assert_invalid` module here and accepts every
;; plain `module`.
;;
;; The expected messages mirror the corpus file's, which is the third-party
;; statement of these rules; the runner only compares the invalid-vs-malformed
;; class, not the text.
(module
  ;; Valid: a constant field narrows covariantly, a mutable field is invariant,
  ;; the subtype may append fields, and parameters may widen while results
  ;; narrow.
  (type $sup (sub (struct (field (ref null eq)))))
  (type $sub (sub $sup (struct (field (ref null i31)) (field i32))))
  (type $f_sup (sub (func (param (ref null i31)) (result (ref null eq)))))
  (type $f_sub (sub $f_sup (func (param (ref null eq)) (result (ref null i31)))))
  (type $mut (sub (struct (field (mut i32)))))
  (type $mut_sub (sub $mut (struct (field (mut i32)))))
  ;; A bottom type fits every member of its own hierarchy: `none` under `i31`
  ;; and under a concrete struct type, `nofunc` under `func`.
  (type $s (sub (struct)))
  (type $bot (sub (struct (field (ref null i31)) (field (ref null $s)))))
  (type $bot_sub (sub $bot (struct (field (ref null none)) (field (ref null none)))))
  (type $r_sup (sub (func (result (ref null func)))))
  (type $r_sub (sub $r_sup (func (result (ref null nofunc))))))

(assert_invalid
  (module
    (type $a (sub (array i32)))
    (type $s (sub $a (struct))))
  "sub type")

(assert_invalid
  (module
    (type $s (sub (struct)))
    (type $a (sub $s (array i32))))
  "sub type")

(assert_invalid
  (module
    (type $t (sub final (func)))
    (type $s (sub $t (func))))
  "sub type")

(assert_invalid
  (module
    (type $t (sub (func)))
    (type $s (sub final $t (func)))
    (type $u (sub $s (func))))
  "sub type")

(assert_invalid
  (module
    (type $sup (sub (struct (field (mut eqref)))))
    (type $sub (sub $sup (struct (field (mut i31ref))))))
  "sub type")

(assert_invalid
  (module
    (type $sup (sub (struct (field i32) (field i32))))
    (type $sub (sub $sup (struct (field i32)))))
  "sub type")

(assert_invalid
  (module
    (type $sup (sub (func (param (ref null eq)))))
    (type $sub (sub $sup (func (param (ref null i31))))))
  "sub type")

(assert_invalid
  (module
    (type $sup (sub (func (result (ref null i31)))))
    (type $sub (sub $sup (func (result (ref null eq))))))
  "sub type")

(assert_invalid
  (module
    (rec
      (type $a (sub $b (struct)))
      (type $b (sub (struct)))))
  "supertype must precede the subtype")

;; The one form the corpus file cannot express: the binary grammar puts no
;; bound on the supertype vector (`x*:Blist(Btypeidx)`), so this decodes and is
;; rejected by validation, exactly as the corpus's `assert_invalid` expects.
(assert_invalid
  (module binary "\00asm\01\00\00\00\01\0f\03\50\00\5f\00\50\00\5f\00\50\02\00\01\5f\00")
  "multiple supertypes")
