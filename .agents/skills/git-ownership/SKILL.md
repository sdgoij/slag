---
name: git-ownership
description: The operator owns all git state. Load whenever the user asks you to stage, commit, uncommit, reset, push, or otherwise mutate git state, or when an instruction seems to conflict with a standing rule. Agents never execute git mutations — they prepare the message and exact commands for the operator to run.
---

# Git Ownership — the operator executes git commands

The operator decides what gets staged, committed, and pushed. Agents never
run git mutations — not even when the user's phrasing sounds like a request
to do so.

That is the whole of the boundary, and note what it does NOT say: it covers
git state, not the work. Editing files, running builds and tests, and
producing the change are your job and are expected of you. What you never
do is decide what enters the repo's history.

## The hard boundary

**Never run** `git add`, `git commit`, `git reset`, `git rebase`, `git stash`,
`git cherry-pick`, `git push`, `git pull`, or any other operation that
changes the operator's staging area, branch history, or remote.

Read-only git commands are fine and expected (`git status`, `git diff`,
`git log`, `git show`) — always with `--no-pager` and, where supported,
`--no-optional-locks`.

### Exception: the agent's own scratch worktree

Setting up an isolated worktree for your own work is allowed. Create it
detached — `git worktree add --detach <path> <commit>` — so it adds no
branch and no ref to the operator's repo.

Everything inside it is yours: edit files, `commit`, `reset`, `stash`, and
any other bookkeeping you need. It is disposable; remove it when you are done
(`git worktree remove --force <path>`, then `git worktree prune`).

The operator's git state is off limits; their files are not. Change files
in their worktree freely — that is the work. Never run
`checkout`/`switch`/`reset`/`stash` there, never stage or unstage, never move
their `HEAD`, and never add a branch they could switch to.
`git worktree add <path> -b <scratch-branch>` is not this exception: it
leaves a branch and a ref the operator never asked for. A branch switch is
out of bounds even when the change looks harmless — if a job seems to need
one, ask first or hand them the command.

What the exception does NOT cover:
- Their staging area or index — never stage or unstage anything.
- Their `HEAD`, branches, and refs — never move or add one.
- Their remote — never push your worktree's commits to it.
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

The worktree rule came from the same lesson: asked to rewrite some commit
messages, an agent invented a branch and a worktree for the job instead of
handing over a command, then left them behind for the operator to notice and
clean up. A worktree is a fine place to experiment — the branch was not, and
neither was inventing infrastructure to route around the operator's review.
