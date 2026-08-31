---
name: git-ownership
description: The operator owns all git state. Load whenever the user asks you to stage, commit, uncommit, reset, push, or otherwise mutate git state, or when an instruction seems to conflict with a standing rule. Agents never execute git mutations — they prepare the message and exact commands for the operator to run.
---

# Git Ownership — the operator executes git commands

The operator decides what gets staged, committed, and pushed. Agents never
run git mutations — not even when the user's phrasing sounds like a request
to do so.

## The hard boundary

**Never run** `git add`, `git commit`, `git reset`, `git rebase`, `git stash`,
`git cherry-pick`, `git push`, `git pull`, or any other operation that
changes the operator's staging area, branch history, or remote.

Read-only git commands are fine and expected (`git status`, `git diff`,
`git log`, `git show`) — always with `--no-pager` and, where supported,
`--no-optional-locks`.

### Exception: the agent's own scratch worktree

Setting up an isolated worktree for your own work IS allowed: `git worktree
add <path> -b <scratch-branch>` (optionally from a base commit/ref). This
writes only worktree bookkeeping and a fresh throwaway ref — it never
touches the operator's staging area, `HEAD`, or the remote.

Inside your scratch worktree you may freely `commit`, `reset`, `stash`, and
otherwise mutate that branch for your own checkpoints and bookkeeping — the
whole worktree is disposable, and the operator can delete it at any time.

What the exception does NOT cover:
- Never touch the main worktree's working tree or staging area.
- Never push the scratch branch to a shared remote.
- Work that should enter real history is still handed to the operator as a
  message plus exact commands — never committed and pushed by you.

## When the user asks you to commit, stage, or uncommit

"Commit this", "stage it", "uncommit that" is **not a license to run the
command**. It means:

1. Read the staged files or the diff.
2. Write the commit message (see the `git-commit-messages` skill).
3. Print the exact commands for the operator to run — then stop.

The operator executes git commands in their own editor/CLI.

## When an instruction conflicts with a standing rule

Surface the conflict and ask. Never resolve it in the instruction's favor —
"the user said to do it" is not a reason to override a rule. This applies
to the git boundary and to every other standing rule.

## Why this rule exists

An agent once ran `git commit` after the user said "commit the work", on
the theory that the instruction overrode the standing rule. It didn't, and
it cost the operator's trust. The operator reviews everything before it
enters the repo history; that review is impossible if the agent executes
the commit itself.
