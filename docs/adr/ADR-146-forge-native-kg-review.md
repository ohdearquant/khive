# ADR-146: Forge-Native KG Review — Pull Requests and Issues as the Collaboration Surface

**Status**: proposed\
**Date**: 2026-08-07\
**Authors**: khive maintainers

## Context

[ADR-010](ADR-010-kg-versioning.md) fixed the strategic position: "GitHub for Knowledge
Graphs." KG state is serialized as sorted NDJSON in a git repository, git provides
versioning, and the forge provides the social layer. [ADR-020](ADR-020-git-native-kg-implementation.md)
records the original implementation contract for that position: directory layout,
NDJSON format, two-layer storage, CLI verbs, merge semantics. Its 2026-10-09
amendment supersedes parts of that workflow in favor of ADR-101/102 (corrected by
Amendment 1). [ADR-101](ADR-101-kg-changeset-model.md)
defines the semantic change-set (producer envelope plus ordered operations), and
[ADR-102](ADR-102-tiered-validate-and-merge.md) defines tiered validation and the
independent-review gate over it. [ADR-145](ADR-145-local-first-kg-workbench.md) added the
local-first review workbench and the `khive.review.v1` read contract.

What no ADR yet states is how these compose into one review experience. ADR-145's first
slice renders fixtures, and its UI vocabulary (review pages, gates, approvals) is not yet
bound to the forge objects that ADR-010 named as the social layer. Left unstated, the
workbench drifts toward a parallel review system: its own notion of a review, its own
identifiers, its own lifecycle, none of which a collaborator's existing tooling
understands. ADR-020 already rejected a custom VCS layer for exactly this reason; the
same argument applies one level up, at the review layer.

This ADR closes that gap. It is an alignment contract, not a new subsystem.

## Decision

### D1 — A KG review IS a pull request; a curation ask IS an issue

There is no khive-native review object (superseded by Amendment 1, Decision 3). The
unit of proposed KG change is a pull request on a repository whose `.khive/kg/`
NDJSON changed, with ADR-101 defining its semantic operation model and ADR-102
defining validation and tier routing (corrected by Amendment 1). The separate
ADR-046 live-proposal workflow remains available. The unit of
requested-but-unimplemented curation ("this entity should be split," "this edge
looks wrong") is a forge issue on the same repository.

The workbench, the CLI, and any future server surface are presentation and
composition layers over those two objects. They never mint a second system of
record. A review that exists only inside a khive store, invisible to `git log` and
the forge, is a design error under this ADR (superseded by Amendment 1, Decision 3,
which scopes this rule to forge collaboration).

The `khive.review.v1` contract already anticipates this: its `pull_request` review
kind carries full repository identities (owner, name, base and head SHAs) and
pull-request identities (number, title, state, author, head SHA). This ADR promotes
those fields from fixture decoration to the binding: a `pull_request` bundle
describes a real PR, and a bundle whose head SHA no longer matches the live PR head
is stale and blocks approval, as specified by Amendment 1, Decision 4. ADR-145 D3
names PR-head staleness, while its D4 comparator uses the repository head carried in
the bundle (corrected by Amendment 1).

### D2 — The UX contract is the forge PR page, with a semantic diff pane

The workbench mirrors the structure every collaborator already knows:

| Forge concept                       | KG review surface                                                    |
| ----------------------------------- | -------------------------------------------------------------------- |
| Conversation tab                    | Review conversation (ingested forge comments, read-only first)       |
| Files-changed tab                   | Semantic diff: ADR-101 operations rendered per entity/edge/note      |
| Checks tab                          | ADR-102 validation findings, one check row per rule class            |
| Required reviews / protected branch | Tier routing and the independent-review gate, shown as review policy |
| Approve / Request changes / Comment | The same three verbs, no invented review states                      |
| Merge box                           | Merge state readout; merging happens forge-side                      |

KG-specific intelligence (tier summaries, affected-subgraph projection, evidence
anchors) appears as additional panels inside this structure, never as a replacement for
it. Where the first slice invented layout that conflicts with this mapping, the mapping
wins in the next slice.

### D3 — One composition path from repository state to rendered review

The CLI review operation emits the `changeset` variant from a staged change-set and
rules. It performs partial-view validation and deliberately has no repository,
database, or forge-metadata input (corrected by Amendment 1). The target
`pull_request` composition combines three sources:

1. **The change set**: ADR-101 ordered operations and structured semantic diff under
   ADR-101 D1/D2/D5 and ADR-102 D1-D3, not the superseded ADR-020 §7 merge workflow
   (corrected by Amendment 1).
2. **Forge metadata**: PR number, title, body, state, author, comment stream, fetched
   through a thin adapter. GitHub is the first adapter; the bundle schema stays
   forge-neutral.
3. **Validation**: ADR-102 tier classification and findings with an explicit
   validation scope. The shipped CLI reports `commit_time_partial_view`, not
   validation of the full repository head state (corrected by Amendment 1).

The workbench consumes the composed bundle unchanged. ADR-145's golden vector covers
the `changeset` variant; it does not establish shared `pull_request` production or
byte parity across CLI, CI, and browser renderings. Additional variants require
their own shared vectors (corrected by Amendment 1).

### D4 — Issues close the loop through the graph

Curation asks are forge issues. The existing git pack already ingests commits,
issues, and pull requests into the graph as note kinds; this ADR makes that
ingestion the designated memory of the review process. The existing PR ingest
creates a `pull_request` note from the PR body and metadata, annotating its project
anchor. Conversation ingestion and links to the affected KG entities remain target
behavior (corrected by Amendment 1). A merged KG PR and its review conversation
become graph notes linked to the entities they touched, so the graph carries its own
curation history and retrieval can surface "this entity was disputed in review" as
context. No new note kinds are introduced.

### D5 — Write-back ladder

Review actions gain capability in slices, and each capability is declared in the
bundle's capability block rather than assumed by the UI. The Rollout column names the
[ADR-145](ADR-145-local-first-kg-workbench.md) Rollout step each slice corresponds to,
so the two documents describe one ladder rather than two:

| Slice | ADR-145 Rollout step                | Capability                                                                                                                                                 | Persistence              |
| ----- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------ |
| 1     | 1 (read-only vertical slice)        | Local-only decisions (shipped)                                                                                                                             | Browser session only     |
| 2     | 3 (authenticated repository reads)  | Forge read: live PR metadata, comments, check states via adapter                                                                                           | Read-only                |
| 3     | 4 (governed writes)                 | Forge write-back: approve, request changes, comment — **gated, see below**                                                                                 | Forge is the record      |
| 4     | 5 (editing and conflict resolution) | Merge remains forge-side under branch protection; post-merge `kg sync` rebuilds the working database (ADR-020 §11) (superseded by Amendment 1, Decision 3) | Repository is the record |

ADR-145 Rollout step 2 (shared pure core: stable diff and rule crates, normative schema
plus golden vectors, native/WASM parity, strict committed-state Git adapters) has no
slice here because it is prerequisite engineering rather than a review capability. It
still gates slice 2, and ADR-145 governs it unchanged.

**Slice 3 is gated by ADR-145, and this ADR does not open that gate.** ADR-145 D5
states that the workbench MUST NOT translate an approval in one system into an apply
or approval in another "until a later ADR defines identity mapping, authorization,
idempotency, partial-failure recovery, and audit events," and its Rollout step 4
repeats the requirement as ratified decisions on outbound publication,
authorization, idempotency, hygiene, and audit. Their union contains seven subjects
(corrected by Amendment 1): identity mapping, authorization, idempotency,
partial-failure recovery, audit, outbound publication, and hygiene.

Amendment 1 maps these subjects to existing decisions and open requirements
(corrected by Amendment 1). Slice 3 therefore remains blocked until a subsequent ADR
discharges that precondition set explicitly (superseded by Amendment 1, Decision 2,
which credits settled subjects and keeps the unresolved subjects blocking). A
per-action capability toggle, default off, is an implementation property of slice 3
once it is unblocked; it is not a substitute for the ratified decisions, and nothing
in this ADR should be read as amending or weakening ADR-145 D5.

Slice numbering is a dependency order, not a schedule. Skipping a slice is not
permitted: write-back without live read has no stale-detection substrate. Reaching
slice 2 does not make slice 3 available, because slice 3's gate is a decision gate, not
an ordering one.

### D6 — Gates are checks, not UI decorations

The tier-routing requirement and the independent-review gate (ADR-102) are enforced
where the forge enforces everything else: as required status checks and review
policy on the protected branch. This is the enforcement requirement, not a claim
that the initialization command configures branch protection (corrected by Amendment
1). Initialization with CI generation enabled produces a workflow triggered by
pushes and pull requests touching `.khive/kg/**`; its validation step uses the
GitHub output format. It generates no tier or independent-review gate step
(corrected by Amendment 1). The workbench renders the same gate state it fetches; a
gate that exists only in the browser is advisory, and this ADR treats advisory gates
as absent. The shipped browser checks are advisory local feedback and do not
establish forge enforcement (corrected by Amendment 1).

## Non-Goals

- No custom VCS or sync server (ADR-010, ADR-020 already rejected these; unchanged).
- No CRDT or automatic semantic merge (ADR-010 rejection stands).
- No expansion of the `live_proposal` variant; the event-sourced proposal lane
  (ADR-046) remains a later, explicitly typed link from a `pull_request` bundle.
- No forge-agnostic abstraction beyond the thin adapter seam: the adapter trait exists
  so the bundle schema never embeds GitHub-specific field shapes, not to promise
  day-one support for other forges.

## Alternatives Considered

| Alternative                                              | Why rejected                                                                                                                                             |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Native khive review objects (PR-shaped, stored in khive) | Recreates the custom-VCS mistake one layer up: a second lifecycle, invisible to git/forge tooling, with sync burden against the real one.                |
| Serialize reviews as files in the repo                   | Review state is social, not graph state; committing it pollutes KG history and conflicts with forge conversation as the record.                          |
| Forge-first UI with no local workbench                   | Loses the local-first property ADR-145 established: semantic diff, offline review of a bundle, and agent-produced changesets without a forge round-trip. |
| UI-enforced gates only                                   | Advisory by construction; any client that skips the UI skips the gate. CI-enforced checks are the only gate the merge path actually sees.                |

## Consequences

Slice 3 implementation waits for the open identity-binding, hygiene, idempotency,
and partial-failure recovery contracts (Amendment 1, Decision 2).

### Positive

- Zero new review concepts: collaborators and agents reuse PR and issue muscle memory,
  and every forge tool (notifications, CLI, mobile, CI) works on KG reviews unchanged.
- The graph learns from its own review history through existing git-pack ingestion.
- The workbench's mock surfaces get an unambiguous functional target: each panel maps
  to a named source (change set, forge adapter, validation) rather than to invented
  data.

### Negative

- Full review flow requires a forge remote; purely local repositories get the changeset
  and validation panels but no conversation or write-back. This is the ADR-010 trade
  accepted again, not a new cost.
- Forge adapter surface area (auth, rate limits, pagination) becomes part of the review
  path's operational envelope at slice 2, ahead of any server deployment.

### Neutral

- ADR-145 is unchanged in substance; its first slice is re-scoped as slice 1 of the
  D5 ladder, and its UI vocabulary is bound to the D2 mapping for subsequent slices
  (superseded by Amendment 1, Decision 1 for the forge-write routing requirement).
- ADR-101/102 govern the semantic change-set and validation prerequisites. The
  superseded ADR-020 project-repository merge verbs are not prerequisites for this
  ladder (corrected by Amendment 1). The superseded prerequisite sentence is
  retained in Amendment 1, Decision 3.

## Implementation Status

CLI surface at the evidence revision (`kkernel kg`): `apply`, `init`, `validate`,
`fetch`, `update`, `export`, `import`, `status`, `diff`, `hook`, `commit`, and
`review` — present. `sync` is a visible alias for `fetch`; `resolve` and `migrate`
are absent (corrected by Amendment 1). The alias does not perform ADR-020's
working-database rebuild. The presence of `diff` does not restore the superseded
ADR-020 integration workflow. The workbench ships slice 1 (fixture and import,
local-only decisions). Slice 2's live read adapter and live PR binding remain
unimplemented in that entry point. The git pack separately implements policy-gated
`git.pr_review` and `git.pr_merge`; their existence does not enable workbench slice
3 (corrected by Amendment 1).

## References

- [ADR-010](ADR-010-kg-versioning.md): KG versioning strategy — strategic root
- [ADR-020](ADR-020-git-native-kg-implementation.md): Git-native implementation contract
- [ADR-046](ADR-046-event-sourced-proposals.md): Event-sourced proposals — later variant
- [ADR-101](ADR-101-kg-changeset-model.md): Change-set model
- [ADR-102](ADR-102-tiered-validate-and-merge.md): Tiered validation and merge gate
- [ADR-145](ADR-145-local-first-kg-workbench.md): Local-first KG workbench and
  `khive.review.v1`

## Amendment 1 (2026-10-23) -- Existing forge writes, explicit gates, and live PR heads

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#4977](https://github.com/ohdearquant/khive/issues/4977).

**Trigger.** The two prerequisite lists in ADR-145 name seven distinct subjects, while D5 counted five
(`docs/adr/ADR-145-local-first-kg-workbench.md:203`, `docs/adr/ADR-145-local-first-kg-workbench.md:312`). The accepted
forge-write contract already names review and merge verbs (`docs/adr/ADR-182-git-dev-loop-verbs.md:52`), but the browser
compares two heads captured inside one bundle (`apps/kg-editor/src/lib/review-utils.ts:15`). The CLI produces a
partial-view changeset report (`crates/kkernel/src/kg/review.rs:135`). This amendment separates existing behavior from
the remaining integration requirements. Evidence locations refer to repository revision
`8f2f867df06c8fe0909fd6d2c4d1bc60f220fbe6`.

### Decision 1 -- Reuse the existing forge write path

Slice 3 MUST route approval, change requests, and review comments through `git.pr_review`. Merge requests use
`git.pr_merge` under the same existing write surface. The workbench MUST NOT introduce another forge mutation path. This
selects the policy and audit boundary in ADR-108 and ADR-182 instead of duplicating authorization in a browser or server
adapter. Merge remains a forge operation even when invoked through khive.

ADR-108 requires Gate dispatch and audit, and its Amendment 1 adds an independent fail-closed allowlist
(`docs/adr/ADR-108-git-write-surface.md:41`, `docs/adr/ADR-108-git-write-surface.md:81`,
`docs/adr/ADR-108-git-write-surface.md:393`). ADR-182 extends that surface to review and merge with per-verb policy and
receipts (`docs/adr/ADR-182-git-dev-loop-verbs.md:52`, `docs/adr/ADR-182-git-dev-loop-verbs.md:62`). The handlers
consult the allowlist and policy before mutation, submit review events with `commit_id`, and send a merge request with
the expected SHA (`crates/khive-pack-git/src/remote_handlers.rs:371`,
`crates/khive-pack-git/src/remote_handlers.rs:578`, `crates/khive-pack-git/src/remote_handlers.rs:654`). The closed
input contracts are `git.pr_review(repo, number, verdict, body, expected_head, session_id?)` and `git.pr_merge(repo,
number, method, subject, body, expected_head, session_id?)` (`crates/khive-pack-git/src/input_schema.json:357`,
`crates/khive-pack-git/src/input_schema.json:396`).

For these operations, this decision replaces ADR-145 D6's prospective `git.publish_*` routing requirement
(`docs/adr/ADR-145-local-first-kg-workbench.md:227`). It preserves the prohibition on direct adapter mutations and
leaves the remaining slice 3 prerequisites intact. It does not select a new publication surface for other operations.

### Decision 2 -- Account for all seven prerequisites

The union of ADR-145 D5 and Rollout step 4 governs slice 3. The following table records the disposition of every
subject. Existing write capability does not settle the remaining integration contracts.

| Subject                  | Disposition for slice 3                                                                                                                                        | Decision and evidence                                                                                                                                                                                                                     |
| ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Authorization            | Settled by the existing write path.                                                                                                                            | Gate plus fail-closed allowlist, then per-verb policy: `docs/adr/ADR-108-git-write-surface.md:393`; `docs/adr/ADR-182-git-dev-loop-verbs.md:102`; `crates/khive-pack-git/src/remote_handlers.rs:371`.                                     |
| Audit                    | Settled by the existing write path.                                                                                                                            | Allowed and denied writes carry audit and receipts: `docs/adr/ADR-108-git-write-surface.md:81`; `docs/adr/ADR-182-git-dev-loop-verbs.md:180`; `crates/khive-pack-git/src/remote_handlers.rs:328`.                                         |
| Outbound publication     | Settled for review and merge routing by Decision 1 and ADR-108/182.                                                                                            | `docs/adr/ADR-108-git-write-surface.md:242`; `docs/adr/ADR-182-git-dev-loop-verbs.md:52`. Content hygiene and project export restrictions remain separate requirements.                                                                   |
| Identity mapping         | Actor-to-credential and platform-login mapping is decided by ADR-182. Binding the workbench's model-family selection to that identity remains open.            | Actor-only mapping and resolved-caller lookup: `docs/adr/ADR-182-git-dev-loop-verbs.md:148`, `docs/adr/ADR-182-git-dev-loop-verbs.md:224`. The browser supplies a model-family selection: `apps/kg-editor/src/components/studio.tsx:724`. |
| Hygiene                  | Open for the reused review/merge path. ADR-112 proposed a publication-hygiene scan and is withdrawn; no accepted record applies a hygiene scan to these verbs. | Withdrawn status: `docs/adr/ADR-112-git-outbound-publish-verbs.md:3`; its scan: `docs/adr/ADR-112-git-outbound-publish-verbs.md:267`; project publication boundary: `docs/adr/ADR-145-local-first-kg-workbench.md:96`.                    |
| Idempotency              | Open. Reusing the verbs does not establish an idempotent workbench submission contract.                                                                        | Review and merge schemas have no idempotency key: `crates/khive-pack-git/src/input_schema.json:357`, `crates/khive-pack-git/src/input_schema.json:396`.                                                                                   |
| Partial-failure recovery | Open for the workbench integration. Existing receipt dispositions and limited reconciliation are preserved.                                                    | `docs/adr/ADR-182-git-dev-loop-verbs.md:170`; an unknown review receipt remains unchanged in `crates/khive-pack-git/src/local_handlers.rs:788`.                                                                                           |

ADR-182 already specifies `not_committed`, `committed`, and `unknown` dispositions, forbids blindly retrying a mutation,
and defines reconciliation that does not repeat a mutation. A merge can be reconciled from platform state; a PR review
has no receipt-specific platform marker and can remain unknown (`docs/adr/ADR-182-git-dev-loop-verbs.md:170`,
`crates/khive-pack-git/src/remote_handlers.rs:1009`). These are existing constraints, not a complete slice 3 recovery
decision. This amendment defines neither a new retry policy nor an automatic recovery action.

Slice 3 implementation waits for the open identity-binding, hygiene, idempotency, and partial-failure recovery
contracts. Recording the routing decision and implementing slice 2's live-head check do not wait for those contracts. A
capability flag cannot discharge a decision prerequisite. ADR-145's separate project-export boundary continues to apply
(`docs/adr/ADR-145-local-first-kg-workbench.md:79`, `docs/adr/ADR-145-local-first-kg-workbench.md:96`).

### Decision 3 -- Use the change-set contracts and preserve separate workflows

D1's pull-request rule applies to project KG collaboration. It does not abolish the ADR-046 live-proposal workflow or
make a local changeset review a forge object. The existing `review` dispatch resolves a proposal, records a review
decision, and can invoke its apply path (`crates/khive-pack-kg/src/dispatch.rs:207`,
`crates/khive-pack-kg/src/handlers/proposal.rs:338`, `crates/khive-pack-kg/src/handlers/proposal.rs:458`). This scope
preserves ADR-145's distinct workflows (`docs/adr/ADR-145-local-first-kg-workbench.md:194`).

D1 and D3 use ADR-101's ordered typed operations and NDJSON-delta model, together with ADR-102's validation and
independent-review rules (`docs/adr/ADR-101-kg-changeset-model.md:76`, `docs/adr/ADR-101-kg-changeset-model.md:159`,
`docs/adr/ADR-102-tiered-validate-and-merge.md:91`, `docs/adr/ADR-102-tiered-validate-and-merge.md:123`). ADR-101 D5
supplies the structured-diff contract (`docs/adr/ADR-101-kg-changeset-model.md:212`). ADR-020's 2026-10-09 amendment
explicitly supersedes its former project-repository diff, conflict-resolution, and migration workflow, including
sections 7, 9, and 10 (`docs/adr/ADR-020-git-native-kg-implementation.md:930`). Those historical sections are not
D1/D3's authority.

The superseded prerequisite sentence is retained here as history, outside the active Consequences:

> The remaining unimplemented ADR-020 verbs (`kg diff`, `kg resolve`, `kg update`, `kg migrate`, and `kg sync` as a CLI
> verb) are unblocked prerequisites for slices 2+ and tracked as implementation work, not redesigned here
> (superseded by Amendment 1, Decision 3).

The `sync` CLI spelling dispatches `fetch`; it does not implement the working-database rebuild described in D5's
original slice 4 row (`crates/kkernel/src/kg/types.rs:24`, `crates/kkernel/src/kg/dispatch.rs:22`). That row does not
supply a post-merge rebuild command. No replacement synchronization behavior is decided here. ADR-102's operational
history remains local-only, distinct from project KG collaboration (`docs/adr/ADR-102-tiered-validate-and-merge.md:213`,
`docs/adr/ADR-145-local-first-kg-workbench.md:43`).

### Decision 4 -- Compare the bundle head with the live pull request head

For #4977, the authoritative freshness comparator is the live head SHA of the pull request identified by the bundle,
read through the forge read adapter, versus the bundle's captured `pull_request.head_sha`. Read it when loading a live
review and again before approval. A different SHA makes the review stale and blocks approval. If the live head cannot be
read, approval is refused, including when a previous read matched. Equality establishes freshness only; all other review
gates still apply.

The shipped comparator is `bundle.pull_request.head_sha !== bundle.repository.head_sha`
(`apps/kg-editor/src/lib/review-utils.ts:15`). Both values come from the imported or fixture bundle, so equality cannot
establish live freshness. #4977 replaces that as the live freshness test. The existing internal mismatch remains
parseable and blocking; it is not evidence of a successful forge read
(`apps/kg-editor/src/lib/review-bundle.test.ts:48`). ADR-145's repository-head wording and the schema annotation
describe that captured-bundle check (`docs/adr/ADR-145-local-first-kg-workbench.md:174`,
`docs/schemas/khive-review-v1.schema.json:2104`, `apps/kg-editor/scripts/review-json-schema.mjs:20`).

The live read MUST NOT rewrite the captured head to make a stale bundle appear current. Once slice 3 is enabled,
`expected_head` carries the reviewed bundle head into the existing write path, which reads the PR head before mutation
and binds the review submission to that commit (`crates/khive-pack-git/src/remote_handlers.rs:505`,
`crates/khive-pack-git/src/remote_handlers.rs:578`). This amendment does not introduce a second write operation or
change the existing forge-side race semantics.

### Decision 5 -- State the implemented capabilities precisely

The CLI accepts a staged change-set and explicit rules, reuses commit-time partial-view validation, and emits
`review_kind: "changeset"` with `github_writes: false`. Repository and forge identities are absent by design
(`crates/kkernel/src/kg/review.rs:25`, `crates/kkernel/src/kg/review.rs:135`, `crates/kkernel/src/kg/review.rs:151`).
Uncovered update, delete, restore, and merge operations produce blocking coverage findings
(`crates/kkernel/src/kg/review.rs:188`). The shared golden compares changeset JSON values in Rust and is consumed by
TypeScript; it does not prove a live PR composition path or identical rendering bytes
(`crates/kkernel/tests/kg_review.rs:167`, `apps/kg-editor/src/lib/review-json-schema.test.ts:25`).

The `/review` page loads the fixture adapter, and imported bundles retain no-write capabilities
(`apps/kg-editor/src/app/review/page.tsx:4`, `apps/kg-editor/src/lib/adapters/fixture-review-source.ts:5`,
`apps/kg-editor/src/components/studio.tsx:1023`). D2 is a target mapping: local decisions and displayed bundle
conversation do not establish forge comment ingestion or write-back (`apps/kg-editor/src/components/studio.tsx:212`,
`apps/kg-editor/src/components/review-activity-timeline.tsx:41`). The git pack separately reads platform reviews to
check merge eligibility (`crates/khive-pack-git/src/remote_handlers.rs:892`). That read does not populate workbench
conversation or graph notes.

D4's PR ingestion exists: it creates a note from PR body and metadata and annotates the project anchor. Ingesting review
conversation and linking it to affected KG entities remain target behavior
(`crates/khive-pack-git/src/ingest/github_ingest.rs:245`). Existing commit provenance links are not evidence that this
review-conversation path is implemented.

D6 remains an enforcement requirement. The generated workflow contains a checkout step and validation in the GitHub
output format, with no tier or independent-review gate step (`crates/kkernel/src/kg/init.rs:39`,
`crates/kkernel/src/kg/init.rs:128`). Browser family, finding, and check tests remain local feedback; they do not
configure protected-branch policy (`apps/kg-editor/src/lib/review-utils.ts:22`,
`apps/kg-editor/src/components/studio.tsx:212`). The CLI inventory is defined by `KgCommand`, including `apply`,
`update`, `diff`, and the `sync` alias (`crates/kkernel/src/kg/types.rs:13`). These corrections avoid treating a
displayed or declared capability as implemented enforcement.

### Consequences

- Slice 3 implementation waits on four open contracts named in Decision 2: the binding of the workbench's
  model-family selection to the forge identity, publication hygiene for review and merge, an idempotent submission
  contract, and partial-failure recovery. This amendment records the routing decision; it does not discharge them.
- Slice 2's live-head check (Decision 4) does not wait on those contracts.
- The workbench has one forge write path, `git.pr_review` and `git.pr_merge`, so its authorization and audit are the
  git pack's and are not reimplemented in an adapter.

### Acceptance

Existing evidence below identifies source or assertion locations. Pending evidence names the file and scenario required
by the specified behavior; it does not claim that the scenario is already implemented or passes.

| ID  | Statement                                                                                                             | Evidence                                                                                                                                                                                                                                                                                                       |
| --- | --------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1  | Review and merge write-back use `git.pr_review` and `git.pr_merge`; adapters contain no direct forge mutation path.   | Decision 1; existing handlers at `crates/khive-pack-git/src/remote_handlers.rs:546` and `crates/khive-pack-git/src/remote_handlers.rs:654`. new test `apps/kg-editor/src/components/studio.test.tsx::routes_review_and_merge_through_git_verbs` (to add) must assert these verbs and no direct forge mutation. |
| A2  | All seven prerequisites have a disposition, and unresolved subjects keep slice 3 disabled.                            | Decision 2 table; `apps/kg-editor/src/lib/adapters/fixture-review-source.ts:7` and `apps/kg-editor/src/components/studio.tsx:1023` retain no-write capabilities.                                                                                                                                               |
| A3  | Existing policy, identity, and last-pusher refusals are preserved by write-path reuse.                                | `crates/khive-pack-git/src/remote_handlers.rs:371`; `remote_last_pusher_refuses_review_and_merge_then_third_login_succeeds` in `crates/khive-pack-git/src/remote_tests/review_provenance_tests.rs:2`.                                                                                                          |
| A4  | D1/D3 and active Consequences do not require the superseded ADR-020 project merge workflow; ADR-046 remains separate. | Decision 3; `docs/adr/ADR-020-git-native-kg-implementation.md:930`; `crates/khive-pack-kg/src/dispatch.rs:207`.                                                                                                                                                                                                |
| A5  | On live load, equal captured heads inside a bundle do not permit approval when the forge head differs.                | new test `apps/kg-editor/src/components/studio.test.tsx::refuses_changed_live_head_on_load` (to add) for #4977: hold both captured heads at A, return live head B, assert stale and approval refused.                                                                                                          |
| A6  | Approval re-reads the live PR head; a change after load blocks approval.                                              | new test `apps/kg-editor/src/components/studio.test.tsx::rechecks_live_head_before_approval` (to add) for #4977: load at A, move live head to B before approval, assert no approval action.                                                                                                                    |
| A7  | An unreadable live head at load or before approval refuses approval, including after a prior match.                   | new test `apps/kg-editor/src/components/studio.test.tsx::refuses_unreadable_live_head` (to add) for #4977: fail each read independently and assert no approval action.                                                                                                                                         |
| A8  | A live-head match does not bypass other gates or rewrite the bundle's captured head.                                  | new test `apps/kg-editor/src/lib/review-bundle.test.ts::preserves_captured_head_and_other_gates` (to add) for #4977; retain the same-family and findings refusals at `apps/kg-editor/src/lib/review-bundle.test.ts:181` and `apps/kg-editor/src/lib/review-bundle.test.ts:262`.                                |
| A9  | CLI JSON is a read-only changeset report, and uncovered update rules fail closed with partial-view scope.             | `json_report_is_ordered_versioned_and_read_only` at `crates/kkernel/tests/kg_review.rs:141`; `uncovered_update_rules_fail_closed_in_partial_view` at `crates/kkernel/tests/kg_review.rs:300`.                                                                                                                  |
| A10 | Shared golden evidence covers the changeset variant; it does not certify live pull-request composition.               | `crates/kkernel/tests/kg_review.rs:167`; `apps/kg-editor/src/lib/review-json-schema.test.ts:25`; `docs/adr/ADR-145-local-first-kg-workbench.md:162`.                                                                                                                                                           |
| A11 | PR note ingestion annotates the project anchor; D4 does not claim implemented review-conversation links.              | `crates/khive-pack-git/src/ingest/github_ingest.rs:245`; D4 and Decision 5.                                                                                                                                                                                                                                    |
| A12 | The initialization template runs validation without claiming a generated tier gate or protected-branch setup.         | `crates/kkernel/src/kg/init.rs:39` and `crates/kkernel/src/kg/init.rs:128`; D6.                                                                                                                                                                                                                                |
| A13 | CLI status includes `apply`, `diff`, `update`, and `sync` as a fetch alias, with no `resolve` or `migrate` variant.   | `crates/kkernel/src/kg/types.rs:13` and `crates/kkernel/src/kg/dispatch.rs:18`.                                                                                                                                                                                                                                |
