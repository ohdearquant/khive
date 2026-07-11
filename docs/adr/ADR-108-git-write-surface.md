# ADR-108: Git Write Surface Through khive (Phase B)

**Status**: Proposed\
**Date**: 2026-07-11\
**Authors**: khive maintainers\
**Depends on**: ADR-088 (Git-Lifecycle Pack) and its Amendment 1 (`git.digest`), ADR-018
(Authorization Gate), ADR-017 (Pack Standard), ADR-016 (Request DSL), ADR-007 Rev 7
(Namespace as Attribution-Only)\
**Related**: ADR-085 (Code Pack - admin-only ingest path precedent), ADR-004 (Substrate
Observables - `Event` store used for audit)

This is a decision ADR for a human spec gate. It enumerates forks with trade-offs rather
than picking silently. Open Questions are marked explicitly and are not resolved here.

## Context

`khive-pack-git` (ADR-088, amended by ADR-088 Amendment 1) is read/ingest-only today. It
populates `commit`, `issue`, and `pull_request` notes from local `.git` history and `gh`,
and exposes exactly one agent-facing verb, `git.digest`, which reads a repository and
writes graph records - never git-repository state. Nothing in the current surface performs
a `git commit`, `git push`, branch creation, or any other mutation of a git repository or
its remote.

Two design inputs motivate a write surface:

1. A development-tool git wrapper is planned that routes an agent's git writes (commit,
   branch, push) through khive instead of invoking `git`/`gh` directly. This gives the
   khive Gate (ADR-018) a chokepoint it does not have today: git writes issued directly by
   an agent process via shell `git` bypass khive entirely, so no policy, audit, or
   protected-branch rule can apply to them.
2. Once such a surface exists, fleet-level policy becomes possible: local hooks (or an
   agent harness) can deny direct `git push` / `gh pr create` invocations and require the
   khive-mediated path instead, giving one real enforcement point for "which principal may
   write to which repo/branch" instead of relying on convention.

Today, no such enforcement point exists. Any process with local git credentials can push to
any branch of any repository it can reach; khive has no visibility into git writes and no
way to deny one.

The gap this ADR addresses: khive has an authorization seam (the Gate) and an audit plane
(the `Event` substrate, ADR-004) for verb dispatch, but nothing routes git mutations through
that seam. This ADR specs adding write verbs to the git pack, gated at dispatch, audited via
the event plane, exactly as every other khive verb already is.

## Decision

This ADR proposes adding a write-verb surface to `khive-pack-git`, structurally consistent
with the pack's existing `git.digest` verb (agent-facing, `pack.verb` namespaced per
ADR-023) and dispatched through the same `VerbRegistry` / `Gate` seam every other verb uses.
It deliberately does **not** fix the four forks below - those are the open decisions this
ADR exists to present to the spec gate.

### Candidate verb set (subject to Fork (a))

At minimum, three write operations are named as design inputs:

- `git.commit(repo, message, paths?, author?)` - stage and commit. `paths` narrows to a
  subset of the working tree; absent, commits everything currently staged/changed
  (exact semantics depend on Fork (a)).
- `git.branch(repo, name, from?)` - create a branch, optionally from a named ref/SHA.
- `git.push(repo, branch, remote?, force?)` - push a branch to a remote.
  `force` is a parameter only for validation purposes: see the force-push rule below, which
  makes every `force=true` request a hard deny regardless of policy.

A broader candidate list, to be narrowed at the spec gate: `git.checkout`, `git.merge`,
`git.tag`, `git.pull` (fetch+merge/rebase - arguably read-adjacent, not a write to the
target repo's canonical history, but it mutates the local working copy), `git.fetch`
(unambiguously read-only against the remote, local-write only).

### Hard rules (not forked - these apply regardless of how the forks below resolve)

1. **Force-push is always denied.** No policy, obligation, or actor grants `force=true`.
   This is enforced at the handler, not left to policy configuration - a Gate
   misconfiguration (e.g., a permissive `AllowAllGate` in a networked deployment) must not
   be able to authorize a force-push. If a real workflow needs history rewrite, it is
   explicitly out of scope for this surface; the actor uses direct git access outside
   khive, and that access is exactly what fleet-level policy (design input 2) is meant to
   discourage but this ADR cannot itself prevent.
2. **Full audit via the event plane.** Every write-verb dispatch - allowed or denied -
   produces an `AuditEvent` (ADR-018) and an `Event` record (ADR-004) with `kind =
 "git.write"` (or a per-verb kind, e.g. `"git.commit"` - see Fork (c) for whether policy
   objects also need finer-grained kinds). The audit record carries actor, repo, branch,
   verb, decision, and - for `git.commit` - the resulting SHA on success. This is
   additional to git's own commit-graph audit trail (which records _what_ changed) and
   answers a different question: _who, through khive, asked for this write, and was it
   allowed_.
3. **Protected-branch rules are enforced at the Gate, not hardcoded in the pack.** The pack
   handler does not itself know which branches are protected; it constructs a
   `GateRequest` (actor, repo/branch as policy-input fields, verb) and the Gate decides.
   This follows ADR-018's existing model exactly - the pack is not the policy author.
4. **This surface never grants elevated git credentials to a caller.** The daemon process
   that executes the write must already have write access to the target repo/remote
   (whatever mechanism grants that - see Fork (b)); khive does not become a credential
   escalation path. A caller who lacks git write access outside khive gains none by using
   this verb surface if execution goes through the daemon's own credentials - this is
   itself a consequence discussed under Fork (b), not a settled design.

### Fork (a): Verb granularity

**A1 - Thin, 1:1 git verbs.** `git.commit`, `git.branch`, `git.push` map directly to `git
commit`, `git branch`, `git push`. Composable: an agent chains `commit | branch | push`
itself, or the client-side git wrapper (design input 1) composes them.

- Pro: small, predictable surface; each verb has one clear failure mode; matches the
  existing `git.digest` precedent of one verb, one operation.
- Con: multi-step workflows (e.g., "commit and push in one round-trip") cost multiple MCP
  calls; partial-failure states are more visible to the caller (e.g., commit succeeds,
  push fails) which is arguably a feature, not a bug, for audit granularity.

**A2 - Task-level verbs.** `git.publish_branch(repo, branch, message, paths?)` performs
commit + push (and branch-create if the branch does not exist) as one verb, matching the
actual shape of "agent finished a task, wants to publish it" rather than exposing raw git
primitives.

- Pro: matches the motivating consumer (the git wrapper) more directly - one call per
  logical action; fewer round-trips for the common path; a single Gate decision per logical
  publish action instead of three, which may be what protected-branch policy actually wants
  to reason about.
- Con: a compound verb makes partial failure semantics (git succeeds at commit, fails at
  push) a design problem inside one handler rather than a sequencing problem the caller
  already handles; harder to compose with future automation (e.g., a wrapper that wants a
  branch created without an immediate push cannot use `publish_branch` alone).

**A3 - Both.** Ship the thin primitives (A1) and layer `publish_branch` as a convenience
verb built from them, matching how `git.digest` is itself a convenience layer over
`ingest::run_ingest`, which the CLI (`kkernel git-ingest`) also calls directly.

**Open Question 1**: which of A1 / A2 / A3, and - if A1 or A3 - the exact verb list (is
`git.tag` in scope for Phase B, or deferred). Not decided here.

### Fork (b): Execution home

**B1 - Daemon-side `git2-rs` (libgit2 bindings).** The khive daemon (`kkernel mcp
--daemon`, ADR-049) links `git2` and performs commit/branch/push operations as native
library calls against the repository's `.git` directory, reusing the same daemon process
that already holds warm ANN/embedder state.

- Pro: no shell-out, no argument-injection surface via a spawned process; structured error
  types instead of parsing git's stderr; consistent with the read-side ingester
  (`crates/khive-pack-git/src/ingest.rs`), which already shells `git log`/`git show` via
  `std::process::Command`, not `git2` - so B1 would be a new dependency and a divergence
  from the existing read-path implementation choice, not a continuation of it.
- Con: `git2-rs`/libgit2 has historically lagged behind system git on some auth/transport
  edge cases (SSH agent forwarding, some credential helpers); push authentication becomes
  khive's problem to implement correctly (credential-helper protocol, SSH known_hosts) matching what would otherwise have been in the local git config.

**B2 - Shell to system git with a hardened argument allowlist.** Continue the read-side
pattern (`ingest.rs` already shells `git log --name-only`, etc.) for writes: construct
`git commit -m <message> -- <paths>`, `git push <remote> <branch>` as argument vectors
(never a shell string - `std::process::Command::new("git").args([...])`, no shell
interpolation), with a fixed allowlist of subcommands and flags the handler is willing to
construct. No caller-supplied argument reaches the process boundary unvalidated; e.g.
`message` is passed as a single `-m` argument value (never concatenated into a shell
string), and `branch`/`remote`/`paths` are validated against a restrictive character set
before being placed in the argv array.

- Pro: reuses the exact git installation and credential configuration (SSH keys, credential
  helpers, `.netrc`, `gh auth`) the operator's environment already has working - the same
  reasoning ADR-088 §5 used to justify `gh` CLI shelling over a direct REST client for
  issues; consistent with the existing ingester's implementation choice.
- Con: shelling out is a documented injection-risk pattern; the "hardened allowlist" is only
  as strong as its implementation and needs its own adversarial review (argument vectors
  are the safe primitive - the risk is a future edit that concatenates instead of passing
  argv, or a flag that was not meant to be allowlisted, e.g. `--upload-pack` on push).

**Open Question 2**: B1 or B2. This ADR's default lean, given the existing ingester's
precedent (`ingest.rs` already shells system `git`) and ADR-088 §5's explicit precedent for
preferring the operator's existing tool credentials over reimplementing transport/auth, is
toward B2 - but this is explicitly not decided here; the spec gate may weigh the injection
surface differently.

### Fork (c): Policy declaration

**C1 - Static config.** Protected-branch rules, per-repo write allowlists, and per-actor
permissions live in `khive.toml` (or a dedicated `[git]` config block), read at daemon
startup. Simple, no new trait surface, but every policy change requires a config edit and
daemon restart (or a hot-reload mechanism this ADR would also need to spec).

**C2 - Gate policy objects.** Reuse the existing `Gate` trait (ADR-018) unmodified: the git
pack constructs a `GateRequest` with `namespace`, `verb` (`git.commit` etc.), and `args`
carrying `repo`/`branch` as part of the request body; a `RegoGate` or custom `Gate` impl
decides allow/deny using those fields as policy input, exactly as any other verb's dispatch
does today. No new trait, no new enforcement code path - the git pack becomes another
`Gate` consumer.

- Pro: zero new authorization machinery; protected-branch and per-repo/per-actor rules are
  expressed the same way every other khive policy is (Rego, or a custom `Gate` impl);
  consistent with ADR-018's stated goal of one enforcement seam for all verbs.
- Con: `repo` and `branch` are not first-class `GateRequest` fields today (`GateRequest` has
  `actor`, `namespace`, `verb`, `args`, `context` - repo/branch would travel inside `args`,
  which a policy author must know to inspect, rather than as typed top-level fields a policy
  schema can rely on). This is a minor extension surface, not a new trait.

**Open Question 3**: C2 is the ADR-018-consistent default - this ADR does not see a case
for a git-specific policy mechanism when the Gate trait already exists for exactly this
purpose. What remains open is whether `repo`/`branch` need to become typed `GateRequest`
fields (an ADR-018 amendment) or stay inside `args` (no amendment, policy authors read
`input.args.repo`). Left to the spec gate.

### Fork (d): Composition with external-PR / fork-repo trust rules

khive's standing operational rule (external-PR protocol) treats fork-PR content as
untrusted: no auto-merge, no admin-merge, no branch-update from a fork PR without a human
in the loop. A write surface that lets an agent `git.push` on khive's behalf intersects this
rule in two distinct ways that need to be kept separate:

1. **An agent pushing khive-authored commits to a branch it controls** (its own feature
   branch, opening a PR) - this is the mainline use case design input 1 describes, and does
   not touch the external-PR rule at all: it is agent-authored content being written by the
   agent's own action, mediated by khive instead of raw git.
2. **A write surface that could be used to write fork-PR content into a repo** (for example,
   a future verb that applies a fork PR's diff and pushes it) is categorically different and
   is explicitly **out of scope for Phase B as specified here**. This ADR's verb set
   (commit / branch / push of content the calling agent itself produced) does not include
   "apply and push a diff from an untrusted external source." If a future verb needs to
   touch fork-PR content, it is a separate ADR that must explicitly reconcile with the
   external-PR protocol, not an extension folded into this one.

**Open Question 4**: whether the Gate policy layer (Fork (c), C2) should carry an explicit
`source_trust` or equivalent field to make (1) vs (2) distinguishable in policy, or whether
keeping (2) entirely unbuilt (not just unpolicied) is the correct boundary for Phase B. This
ADR leans toward the latter - not building the capability rather than trusting policy to
gate it correctly - but does not rule on it.

## Blast radius on ADR-088

This ADR is an **amendment**, not a supersession, of ADR-088. ADR-088 §6 states three
explicit non-goals: "Not a GitHub API mirror," "Not first-class git entities," and - most
directly relevant - **"No write-back. khive never pushes commits, comments, or state
changes to GitHub; one-directional, git/GitHub → graph only."**

This ADR proposes reversing that specific clause. Concretely:

- ADR-088 §6's "No write-back" bullet is superseded by this ADR, conditioned on
  acceptance: khive gains a write path, gated and audited, distinct from the read-only
  ingest path ADR-088 specifies.
- ADR-088's note-kind taxonomy (`commit`, `issue`, `pull_request`), edge usage
  (`annotates`, `precedes` per Amendment 1's ingest enrichment), and ingester machinery are
  entirely unaffected. Write verbs do not touch the graph-side note kinds directly; a
  `git.commit` call may, as a follow-on convenience (not specified here - see Open Question
  5 below), trigger a `git.digest` pass so the new commit becomes a graph record, but this
  is a composition of two already-independent verbs, not a structural change to either.
- ADR-088's `khive-pack-git` crate gains new handlers (write verbs) alongside its existing
  `git.digest` handler and background ingester; `REQUIRES = ["kg"]` is unchanged, no new
  edge relations are introduced by this ADR (write verbs do not create graph records at
  all - they mutate a git repository, a system outside the KG substrate).
- The pack's non-goal "khive does not build a GitHub replacement" (inherited from ADR-010)
  is unaffected: this surface does not touch GitHub's PR/review/CI machinery, only local git
  operations (commit, branch, push) that any git client already performs.

**Open Question 5**: whether a successful `git.commit`/`git.push` should auto-trigger
graph ingestion of the new commit (closing the loop between write and the existing
read-side note kinds), or whether that stays a separate, caller-initiated `git.digest`
call. Left open; a naive auto-trigger risks conflating a fast, latency-sensitive write path
with a potentially slow ingest pass in the same call.

## Threat and Risk Notes

(Non-exhaustive. The full threat model for an untrusted caller belongs to ADR-109, which
this write surface composes with.)

This ADR assumes a **trusted or semi-trusted** caller (an agent already authorized to reach
the MCP surface at all, per ADR-018's existing agent-binary trust boundary). It does not
itself define a threat model for an untrusted caller invoking these verbs - that
composition is explicitly deferred to ADR-109 (Fork (d) in that ADR). The rules above
(force-push denial, protected-branch enforcement, full audit) are baseline hygiene for any
caller class; they are necessary but not sufficient for an untrusted-caller threat model.

## Alternatives Considered

| Alternative                                                                      | Why not adopted outright (still may inform Open Questions)                                                                                                                                                                                                                     |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| No khive write surface; keep the wrapper tool shelling raw `git`/`gh` directly   | Defeats the entire motivating premise (design input 1): no policy chokepoint, no audit, fleet-level hook denial has nothing to route through.                                                                                                                                  |
| Generic "run arbitrary git command" verb (`git.exec(args: [...])`)               | Rejected outright, not merely forked. Defeats the hardened-allowlist premise of both B1 and B2; makes Gate policy unable to reason about the operation (`verb` would always be `git.exec`, collapsing commit/branch/push/force-push into one undifferentiated policy surface). |
| Route git writes through the existing `git.digest` verb by adding a `write` mode | Rejected. `git.digest` is read/ingest-shaped (source, max_items, include, cursor-resumable); overloading it with write semantics conflates two operations with entirely different audit, policy, and failure-mode needs.                                                       |

## Consequences

### Positive (conditioned on spec-gate resolution of the Open Questions)

- Gives khive a real authorization chokepoint for git writes, closing the gap design input 2
  depends on (fleet-level hook denial of direct `git`/`gh` writes has something to route
  through).
- Full audit trail for git mutations performed through khive, in the same `Event` substrate
  already used for every other verb (ADR-004), queryable via GQL/SPARQL.
- Reuses the existing Gate trait (ADR-018) rather than inventing new authorization
  machinery, if Fork (c) resolves to C2.

### Negative

- New write surface is new attack surface: a bug in the argument-construction path (Fork
  (b), B2) or a Gate misconfiguration is now capable of mutating a real git repository, not
  just khive's own graph. This is qualitatively different risk from every existing khive
  verb, which is confined to khive's own storage.
- Whichever execution home is chosen (Fork (b)), the daemon process needs standing git
  write credentials for whatever repos this surface targets - a new secret-management
  surface that does not exist for the read-only ingest path (`gh` read scopes are lower
  privilege than push credentials).
- Four genuinely open forks mean this ADR cannot be implemented as written; it requires a
  spec-gate ruling on Open Questions 1-5 before `khive-pack-git` gains any write handler.

## Open Questions

1. Verb granularity (Fork (a)): A1 thin verbs, A2 task-level `publish_branch`, or A3 both.
2. Execution home (Fork (b)): B1 daemon-side `git2-rs`, or B2 hardened system-git shell-out.
   This ADR leans B2 for consistency with the existing ingester but does not decide it.
3. Policy declaration (Fork (c)): confirmed lean toward C2 (Gate policy objects, no new
   trait); open sub-question is whether `repo`/`branch` become typed `GateRequest` fields.
4. Composition with external-PR trust rules (Fork (d)): whether policy needs an explicit
   `source_trust` signal, or whether not building fork-diff-write capability is sufficient
   boundary for Phase B.
5. Whether a successful write auto-triggers `git.digest` ingestion of the new commit, or
   stays a separate caller-initiated call.

## References

- ADR-088 - Git-Lifecycle Pack (amended by this ADR's proposed reversal of the "No
  write-back" non-goal)
- ADR-088 Amendment 1 - `git.digest`, the existing read/ingest agent-facing verb this
  proposal is structurally modeled on
- ADR-018 - Authorization Gate; `Gate`/`GateRequest`/`GateDecision`, the reused enforcement
  seam (Fork (c), C2)
- ADR-017 - Pack Standard; `HandlerDef`, `PackRuntime::dispatch`, the mechanism new verbs
  register through
- ADR-016 - Request DSL; the wire surface these verbs would be reachable through
- ADR-004 - Substrate Observables; `Event` store, the audit-persistence sink (Fork rule 2)
- ADR-007 Rev 7 - Namespace as attribution; write-verb dispatch stamps namespace/actor
  exactly as every other verb does, no new namespace semantics
- `crates/khive-pack-git/src/ingest.rs` - existing shell-to-system-git precedent informing
  Fork (b)
- `crates/khive-pack-git/src/pack.rs`, `src/handlers.rs` - existing pack structure new write
  handlers would extend
