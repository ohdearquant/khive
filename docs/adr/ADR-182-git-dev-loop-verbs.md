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

## Amendment 1 (2026-09-08): credential rule, enforcement order, fork pull requests, checkout scope

Adopted on the first whole-file read against ADR-108. The header's "all of it stands" reads as "all
of it stands except hard rule 4, amended in item 1".

1. **Credential rule (amends ADR-108 hard rule 4).** ADR-108 has khive never become a credential
   broker: writes run on the daemon's own credentials. This record amends that for actor-bound
   operations: a per-actor credential reference is resolved by the daemon at call time, is never
   returned to any caller, is never stored (the table holds keychain reference names only), and the
   receipt names `credential: actor | daemon`. No verb, receipt or error carries a secret value.
2. **Enforcement order.** The Gate (ADR-018) decides first, as today; `tool.check` decides second;
   both decisions are in the receipt; a refusal at either seam is the refusal, and the pack remains
   not the policy author. Where a deployment wants one seam, the Gate consults `tool.check`.
3. **Fork pull requests (ADR-108 fork (d)).** `git.pr_review(approve)` and `git.pr_merge` on a pull
   request whose head repository differs from the base are refused unless a policy row named
   `git.pr_review.fork` or `git.pr_merge.fork` allows the actor; the administrator flag is never
   passed on a fork pull request.
4. **Checkout scope.** `git.checkout` is a read-only object read (`ls-tree` and object reads into
   blobs); it is not the working-tree checkout ADR-108 left out of scope, and it leaves the
   repository's working tree untouched.

Acceptance arms added: 9 no response, receipt or table row contains a credential value (a decoy
value planted in the keychain item is absent from every output); 10 a Gate deny produces a receipt
with the Gate decision and no `tool.check` call; 11 approving a fork pull request without the row is
refused, and a fork merge never carries the administrator flag; 12 `git status` is identical before
and after `git.checkout`.

## Amendment 2 (2026-09-08): exact compares, actor-only credentials, dispositions, receipts

Adopted on the second whole-file read against the loop driver's contract page. Each item is a
contract the driver's tests bind to; the record above stands where not restated, and Amendment 1
item 1 is amended again in item 3.

1. **Exact compares on every ref move.** `git.branch(repo, name, from, expected?)`: `from` is a sha
   or a ref name; when `expected` is given, `from` must resolve to it at call time. The ref is created
   with `git update-ref refs/heads/<name> <sha> <zero-oid>`, a create-only compare-and-swap, so an
   existing ref refuses and nothing is written. `git.commit(repo, branch, tree, message,
   expected_head, session_id?)`: the new commit's parent is `expected_head` and the ref move is
   `update-ref refs/heads/<branch> <new> <expected_head>`; a tip elsewhere refuses before any ref
   moves. `git.push(repo, branch, expected_local, expected_remote, session_id?)`: `expected_local`
   must equal the local tip at call time; `expected_remote` is required, a sha or an explicit `null`
   meaning the remote branch must not exist, and omission refuses as `invalid_params`; the push is
   `git push <remote> <expected_local>:refs/heads/<branch>
   --force-with-lease=refs/heads/<branch>:<expected_remote>` after a fast-forward check, so a remote
   that moves between the check and the push refuses at the server. `git.pr_review` submits its
   review with `commit_id = expected_head` after reading the head from the platform; `git.pr_merge`
   merges with the platform's match-head option, so a head that moves after the pre-check is refused
   by the platform itself. Every other `expected_*` value is a 40-hex sha; absent or `null` refuses.
   `git.branch.expected` stays optional.
2. **Commits by plumbing.** `git.commit` builds the commit from the khive tree with
   `hash-object -w --no-filters`, `mktree`, `commit-tree` and `update-ref`; no checkout, index or
   working tree is touched; a path the manifest omits is a deletion; `100644` and `100755` are the
   only modes. This replaces the Decision's detached-worktree materialization.
3. **Actor-only credentials (amends Amendment 1 item 1).** `git.commit`, `git.push`, `git.pr_open`,
   `git.pr_review` and `git.pr_merge` resolve the caller's `[git_write.actors]` row at every call; an
   actor without a row refuses with reason `actor_unmapped`; these verbs have no daemon fallback and
   `credential.source` takes only the value `actor`. Author name and email come only from that row;
   the params `author`, `actor` and `credential` refuse as `invalid_params`. The credential value is
   read through `[git_write] credential_resolver`, an argv template with an absolute program, no
   shell and `{ref}` as its only template argument (default: the platform keychain lookup by
   reference name), validated at startup; a resolver failure is `actor_unmapped`. The receipt carries
   `credential: {source: "actor", ref, platform_identity}`.
4. **No hooks, filters or configuration.** Every git invocation runs with
   `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgsign=false
   -c credential.helper= -c core.sshCommand=/usr/bin/false`, `GIT_CONFIG_NOSYSTEM=1` and
   `GIT_CONFIG_GLOBAL=/dev/null`; object writes use `--no-filters` and reads use `cat-file`, so clean
   and smudge filters never run. Only `https` remotes are supported in this slice; another scheme
   refuses with reason `remote_scheme`.
5. **Repository and reviewer identity.** Before any platform write the CLI's reported slug and
   visibility must both equal the configured expectation for `repo`. Self-approval is refused at the
   platform login: the reviewer's login, read through its own credential, must differ from the pull
   request author's login, so two actors on one platform account refuse, and two actor rows naming
   the same credential reference refuse likewise. `git.pr_merge` requires an approved review on
   `expected_head` from a login other than the author, read from the platform before the merge call.
   The administrator flag needs the `git.pr_merge.admin` row and is never passed for a fork.
6. **Dispositions.** Every receipt carries `disposition: not_committed | committed | unknown`. A
   mutation writes its receipt row as `unknown` before the effect and updates it to `committed` or
   `not_committed` afterwards; a lost reply or a failed audit append leaves `unknown` or `committed`
   in the durable row, never a refusal, and a caller never retries a mutation. `git.reconcile(receipt)`
   is read-only: it re-reads the ref or the pull request and settles an `unknown` row to what the
   platform holds.
7. **Receipts.** `git.receipts(repo?, session_id?, limit, offset)` returns `{receipts, next_offset}`
   scoped to the calling actor; an `actor` filter naming another actor refuses. A receipt carries
   `id`, `namespace`, `actor`, `session_id`, `verb`, `repo`, `inputs`, `gate: {decision, source:
   "git_write.allowed", id}` (the allow-list entry index, or `deny:<reason>`), `policy: {decision,
   source, id}` (null when the gate denied, because `tool.check` was never called), `fork_policy` on a
   cross-repository pull request, `credential`, `timing: {started_at, finished_at}`, `disposition`,
   `result` and `reason`. `git.gates(repo)` lists the effective allow-list rows with those ids. No
   receipt, error or table row carries a credential value.
8. **Trees and diffs.** `git.checkout(repo, ref)` returns `{commit, tree}` from `rev-parse`,
   `ls-tree -r` and `cat-file blob`; symlink and submodule entries refuse. `git.diff(repo,
   input_kind, base, head)` with `input_kind` `commits` or `trees` runs `git diff-tree -p
   --no-ext-diff --no-textconv --no-color --no-renames <base> <head>` and `--numstat` for
   `summary: {files, additions, deletions}`; tree inputs are written to a scratch repository with
   `mktree` first. The receipt's `inputs` are exactly `{input_kind, base, head}`.
9. **Test-only faults.** `[git_write] contract_faults = true` refuses at startup unless the binary was
   built with the `contract-faults` feature, and the refusal is named in the startup log.
10. **Operator read (Proposed).** A policy-gated `git.receipts.all` for an operator actor enters as
    Proposed so the audit ledger has a verb; the driver's tests do not require it.

Acceptance arms added: 13 an existing ref refuses `git.branch` with nothing written; 14 a moved
parent refuses `git.commit` before any ref moves, and a committed tree equals its manifest with the
index and working tree unchanged; 15 `author`, `actor` and `credential` params refuse, and an
unmapped actor refuses with no daemon credential read; 16 hostile hooks, filters and configuration
leave no marker and the committed bytes are exact; 17 a moved local tip, a moved remote tip, and a
remote moved at the ref-move seam each refuse with both ref sets unchanged; 18 `force`,
`force_with_lease` and `refspec` in every form refuse; 19 an omitted `expected_remote` refuses with
no effect, an explicit `null` creates only an absent remote branch, and an explicit `null` against an
existing rival head preserves the rival; 20 a slug or visibility mismatch refuses `git.pr_open`; 21
approval by the author's login or by a second actor on the same platform account refuses, and a
review binds `commit_id` to `expected_head`; 22 a head moved before or after the check refuses the
merge, and `ask`, `deny`, an expired grant, a foreign grant, a wrong scope and a missing review each
refuse; 23 a lost reply after a committed push or merge leaves a `committed` or `unknown` row with
exactly one native call, and `git.reconcile` settles it to `committed`; 24 receipts page by
`session_id`, a foreign `actor` filter refuses, and a planted decoy credential value is absent from
every reply, receipt, table row and raw record; 25 the resolver is read exactly once per mutation
and returns the rotated value; 26 a gate deny records no `tool.check` call and a null policy, while a
gate allow records exactly one call with the policy id.
