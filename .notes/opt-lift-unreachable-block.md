# The opt lift's `Malformed("unreachable block")` on dead code after a `return`

**Status: fixed (2026-10-11).** Distinct from the typer-guard miscompile
(`.notes/typer-guard-miscompile.md`); found while building a minimal repro for
it.

## Symptom

A certified body with an early `return` refused the optimizing tier:

```
opt bail: Malformed("unreachable block") (N steps)
```

## Root cause

`lift_impl` minted a block start after **every** terminator:

```rust
if is_terminator(step) { starts[i + 1] = true; }
```

but a `Return`/`Throw` has no successor and an unconditional `Jump` only
reaches its target (`block_successors` records those). The bytecode compiler
leaves dead steps after a `return` (a trailing `ListEnd; Jump(<outer-return>)`),
so `starts[i + 1]` minted a block no edge entered, and `stack_depths`'
reachability fixpoint refused the whole body.

## Fix

Mint `i + 1` only when control actually falls through, the conditionals
(`JumpIfFalse`/`JumpIfTrue` and the fused tests, whose `block_successors` shape
is `[target, end]`):

```rust
if is_terminator(step)
    && !matches!(step, Step::Jump(_) | Step::Return | Step::Throw { .. })
{
    starts[i + 1] = true;
}
```

The dead steps then ride the preceding block's range, past its terminator, and
are ignored.

## The soundness bug this exposed (and its interim fix)

Removing the spurious block made a `return` inside a `for…of` **lift**, and it
miscompiled: the opt lift lowers a `Return` to a bare machine-code return, with
**no iterator close**. The interpreter routes such a return through
`return_control` → `control_transfer`, which closes the iterator and lets a
throwing `return()` *replace* the return (spec 14.7.5.6 step 7 / 7.4.6). The
bail had been *accidentally* preventing this lift.

`Helper::ForOfCloseAll` is the **throw** variant (`iterator_close_throw`,
errors swallowed), so it is not the correct close for a return. A correct fix
needs the error-escaping `close_for_of_return` (via a `return_control`-style
dispatch). Until then the lift **refuses** a `Return` that can run with an open
for-of, any return but the body's final step:

```rust
Step::Return => {
    if has_for_of && ti + 1 != n {
        return Err(Unsupported::Step("Return"));
    }
    TermRec::Ret(stack.pop().ok_or(Unsupported::Stack)?)
}
```

That is sound (a body's final step can have no open loop) and keeps the win:
bodies with an early return and **no** for-of now lift; for-of bodies with a
mid-body return keep falling back to the per-step tier.

## Gates

`cargo test -p jit --lib` 356/0; clippy `-D warnings` clean; test262 `language`
23726/0/0/0/0 and `built-ins` 23820/0/1/0/0; Deno surface 29/29.

## Still to do

Emit the spec-correct close (`close_for_of_return`) on an opt `Return` so
for-of bodies with a mid-body return lift again, then the refusal above comes
out.
