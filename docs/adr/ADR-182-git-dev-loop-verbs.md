# ADR-182: Git Verbs for the Dev Loop: Trees In and Out, Commit as Actor, Policy-Gated Push, Pull Requests

- **Status**: Proposed
- **Date**: 2026-09-08
- **Extends**: [ADR-108](ADR-108-git-write-surface.md) and its Amendment 1 (write verbs over an
  allow-listed repo set, force-push denied, hooks disabled; all of it stands),
  [ADR-181](ADR-181-exec-verb-sandboxed-run.md) (the tree manifest)
- **Relates to**: [ADR-180](ADR-180-tool-pack.md) (policy per actor), [ADR-088](ADR-088-git-lifecycle-pack.md)
  (provenance ingest), [ADR-018](ADR-018-authorization-gate.md)

## Context

ADR-108 lets an actor commit, branch and push through khive against repositories an operator has
allow-listed, with the hard rules that make that safe. Three things keep it from carrying a whole
development loop. A repository's content cannot be read into khive as one object, so an agent
without a filesystem cannot see what it is changing, and `git.commit` takes paths in a working tree
the agent has no hands on. Identity is the daemon's: the author is whatever git config says, and the
push uses whatever credential helper the daemon inherits, so "who asked" and "whose permission" are
the same answer. Pull requests are outside khive entirely; the fleet's rule that a second account
reviews is a memory, not a policy. The loop the presentation shows is one pull request end to end
with a second actor approving under its own permissions, every step a receipt. Underlying git and the
GitHub CLI stay underneath; khive adds no format.

## Decision

Verb names below are the proposal; the loop driver's contract tests finalize them.

**Trees in and out.** `git.checkout(repo, ref)` reads the tree at `ref` (`git ls-tree -r` plus object
reads, no working-tree checkout) into blobs and returns an ADR-181 tree manifest reference. `git.diff`
takes either two refs of a repo or two tree references and returns a unified diff as a blob plus a
summary (files, insertions, deletions). `git.commit` accepts `tree` in place of `paths`: the tree is
materialized into a detached, hooks-disabled worktree of the repository at the branch head, the
difference is staged and committed. `repo` remains subject to the ADR-108 allow-list.

**Identity.** The commit author is derived from the calling actor through the `[git_write.actors]`
table (actor label to name, email and credential reference); a caller-supplied `author` is accepted
only when it equals the derived one. The credential reference is a keychain item name; no secret is
stored. An actor without a row falls back to ADR-108 behaviour (the daemon's identity) and the receipt
says `credential: daemon`.

**Push as a policy-gated ref move.** `git.push(repo, branch)` now requires two allows: the ADR-108
allow-list and `tool.check(actor, "git.push")`; the decision and its source id go into the receipt.
Force-push stays denied unconditionally. The push runs with the actor's credential when a row exists.

**Pull requests over the CLI.** `git.pr_open(repo, head, base, title, body)`, `git.pr_review(repo,
number, verdict, body)` with `verdict` in `approve`, `request_changes`, `comment`, and
`git.pr_merge(repo, number, method, subject, body)` with `method` in `squash`, `merge`. Each consults
`tool.check(actor, <verb>)`, runs `gh` with `GH_TOKEN` resolved from the actor's credential reference
and never from the daemon's session, and before any write asserts that the repository slug and
visibility the CLI reports equal the configured expectation for `repo`. `approve` by the actor that
opened the pull request is refused before the CLI runs; the reviewer's decision is what the platform
records afterwards. `pr_merge` passes the administrator flag only when a policy row named
`git.pr_merge.admin` allows it for the actor.

**Receipts.** Every verb writes the ADR-108 audit event with a per-verb kind, carrying actor, repo,
branch or pull request number, the tree references it consumed or produced, the resulting sha or URL,
and the tool policy decision id. `git.receipts(repo, actor, limit)` reads them.

## Acceptance

1. `git.checkout` of a ref returns a tree whose entry count and per-file content match
   `git ls-tree -r` and `git cat-file` at that ref.
2. `git.commit(tree)` produces a commit whose tree equals the materialized manifest; author name and
   email are the actor's row; a caller-supplied `author` that differs is refused; no hook runs
   (ADR-108 Amendment 1 control stays green).
3. `git.push` with a `deny` policy for the actor is refused before any network call; with the check
   removed (mutation control) the push goes through; with `allow` it goes through and the receipt
   names the policy id.
4. `git.pr_open` against a repository whose reported slug differs from the configured one is refused.
5. `git.pr_review(approve)` by the opening actor is refused; by a second actor with its own credential
   row it succeeds and the platform's review state reads approved.
6. `git.pr_merge` with `ask` policy is refused; after `tool.grant` it merges; the merge commit sha is
   in the receipt.
7. Every verb's receipt carries the decision id and the tree or sha it acted on; a refused call has a
   receipt too.
8. Force-push remains denied through every new parameter combination (ADR-108 rule 1 re-run).

## Known rough edges

The GitHub CLI is a runtime dependency of the pull request verbs. The actor credential table is
pack-local until the provider pack owns profiles and credential references. Local merge, rebase and
tags stay out of scope, as in ADR-108. Read-only checkout of a remote goes through the ADR-088 cache
and inherits its bounds.
