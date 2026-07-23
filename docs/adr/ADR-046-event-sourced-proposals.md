# ADR-046: Event-Sourced Agent KG Proposals

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Depends on**:

- ADR-014 (Curation Operations: apply step rides on existing curation primitives)
- ADR-017 (Pack Standard: KG pack handler surface; async event-consumer worker registration is deferred)
- ADR-018 (Authorization Gate: gates proposal verbs and mints the namespace token used by apply)
- ADR-022 (Events Query Surface: proposals live as events)
- ADR-041 (Event Provenance Projection: open-proposal projection lives here)

## Context

Callers can read and search the KG but also need a reviewable way to suggest
typed changes without applying them immediately.

The accepted decision selected option (c) **event-sourced proposals**: the
proposal lifecycle is encoded purely as events on the existing log substrate
(ADR-022). No new substrate and no new branch model. The projection table
(ADR-041) handles "show me all open
proposals" as a query that doesn't require scanning every event.

### What this ADR adds

- Four new `EventKind` variants for the proposal lifecycle
- Three new agent-facing verbs: `propose`, `review`, and `withdraw`
- Handler-invoked proposal worker structs: `ProposalsProjectionWorker` maintains `proposals_open`, and `ProposalApplyWorker` is called from `review(decision=Approve)` after the review transition to execute the changeset and emit `ProposalApplied`.
- A fold-derived "open proposals" projection table for query-time filtering
- The Authorization Gate (ADR-018) boundary for proposal verbs and apply authority

### What this ADR does NOT add

- A new substrate (Proposal is not a peer of Entity/Note/Edge/Event)
- A new note kind (proposals are not notes: they're event chains)
- Cross-namespace proposal flow (proposals are namespace-scoped, same as their
  target records)
- Configurable approval thresholds or quorum policy

### Why not a Proposal substrate

A substrate has its own table, store trait, lifecycle. Proposals don't need
that: they're transient state derived from a chain of events. The event log
already carries timestamps, namespace isolation, immutability, and replay. A
new substrate would duplicate all of that. The projection table is what
substantive substrates have; proposals only need the projection.

### Why not a `proposal` note kind

Notes carry semantic content (an `observation` is a thing the agent observed;
a `decision` is a thing the team decided). A proposal is a _workflow object_:
its content is the _proposed change_, not commentary on existing records.
Routing it through the note shape would force the changeset payload into a
note's `body` field with no schema validation.

## Decision

### 1. Four new EventKinds

Added to the shared `EventKind` enum:

```rust
pub enum EventKind {
    // ... existing ...
    ProposalCreated,       // agent created a proposal
    ProposalReviewed,      // human/agent decided approve | reject | comment
    ProposalApplied,       // worker executed the changeset
    ProposalWithdrawn,     // original proposer rescinded before review
}
```

These follow the existing event payload model: each has a typed payload
shape, validated by the event substrate.

### 2. Payload shapes

```rust
pub struct ProposalCreatedPayload {
    pub proposal_id:  Uuid,                  // canonical id, used in subsequent events
    pub proposer:     String,                // actor (agent id, user id)
    pub title:        String,                // short, human-readable
    pub description:  String,                // long-form rationale
    pub changeset:    ProposalChangeset,     // the actual proposed change
    pub reviewers:    Vec<String>,           // optional invited reviewers; empty = open review
    pub expiry:       Option<Timestamp>,     // optional auto-withdraw deadline
    pub parent_id:    Option<Uuid>,          // Set when amending an earlier proposal per RequestChanges; None for net-new proposals.
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalChangeset {
    /// Add a new entity. Fields validated against ADR-001 + pack kind specs.
    AddEntity { entity: EntityDraft },
    /// Modify an existing entity's properties / tags / description.
    UpdateEntity { id: Uuid, patch: EntityPatch },
    /// Add a new edge. Validated against ADR-002 endpoints + pack EDGE_RULES.
    AddEdge { source: Uuid, target: Uuid, relation: EdgeRelation, weight: Option<f32> },
    /// Add a note (entity-annotating or stand-alone).
    AddNote { note: NoteDraft },
    /// Merge two entities (ADR-014 §merge).
    MergeEntities { into: Uuid, from: Uuid },
    /// Supersede an entity with another (sets `supersedes` edge).
    SupersedeEntity { old: Uuid, new: Uuid },
    /// Compound: an ordered sequence of the above, applied atomically.
    /// Only single-step Compound is accepted at propose-time
    /// and legacy-apply-time: see "Compound changeset semantics (Fix 4)" below.
    Compound { steps: Vec<ProposalChangeset> },
}

pub struct ProposalReviewedPayload {
    pub proposal_id: Uuid,
    pub reviewer:    String,
    pub decision:    ProposalDecision,
    pub comment:     Option<String>,
}

pub enum ProposalDecision {
    Approve,
    Reject,
    Comment,        // not a decision; just adds a comment to the review thread
    RequestChanges, // proposer can amend (a new ProposalCreated with parent_id) and resubmit
}

pub struct ProposalAppliedPayload {
    pub proposal_id:   Uuid,
    pub applied_at:    Timestamp,
    pub applied_by:    String,            // worker provenance, not authorization identity
    pub result:        ApplyResult,
}

pub enum ApplyResult {
    Success { created_records: Vec<Uuid> },
    Failed {
        error: String,
        applied_step_count: u32, // 0 if compound proposal failed before any step; >0 if partial
    },
}

pub struct ProposalWithdrawnPayload {
    pub proposal_id: Uuid,
    pub by:          String,             // proposer; must match the original proposer
    pub reason:      Option<String>,
}
```

`ProposalChangeset` is a closed enum: no ad-hoc change types.

**Compound changeset semantics (Fix 4):** A `Compound([step1, step2, ...])` proposal
would apply steps in order within one SQLite write transaction. If any step's
runtime validation failed, the transaction would roll back and the worker would emit
`ProposalApplied { result: Failed { error, applied_step_count: 0 } }`.

> **Current restriction:** multi-step `Compound`
> (more than one step, including nested `Compound` containing more than one step) is
> rejected at propose-time and legacy-apply-time: `propose` returns
> `InvalidInput("multi-step Compound proposals are not supported until atomic proposal
> apply is available")` (`crates/khive-pack-kg/src/handlers/proposal.rs`,
> `has_multi_step_compound`). This is pending a real runtime/storage atomic-apply
> primitive that can span multiple public mutations: today `create_entity`, `link`,
> `merge`, and event-append are separate transactions, so the single-SQLite-transaction
> guarantee described below does not yet hold for genuinely multi-step compounds.
> Single-step `Compound` is unaffected and applies as described.

Cross-store atomicity (e.g., entity creation in SQLite + vector insert in
sqlite-vec) follows the same single-transaction model: v1 backends are
co-located. Future multi-backend deployments may relax this; the cross-backend
caveat is tracked at ADR-014. ADR-014 does NOT expose a multi-step transactional
primitive today; v1 correctness relies on the co-located SQLite assumption, not
on an ADR-014 guarantee. If a future ADR-014 amendment introduces
`runtime.curation.atomic_apply(steps)`, this section will be revised.

### 3. Verb surface: three new verbs

| Verb       | Speech act (ADR-025) | Visibility | Purpose                                                                                                                                                   |
| ---------- | -------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `propose`  | commissive           | Verb       | Create a proposal. Emits `ProposalCreated`. Returns the proposal id.                                                                                      |
| `review`   | declaration          | Verb       | Approve / reject / comment / request-changes. Emits `ProposalReviewed`.                                                                                   |
| `withdraw` | commissive           | Verb       | Rescind a proposal (proposer-only). Emits `ProposalWithdrawn`. Rejected if status is `applied`, `withdrawn`, `rejected`, or `applying` (in-flight apply). |

Apply is NOT a verb:

- **Apply**: v1 has no manual `apply` verb. `review(decision=Approve)` records the
  `ProposalReviewed` transition, then synchronously invokes
  `ProposalApplyWorker::maybe_apply(...)` before returning. Apply success or
  failure is still represented by a separate `ProposalApplied` event.

**Why `withdraw` is a verb (not `update`):** `update` in ADR-014 dispatches only
on `kind ∈ {entity, edge, note}`. Proposal events are NOT mutable substrate
records: they are append-only (ADR-022). A proposal event ID is not a record ID
in ADR-014's grammar. Routing withdrawal through `update` would require ADR-014
to understand proposal events as a mutable target, which contradicts event
immutability. `withdraw` is a NEW event in the chain (a `ProposalWithdrawn`),
not a mutation of a prior event.

**Coexistence with direct verbs:**

ADR-018 determines whether a caller may invoke direct mutating verbs (`create`,
`link`, `update`, `delete`, `merge`) and whether it may invoke `propose`, `review`,
or `withdraw`. Proposal processing does not weaken that decision. A request denied
at dispatch emits no proposal event and performs no KG mutation.

The proposal mechanism records and applies reviewed changes. It is not a substitute
for configuring an authorization gate appropriate to the exposed request surface.

```rust
// propose verb signature
pub struct ProposeArgs {
    pub title:       String,
    pub description: String,
    pub changeset:   ProposalChangeset,
    pub reviewers:   Vec<String>,         // optional
    pub expiry:      Option<Timestamp>,
}

// review verb signature
// SUPERSEDED by 2026-06-14 amendment (see §Amendment below):
// wire input param renamed proposal_id → id; internal payload field unchanged.
pub struct ReviewArgs {
    pub proposal_id: Uuid,  // SUPERSEDED wire name; current wire param is `id`
    pub decision:    ProposalDecision,
    pub comment:     Option<String>,
}

// withdraw verb signature
// SUPERSEDED by 2026-06-14 amendment (see §Amendment below):
// wire input param renamed proposal_id → id; internal payload field unchanged.
pub struct WithdrawArgs {
    pub proposal_id: Uuid,  // SUPERSEDED wire name; current wire param is `id`
    pub rationale:   Option<String>,
}
```

### 4. Open-proposal projection table

The projection table from ADR-041 (`event_observations`) doesn't cover the
proposal lifecycle (it's about provenance, not workflow state). Proposals get
their own projection: a small `proposals_open` table that the runtime
maintains as a fold over the four proposal events:

```sql
CREATE TABLE proposals_open (
    proposal_id    TEXT PRIMARY KEY,
    namespace      TEXT NOT NULL,
    proposer       TEXT NOT NULL,
    title          TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('open', 'changes_requested', 'approved', 'applying', 'rejected', 'applied', 'withdrawn')),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    expiry         INTEGER,
    last_decision  TEXT,                      -- bare decision string from the most recent ProposalReviewedPayload
    review_count   INTEGER NOT NULL DEFAULT 0,
    approve_count  INTEGER NOT NULL DEFAULT 0,
    reject_count   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_proposals_open_ns_status ON proposals_open(namespace, status);
CREATE INDEX idx_proposals_open_proposer ON proposals_open(namespace, proposer);
CREATE INDEX idx_proposals_open_updated  ON proposals_open(namespace, updated_at DESC);
```

`ProposalsProjectionWorker` is invoked by KG handlers to maintain this table.
In v1 this is synchronous handler-invoked code, not a registered `PackEventConsumer`
background worker:

- `ProposalCreated` → INSERT with status='open'
- `ProposalReviewed` → UPDATE counts; if `decision = Approve` and approval
  threshold met, set status='approved' (threshold logic in §6)
- `ProposalApplied` → UPDATE status='applied' (CAS: `WHERE status='applying'`)
- `ProposalWithdrawn` → UPDATE status='withdrawn'

**`applying`: transient in-flight state (V18 amendment):** The apply worker
atomically transitions status from `'approved'` to `'applying'` (a CAS UPDATE)
before executing any KG mutations. This prevents a concurrent `withdraw` from
landing while the apply is in progress: `withdraw`'s own CAS requires
`status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')`, so it fails
with an error when the apply worker holds `'applying'`. The apply worker
transitions to `'applied'` on success, or reverts to `'approved'` on failure so
the proposal is not permanently stuck. `'applying'` is never written to the
event log: it is a purely transient projection state.

Hard-state (status != 'open' | 'changes_requested') rows are retained for
audit. A `proposal_cleanup` operator command is deferred; future work must
define the CLI surface, retention policy, and safe-delete semantics.

**Review history retrieval (Fix 7):** The projection stores only aggregates
(`review_count`, `approve_count`, `reject_count`). Individual `ProposalReviewed`
events live in the event log. To retrieve all reviews on a proposal, query the
event log with:

```rust
EventFilter {
    kinds: vec![EventKind::ProposalReviewed],
    payload_proposal_id: Some(proposal_id),
    ..Default::default()
}
```

ADR-022 §3a `EventFilter` is extended in this ADR with an optional
`payload_proposal_id: Option<Uuid>` field: backed by an expression index on
`events.payload->>'proposal_id'` (SQLite expression index, added in the
migration that creates `proposals_open`).

`get(id=<proposal_id>)` resolves to the `ProposalCreated` event payload; review
history is a separate query via the extended `EventFilter`. The `get` verb does
NOT return review history inline.

### 5. Handler-invoked `ProposalApplyWorker` (v1)

v1 does not register `ProposalApplyWorker` as a `PackEventConsumer`; that runtime
infrastructure is not shipped. `handle_review` emits/commits the review transition
first, then calls `ProposalApplyWorker::maybe_apply(token, proposal_id, registry).await`
for approvals. This preserves the event contract while making apply latency part of
`review(approve)` in v1.

Call flow:

1. `handle_review` resolves the proposal id and validates state.
2. `reviewed_and_emit` atomically advances `proposals_open` and inserts `ProposalReviewed`.
3. On `Approve`, `ProposalApplyWorker::maybe_apply` claims `approved` to `applying`, applies the changeset, emits `ProposalApplied`, then marks `applied` or reverts to `approved` on failure.

Future async worker wiring, if added, must filter by `EventKind::ProposalReviewed`,
not by verb string. Current v1 code calls the worker directly from `handle_review`.

On each approved review handled by `handle_review`, `ProposalApplyWorker::maybe_apply`:

1. Reads the proposal's current state from `proposals_open`.
2. If `decision = Approve` AND approval threshold reached AND no Reject vote
   recorded AND not already applied/withdrawn: proceed to apply.
3. Calls `ProposalApplier::apply(changeset)` which dispatches each
   `ProposalChangeset` arm to the existing runtime API:
   - `AddEntity` → `runtime.entities.create(...)`
   - `UpdateEntity` → `runtime.entities.update(...)`
   - `AddEdge` → `runtime.graph.link(...)`
   - `AddNote` → `runtime.notes.create(...)`
   - `MergeEntities` → `runtime.curation.merge_entities(...)`
   - `SupersedeEntity` → adds `supersedes` edge via `runtime.graph.link(...)`
   - `Compound` → recursive within a single transaction (multi-step Compound
     currently rejected before this stage: see the current-restriction note above)
4. Emits `ProposalApplied` with `Success { created_records }` or `Failed { error }`.

The apply worker receives the same `NamespaceToken` that authorized the review
operation. It does not accept a namespace override and cannot mint a broader token.
All reads and writes performed by the changeset are therefore pinned to the
proposal's authorized namespace. Apply-event attribution is implementation-owned
provenance and is not a caller-selectable authorization identity.

### 6. Approval threshold

The shipped rule is **one approval and no recorded rejection**. The `review`
handler rejects an approval from the same non-local attribution that created the
proposal before it emits `ProposalReviewed`. A rejection by the proposer remains
valid. The embedded `local` attribution represents one local principal and is
allowed to review its own proposal.

The optional `reviewers` list is descriptive metadata in this version; it is not
an authorization list. Configurable thresholds, enforced reviewer lists, and a
quorum policy are not part of the shipped contract. Systems requiring separation
of duties must enforce reviewer eligibility in the ADR-018 gate and must not treat
the one-approval rule as a quorum protocol.

### 8. Authorization

Per ADR-018, the gate evaluates `propose`, `review`, and `withdraw` before their
handlers run. The canonical verb, caller attribution, arguments, and target
namespace are part of that decision. The apply worker runs only after an
authorized approval and uses the review request's namespace token.

Cross-namespace application is prohibited by construction: the proposal is loaded
through that token, every prepared operation receives that token, and neither the
proposal payload nor the apply worker may replace its namespace.

### 9. Failure modes

| Condition                                         | Behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Proposer withdraws after Approve but before Apply | If `withdraw` arrives before the apply worker claims `'applying'`: `ProposalWithdrawn` emitted; worker sees status≠'approved' (pre-apply CAS fails); skips KG mutations; no `ProposalApplied` emitted. If `withdraw` arrives after the apply worker claims `'applying'`: `withdraw` CAS finds status='applying' and returns an error: the withdraw is rejected. KG mutations proceed and `ProposalApplied` is emitted normally.                                                                                                                                                                                                                                                                        |
| Apply fails (validation, network, etc.)           | `ProposalApplied { Failed }` emitted; status is reverted from `'applying'` back to `'approved'` (best-effort CAS) so the proposal is not permanently stuck. Apply retry is deferred to a follow-up ADR. v1 behavior: failed applies return to `'approved'`; operators may issue a new `propose` (with `parent_id` referencing the failed proposal) to retry. Direct re-emission of `apply` events is not supported in v1.                                                                                                                                                                                                                                                                              |
| Review authorization denied                       | No review event is emitted and the apply worker is not invoked.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Reviewer reverses Approve to Reject               | Each review is its own event; the worker uses the latest decision per reviewer. If a previously-approved proposal hits Reject before Apply fires, status moves to 'rejected'.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Two reviewers race (both Approve simultaneously)  | Each `review(approve)` call invokes the apply worker synchronously after `reviewed_and_emit`. The `reviewed_and_emit` CAS serializes concurrent reviews at the projection layer; the apply worker’s `approved → applying` CAS ensures only one invocation executes the changeset. The worker checks `proposals_open.status` before applying; if already `applied` or `applying`, it returns without re-executing.                                                                                                                                                                                                                                                                                      |
| Proposal expires                                  | Expiry automation is not shipped in v1. Until a separately specified expiry worker exists, an expired proposal remains visible and cannot be treated as automatically withdrawn.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Stale-target conflict (Fix 6)                     | An `UpdateEntity` or `MergeEntities` proposal targets a specific entity ID. Between propose-time and apply-time the entity may be independently modified. v1 default: **last-writer-wins**: the proposal applies its patch unconditionally. Optional: proposals may include `expected_version: Option<u64>` in the payload (if entity versioning is introduced via ADR-014 amendment). The apply worker would then check `current_version == expected_version` and emit `ProposalApplied { Failed { error: "stale: target was modified since proposal", current_version, expected_version } }` on mismatch. v1 does NOT introduce entity versioning; this knob is gated on a future ADR-014 amendment. |

### 10. CLI / MCP surface summary

| Surface                                                           | Action                                                   | How                                    |
| ----------------------------------------------------------------- | -------------------------------------------------------- | -------------------------------------- |
| MCP `propose(...)`                                                | Create a proposal                                        | Verb                                   |
| MCP `review(id, decision, comment?)`                              | Cast a review                                            | Verb                                   |
| MCP `withdraw(id, rationale?)`                                    | Withdraw a proposal (proposer-only)                      | Verb                                   |
| MCP `list(kind=proposal, status="open")`                          | Browse open proposals                                    | Lists from `proposals_open` projection |
| MCP `get(id=<proposal_id>)`                                       | Fetch a single proposal's `ProposalCreated` payload      | Resolves to the event payload          |
| CLI `kkernel exec 'kg.proposal_cleanup(older_than="<duration>")'` | Archive resolved proposals (deferred: not shipped in v1) | Future operator housekeeping           |

`list(kind=proposal)` dispatches to a new `kg.list_proposals` handler under
the kg pack: it queries `proposals_open` directly, supports the standard
`status` / `proposer` / `namespace` filters, and returns in the verbose
canonical shape (ADR-045 trims for agent mode).

## Rationale

### Why the approval rule is deliberately narrow

The event model provides review history and a deterministic apply trigger. It does
not claim to provide quorum governance. One approval is sufficient for the local
workflow, while ADR-018 remains the authoritative control over who may submit a
review. More elaborate governance requires a separate, enforced policy contract.

### Why a projection table (and not just fold over events on every list)

ADR-041's rationale applies: projection-on-write is much cheaper at query time
than fold-on-read for any non-trivial event volume. A 10,000-proposal log
folded on every `list(kind=proposal, status="open")` call would be unusable;
the projection table makes it index-scan-fast.

### Why `withdraw` is its own verb (not `update`)

`update` in ADR-014 dispatches on `kind ∈ {entity, edge, note}`. Proposal
events are append-only (ADR-022): they are NOT mutable substrate records. A
proposal event ID is not a valid target for ADR-014's `update` grammar.
Routing withdrawal through `update` would require ADR-014 to treat events as
mutable records, contradicting event immutability. `withdraw` is a dedicated
commissive verb that emits a NEW `ProposalWithdrawn` event: it does not
mutate any prior event. The handler enforces proposer-only access (by checking
`proposals_open.proposer == actor.id`) before emitting.

### Why apply is a separate worker step, but invoked synchronously in v1

v1 separates review from apply in the event model, not in the scheduler. The
review transition is committed first; then the handler invokes the apply worker.
This keeps review and apply audit events distinct, and apply failures surface as
`ProposalApplied { Failed }`. Because no `PackEventConsumer` runtime is shipped,
`review(approve)` currently includes apply latency. A future event-consumer
implementation can move this invocation out of the handler without changing the
proposal event contract.

### Why approval triggers apply

The shipped event transition is deterministic: one accepted approval with no
recorded rejection moves the proposal to `approved`, and the handler then invokes
the apply worker. There is no separate apply verb or configurable quorum in this
version. Eligibility to submit the approving review remains an ADR-018 gate decision.

### Why a closed `ProposalChangeset` enum

Open changesets ("here's a JSON object, just apply it") are unimplementable
safely: the apply step would have to interpret arbitrary JSON against pack
schemas with no static guarantees. Closing the enum at the proposal-creation
boundary means the apply worker is a finite dispatch: each arm calls a known
runtime method with statically-typed inputs.

The cost is that proposals can't express every conceivable change. v1 covers
the common cases (add entity, add edge, add note, update entity, merge,
supersede, compound). Future arms add to the enum via additive semver bumps.

## Alternatives Considered

| Alternative                                                            | Why rejected                                                                                                                                                                  |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Proposal-as-Note (option a)                                            | Forces changesets into note `body`; loses schema validation; muddies the note-kind taxonomy                                                                                   |
| PendingEdit substrate (option b)                                       | New substrate, new store, new VCS dimension: heavyweight for a workflow object                                                                                                |
| Git-native subset (option d)                                           | Re-imports the git assumption the decision explicitly avoided                                                                                                                 |
| Inline apply without a separate worker step or `ProposalApplied` event | Rejected: v1 may call `ProposalApplyWorker` synchronously from `review`, but apply remains a separate worker struct and emits `ProposalApplied` for audit/failure separation. |
| Approve = side-effect of `update(id=<proposal>, status=approved)`      | Conflates review (a typed decision) with record mutation; loses the review-history audit trail                                                                                |
| Per-proposer namespace for proposals                                   | Cross-cuts the namespace-isolation invariant; agents can't propose changes targeting namespaces they can read                                                                 |
| Open changeset format (JSON blob)                                      | Can't validate at proposal time; failure surfaces deep inside apply with poor error messages                                                                                  |

## Consequences

### Positive

- Proposal workflow lands without a new substrate, new store trait, or git
  dependency. Minimal incremental complexity.
- Cross-pack proposals work the same way as KG-pack proposals; any pack can
  consume the four `EventKind`s.
- The audit trail is the event log. Review history for a proposal is retrieved
  via the events query surface (ADR-022):

  ```rust
  EventFilter::default()
      .with_kinds(vec![EventKind::ProposalReviewed])
      .with_payload_predicate("proposal_id", PropertyOp::Eq(proposal_id))
      .ordered_by(Newest)
  ```

  `get(id=<proposal_id>)` resolves to the `ProposalCreated` event payload;
  review history is a separate query via this EventFilter.

### Negative

- The `proposals_open` projection adds one table + one worker. Small overhead,
  but real.
- `ProposalChangeset` is a closed enum: every new change shape requires an
  amendment. This is the cost of validation-at-create-time.
- The one-approval rule is not a quorum or separation-of-duties mechanism.
  Reviewer eligibility must be enforced at the authorization gate.

### Neutral

- The proposal verbs are registered alongside the existing KG handlers and use
  the same ADR-016 single-tool `request` envelope.

## Implementation

### Crate placement

- Verb handlers: `crates/khive-pack-kg/src/handlers.rs`
- Apply worker: `crates/khive-pack-kg/src/apply_worker.rs`
- Projection table + projection worker: `crates/khive-pack-kg/src/projection_worker.rs`
- Payload types: `khive-types::events::proposal_payloads`

### Migration

The `proposals_open` projection table was created in migration V15 in
`crates/khive-db/src/migrations.rs`. The `applying` transient status and
its CAS invariants were added in V18.

A `VersionedMigration` in `crates/khive-db/src/migrations.rs`:

```rust
VersionedMigration {
    version: 15,
    name: "proposals_open",
    up: PROPOSALS_OPEN_DDL,
}
```

DDL (`PROPOSALS_OPEN_DDL`):

1. Create `proposals_open` table (DDL in §4)
2. Create the three indexes on `proposals_open`
3. Create expression index: `CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'))`: backing the `EventFilter.payload_proposal_id` query extension from §4
4. Backfill is unnecessary: no prior proposals exist

### Worker invocation

v1 does not register background `PackEventConsumer` workers in KG pack initialization.
The KG pack registers `propose`, `review`, and `withdraw` in `KG_HANDLERS`; those
handlers invoke worker structs directly:

- `handle_propose` -> `ProposalsProjectionWorker::on_proposal_created`
- `handle_review` -> `ProposalsProjectionWorker::reviewed_and_emit`, then
  `ProposalApplyWorker::maybe_apply` on approve
- `handle_withdraw` -> `ProposalsProjectionWorker::withdrawn_and_emit`

Future async worker registration may reuse the same `EventKind` filters, but it is deferred.

### Handler registration

Handler declarations in the KG pack manifest use the canonical `HandlerDef/HANDLERS`
form (ADR-017 §pack handler trait shape; `VerbDef/VERBS` is deprecated):

```rust
pub const HANDLERS: &[HandlerDef] = &[
    // ... existing handlers ...
    HandlerDef {
        name:        "propose",
        description: "Create a proposal for a KG change.",
        visibility:  Visibility::Verb,
        category:    Category::Proposals,
        params:      &PROPOSE_PARAMS,
    },
    HandlerDef {
        name:        "review",
        description: "Approve, reject, comment, or request changes on a proposal.",
        visibility:  Visibility::Verb,
        category:    Category::Proposals,
        params:      &REVIEW_PARAMS,
    },
    HandlerDef {
        name:        "withdraw",
        description: "Rescind a proposal (proposer only).",
        visibility:  Visibility::Verb,
        category:    Category::Proposals,
        params:      &WITHDRAW_PARAMS,
    },
];
```

All three entries have `visibility: Visibility::Verb`: they are externally
invokable by agents via the `request` DSL. Internal subhandlers (if any) would
use `Visibility::Subhandler`.

ADR-023's exhaustive KG handler table includes `propose`, `review`, and
`withdraw` as public verbs.

### Identity model: event UUID vs proposal_id

Each proposal-lifecycle event has its own `event.id` (UUID assigned at emit
time). The `proposal_id` is a separate logical aggregate identifier that threads
together `ProposalCreated`, `ProposalReviewed`, `ProposalWithdrawn`, and
`ProposalApplied` events for one proposal.

```rust
pub struct Event {
    pub id:        Uuid,                // unique per event
    pub kind:      EventKind,
    pub aggregate: Option<AggregateRef>,
    pub payload:   EventPayload,
}

pub struct AggregateRef {
    pub kind: AggregateKind,            // e.g., AggregateKind::Proposal
    pub id:   Uuid,                     // proposal_id
}
```

Therefore:

```text
ProposalCreated.event.id              != proposal_id
ProposalCreated.event.aggregate.id    == proposal_id
ProposalReviewed.event.aggregate.id   == proposal_id
```

v1 implementation uses a JSON payload index
(`idx_events_payload_proposal_id`) as a bridge: the `proposal_id` field in
each event's JSON payload is indexed via SQLite expression index. A future ADR
may promote `aggregate_id` / `aggregate_kind` to first-class event columns;
for v1, the JSON path is sufficient.

Lookup wire shape:

- `get(id=<event_uuid>)` resolves to the specific event record by event UUID.
- `get(id=<proposal_id>)` resolves raw proposal IDs and short prefixes via
  `proposals_open` and returns the `ProposalCreated` event payload from the
  event log.
- For full review history, use the events query surface with
  `EventFilter { kinds: vec![EventKind::ProposalReviewed], ... }` and a
  payload predicate on `proposal_id`.

## References

- ADR-014 (Curation Operations): `merge_entities` and atomic compound updates
  consumed by the apply worker
- ADR-017 (Pack Standard): handler declaration surface used by the KG pack; proposal `PackEventConsumer` registration remains deferred
- ADR-018 (Authorization Gate): gates proposal verbs and supplies apply authority
- ADR-022 (Events Query Surface): proposal events live as substrate events
- ADR-016 (Request DSL): `propose`, `review`, and `withdraw` ride the standard
  single-tool `request` envelope
- ADR-041 (Event Provenance Projection): projection-table pattern this ADR
  reuses

## Amendment (2026-06-14): proposal_id → id wire-key rename

**Scope**: wire-result keys and input params only. Internal struct fields,
DB columns, and event payload fields are unchanged.

- `propose` result key: `proposal_id` → `id`
- `review` result key: `proposal_id` → `id`; input param `proposal_id` → `id`
- `withdraw` result key: `proposal_id` → `id`; input param `proposal_id` → `id`
- `list(kind=proposal)` row key: `proposal_id` → `id`
- `get(id=<proposal_uuid>)` result key: `proposal_id` → `id`

**Clean break**: `ReviewParams` and `WithdrawParams` use `#[serde(deny_unknown_fields)]`,
so callers still passing `proposal_id=` receive an immediate deserialization error.
No dual-emit.

**Unchanged permanently**: `ProposalCreatedPayload.proposal_id` struct field,
`proposals_open.proposal_id` DB column, `EventFilter.payload_proposal_id` filter field,
and all internal worker references.
