# ADR-145: Local-First KG Workbench — GitHub-Backed Semantic Review

**Status**: Proposed\
**Date**: 2026-08-07\
**Authors**: khive maintainers\
**Depends on**: [ADR-010](ADR-010-kg-versioning.md),
[ADR-020](ADR-020-git-native-kg-implementation.md),
[ADR-046](ADR-046-event-sourced-proposals.md),
[ADR-101](ADR-101-kg-changeset-model.md),
[ADR-102](ADR-102-tiered-validate-and-merge.md), and
[ADR-108](ADR-108-git-write-surface.md)\
**Proposes amendments to**: [ADR-020](ADR-020-git-native-kg-implementation.md), specifically
"Binary topology — `khive` vs `kkernel`" (replace the Deno-wrapper claim with the shipped
npm-to-`kkernel` routing contract) and §5 (add the read-only `kg review` command). These
amendments take effect only if this ADR is accepted.\
**Related**: [ADR-034](ADR-034-kg-validation-pipelines.md),
[ADR-055](ADR-055-epistemic-edge-relations.md),
[ADR-089](ADR-089-context-verb.md), and
[ADR-112](ADR-112-git-outbound-publish-verbs.md)

## Context

Atlas and other continuous curators make the graph more useful while making its evolution harder
to inspect. A reviewer can see that the live KG grew and that recall improved, but cannot reliably
answer four basic questions:

1. What semantic assertions changed in one curation batch?
2. Which source evidence and producer identity justified each change?
3. Which rules and independent reviewer admitted the change?
4. Can the exact reviewed state be replayed, compared, and reverted?

khive already owns most of the substrate needed to answer those questions. ADR-010 and ADR-020 put
project KG entities and edges in canonical NDJSON under Git. ADR-101 defines an attributed, ordered
change-set with stage-time IDs and preimages. ADR-102 defines tiered validation and an independent
model-family gate. The live graph exposes search, traversal, context, proposals, and review. GitHub
already supplies commits, branches, pull requests, comments, checks, and access control.

The missing piece is not another version-control system. It is a semantic workbench over those
existing contracts: a fast place to read a graph change as a graph change, inspect its evidence and
affected neighborhood, and make a review decision without confusing Git state, live-graph state,
or private operational memory.

Two superficially similar repositories must remain distinct:

- ADR-102's **operational history repository** may contain live exports, notes, and memories. Its
  no-remote rule is binding.
- ADR-020's **project KG repository** contains the explicitly versioned project surface. Its v1
  coverage is entities and edges, and Git is already its transport.

Treating the first repository as though it were the second would silently publish material that was
never approved for sharing. Treating the second as though it could never have a remote would negate
ADR-010's Git-native collaboration strategy.

## Decision

### D1 — Product boundary: semantic workbench, not a second forge

The product is a **local-first KG workbench** layered over Git, GitHub, and khive:

- Git owns object identity, commits, parentage, refs, merges, and transport.
- GitHub owns pull requests, repository permissions, comments, checks, approvals, and merge policy.
- khive owns KG validation, structured semantic diffs, change-set provenance, graph context,
  retrieval, and review-gate explanations.
- The workbench composes those surfaces into a graph-native reading and review experience.

The workbench MUST NOT implement a parallel branch store, commit DAG, pull-request database, or
GitHub-compatible identity system. Terms such as "commit", "branch", "approval", and "merge"
refer to their Git or GitHub objects unless explicitly qualified as a khive live proposal.

### D2 — Two history classes and an explicit publication boundary

The two repository classes are separate trust domains:

| Class               | Normative content                                                                              | Remote policy                                                                   | Consumer                              |
| ------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- | ------------------------------------- |
| Operational history | ADR-101 change-sets and snapshots of live substrates, potentially including notes and memories | No remote, unchanged from ADR-102 D6                                            | local replay, audit, recovery         |
| Project KG          | `.khive/kg/entities.ndjson`, `.khive/kg/edges.ndjson`, `schema.yaml`, and `rules.toml`         | Git remote permitted by ADR-010/020; public publication requires the gate below | GitHub collaboration and distribution |

No command may convert an operational-history repository into a project KG repository in place.
Publication is an explicit export into a distinct repository with its own object database, config,
and remote set. A worktree of the operational repository is not a distinct trust domain and is
forbidden as an export target. Derived review bundles, retrieval context, evidence caches, comments,
and operational change-sets are not committed to the project KG path set in v1.

The v1 publishable coverage is exactly:

```json
{ "entities": true, "edges": true, "notes": false }
```

Tasks, memories, sessions, events, knowledge-atom bodies, and proposal records are not publishable
KG snapshot content. Live search, recall, traverse, and context results may be displayed as review
context, but MUST be labeled as live or captured context and MUST NOT be represented as part of the
reviewed Git commit.

A future public-write adapter MUST fail closed unless all of these are true:

1. the target is configured as a project KG repository, never the operational repository;
2. the exported coverage is explicit and is no wider than the v1 coverage above;
3. validation and a publication-hygiene scan cover every tracked path and pass;
4. the exact source commit, ratified canonical KG content hash, coverage, and export policy are
   recorded; the under-specified current hash is not a public-publication gate;
5. authentication and idempotency satisfy the ratified outbound-publication contract.

This ADR does not ratify ADR-112 and therefore does not by itself authorize GitHub writes.

### D3 — Five identities remain distinct

Every review surface and machine-readable artifact MUST keep these identities separate:

| Identity                                | Purpose                                             |
| --------------------------------------- | --------------------------------------------------- |
| Git commit SHA                          | immutable repository history and parentage          |
| canonical KG content hash               | semantic identity of the declared snapshot coverage |
| GitHub pull-request number and head SHA | collaboration and stale-review detection            |
| ADR-101 `batch_id`                      | producer-attributed staged curation batch           |
| ADR-046 proposal UUID                   | live event-sourced proposal and apply lifecycle     |

No one identifier is an alias for another. UI truncation is presentation only; machine artifacts
carry full identifiers. A review becomes stale when its GitHub head SHA changes, even if the
change-set batch ID remains the same.

### D4 — `khive.review.v1` is the shared read contract

CLI, CI, server adapters, and the browser consume one versioned semantic review bundle. Every
`khive.review.v1` value has a required `review_kind` discriminator:

| `review_kind`  | Required core                                                                                                         | Allowed enrichment                                                                                                 |
| -------------- | --------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `changeset`    | capability declaration, ADR-101 envelope and ordered operations, tier summary, validation scope/findings, review gate | none invented; repository and PR identities are absent unless independently supplied by a later adapter contract   |
| `pull_request` | the complete `changeset` core plus full Git base/head identities                                                      | GitHub PR metadata, structured semantic changes, evidence, affected subgraph, captured retrieval, and conversation |

A later `live_proposal` variant may carry an ADR-046 proposal UUID, but is not defined by this first
slice. A `pull_request` bundle may carry an optional, explicitly typed live-proposal link; absence is
not encoded as a synthetic UUID.

The shared core contains:

- full base and head Git SHAs in the `pull_request` variant;
- canonical KG hashes and snapshot coverage only when produced by a named, ratified
  canonicalization algorithm; until that prerequisite exists the capability is `unavailable`, and
  fixture hashes are explicitly marked `fixture` and have no correctness authority. The first-slice
  schema rejects `verified`; that state is reserved for a later schema amendment naming the
  ratified algorithm and verifier trust path. Hashes computed by the current, unratified
  canonicalization code map to `hash_status: "unavailable"` with `algorithm`, `base_hash`, and
  `head_hash` all null: a producer running today's algorithm MUST NOT emit its output under
  `fixture` (reserved for synthetic demo vectors) or any other status, and import validation
  rejects an `unavailable` identity that carries any hash or algorithm value, so unratified
  hashes cannot enter a bundle under any label;
- the ADR-101 producer envelope and ordered operation summaries when a change-set is present;
- deterministic entity, edge, and eventually note changes, each expressed as semantic subject plus
  ordered `{path, before, after}` fields;
- validation findings with stable rule IDs and severity;
- tier classification and independent-review eligibility;
- explicit capabilities describing source, mutability, WASM availability, and unavailable actions.

GitHub metadata, evidence links, comments, and affected-subgraph projections are enrichments around
that stable core. The `pull_request` variant carries an `enrichment_status` for semantic changes,
evidence, affected graph, commits, activity, and retrieval. An unavailable enrichment has an empty
page and status `unavailable`; an empty available page means "known to contain no items." Missing
enrichment is never encoded as invented data. The normative JSON schema is
`docs/schemas/khive-review-v1.schema.json`. The initial shared vector is
`docs/schemas/examples/khive-review-v1-changeset.json`; Rust and TypeScript changeset models must
both consume it before they are called compatible. Each additional review variant must likewise
ship shared golden vectors consumed by every implementation that claims support for that variant.

The bundle is a read model, not a write command. Importing or rendering one MUST NOT mutate the live
graph, a Git repository, or GitHub.

Ordering is deterministic: operations retain ADR-101 order; substrate changes sort by substrate,
semantic subject key, then field path; findings sort by severity then rule ID. JSON producers MUST
emit the schema version, discriminator, and full identifiers.

Cross-field invariants fail closed. Invalid tier, summary, validation, hash, and availability
combinations are import-validation errors. A PR head that differs from the current repository head
is deliberately still parseable so the workbench can present the review as stale, but it blocks
approval. Tier counts cover every ordered operation; complete semantic-change page counts agree
with the summary; `passed` agrees with the error count; `unavailable` hash status carries no
algorithm or hash; and `fixture` hash status carries an algorithm plus base/head hashes. `fixture`
hashes are display data only. A future `verified` state may serve as correctness evidence only after
a ratified algorithm and verifier trust path are named.

Adapter-backed collections use the same page envelope independently:

```json
{ "items": [], "next_cursor": null, "truncated": false }
```

`next_cursor` is an opaque string or `null`; `truncated` is true whenever any configured work,
result, or byte budget stopped collection. The deterministic first-slice fixture and the in-memory
change-set operation list are bounded test inputs, not an exception permitting unbounded server
responses.

### D5 — Review gates do not collapse the two khive workflows

ADR-046 live proposals and ADR-101 staged change-sets remain distinct workflows:

- approving an ADR-046 proposal applies its supported single operation to the live graph;
- approving a GitHub review admits a Git commit through repository policy;
- reviewing an ADR-101 change-set evaluates the ordered batch and ADR-102 tier rules.

The workbench may show all three, but MUST NOT translate an approval in one system into an apply or
approval in another until a later ADR defines identity mapping, authorization, idempotency,
partial-failure recovery, and audit events.

For ADR-102 tier-2 changes, the producer model family and reviewer model family are mandatory. A
same-family reviewer is ineligible. The interface MUST explain the refusal and MUST NOT present a
cosmetic success state. In the initial local-only editor, review choices are local annotations and
are labeled as not persisted.

### D6 — Adapter and trust topology

The first web implementation lives in `apps/kg-editor` as a Next.js App Router application.
Browser code receives serializable review bundles and bounded graph projections. It does not open
SQLite, read arbitrary repository paths, hold GitHub tokens, or spawn processes.

Server-only adapters have these responsibilities:

1. **Git adapter** — read allowlisted `.khive/kg/*.ndjson` paths, refs, and standard Git history
   using argv-only process execution.
2. **khive adapter** — invoke the npm-distributed `khive` executable, which routes to `kkernel`,
   using JSON-form operations or typed CLI arguments through an argv array without a shell. khive
   is a binary package, not a TypeScript SDK. The live store is reached only through khive's
   supported daemon/MCP topology, never by opening its database from Next.js.
3. **GitHub read adapter** — after a separate authorization decision, use a server-side GitHub App
   and least-privilege installation tokens. It is read-only. Tokens and repository filesystem paths
   never cross the server/client boundary. Future writes go through ratified `git.publish_*` khive
   verbs or a later ADR that explicitly supersedes that surface; the Next.js adapter does not call
   GitHub mutation APIs directly.

Repository roots and refs are resolved against configured allowlists. User-controlled strings are
never interpolated into a shell command. Route handlers that need process access use the Node.js
runtime and return bounded, schema-validated payloads.

The initial open-source slice ships a deterministic fixture adapter, JSON bundle import/export,
and explicit `Demo data · no writes` capability messaging. It performs no deployment and no
GitHub or live-graph mutation.

### D7 — WASM capability is reported, not assumed

ADR-101 D5 requires pure, filesystem-free changeset, rule-evaluation, and structured-diff crates
with native/WASM byte parity. Only a crate that actually builds for `wasm32`, exposes a browser
binding, and passes parity tests may be advertised as available.

The editor MUST NOT reimplement Rust rules or diff logic in TypeScript and call that parity. Until
the rule and diff packages exist, the browser reports those capabilities as unavailable and uses a
server-produced or fixture review bundle. The existing `khive-changeset` crate may be reused once a
real browser package is produced; Rust target compatibility by itself is not a JavaScript binding.

### D8 — First CLI slice is read-only and npm-routed

The first CLI addition is:

```text
khive kg review <changeset.ndjson> --rules <rules.toml> \
  [--reviewer-model-family <family>] [--format text|json|github]
```

It parses the strict ADR-101 change-set, evaluates the existing commit-time **partial-view** rules,
classifies operations conservatively under the ADR-102 floor, and emits the `changeset` variant of
the versioned review contract. The report's validation scope is
`commit_time_partial_view`. The existing projector covers create/link only; every
update/delete/merge emits a deterministic coverage error, routes to tier 2, and forces
`approval_ready=false`. The command therefore reports reviewer-family eligibility only as one gate
input and never claims full ADR-102 approval eligibility. It does not stage, commit, apply, push, or
publish anything.

The command is implemented in `kkernel`, because the npm `khive` shim delegates directly to that
binary. A command implemented only in the separate Deno source tree is not part of the npm public
surface.

The existing `khive kg commit` retains its ADR-102 meaning and remote refusal. It MUST NOT be
repurposed as a project-KG or GitHub commit command. Humans may use standard Git; agents may use the
hardened ADR-108 Git verbs within their existing authorization boundary.

Full rule eligibility and committed-state semantic diff/log follow only after the pure,
deterministic evaluator and diff contracts required by ADR-101 exist. Raw `git log` remains the
history authority meanwhile.

### D9 — Performance and boundedness

Large KGs are never sent to the browser as an unbounded graph. List, diff, evidence, retrieval, and
subgraph data are independently pageable. Affected-subgraph expansion is bounded by hop, node,
edge, work, and response-byte budgets and reports truncation. Search and filters operate on the
current page unless the server contract explicitly states otherwise.

The default page is server-rendered; only diff controls, graph selection, import/export, and review
interactions require client state. Expensive semantic computation belongs in the pure Rust core or
server adapter, not in React render paths.

### D10 — Supersession

If accepted, this ADR supersedes `docs/design/kg-versioning-frontend.md`. That draft modeled custom snapshots,
branches, log verbs, and merge commands that ADR-010 and ADR-020 assigned to Git, and cited ADR
numbers that now govern unrelated subsystems. It is retained only as historical design material and
must not be used as an implementation contract.

This ADR is the proposed authority for review presentation only. Conflict classification and
resolution semantics still require a dedicated future ADR.
`khive-merge` references to ADR-039 are invalid: ADR-039 governs note deduplication merge, not
three-way project KG snapshot merge. The excluded `khive-merge` crate remains non-production until
it is realigned to a ratified review/conflict contract and a true Git DAG model.

## Rollout

1. **Read-only vertical slice** — proposed ADR, fixture-backed editor, discriminated JSON bundle
   boundary, and npm-routed CLI report that is explicitly partial-view and fail-closed.
2. **Shared pure core** — stable structured diff and rule crates, the normative schema plus golden
   vectors, native/WASM parity, full ADR-102 eligibility, and strict committed-state Git adapters.
3. **Authenticated repository reads** — GitHub App installation flow, PR/check/comment ingestion,
   pagination, caching, and stale-head protection.
4. **Governed writes** — only after outbound publication, authorization, idempotency, hygiene,
   and audit decisions are ratified; then enable comments/checks/approvals one capability at a time.
5. **Editing and conflict resolution** — draft change-set authoring, source evidence capture,
   merge preview, and explicit application bridges under later decisions.

## Consequences

- Reviewers get a graph-native view while Git and GitHub remain the authoritative collaboration
  substrate.
- Continuous agent curation becomes attributable and replayable at the change-set boundary.
- Private operational memory stays outside publishable project history by construction.
- The honest no-write/no-WASM initial slice is less capable than a simulated full product, but its
  contracts can be implemented without later identity or trust-boundary migration.
- Two review workflows remain visible until an explicit bridge is designed; this is additional UX
  complexity in exchange for avoiding unsafe auto-application.
- A stable review bundle adds versioning and compatibility work, but prevents CLI, CI, and UI from
  inventing divergent semantic-diff shapes.

## Alternatives considered

### Build a khive-native GitHub replacement

Rejected. Branches, PRs, permissions, checks, comments, and merging are existing Git/GitHub
capabilities. Rebuilding them adds little KG-specific value and contradicts ADR-010.

### Push ADR-102's operational repository to GitHub

Rejected. It can contain notes and memories and is normatively local-only. An explicit export into
a separately configured project KG repository is the safe boundary.

### Use the existing excluded `khive-merge` crate as the web contract

Rejected for the initial slice. It is not in the workspace, has no production caller, lacks stable
serialized review output, assumes single-parent history for ancestry, and cites the unrelated
ADR-039 as its authority.

### Implement the editor directly against SQLite or an ad-hoc HTTP gateway

Rejected. Direct database access violates the single-owner live-store topology and creates a new
security boundary. No supported khive HTTP gateway exists today.

### Port the unshipped Deno diff and log commands into the browser

Rejected. The npm package does not route to them, their ontology is static and stale relative to
pack-loaded runtime kinds, and browser reimplementation would diverge from the required Rust/WASM
core.

## Acceptance criteria

- The editor imports and renders both discriminated `khive.review.v1` variants without inventing
  missing Git, GitHub, hash, or live-proposal identities.
- Demo, live, captured, read-only, local-only, and unavailable capabilities are visually explicit.
- Same-model-family approval is refused and covered by a test.
- The CLI command is reachable through the npm shim's unchanged argv forwarding to `kkernel`.
- The CLI never mutates Git, GitHub, or the live graph and has binary-level tests for JSON output,
  invalid change-sets, rule failures, reviewer-family eligibility, and fail-closed uncovered-op
  coverage.
- ADR reference lint, Rust tests, frontend lint/typecheck/unit tests, and `next build` pass.
- No deployment manifest, GitHub token, arbitrary repository path, or live database handle is
  introduced by the first slice.
