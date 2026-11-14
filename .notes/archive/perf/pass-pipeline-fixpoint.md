# Is the pass pipeline's single round enough?

**Status: verified (2026-10-07).** `pass/mod.rs` orders the pipeline (fold,
narrow, cse, licm, dce) and documents it as a single-round fixpoint — "a later
pass never enables an earlier one". The only ordering that could break that is
**LICM after CSE**: LICM hoists an instruction into the preheader, where it could
become CSE-able with an instruction already there (CSE already ran).

Checked by turning `run` into a bounded fixpoint and reporting whether the second
round changed anything, across the tier's hot shapes (`arith_loop`, `lcg_loop`,
`licm_loop`, `for_loop`, `read_loop`, `read_loop_nonleaf`) and two tailored probes
for the LICM→CSE gap — an invariant `FrameLoad` loaded in both the preheader and
the loop (`preheader_load.js`), and an invariant arithmetic op computed in both
(`preheader_cse.js`). **Every second round found no work**
(`fold=narrow=cse=licm=dce=false`). An interleaved A/B of the fixpoint vs the
single round measured 0.99–1.05× (noise around 1.0, as expected for identical
output), so the extra round buys nothing and costs compile time. The pipeline
stays single-round.

Why the LICM→CSE gap is unreachable in practice:

- For LICM to hoist an arithmetic op, `narrow` must prove its operands numeric —
  i.e. numeric frame slots. Two identical ops over the same numeric slots in one
  block would already have been folded to a constant, so the preheader cannot
  hold a matching op for a hoisted one to merge with.
- An invariant `FrameLoad` hoist does not merge with the preheader's copy because
  the two land in different blocks and CSE is block-local (`preheader_load.js`).

So the documented invariant holds — empirically, not by proof. A regression test
(`the_pipeline_reaches_its_fixpoint_in_one_round`) pins the property on a body
that changes in round 1 and must not change in round 2.
