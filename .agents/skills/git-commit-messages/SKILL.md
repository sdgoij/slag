---
name: git-commit-messages
description: Write proper Git commit messages with subject line and descriptive body. Load when asked to write a commit message, generate a changelog, or summarize changes.
---

# Git Commit Messages

When asked to write a commit message, produce the entire message (subject + body) in your response. **Do not ask for clarification** — read the staged files or the diff and write the message yourself.

## Format

```
<type>: <subject, imperative mood, no period>

<body — explain what and why, not how. Omit if the subject alone suffices.>
```

**Subject line rules:**
- Imperative mood ("Add feature", not "Added feature" or "Adds feature")
- No trailing period
- Type prefix: `feat:`, `fix:`, `test:`, `refactor:`, `docs:`, `chore:` etc.
- Don't repeat information from the subject in the message body

**Body rules:**
- Only include when the subject alone isn't enough
- Explain *what* changed and *why* — not *how*
- Separate from subject with a blank line
- **Never hard-wrap.** Write each paragraph as ONE long line and let the reader's client wrap it, exactly like markdown prose. Do not insert line breaks at a column. Do not "keep lines short". Measure length in paragraphs and sentences, not characters.

## Why there is no line-length rule

Hard-wrapped bodies look right only in the terminal width they were written for. Every reader — `git log`, the forge UI, a review tool, another agent — re-wraps or quotes them at a different width, so the artificial breaks resurface as mid-sentence gaps and ragged text, and anyone reflowing the message later has to guess which breaks were intentional. Git and markdown both treat a single newline as a soft break, so a hard-wrapped paragraph is still ONE logical line to anything that parses it: the breaks are pure noise, while a real paragraph break is the only newline that carries meaning. Never wrap. A subject may be a full sentence and a body paragraph may run long — that is fine.

## Example Patterns

### Pattern 1: File enumeration (many files, same crate)

```
feat: add VT crate and text search to buffer

- Create vt crate with 8 modules:
  - state_machine.rs: DFA parser with 19 states
  - state.rs: VtState enum
  - parameters.rs: VT parameter parsing
  - dispatch_types.rs: VtId, VtSequence, DeviceAttributesResponse
  - ascii.rs: ASCII constants + helpers
  - base64.rs: Base64 encode/decode
  - charsets.rs: G0-G3 character set management
  - lib.rs: Module declarations and re-exports
- Add search.rs to buffer crate with text search and regex (*, ?)
- Fix OutputCellIterator::new clamp bug
```

### Pattern 2: Categorical grouping (multiple subsystems)

```
feat(adapter): terminal adapter with comprehensive VT dispatch

**New modules:**
- dispatch_types.rs - Core VT types and enums
- term_dispatch.rs - ITermDispatch trait with 80+ methods
- interact_dispatch.rs - IInteractDispatch callbacks
- adapt_dispatch.rs - Main AdaptDispatch with mode flags
- terminal_output.rs - GL/GR character set translation

**Bug fixes:**
- Fixed VTID bit-packing to use array storage
- Fixed duplicate enum variant values
- Fixed const fn limitations

**Utility fixes:**
- Removed redundant to_string() shadowing Display
- Simplified needless bool expressions
```

### Pattern 3: Concise single-purpose change

```
feat: add terminal connection layer with PTY backend support

Implement PtyTerminalConnection in server crate with TermConnection trait for both Windows and Unix platforms. Wire up re-exports in winterm crate so the connection types are accessible from the app.
```

### Pattern 4: Test-focused commit

```
test(buffer, vt): add snapshot, property, and fuzz tests

Add remaining Phase 2/3 tests. Brings total from 229 to 663.

- row.rs: mixed-width rendering (ASCII + CJK + emoji)
- text_buffer.rs: full buffer rendered output grid
- selection.rs: selection rect bounds rendering
- search.rs: search finds written text (literal + regex)
- state_machine.rs: deterministic fuzz with 1000 iterations
```

### Pattern 5: Analysis / probe write-up

```
perf(runtime,jit): hoist invariant global reads out of certified loops

Extend the member-read LICM to bare global identifiers, closing the `global read` row's read half: jit 3.13 -> 1.37ms, ~9.9x off node -> ~4.3x. The row's `g` resolves as `BindingLoc::Env` in a function body, not `Global`, so the `env` flag and its `clean_chain` gate are the actual fix — a cut admitting only `Global` moved nothing.

The residual is not the read: with the loop bound made a literal the same fast copy measures 0.705ms (matching `property read` and the arithmetic floor), so the remaining gap is the `JumpIfRelLimit { limit: Slot }` test shape. Queued: the loop-limit analogue of `NumRhs::Slot`.

Verified: clippy -D warnings clean; cargo test --workspace 4780 pass / 0 fail; six test262 sweeps at baseline.
```

Note the shape: the subject is one sentence, the body is a few unwrapped paragraphs, and blank lines are the only meaningful breaks.

## DOs and DON'Ts

| DO | DON'T |
|----|-------|
| Read the diff or staged files first | Ask "what files changed?" |
| Write the full message in one shot | Send just the subject line and wait |
| Explain what and why | Explain how (the code shows that) |
| Use imperative mood | Use past tense ("Added", "Fixed") |
| Group related changes | List every file individually |
| Write each paragraph as one unwrapped line | Hard-wrap the body at a column |
| Close with notes (tests, migration) | Leave the body empty when there's substance |

## If the user says "better" or "descriptive"

They want a body. If you only gave a subject line, add a body explaining the changes grouped by purpose. Read the actual diff to get the details right.
