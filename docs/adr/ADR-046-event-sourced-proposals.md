# ADR-046: Event-Sourced Agent KG Proposals

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Depends on**:

- ADR-014 (Curation Operations — apply step rides on existing curation primitives)
- ADR-017 (Pack Standard — KG pack handler surface; async event-consumer worker registration is deferred)
- ADR-018 (Authorization Gate — gates the apply step; Gate-error posture is amended by ADR-129)
- ADR-022 (Events Query Surface — proposals live as events)
- ADR-032 (Brain Profile Orchestration — future proposal-event Fold design; no v1 consumer)
- ADR-041 (Event Provenance Projection — open-proposal projection lives here)

## Context

khive agents can read and search the KG; they cannot propose changes for
review. Old khive ADR-075 specified agent-driven PRs via MCP verbs (`branch`,
`commit`, `pr`) layered on the git-native KG. v1 ADR-020 explicitly excludes
git operations from MCP — agents do not drive the KG through git. The agent
workflow ("propose change → reviewer approves → change lands") was dropped
without a replacement.

The accepted decision selected option (c) **event-sourced proposals**: the
proposal lifecycle is encoded purely as events on the existing log substrate
(ADR-022). No new substrate. No new branch model. No git verbs over MCP. The
proposal events preserve input for a future brain fold, but shipped v1 has no
automatic shared-log consumer (ADR-032 §6). The projection table (ADR-041)
handles "show me all open proposals" as a query that doesn't require scanning
every event.

### What this ADR adds

- Four new `EventKind` variants for the proposal lifecycle
- Three new agent-facing verbs: `propose`, `review`, and `withdraw`
- Handler-invoked proposal worker structs: `ProposalsProjectionWorker` maintains `proposals_open`, and `ProposalApplyWorker` is called from `review(decision=Approve)` after the review transition to execute the changeset and emit `ProposalApplied`.
- A fold-derived "open proposals" projection table for query-time filtering
- The Authorization Gate (ADR-018) wiring on the apply step

### What this ADR does NOT add

- A new substrate (Proposal is not a peer of Entity/Note/Edge/Event)
- A new note kind (proposals are not notes — they're event chains)
- Cross-namespace proposal flow (proposals are namespace-scoped, same as their
  target records)
- Auto-apply on N approvals (requires explicit operator policy; deferred)

### Why not a Proposal substrate

A substrate has its own table, store trait, lifecycle. Proposals don't need
that — they're transient state derived from a chain of events. The event log
already carries timestamps, namespace isolation, immutability, and replay. A
new substrate would duplicate all of that. The projection table is what
substantive substrates have; proposals only need the projection.

### Why not a `proposal` note kind

Notes carry semantic content (an `observation` is a thing the agent observed;
a `decision` is a thing the team decided). A proposal is a _workflow object_ —
its content is the _proposed change_, not commentary on existing records.
Routing it through the note shape would force the changeset payload into a
note's `body` field with no schema validation.

## Decision

### 1. Four new EventKinds

Added to ADR-032 §3's enum:

```rust
pub enum EventKind {
    // ... existing ...
    ProposalCreated,       // agent created a proposal
    ProposalReviewed,      // human/agent decided approve | reject | comment
    ProposalApplied,       // worker executed the changeset
    ProposalWithdrawn,     // original proposer rescinded before review
}
```

These follow the existing event payload model — each has a typed payload
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
    /// Modify an existing entity's properties / tags / description / entity type
    /// (absent = unchanged, null = clear, string = set-and-validate).
    UpdateEntity { id: Uuid, patch: EntityPatch },
    /// Add a new edge. Validated against ADR-002 endpoints + pack EDGE_RULES.
    AddEdge { source: Uuid, target: Uuid, relation: EdgeRelation, weight: Option<f32> },
    /// Add a note (entity-annotating or stand-alone).
    AddNote { note: NoteDraft },
    /// Merge two entities (ADR-014 §merge).
    MergeEntities { into: Uuid, from: Uuid },
    /// Supersede an entity with another (sets `supersedes` edge).
    SupersedeEntity { old: Uuid, new: Uuid },
    /// Classify an EXISTING memory-supersession edge's governance in place.
    /// Added by the 2026-08-16 amendment (see §Amendment below); the storage
    /// contract is ADR-159 §5. Never mutates `graph_edges`.
    ClassifyExistingEdgeGovernance {
        edge_id: Uuid,
        expected: ExpectedEdgePreimage,
        disposition: GovernanceDisposition,
        reason_code: Option<GovernanceReasonCode>,
    },
    /// Compound: an ordered sequence of the above, applied atomically.
    /// NOTE: as of PR #517, only single-step Compound is accepted at propose-time
    /// and legacy-apply-time — see "Compound changeset semantics (Fix 4)" below.
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
    pub applied_by:    String,            // typically the propose-apply worker id
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

`ProposalChangeset` is a closed enum — no ad-hoc change types.

**Compound changeset semantics (Fix 4):** A `Compound([step1, step2, ...])` proposal
applies steps in order. The apply worker uses a single SQLite write transaction
wrapping all steps (since all v1 backends share the same SQLite connection per
`khive-db`). If ANY step's runtime validation fails, the entire transaction
rolls back and the worker emits

> **Current restriction (PR #517, containment fix for #423):** multi-step `Compound`
> (more than one step, including nested `Compound` containing more than one step) is
> rejected at propose-time and legacy-apply-time — `propose` returns
> `InvalidInput("multi-step Compound proposals are not supported until atomic proposal
> apply is available")` (`crates/khive-pack-kg/src/handlers/proposal.rs`,
> `has_multi_step_compound`). This is pending a real runtime/storage atomic-apply
> primitive that can span multiple public mutations — today `create_entity`, `link`,
> `merge`, and event-append are separate transactions, so the single-SQLite-transaction
> guarantee described below does not yet hold for genuinely multi-step compounds.
> Single-step `Compound` is unaffected and applies as described.

`ProposalApplied { result: Failed { error, applied_step_count: 0 } }`.

Cross-store atomicity (e.g., entity creation in SQLite + vector insert in
sqlite-vec) follows the same single-transaction model — v1 backends are
co-located. Future multi-backend deployments may relax this; the cross-backend
caveat is tracked at ADR-014. ADR-014 does NOT expose a multi-step transactional
primitive today; v1 correctness relies on the co-located SQLite assumption, not
on an ADR-014 guarantee. If a future ADR-014 amendment introduces
`runtime.curation.atomic_apply(steps)`, this section will be revised.

### 3. Verb surface — three new verbs

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
records — they are append-only (ADR-022). A proposal event ID is not a record ID
in ADR-014's grammar. Routing withdrawal through `update` would require ADR-014
to understand proposal events as a mutable target, which contradicts event
immutability. `withdraw` is a NEW event in the chain (a `ProposalWithdrawn`),
not a mutation of a prior event.

**Coexistence with direct verbs (Fix 2 / ADR-018 policy):**

ADR-018 authorization gates determine whether an actor can call direct mutating
verbs (`create`, `link`, `update`, `delete`, `merge`) or must route through
proposals. The proposal flow is OPT-IN per deployment via the ADR-018 policy
fragment:

```rego
# Example: agents must propose, operators can apply directly
allow if {
    input.actor.kind == "agent"
    input.verb in ["propose", "review", "withdraw"]
}
# Direct mutating verbs require operator role
allow if {
    input.actor.kind == "user"
    input.actor.role == "operator"
    input.verb in ["create", "link", "update", "delete", "merge"]
}
```

The default gate (AllowAllGate) allows both paths — single-developer deployments
rarely need the proposal review gate. Multi-actor deployments configure ADR-018
to force agents through proposals. This ADR does NOT mandate gating; it provides
the mechanism, ADR-018 provides the policy.

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
their own projection — a small `proposals_open` table that the runtime
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
- successful `ProposalApplied` → atomically UPDATE status='applied' (CAS:
  `WHERE status='applying'`) and INSERT the success event; a missed CAS or update
  error publishes no success event
- apply failure known to precede a durable changeset commit → INSERT the
  failure event, then best-effort revert status from 'applying' to 'approved'
- `ProposalWithdrawn` → UPDATE status='withdrawn'

**`applying` — in-flight or reconciliation state (V18 plus 2026-08-11 amendment):** The apply worker
atomically transitions status from `'approved'` to `'applying'` (a CAS UPDATE)
before executing any KG mutations. This prevents a concurrent `withdraw` from
landing while the apply is in progress — `withdraw`'s own CAS requires
`status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')`, so it fails
with an error when the apply worker holds `'applying'`. The apply worker
transitions to `'applied'` and inserts the success event in one transaction, or
reverts to `'approved'` only when failure is known to precede the durable
changeset commit. A failure after that commit leaves `'applying'` as the
non-replayable reconciliation state.
The success event INSERT is conditional on the transition changing exactly one
row, so a missed CAS cannot publish success. `'applying'` is never written to
the event log — it is a projection-only state, normally transient but retained
when a durable apply needs post-commit reconciliation.

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
`payload_proposal_id: Option<Uuid>` field — backed by an expression index on
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
3. On `Approve`, `ProposalApplyWorker::maybe_apply` claims `approved` to `applying`
   and applies the changeset. Success atomically marks `applied` and inserts
   `ProposalApplied`; a failure known to be pre-commit inserts
   `ProposalApplied { Failed }` and then reverts to `approved`. A post-commit
   maintenance/read failure leaves `applying` for reconciliation.

Future async worker wiring, if added, must filter by `EventKind::ProposalReviewed`,
not by verb string. Current v1 code calls the worker directly from `handle_review`.

On each approved review handled by `handle_review`, `ProposalApplyWorker::maybe_apply`:

1. Reads the proposal's current state from `proposals_open`.
2. If `decision = Approve` AND approval threshold reached AND no Reject vote
   recorded AND not already applied/withdrawn — proceed to apply.
3. Calls `ProposalApplier::apply(changeset)` which dispatches each
   `ProposalChangeset` arm to the existing runtime API:
   - `AddEntity` → `runtime.entities.create(...)`
   - `UpdateEntity` → `runtime.entities.update(...)`
   - `AddEdge` → `runtime.graph.link(...)`
   - `AddNote` → `runtime.notes.create(...)`
   - `MergeEntities` → `runtime.curation.merge_entities(...)`
   - `SupersedeEntity` → adds `supersedes` edge via `runtime.graph.link(...)`
   - `Compound` → recursive within a single transaction (multi-step Compound
     currently rejected before this stage — see the current-restriction note above)
4. On success, calls `applied_and_emit`, which executes the `applying` →
   `applied` CAS and the conditional `ProposalApplied { Success { created_records } }`
   INSERT in one write transaction. On failure known to precede a durable
   changeset commit, emits `ProposalApplied { Failed { error } }` and
   best-effort reverts to `approved`. After `Committed`, later failures emit no
   failed event and do not revert.

Authorization (ADR-018) checks the apply attempt. The worker's actor identity
is (Fix 9):

```rust
ActorRef { kind: "system".to_string(), id: "propose-apply".to_string() }
```

The gate evaluates: "can `system:propose-apply` write into namespace X?"
against the policy. Deny → emits `ProposalApplied` with
`Failed { error: "denied by policy: ..." }` and the proposal lands in
status='approved' but unapplied — a deployment-config issue the operator
resolves by adjusting the policy. Production deployments configuring ADR-018
Rego policies should include this actor class explicitly. The default gate
(AllowAllGate) permits it transparently.

### 6. Approval threshold

v1 default: **one approve from any qualified reviewer, no recorded reject**.
"Qualified reviewer" means an actor not equal to the proposer (when
`allow_self_approve = false`) and, if the proposal listed explicit `reviewers`,
in that list (otherwise any actor counts).

**Self-approve prevention (Fix 5):** The `review` verb HANDLER (not the
projection worker or apply worker) reads `proposals_open.proposer` and rejects
with `RuntimeError::SelfApprovalForbidden { proposal_id, actor_id }` BEFORE
emitting any `ProposalReviewed` event. This gives the reviewer immediate
feedback. The check fires only when `decision=approve`; rejecting one's own
proposal is allowed (treated as withdrawal-via-reject). When
`ProposalPolicy::allow_self_approve = true`, the check is skipped entirely.

The shipped v1 default is **one approve from any non-self actor, no recorded
reject**. The inline self-approval guard in the `review` handler is the only
shipped policy enforcement point.

Configurable approval thresholds, pack manifest TOML configuration
(`[packs.kg.proposals]`), `ProposalPolicy` struct instantiation, and
`require_listed_reviewer` are deferred. Multi-actor deployments requiring
configurable thresholds or reviewer lists must await a future ADR amendment
before those controls are available.

### ProposalPolicy: pack-owned, gate-enforced (deferred)

`ProposalPolicy`, `ProposalGatePolicy`, and `PackGatePolicy` are deferred.
The shipped v1 enforcement is the inline self-approval guard in `handle_review`:
the handler reads `proposals_open.proposer` and rejects with
`RuntimeError::SelfApprovalForbidden { proposal_id, actor_id }` when
`decision=approve` and `actor.id == proposer`. This check fires before any
event is emitted, giving immediate feedback.

The full configurable policy struct, gate registration, and
`VerbRegistryBuilder::with_pack_policy` wiring are future work. When shipped,
`ProposalGatePolicy` will register with the ADR-018 authorization gate as the
authoritative trust boundary; the handler's inline check will remain as a
defense-in-depth layer but not the sole enforcement point.

### 7. Brain integration

Brain profiles (ADR-032) can fold over proposal events the same way they fold
over `RecallExecuted` / `FeedbackExplicit`. Brain can learn:

- Which proposers' proposals get approved more often (proposer-quality posterior)
- Which changeset shapes get rejected (per-shape failure rate)
- Reviewer agreement patterns (do reviewers A and B usually agree?)

These are future brain extensions — v1 brain doesn't include proposal-specific
folds. The event log carries the signal; brain will learn from it when an
ADR specifies what to optimize.

`served_by_profile_id` is NOT set on proposal events — they are not
profile-served (they're authored by agents directly, not by a brain-resolved
profile decision).

### 8. Authorization

Per ADR-018, the gate evaluates each verb call against the policy. The new
verbs and the apply worker each have policy hooks:

- `propose`: policy decides whether `actor` can create proposals in
  `namespace`. Default policy: any authenticated agent can propose. Operators
  who need restrictions add a rego rule.
- `review`: policy decides whether `actor` can review proposals in
  `namespace`. Default: any actor. Operators may restrict to specific roles.
- `propose-apply` worker: the worker emits `ActorRef { kind: "system", id: "propose-apply" }`
  as its actor identity. The authorization gate evaluates this identity against
  the active policy; with the default gate (AllowAllGate) it is permitted
  transparently. A dedicated `system:propose-apply` policy rule is future work;
  production deployments requiring explicit cross-namespace injection prevention
  should add a rego rule for this actor class when configuring ADR-018.

### 9. Failure modes

| Condition                                                       | Behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Proposer withdraws after Approve but before Apply               | If `withdraw` arrives before the apply worker claims `'applying'`: `ProposalWithdrawn` emitted; worker sees status≠'approved' (pre-apply CAS fails); skips KG mutations; no `ProposalApplied` emitted. If `withdraw` arrives after the apply worker claims `'applying'`: `withdraw` CAS finds status='applying' and returns an error — the withdraw is rejected. KG mutations proceed and `ProposalApplied` is emitted normally.                                                                                                                                                                                                                                                                        |
| Apply fails before commit (validation, prepare, rollback, etc.) | `ProposalApplied { Failed }` emitted; status is reverted from `'applying'` back to `'approved'` (best-effort CAS) so the proposal is not permanently stuck. Apply retry is deferred to a follow-up ADR. v1 behavior: failed applies return to `'approved'`; operators may issue a new `propose` (with `parent_id` referencing the failed proposal) to retry. Direct re-emission of `apply` events is not supported in v1.                                                                                                                                                                                                                                                                               |
| Post-commit reindex or created-record resolution fails          | No `ProposalApplied { Failed }` event is emitted and the projection is not reverted. The graph mutation is already durable, so the proposal remains `applying` as an explicit reconciliation state. A repeated worker pass sees status != `approved` and skips the changeset, preventing replay. The condition is logged with `committed=true`, `retryable=false`, and a typed stage.                                                                                                                                                                                                                                                                                                                   |
| Apply policy denied                                             | Same as a pre-commit apply failure with `error = "denied by policy"`. Operator adjusts policy and issues a new `propose` (with `parent_id`) to retry; direct `apply` re-emission is not supported in v1.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Success finalization CAS misses or errors                       | The transaction publishes no `ProposalApplied { Success }` event. A failed batch leaves the projection at `applying`; committed KG mutations remain visible, but event consumers cannot observe success while projection readers still see a non-applied proposal. The condition is logged for operator reconciliation.                                                                                                                                                                                                                                                                                                                                                                                 |
| Reviewer reverses Approve to Reject                             | Each review is its own event; the worker uses the latest decision per reviewer. If a previously-approved proposal hits Reject before Apply fires, status moves to 'rejected'.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Two reviewers race (both Approve simultaneously)                | Each `review(approve)` call invokes the apply worker synchronously after `reviewed_and_emit`. The `reviewed_and_emit` CAS serializes concurrent reviews at the projection layer; the apply worker’s `approved → applying` CAS ensures only one invocation executes the changeset. The worker checks `proposals_open.status` before applying; if already `applied` or `applying`, it returns without re-executing.                                                                                                                                                                                                                                                                                       |
| Proposal expires                                                | A background sweep (TBD: cron-style, not v1) emits `ProposalWithdrawn` with `by = "system:expiry"` on proposals past their `expiry` timestamp.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Stale-target conflict (Fix 6)                                   | An `UpdateEntity` or `MergeEntities` proposal targets a specific entity ID. Between propose-time and apply-time the entity may be independently modified. v1 default: **last-writer-wins** — the proposal applies its patch unconditionally. Optional: proposals may include `expected_version: Option<u64>` in the payload (if entity versioning is introduced via ADR-014 amendment). The apply worker would then check `current_version == expected_version` and emit `ProposalApplied { Failed { error: "stale: target was modified since proposal", current_version, expected_version } }` on mismatch. v1 does NOT introduce entity versioning; this knob is gated on a future ADR-014 amendment. |

### 10. CLI / MCP surface summary

| Surface                                                           | Action                                                    | How                                    |
| ----------------------------------------------------------------- | --------------------------------------------------------- | -------------------------------------- |
| MCP `propose(...)`                                                | Create a proposal                                         | Verb                                   |
| MCP `review(id, decision, comment?)`                              | Cast a review                                             | Verb                                   |
| MCP `withdraw(id, rationale?)`                                    | Withdraw a proposal (proposer-only)                       | Verb                                   |
| MCP `list(kind=proposal, status="open")`                          | Browse open proposals                                     | Lists from `proposals_open` projection |
| MCP `get(id=<proposal_id>)`                                       | Fetch a single proposal's `ProposalCreated` payload       | Resolves to the event payload          |
| CLI `kkernel exec 'kg.proposal_cleanup(older_than="<duration>")'` | Archive resolved proposals (deferred — not shipped in v1) | Future operator housekeeping           |

`list(kind=proposal)` dispatches to a new `kg.list_proposals` handler under
the kg pack — it queries `proposals_open` directly, supports the standard
`status` / `proposer` / `namespace` filters, and returns in the verbose
canonical shape (ADR-045 trims for agent mode).

## Rationale

### Why one-approval default (not M-of-N)

v1 is small-team / single-agent typical. Requiring two approvers when there's
one reviewer in the room is friction without payoff. M-of-N is a policy
deployments enable when they need it — the threshold is a config knob, not a
v1 hardcoded rule.

### Why `allow_self_approve = true` is the default

The default deployment model is predominantly single-developer. Defaulting to
`allow_self_approve = false` would make the proposal flow unusable out-of-box
for solo developers — there is no second actor to approve. The safer posture
(`allow_self_approve = false`, `approval_threshold = 2`) is a deliberate
multi-actor deployment choice. Multi-actor deployments opt into stricter
defaults; single-developer deployments work without any config change.

### Why a projection table (and not just fold over events on every list)

ADR-041's rationale applies: projection-on-write is much cheaper at query time
than fold-on-read for any non-trivial event volume. A 10,000-proposal log
folded on every `list(kind=proposal, status="open")` call would be unusable;
the projection table makes it index-scan-fast.

### Why `withdraw` is its own verb (not `update`)

`update` in ADR-014 dispatches on `kind ∈ {entity, edge, note}`. Proposal
events are append-only (ADR-022) — they are NOT mutable substrate records. A
proposal event ID is not a valid target for ADR-014's `update` grammar.
Routing withdrawal through `update` would require ADR-014 to treat events as
mutable records, contradicting event immutability. `withdraw` is a dedicated
commissive verb that emits a NEW `ProposalWithdrawn` event — it does not
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

### Why no auto-apply on N approvals

Mentioned in #6: the rule is "the configured threshold." Auto-applying on
N=1 IS the default — there's no separate "auto" toggle. Operators who want
"require human approval, never auto-apply" set `approval_threshold` to a
sentinel and never set the relevant policy — applies are gated by policy,
not by a count.

### Why a closed `ProposalChangeset` enum

Open changesets ("here's a JSON object, just apply it") are unimplementable
safely — the apply step would have to interpret arbitrary JSON against pack
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
| PendingEdit substrate (option b)                                       | New substrate, new store, new VCS dimension — heavyweight for a workflow object                                                                                               |
| Git-native subset (option d)                                           | Re-imports the git assumption the decision explicitly avoided                                                                                                                 |
| Inline apply without a separate worker step or `ProposalApplied` event | Rejected: v1 may call `ProposalApplyWorker` synchronously from `review`, but apply remains a separate worker struct and emits `ProposalApplied` for audit/failure separation. |
| Approve = side-effect of `update(id=<proposal>, status=approved)`      | Conflates review (a typed decision) with record mutation; loses the review-history audit trail                                                                                |
| Per-proposer namespace for proposals                                   | Cross-cuts the namespace-isolation invariant; agents can't propose changes targeting namespaces they can read                                                                 |
| Open changeset format (JSON blob)                                      | Can't validate at proposal time; failure surfaces deep inside apply with poor error messages                                                                                  |

## Consequences

### Positive

- Proposal workflow lands without a new substrate, new store trait, or git
  dependency. Minimal incremental complexity.
- Proposal events preserve the signal needed for future proposer-quality and
  reviewer-agreement folds; v1 brain has no proposal-specific fold or automatic delivery.
- Cross-pack proposals work the same way as KG-pack proposals. Packs can query the four
  `EventKind`s explicitly; automatic pack consumption remains deferred with ADR-017.
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
- `ProposalChangeset` is a closed enum — every new change shape requires an
  amendment. This is the cost of validation-at-create-time.
- Operators must configure approval policy per-pack for multi-actor
  deployments; the default (`approval_threshold=1, allow_self_approve=true`)
  prioritizes single-developer ergonomics over review enforcement. Multi-actor
  deployments must explicitly tighten this.

### Neutral

- The shared `EventKind` enum gains four proposal kinds, but v1 brain has no
  proposal-specific fold and receives no automatic shared-log delivery.
- The three new verbs (`propose`, `review`, `withdraw`) bring the pack-kg
  verb count from 11 to 14. The verb surface stays well under the ADR-016
  single-tool `request` envelope's practical limits.

## Implementation

### Crate placement

- Verb handlers: `crates/khive-pack-kg/src/handlers.rs`
- Apply worker: `crates/khive-pack-kg/src/apply_worker.rs`
- Projection table + projection worker: `crates/khive-pack-kg/src/projection_worker.rs`
- Payload types: `khive-types::events::proposal_payloads`

### Migration

The `proposals_open` projection table was created in migration V15 in
`crates/khive-db/src/migrations.rs`. The `applying` projection status and its
CAS invariants were added in V18.

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
3. Create expression index: `CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'))` — backing the `EventFilter.payload_proposal_id` query extension from §4
4. Backfill is unnecessary — no prior proposals exist

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

All three entries have `visibility: Visibility::Verb` — they are externally
invokable by agents via the `request` DSL. Internal subhandlers (if any) would
use `Visibility::Subhandler`.

**ADR-023 amendment required (Cross-cut 1):** The kg pack handler table in
ADR-023 must be amended to add `propose`, `review`, `withdraw` — bringing the
pack-kg handler count from 11 to 14.

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
(`idx_events_payload_proposal_id`) as a bridge — the `proposal_id` field in
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

- Old khive ADR-075 (Agent-Driven PR Workflow) — original git-based design,
  superseded by this ADR
- ADR-014 (Curation Operations) — `merge_entities` and atomic compound updates
  consumed by the apply worker
- ADR-017 (Pack Standard) — handler declaration surface used by the KG pack; proposal `PackEventConsumer` registration remains deferred
- ADR-018 (Authorization Gate) — gates the apply step
- ADR-022 (Events Query Surface) — proposal events live as substrate events
- ADR-016 (Request DSL) — `propose`, `review`, and `withdraw` ride the standard
  single-tool `request` envelope
- ADR-032 (Brain Profile Orchestration) — future brain folds may extend to proposal events
  in future ADRs
- ADR-041 (Event Provenance Projection) — projection-table pattern this ADR
  reuses
- Design decision 2026-05-23: option (c) selected — "event sourced proposal
  sounds fine"

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
No dual-emit. Matches PR #109 (`note_id → id`) discipline.

**Unchanged permanently**: `ProposalCreatedPayload.proposal_id` struct field,
`proposals_open.proposal_id` DB column, `EventFilter.payload_proposal_id` filter field,
and all internal worker references.

## Amendment (2026-07-31): atomic success finalization (#1433)

Successful proposal application finalizes the projection and publishes the
`ProposalApplied { Success }` event through one `execute_batch` transaction. The
projection CAS runs first; the event INSERT is guarded by connection-local
`changes() = 1`. A false CAS therefore inserts no event, and any projection or
event-write error rolls back both statements. The already-committed KG changeset
is not part of this finalization transaction, but success never becomes externally
visible while `proposals_open` remains `applying`. Because this path bypasses the
`EventStore::append_event` seam, it increments ADR-103 `event_rows` exactly once
after and only after the two-row atomic batch succeeds.

## Amendment (2026-08-11): durable apply reconciliation boundary

`AtomicRunOutcome::Committed` is the proposal apply worker's irreversible
retry-safety boundary. After that outcome is observed, failure of deferred
reindexing or committed-created-record resolution MUST NOT flow through the
ordinary apply-failure branch: it emits no `ProposalApplied { Failed }`, does
not run the `applying -> approved` revert, and is not eligible for automatic
replay.

Instead, the worker logs a typed reconciliation stage
(`post_commit_reindex` or `created_record_resolution`) with `committed=true`
and `retryable=false`, and leaves `proposals_open.status='applying'`. This is a
deliberate durable reconciliation state, not evidence that base DML is still in
flight. The normal worker entry guard only claims `approved`, so another pass
cannot execute the changeset again. Operator reconciliation may repair the
derived index or recover the created-record identifiers and then perform the
existing atomic success finalization; it must never reapply the base mutation.

Failures before a committed outcome is observed retain the existing contract:
emit `ProposalApplied { Failed }` and best-effort revert to `approved`. A
rolled-back atomic unit is therefore retryable under the proposal workflow; a
committed unit with incomplete post-processing is not.

## Amendment (2026-08-16): edge-governance authority for memory supersession

**Depends on**: ADR-159 (edge-governance provenance — storage shape, stamping
paths, closure predicate). **Consumed by**: ADR-157 (supersession chain
canonicalization). Acceptance ordering is normative: ADR-159 must be Accepted
before this amendment, so every type, table, and predicate referenced below
has a merged normative definition at the time this text takes effect; ADR-157
consumes both and lands last. This amendment implements the two contracts
ADR-159 delegates to this ADR: endpoint-scoped reviewer authority on the
reviewed stamping path, and the in-place classification primitive for
migrating existing edges.

### A1. Governance-bearing changesets (definition)

A changeset step is **governance-bearing** when it is either:

- `AddEdge` with `relation = supersedes` whose source and target are both
  live notes of kind `memory` in the proposal's namespace and in the local
  store; or
- `ClassifyExistingEdgeGovernance` whose target edge, expected preimage, and
  both live endpoints all carry the proposal's namespace (A2).

**The classification recurses.** A `Compound` is governance-bearing iff any
step it contains is, evaluated to full nesting depth — the base ADR accepts
single-step `Compound` and applies it recursively, so a shallow top-level
match on the outer variant would let `Compound([AddEdge{supersedes, …}])`
bypass every rule in this amendment. Every propose-time and review-time
check in this amendment evaluates the recursive classification, never the
outer variant alone.

**Two memory-supersedes shapes are rejected at propose time** rather than
left to the plain link path:

- an `AddEdge` with `relation = supersedes` between memory notes in
  _different_ namespaces. Under ADR-159 such an edge would be created
  ungoverned and canonicalization-inert, so an approved proposal would
  produce an edge that silently does nothing — a proposer-facing lie. The
  proposal surface refuses it with a typed error; cross-namespace
  supersession has no proposal route.
- an `AddEdge` with `relation = supersedes` between memory notes where
  either endpoint is not in the local store. Governance is local-only
  (ADR-159 §2); the same silent-inert trap applies.

All other steps — including `SupersedeEntity` (entity-level supersession) and
`AddEdge` for any other relation or endpoint pair — are unaffected by this
amendment and keep the base ADR's review and apply semantics unchanged.

### A2. New changeset arm: `ClassifyExistingEdgeGovernance`

```rust
ClassifyExistingEdgeGovernance {
    edge_id:     Uuid,
    expected:    ExpectedEdgePreimage,   // namespace, source_id, target_id,
                                         // relation, target_backend,
                                         // deleted_at: must be null
    disposition: GovernanceDisposition,  // Authorize | Reject
    reason_code: Option<GovernanceReasonCode>, // closed enum, see below
}
```

The payload may request a classification and supply the expected preimage. It
may **not** name the authorizer, authority scope, review event, timestamp, or
policy revision — those are stamped by the review/apply runtime, never
deserialized from proposal JSON (ADR-159 §5). `deny_unknown_fields` applies;
a payload carrying any authority-shaped field is rejected at propose time.

`GovernanceReasonCode` is a closed enum, not free text. Initial variants:
`migration_authorized`, `migration_rejected_unowned`,
`migration_rejected_disputed`, `revocation`. An unknown code fails
deserialization at propose time; extending the vocabulary is an amendment to
this list, mirrored in ADR-159 §1's decision-row constraint.

**Namespace binding.** This arm is namespace-bound end to end, notwithstanding
the base substrate's namespace-agnostic by-ID reads (ADR-007 Rev 6): at
propose time, `expected.namespace` must equal the proposal's namespace or the
proposal is rejected with a typed error; at apply time, the storage primitive
additionally requires the live edge row's namespace AND both live endpoints'
namespaces to equal the proposal's namespace, failing the apply otherwise. A
proposal in one namespace can therefore never classify, authorize, or reject
an edge in another. This is a governance-plane restriction layered above the
by-ID contract, not a change to it.

Apply dispatches to the storage primitive
`classify_existing_edge_governance`, which in one write transaction: selects
the edge by UUID including current liveness; requires exact equality with the
full expected preimage (`target_backend` compared null-safe and required to
be NULL — governance is local-only per ADR-159 §2, and a cross-backend
target's liveness cannot be validated atomically in this store's write
transaction, ADR-029), `relation = 'supersedes'`, live memory-note
endpoints, and the namespace binding above; consumes and revalidates the
internal authority receipt for **both dispositions** — a rejection is an
authority claim about the edge exactly as an authorization is, and
`Reject` over a governed edge revokes live authority, so an unauthorized
or stale-provider path must not be able to revoke any more than to grant;
appends the governance decision; and reconciles the active projection with
the new disposition. Reconciliation is defined for both prior states, so a
classification can never leave a projection that contradicts the newest
decision: `Authorize` on an unclassified edge inserts the projection row;
`Authorize` on an already-governed edge fails the apply (re-authorizing live
authority is not a meaningful operation — revoke first); `Reject` on an
unclassified edge appends only the decision; `Reject` on an already-governed
edge deletes the active projection row in the same transaction and appends
an ADR-159 invalidation row (cause `revoked_by_decision`) referencing the
displaced decision, which is thereby spent — later reauthorization requires
a fresh decision per ADR-159 §8. It issues **no update to
`graph_edges`** — UUID, `created_at`, `updated_at`, weight, metadata, and
deletion state are preserved byte-for-byte. A stale UUID or preimage, a dead
or non-memory endpoint, or changed authority rolls back with no decision row
and no graph mutation, and the apply emits
`ProposalApplied { Failed { error: "stale: edge preimage changed since proposal" } }`
with the standard pre-commit revert to `approved`. The base ADR's
last-writer-wins stale-target default (§9, Fix 6) explicitly does **not**
apply to this arm: the preimage check is mandatory, not an optional
versioning knob.

`ClassifyExistingEdgeGovernance` is a single step; the multi-step `Compound`
restriction (PR #517) stands unchanged.

### A3. Reviewed governed apply for new supersession edges

When an approved proposal's governance-bearing `AddEdge` step reaches apply,
the worker maps it to the `governed_link` storage primitive — not the plain
`runtime.graph.link(...)` dispatch — passing the re-obtained,
binding-validated authority receipt (A4.3). `governed_link` commits the edge upsert, the governance
decision, and the active projection row in one writer transaction, and it
returns and stamps the actual incumbent edge UUID selected by the
natural-key upsert, so the decision row binds the edge incarnation that
really exists rather than the one the proposer imagined. If the incumbent
edge the natural-key upsert selects is **already actively governed** — a
row for it exists in the active projection — the apply FAILS with the same
typed stale-preimage contract as A2 and no row is written: `governed_link`
never appends a second active decision, never reactivates spent authority,
and never silently preserves the incumbent's authority under a new
decision's name. The existing authority must first be revoked through a
`ClassifyExistingEdgeGovernance` proposal with disposition `Reject` (A2's
reconciliation arm); only an unclassified incumbent is eligible. On any
receipt revalidation failure at apply time the transaction rolls back and
the apply fails with the same stale-preimage contract as A2.

A governance-bearing `AddEdge` in a proposal that is applied WITHOUT a valid
receipt on file (for example, a deployment that disabled the authority
provider between review and apply) must fail the apply; it must not degrade
to a plain ungoverned `link`. Degrading silently would make the proposal
path mint exactly the ungoverned-but-approved edges ADR-159 exists to
distinguish.

### A4. Endpoint-scoped reviewer authority

The base ADR's enforced reviewer test is non-self only:
`require_listed_reviewer` is explicitly deferred in the base ADR, so a
reviewer list on a proposal is today an unenforced annotation. This
amendment does not pretend otherwise, and it does not accept the pretense
either: **a governance-bearing proposal that names an explicit reviewer
list is rejected at propose time** with a typed error until listed-reviewer
enforcement lands in the base ADR — accepting a restriction nothing
enforces manufactures false assurance exactly where authority matters most.
Non-self establishes only that the reviewer is a _different_ actor, and
being different is not being entitled (ADR-159, Context defect 2). This
amendment adds, for governance-bearing changesets only:

1. **Authority is checked at the dispatch site, not in the handler**
   (ADR-127: handlers never authorize; the dispatch site is the sole
   enforcement point — this amendment preserves that invariant rather than
   amending it). The check is a **two-stage dispatch admission**, both
   stages running at the dispatch site before the handler. Stage 1 is the
   unmodified ADR-018 Gate over the raw request (actor, namespace, verb,
   args, context) — the Gate's existing contract is not widened and it
   receives no storage capability. Stage 2 is a governance admission step
   that runs only when the `review` verb carries `decision = Approve`; it
   is equipped with a **read-only proposal resolver** — a dispatch-site
   capability, not a Gate input and not a handler privilege — which loads
   the stored proposal named by the request's proposal id, classifies its
   changeset (A1), and, when governance-bearing, evaluates endpoint-scoped
   authority for action `memory.supersede` over each superseded memory
   named by the changeset (the `AddEdge` target, or the
   `expected.target_id` of a classification step). Denial refuses the
   dispatch with a typed error
   (`ReviewerNotAuthorized { proposal_id, actor_id }`) — parallel to
   `SelfApprovalForbidden`, before the handler runs and before any event
   lands. **Admission errors fail closed for this class**: ADR-129 already
   requires a stage-1 Gate infrastructure error to refuse dispatch, and a
   resolver failure, authority-provider error, or unavailable provider in
   stage 2 independently refuses exactly as a denial does. Neither failure
   path invokes the handler; ungoverned verbs inherit ADR-129's base Gate
   error posture. On allow, stage 2 issues the endpoint-scoped
   `AuthorityReceipt` and the dispatch site hands it to the handler
   in-process (item 2); the handler performs no authorization of its own.
2. The receipt is runtime-internal and non-serializable (ADR-159 §2), and
   it is **mechanically bound**: it carries the `proposal_id`, the
   `review_event_id` it authorizes, and a digest of the complete changeset
   it was evaluated against, alongside the action and authority subject.
   The event-id binding is resolved by **preallocation**: stage 2
   preallocates the `ProposalReviewed` event id before issuing the
   receipt, binds that id into the receipt, and the handler MUST commit
   the `ProposalReviewed` event under exactly that preallocated id in the
   same operation that records the review — the receipt is therefore never
   issued against an event id that the committed event does not carry, and
   a committed event id differing from the receipt's bound id fails the
   apply-time binding validation (item 3). Transport is an **in-process
   dispatch handoff**: the dispatch site passes the receipt to the handler
   invocation directly as a typed in-memory argument. It is NEVER carried
   through ADR-018's obligation interface — every obligation variant is
   serializable by construction and allow-decision obligations are copied
   into audit events, so an obligation-borne receipt would be serialized
   into the audit log, defeating non-serializability. It is never written
   into any event payload. What the event log carries is the unchanged
   `ProposalReviewed` event; the governance decision row carries
   `proposal_id` and the mandatory `review_event_id`, naming the reviewer
   as `authorizer` and the apply worker's identity (`system:propose-apply`)
   only as `applied_by`. Authority is never inferred from the apply
   worker's system identity.
3. At apply time the worker does not rely on any receipt merely found on
   file, and a receipt cannot be carried in memory across processes at all
   (it is non-serializable by construction): the applying process
   **re-obtains** the receipt through the same stage-2 admission seam —
   the authority provider, reached with the same read-only proposal
   resolver, with the same fail-closed error contract — scoped to
   the same action and subjects and bound to the applying proposal's
   `proposal_id`, `review_event_id`, and changeset digest, then validates
   that binding against the proposal being applied. A receipt whose binding
   names any other proposal, review event, or changeset digest never
   satisfies apply — cross-proposal reuse is structurally excluded, not
   merely discouraged. Revalidation covers the exact edge preimage and
   current authority; a bound policy/ownership revision that makes stale
   authority fail closed satisfies the currency half (ADR-159 §2, path 2)
   but never the binding half.
4. **Self-approval is unconditionally forbidden for governance-bearing
   changesets**, regardless of `allow_self_approve`. A proposer who holds
   the authority does not need the proposal path: the owner-bounded direct
   write (ADR-159 §2, path 1) is that actor's route. The reviewed path
   exists precisely for the cross-actor case, so it always requires a
   second actor.

Authority-provider modes follow ADR-159 §7. In **single-principal** mode the
provider attests the deployment's single authority; migration-era
classifications authorize `via = migration_review` under it. In
**multi-actor** mode a real endpoint-authority provider is required; a
deployment whose Gate can answer only "may review" must reject every
governance-bearing approval — fail closed, not fall back to the base ADR's
reviewer test.

### A5. What this amendment does not change

- Review, apply, threshold, and self-approve semantics for every
  non-governance-bearing changeset are byte-for-byte the base ADR's.
- The proposal event kinds, payload shapes (beyond the one new closed enum
  arm), projection table, `applying` CAS contract, and the 2026-07-31 /
  2026-08-11 amendment semantics are unchanged. `governed_link` and
  `classify_existing_edge_governance` are single write transactions, so the
  `AtomicRunOutcome::Committed` reconciliation boundary applies to them
  unmodified.
- No governance field is added to any wire result. Whether an edge is
  governed is observable only through ADR-159's serving behavior and
  diagnostics, not through the proposal API.

### A6. Verification (before dependent implementation merges)

- A propose-time test proving a `ClassifyExistingEdgeGovernance` payload
  carrying an authorizer-shaped field is rejected.
- A review-time pair: an authorized reviewer's approve lands; a
  provider-denied reviewer receives `ReviewerNotAuthorized` and no
  `ProposalReviewed` event exists afterward.
- A self-approve test proving `allow_self_approve = true` does not admit a
  governance-bearing approval.
- An apply-time stale-preimage test: mutate the target edge between approve
  and apply, assert `ProposalApplied { Failed }`, zero graph mutation, and
  byte-identical edge row.
- A no-degrade test: a governance-bearing `AddEdge` applied with the
  authority provider disabled fails; assert no ungoverned edge was created
  by the apply path.
- A byte-preservation test on `classify_existing_edge_governance`: all
  `graph_edges` columns identical before and after an `Authorize`.
- A multi-actor fail-closed test: with a review-only Gate, every
  governance-bearing approval is rejected.
- A namespace-binding pair: a classification whose `expected.namespace`
  differs from the proposal's namespace is rejected at propose time; a
  same-namespace proposal whose live edge or either endpoint turns out to
  carry a different namespace at apply fails the apply with zero mutation.
- A reviewer-list rejection test: a governance-bearing proposal naming an
  explicit reviewer list is rejected at propose time with the typed error.
- A cross-proposal receipt test: two approved proposals targeting the same
  memory; the receipt bound to proposal A's identity must not satisfy
  proposal B's apply.
- A revocation test: `Reject` applied over an actively governed edge
  removes the projection row, appends the `revoked_by_decision`
  invalidation row, and the displaced decision never reactivates on a
  projection rebuild.
- A seam test: with the Gate's authority check disabled, a
  governance-bearing approve must fail closed rather than fall through to
  a handler-side check — proving the handler carries none.
- A fail-closed admission pair: with the authority provider erroring
  (erroring, not denying), a governance-bearing approve is refused with a
  typed error and no `ProposalReviewed` event exists afterward; an
  ungoverned verb dispatched under the same provider error keeps its base
  behavior — proving the override is scoped to the governance class.
- An event-id binding test: the committed `ProposalReviewed` event id
  equals the receipt's preallocated bound id; a forced mismatch fails the
  apply-time binding validation with zero graph mutation.
- A transport test: the audit events and event payloads emitted by a
  governance-bearing approve contain no serialized receipt — asserted on
  the obligation/audit channel contents, proving the receipt rode the
  in-process handoff and nothing else.
- An incumbent-governed upsert test: a governance-bearing `AddEdge` whose
  natural-key upsert selects an actively governed incumbent fails the
  apply with the stale-preimage contract, appends no decision row, and
  leaves the incumbent's projection row untouched.
- A recursion pair on the A1 classification, two distinct proposals since
  the rejected shapes and the governance-bearing class are disjoint by
  A1: (i) a proposal whose only step is one of the propose-time rejection
  shapes nested at depth two or more — for example a cross-namespace
  memory-supersedes inside `Compound([Compound([AddEdge{…}])])` — is
  refused at propose time with the same typed error as its flat form;
  (ii) a proposal whose only governance-bearing step is nested at the
  same depth is classified governance-bearing, so a provider-denied
  reviewer's approve is refused exactly as for the flat form. Mutation
  control on the pair: a classifier restricted to the outer variant (any
  shallow, non-recursive evaluation) must redden both — the nested
  rejected shape is admitted at propose time, and the denied reviewer's
  approve lands on the nested governance-bearing proposal — proving the
  recursive evaluation of A1 is load-bearing rather than incidental.
