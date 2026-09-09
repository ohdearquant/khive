# ADR-182: Git Verbs for the Dev Loop: Trees In and Out, Commit as Actor, Policy-Gated Push, Pull Requests

- **Status**: Accepted (2026-09-09, implemented by the git pack dev-loop verbs)
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

## Amendment 3 (2026-09-08): credential selection bound to the resolved caller, fork merges recorded

Adopted after the review that followed the first exec implementation, which read this record beside
it. Item 1 makes Amendment 2 item 3 exact; item 2 records an argument rather than a change.

1. **The actor is the identity the runtime resolved, never a value the call carries.** The
   `[git_write.actors]` row that selects a credential is looked up by the actor label the runtime
   resolved for the request before the handler ran (ADR-096: the peer's declared identity on the
   daemon socket, or the configured actor of an in-process dispatch), which is the label the receipt
   records as `actor`. No request parameter, no environment variable read by the handler and no
   repository configuration takes part in that lookup; the `actor`, `author` and `credential` params
   refuse (Amendment 2 item 3) so that a caller cannot name a row. A contract fixture therefore binds
   an identity by running a process as that actor, not by passing a field. Two processes with the
   same resolved label share the row, which is a trust decision the deployment takes in
   configuration.
2. **Fork merges (recorded argument).** The review asked that a merge of a cross-repository pull
   request require a human decision. The policy row `git.pr_merge.fork` is that decision: a person
   writes it, it names the actor, and without it every fork merge refuses (Amendment 1 item 3). A
   per-call prompt is not available to a daemon with no console, and the row leaves the person in
   the loop and the decision in the audit. No rule changes.

Acceptance arms added: 27 two processes resolved as different actors get different credential
references for the same call, a process whose resolved actor has no row refuses `actor_unmapped`
while the same call from a mapped actor proceeds (control), and the receipt's `actor` equals the
resolved label in every case.

## Amendment 4 (2026-09-08): symbolic refs, reconcile evidence, the tool pack at call time, gates on reads

Adopted during the first implementation of the local slice, from findings the implementer read off
the source before running anything. Each item records a ruling already given; nothing above is
withdrawn.

1. **Symbolic refs.** Every `update-ref` compare-and-swap runs with `--no-deref`, so the compare and
   the move apply to the named ref itself; a branch ref observed as symbolic refuses before any move
   with reason `ref_symbolic`. The expected-old semantics of Amendment 2 item 1 are unchanged.
2. **Reconcile evidence.** Every ref move runs `update-ref --create-reflog -m
   khive-receipt:<receipt-id>`. `git.reconcile` settles an `unknown` row to `committed` only when the
   reflog of the moved ref carries the marker for that receipt together with the new sha. Equality of
   the current head with the intended sha never settles anything, because a rival can install the
   same sha (a deterministic commit, a concurrent create), and a pruned or absent reflog leaves
   `unknown`.
3. **The tool pack at call time.** The git pack keeps its kg dependency for the legacy verbs. The
   verbs of this record require the tool pack when called and, when its tables are absent, fail
   closed after the gate evaluated, with reason `policy_unavailable` and receipt
   `policy: {decision: "deny", source: "policy_unavailable", id: null}`, so `policy` is null exactly
   when the gate denied (Amendment 2 item 7). A boot-time dependency is a later decision.
4. **Gates on reads, and `git.branch` without a credential.** `git.checkout`, `git.diff` and
   `git.gates` gate on a repo-only match of the allow-list, and `gate.id` is the lowest matching entry
   index; `git.reconcile` gates on the repo stored in the receipt; `git.receipts` is an audit read
   scoped to the calling actor, is not filtered by the current allow-list (removing a row never hides
   history) and writes no receipt of its own, as `git.gates` writes none. `git.branch` takes no
   credential and no actor row; its receipt carries `credential: null`.
5. **Daemon fingerprint.** The daemon's configuration fingerprint covers the whole `[git_write]`
   section (allowed rows, actors, resolver, faults), so any change to it retires the running daemon.

Acceptance arms added: 28 a branch ref made symbolic refuses `git.branch` and `git.commit` with
`ref_symbolic` and nothing moves, while the same call on a plain ref proceeds (control); 29 a receipt
left `unknown` after a rival installed the intended sha stays `unknown` under `git.reconcile`, and
one whose reflog carries its marker settles to `committed`; 30 with the tool pack absent,
`git.commit` with a tree refuses `policy_unavailable` with the gate decision recorded, while the
legacy `git.commit` with `paths` still runs.

## Amendment 5 (2026-09-08): reconcile needs the marker and the ref, policy through the registry

Adopted the same day as Amendment 4, from two further source readings by the implementer. Item 1
tightens Amendment 4 item 2; item 2 fixes the mechanism behind Amendment 4 item 3.

1. **The marker is necessary, not sufficient.** Git's files backend appends the reflog entry before
   it installs the ref, and a failed install does not remove the entry, so a process lost between the
   two leaves the marker with no ref move. `git.reconcile` settles an `unknown` row to `committed`
   only when both hold: the reflog of the ref carries `khive-receipt:<receipt-id>` with the new sha,
   and that sha is the current head of the ref or an ancestor of it. A marker without the ref
   installed, and a head equal to the sha without the marker, both leave `unknown`. No claim of
   crash atomicity is made anywhere in help or receipts.
2. **Policy through the registry.** Pack backends may differ (ADR-028), so a decision read through
   the git pack's own backend handle could see stale tool rows. The verbs of this record obtain their
   policy decision through the registry dispatch of `tool.check`, carrying the caller's identity and
   namespace, never by reading the tool tables directly; the decision is taken before any write
   begins and no writer or lock is held across the dispatch. A failed or absent dispatch is the
   receipted `policy_unavailable` of Amendment 4 item 3.

Acceptance arms added: 31 a hand-appended reflog entry carrying a receipt's marker while the ref
still points elsewhere leaves that receipt `unknown` under `git.reconcile`, and the same marker with
the ref advanced past the sha settles to `committed`; 32 with the git and tool packs on different
backends and a stale allow row planted in the git backend beside a deny row in the tool backend, the
verb refuses with the tool pack's decision and id.

## Amendment 6 (2026-09-08): the remote marker after acknowledgement, `reflog write`, match-head merge, recorded scope, repository configuration, published schema

Adopted from the second implementation slice (push with exact compares, pull-request verbs, fork
rows, the pack schema table). Nothing above is withdrawn; items 1 and 2 tighten Amendment 4 item 2
and Amendment 5 item 1 for the remote case, items 3 to 6 record decisions the slice needed and the
Decision left open.

1. **The remote marker is written after acknowledgement and exact readback.** For `git.push` the
   local marker is appended only after the remote has acknowledged the push and a readback of the
   remote ref returns exactly the pushed sha. A reply lost before the acknowledgement leaves the
   receipt `unknown` with no marker, and `git.reconcile`'s remote arm settles an `unknown` push to
   `committed` only when both hold: the remote ref reads back at the pushed sha, and the marker is
   present; either alone leaves `unknown`. The preflight compares are exact (Amendment 2): the local
   branch head must equal `expected_local`, the remote ref must equal `expected_remote`, and the
   remote must not already equal the candidate, which refuses as `already_at_target` with a receipt
   and no remote effect, so a push that would move nothing is a refusal, never a silent success.
2. **The marker mechanism is `reflog write`, not a ref update.** `update-ref` with identical old and
   new shas appends no reflog entry (measured on the host Git during the slice), and after a push the
   local ref does not move, so the marker is written with
   `git reflog write refs/heads/<branch> <sha> <sha> khive-receipt:<receipt-id>`, which changes no ref,
   index or worktree. A capability probe runs before the resolver and before any network use: a Git
   that does not advertise `reflog write` refuses the push before any remote effect, with a receipt
   naming the Git version and the missing capability, in the toolchain-identity refusal shape the pack
   already uses. No compatibility with older Git is claimed from source; the receipt is the claim.
3. **Merge by match-head compare-and-set, no administrator bypass.** `git.pr_merge` merges through
   the platform's REST endpoint `PUT /repos/{slug}/pulls/{number}/merge` with `sha` set to
   `expected_head`, so a head that moves between the verb's own check and the platform call is
   refused by the platform; the refusal is receipted with disposition `not_committed` and nothing is
   retried. The administrator bypass is not requested by this slice even where a `git.pr_merge.admin`
   row allows it; a later slice that needs the bypass re-states the arm. Self-approval is refused
   before the platform call both for the actor that opened the pull request and for a different actor
   whose credential row resolves to the same platform login, read from the platform. The same pre-check,
   on `git.pr_review(approve)` and on `git.pr_merge`'s required-review read, also refuses when the
   reviewer's platform login equals the login that last pushed `expected_head`, because the platform's
   last-push rule disqualifies exactly that approval and a merge that relies on it strands. That login
   is read from the pack's own ledger, never inferred: it is the platform login on the newest `git.push`
   receipt whose acknowledged head equals `expected_head`; a commit's author or committer, a workflow's
   triggering actor and the platform's events feed do not name the pusher of a ref. With no such receipt
   the pre-check refuses nothing and the receipt records `last_pusher` as unknown with reason
   `no_push_receipt`. Independently, `git.pr_merge`'s required-review read takes the platform's own
   review decision: the pull request's `reviewDecision` must read `APPROVED`, never a count of approving
   reviews, so an approval the platform has disqualified under its last-push rule (an approving review
   under `REVIEW_REQUIRED`) is refused before the merge call even when the push happened outside the
   pack.
4. **Grant scope is recorded, not evaluated.** The tool policy decision matches actor, tool pattern and
   expiry; a grant's `scope` is free text that stays on the grant row, and the receipt cites that row by
   its grant id beside the policy decision and its source, without copying the scope. A grant whose
   tool pattern does not match the verb refuses in the same receipt shape as no grant; no
   repository-scoped authorization is derived from the scope text.
5. **Repository configuration.** The expectation a pull-request verb asserts before any platform
   write comes from `[git_write.repositories."<absolute repository path>"]` with `remote`, `slug` and
   `visibility`; at call time the key must match an ADR-108 allow-list row or the call refuses. A
   repository without a row is refused by the pull-request verbs, never guessed from the remote.
6. **The schema table is published through a runtime hook.** The pack publishes an input schema for
   every git verb through a defaulted runtime hook, `input_schema(verb)` returning the table when the
   pack has one; `help` and the MCP tool list read it, and a pack without a table keeps the parameter
   rendering it has today. No handler definition shape changes.

Acceptance arms added: 33 a push whose remote already equals the candidate refuses `already_at_target`
with a receipt, no marker and no remote effect; 34 a transport cut after the send and before the
acknowledgement leaves the receipt `unknown` with no marker, and `git.reconcile` settles it only when
both the remote readback and the marker hold; 35 `update-ref` with identical shas appends no reflog
entry while `reflog write` does, the control behind item 2; 36 a simulated Git without `reflog write`
refuses before the resolver and the network with the version and the capability in the receipt; 37 a
second actor whose credential resolves to the author's platform login is refused approval before the
platform call; 38 a merge whose head moved between the check and the platform call is refused by the
platform as `not_committed` and not retried; 39 a repository key absent from the allow-list refuses at
call time; 40 `help` on every git verb returns the schema table and a pack without one keeps its
parameter rendering; 41 an approval by the login on the newest `git.push` receipt for `expected_head` is refused before
the platform call citing that receipt; a head with no push receipt takes the approval and records
`last_pusher` unknown; a merge whose `reviewDecision` reads other than `APPROVED` is refused before the
merge call; an approval by a third login under `reviewDecision` `APPROVED` proceeds (control). Mutation controls stated before they ran and both failed at their effects:
removing the preflight together with the remote comparison overwrote a rival bare ref with the
candidate; removing only the platform-login check let an alias actor submit an approval as the author
account.

Corrected the same day, after the slice read the record against its implementation: item 3 names the
source of the last pusher (the pack's push receipts, and the platform's own review decision at merge)
where the first text named a workflow's triggering actor, which is not the pusher; item 4 places the
scope on the grant row the receipt cites, where the first text said the receipt carried it; arm 41
follows item 3.

## Amendment 7 (2026-09-09): merge dispatch refusals per repository

Adopted from a caller's reading of the `git.pr_merge` help against the rule its operators hold for
merges: the account that opened a pull request and the account that last pushed its head do not
perform the merge. Amendment 6 item 5 stands; this adds one optional field to the repository row.

1. **`merge_refusals` on the repository row.** `[git_write.repositories."<path>"]` may list
   `merge_refusals = ["opener", "last_pusher"]`, each entry at most once; any other entry or a
   repeated one fails configuration validation. The list is empty by default, and an empty list
   changes nothing.
2. **`opener`.** `git.pr_merge` is refused with reason `merge_by_opener` when the dispatching actor's
   platform login equals the pull request author's login (evidence `platform_login`), or when this
   namespace holds a committed `git.pr_open` receipt for that number by the dispatching actor or by
   its credential reference (evidence `pr_open_receipt`): the same two readings that refuse a
   self-approval on `git.pr_review`.
3. **`last_pusher`.** `git.pr_merge` is refused with reason `merge_by_last_pusher` when the
   dispatching actor's platform login equals the login on the newest `git.push` receipt whose
   acknowledged head equals `expected_head` (Amendment 6 item 3); with no such receipt the entry
   refuses nothing and `last_pusher` stays unknown on the receipt.
4. **Order and receipt.** Both refusals run after the fork policy and the last-pusher read and before
   the review-decision read and the merge call, so a refused merge writes nothing to the platform.
   The receipt carries `result.merge_refusal: {name, source: "git_write.repositories.merge_refusals",
   evidence}` beside `last_pusher`; a permitted merge carries no `merge_refusal`.

Acceptance arm added: 42 with both entries listed, a merge dispatched by the opening actor, by a
second actor on the opener's platform account, and by the last pusher of `expected_head` each refuse
before any platform write with the named reason and evidence on the receipt, and a merge dispatched by
the approving login that is neither the opener nor the last pusher proceeds (control). Mutation
controls, stated before they ran: removing either entry's check alone lets its refusal arm merge.
