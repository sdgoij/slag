---
name: git-commit-messages
description: Write proper Git commit messages with subject line and descriptive body. Load when asked to write a commit message, generate a changelog, or summarize changes.
---

# Git Commit Messages

When asked to write a commit message, produce the entire message (subject + body) in your response. **Do not ask for clarification** — read the staged files or the diff and write the message yourself.

## Format

```
<type>: <subject line, max 80-ish chars, imperative mood, no period>

<body — explain what and why, not how. Omit if subject alone suffices.
```

**Subject line rules:**
- Max ~120 characters
- Imperative mood ("Add feature", not "Added feature" or "Adds feature")
- No trailing period
- Type prefix: `feat:`, `fix:`, `test:`, `refactor:`, `docs:`, `chore:` etc.
- Don't repeat information from the subject in the message body

**Body rules:**
- Only include when the subject alone isn't enough
- Explain *what* changed and *why* — not *how*
- Wrap at 120 characters
- Separate from subject with a blank line

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

Implement PtyTerminalConnection in server crate with TermConnection
trait for both Windows and Unix platforms. Wire up re-exports in
winterm crate so the connection types are accessible from the app.
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

## DOs and DON'Ts

| DO | DON'T |
|----|-------|
| Read the diff or staged files first | Ask "what files changed?" |
| Write the full message in one shot | Send just the subject line and wait |
| Explain what and why | Explain how (the code shows that) |
| Use imperative mood | Use past tense ("Added", "Fixed") |
| Group related changes | List every file individually |
| Close with notes (tests, migration) | Leave the body empty when there's substance |

## If the user says "better" or "descriptive"

They want a body. If you only gave a subject line, add a body explaining the changes grouped by purpose. Read the actual diff to get the details right.
