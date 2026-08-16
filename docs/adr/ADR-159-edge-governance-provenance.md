# ADR-159: Durable Edge-Governance Provenance for Supersession Canonicalization

**Status**: Proposed
**Date**: 2026-08-15
**Depends on**: ADR-002 (edge ontology), ADR-007 (namespace), ADR-013 (note kinds), ADR-014 (curation), ADR-017 (pack standard), ADR-018 (authorization gate), ADR-039 (note merge), ADR-046 (event-sourced proposals), ADR-055 (epistemic edges)
**Consumed by**: ADR-157 (supersession chain canonicalization, in flight), a forthcoming ADR-046 amendment

---

## Context

Memory-recall chain-head canonicalization (ADR-157) needs a class of GOVERNED
`supersedes` edges: only an edge whose creation was authorized — by the owner
of the superseded memory, or through a reviewed cross-actor proposal — may
substitute one memory note for another in recall output. An ungoverned edge
must remain fully visible to every generic graph surface while being inert
for canonicalization.

An earlier draft encoded governance as a runtime-stamped JSON metadata key
with generic write surfaces refusing caller-supplied values. Analysis of that
draft established five defect classes that any replacement mechanism must
discharge:

1. **Stamp survival across identity mutations.** Governance stamps survived
   generic relinks, note-merge endpoint rewrites, and natural-key
   resurrection without reauthorization — an authorized merge or relink
   caller could make a governed edge control a _different_ target, and an
   undelete could restore authority.
2. **Reviewer difference is not authority.** ADR-046 permits any qualified
   non-self reviewer, so a reviewer with no authority over the superseded
   target could approve an attacker's supersession. The mechanism must
   record _which_ authority stamped, so the ADR-046 amendment can require
   endpoint-scoped authority and carry it through the apply path.
3. **Pre-activation forgery.** A caller-shaped stamp written before
   reserved-key enforcement activates is not durably distinguishable from a
   runtime-produced one. Governance needs a durable marker outside any
   caller-writable surface, and closure must validate it.
4. **No in-place migration primitive.** ADR-046's `AddEdge` changeset op
   carries no edge UUID, preimage, or metadata and applies through generic
   link — it cannot stamp an _existing_ edge in place while preserving its
   UUID, `created_at`, and metadata.
5. **No write fence.** Between migration inventory capture and serving
   cutover, an edge created in the interval would be effective before
   activation, absent from the migration set, and inert after — with no
   disposition record.

### Why metadata-resident governance cannot be rehabilitated

`link` accepts caller metadata and `update(edge)` replaces the same open
JSON through `properties`
(`crates/khive-pack-kg/src/handlers/params.rs:175-200`,
`crates/khive-pack-kg/src/handlers/update.rs:213-220`); the table stores
that JSON in `graph_edges.metadata` (`crates/khive-db/sql/graph-ddl.sql`).
Reserving a key after activation prevents future well-behaved callers from
writing it; it cannot distinguish a byte-identical value written before
enforcement from one emitted by the runtime. Binding such a key to
endpoints prevents authority _transfer_ but does not disprove the original
forgery at those same endpoints. Migration must therefore ignore all
pre-existing metadata governance shapes.

### Why edge UUID is not governance identity

The edge natural key is `(namespace, source_id, target_id, relation)`. A
same-direction re-link revives a soft-deleted row with the same UUID and
`created_at` (ADR-002 §edge upsert). The store's UUID-conflict arm rewrites
`source_id`, `target_id`, and `relation` in place, while the natural-key arm
clears `deleted_at` and preserves the incumbent UUID
(`crates/khive-db/src/stores/graph.rs`). Entity and note merge both update
an incident edge's endpoints in place and retain its ID when no natural-key
conflict exists (`crates/khive-runtime/src/curation.rs`; ADR-039). A UUID
alone therefore names a _slot_ whose meaning mutates. Governance must bind
to the full current incarnation — `(namespace, edge_id, source_id,
target_id, relation, liveness)` — and must never transfer from a dropped
edge to a merge survivor.

### Why the event plane cannot be the governance root

Successful singleton `link` dispatches attempt an audit row carrying edge
ID, endpoints, relation, and caller actor, but ordinary audit append is
explicitly best-effort and non-fatal (`crates/khive-runtime/src/pack.rs`),
coverage is heterogeneous across singleton, bulk, proposal, merge, and
resurrection paths, and no retention guarantee exists (ADR-094). Event
reconstruction could only become the serving root by making audit writes
strict, transactional with every edge mutation, complete, and permanently
retained — at which point it is a more expensive form of the decision table
this ADR specifies.

### Measured baseline

Two measurements on a production store (2026-08-15) size the migration and
settle the activation posture:

- **Census**: 625 `supersedes` edges store-wide (edge listing cross-checked
  against aggregate stats; both surfaces agree), all live, of which **234**
  are memory-to-memory (typed graph match, single page, no truncation).
  Instrument scope: soft-deleted edges are invisible to both surfaces used,
  so the census counts the live population — which is the population the
  migration classifies.
- **Authority coverage**: a 200-note bounded sample of memory notes found
  198 attributed to the shared namespace and 2 to an actor namespace (99%
  anonymous), zero owner-shaped property keys, and 101 of 200 carrying
  caller-writable role tags. There is no durable, policy-usable owner
  signal on the current memory population.

The second measurement is decisive for activation: a deployment whose
memories carry no owner signal cannot claim cross-actor governed
canonicalization. Single-principal mode is the honest initial posture, and
multi-actor activation must fail closed until a real authority provider
exists (§ Activation gate).

---

## Decision

Governance is stored **beside** the graph, in caller-unreachable tables,
bound to the exact edge incarnation, invalidated at the database mutation
seam, with an append-only decision history and a serving projection recall
can join cheaply.

### 1. Storage shape

Four objects, added by versioned migration (ADR-015):

**`edge_governance_decisions`** — append-only record of every authorization
or rejection:

- `decision_id` (PK), `edge_id`;
- bound preimage: `edge_namespace`, `source_id`, `target_id`, `relation`
  (constrained to `supersedes`), `target_backend`;
- `disposition`: `authorized` | `rejected`;
- `via`: `owner_direct` | `reviewed_proposal` | `migration_review` |
  `cutover_fence`;
- authorizer identity (actor kind + id);
- `applied_by` (nullable): the apply worker's system identity on reviewed
  paths; never an authority source;
- authority evidence: action, authority subject (at minimum the superseded
  memory), policy/provider id and revision;
- optional `proposal_id`, mandatory `review_event_id` for reviewed paths;
- `decided_at` and a bounded reason code (closed vocabulary declared by the
  implementing migration; unknown codes are rejected at insert).

**`edge_governance_invalidations`** — append-only ledger of every
invalidation event, written by the database triggers that delete active
markers (§3) and by the one governance-plane writer, the classification
primitive's revocation arm (ADR-046 amendment):

- `invalidation_id` (PK), `decision_id` (references the decision whose
  marker was removed), `edge_id`;
- `cause`: the invalidating event class (endpoint/relation/backend
  rewrite, merge rewire, edge liveness transition, endpoint-note liveness
  transition, hard delete, `revoked_by_decision`);
- `invalidated_at`.

An invalidated decision is permanently spent: no rebuild, migration, or
reauthorization path may treat a decision referenced by an invalidation
row as live authority. Fresh authority requires a fresh decision. This
ledger is what makes marker deletion durable — without it, a projection
rebuild from the append-only decision log could re-mint authority for an
edge whose preimage has recurred (same-key resurrection restores the same
UUID and endpoint tuple, so preimage equality alone cannot distinguish the
original incarnation from a revived one).

**`edge_governance_active`** — one row per currently governed edge, the
serving projection:

- `edge_id` (PK), `decision_id` (unique);
- the same bound namespace/source/target/relation/target_backend;
- `stamped_at`;
- covering index beginning with `target_id`, e.g.
  `(target_id, source_id, edge_id)`.

The projection deliberately repeats the bound preimage so the serving join
fails closed even if an invalidation path regresses: a mismatch between the
projection's bound columns and the live edge row disqualifies the edge
regardless of whether the marker was deleted.

**`edge_governance_state`** — a singleton activation receipt:
`schema_version`, `status` (`inactive` | `active`), `activation_epoch`,
activation timestamp, inventory counts, and the authority-provider
mode/version used for migration. It is a serving gate and migration
receipt, never an edge authority source.

None of these fields is deserialized from `link`, `update`, `create`, or
proposal payloads. The public edge `metadata` column is unchanged and
irrelevant to governance. The design claims unforgeability against verb/API
callers, not against an operator who can rewrite the SQLite file — that is
a different threat model (§ Risks).

### 2. Stamping paths

Exactly two semantic paths may create an active marker:

1. **Owner-bounded direct write.** The Gate/authority provider returns an
   internal, non-serializable `AuthorityReceipt` for action
   `memory.supersede` over the target memory and bound endpoints. The
   runtime edge upsert, decision insert, and active-projection insert
   commit in **one** writer transaction. An ordinary allowed `link` without
   a receipt still creates a graph edge — ADR-017 forbids a pack from
   tightening the base endpoint contract — but the edge is ungoverned and
   canonicalization-inert.
2. **Reviewed cross-actor proposal.** ADR-046 (as amended) supplies the
   approving review's endpoint-scoped `AuthorityReceipt`. The decision
   record names the reviewer as `authorizer` and the apply worker's system
   identity only as `applied_by`. Review/apply must revalidate the exact
   edge preimage and current authority at apply time, or bind a
   policy/ownership revision that makes stale authority fail.

The receipt is runtime-internal, cannot be constructed from request JSON,
and binds authorizer, action, authority subject and scope, source, target,
relation, policy/provider version, and — when reviewed — proposal and
review event. It contains no key or secret.

### 3. Invalidation rules

| Mutation                                                               | Active governance result                                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| New generic edge                                                       | No marker; edge visible, canonicalization-inert.                                                                                                                                                                                                                                                                                                                                  |
| UUID upsert changes namespace, endpoint, relation, or `target_backend` | Trigger deletes the active marker and appends an invalidation row; bound-column join also mismatches.                                                                                                                                                                                                                                                                             |
| Natural-key upsert rewrites `target_backend`                           | Trigger deletes the marker and appends an invalidation row. The store's natural-key conflict arm sets `target_backend = excluded.target_backend` (`crates/khive-db/src/stores/graph.rs`), so backend routing is reachable through generic upsert and must invalidate.                                                                                                             |
| Entity/note merge rewires an endpoint                                  | Trigger deletes the marker on the moved edge and appends an invalidation row. No decision transfers.                                                                                                                                                                                                                                                                              |
| Merge natural-key conflict deletes the incoming edge                   | Incoming marker deleted with an invalidation row; the survivor keeps only its own prior marker, if any.                                                                                                                                                                                                                                                                           |
| Edge `deleted_at` transition (either direction)                        | Trigger deletes the marker and appends an invalidation row.                                                                                                                                                                                                                                                                                                                       |
| Same-key resurrection                                                  | The `deleted_at` transition already deleted any stale marker and recorded a durable invalidation; only a fresh governed transaction may re-stamp after resurrection. The ledger row is load-bearing here: the revived row carries the original UUID and preimage, so without it a later projection rebuild could not tell the incarnations apart.                                 |
| Endpoint note `deleted_at` transition (either direction)               | Trigger deletes the marker on every incident governed edge and appends invalidation rows. Note soft-delete leaves edges in place (ADR-014), and the note upsert can restore the same note ID with `deleted_at = NULL` and wholly new content (`crates/khive-db/src/stores/note.rs`); authority granted over a memory does not survive that memory's disappearance or replacement. |
| Hard delete / cascade                                                  | Trigger deletes the marker and appends an invalidation row; decision history remains.                                                                                                                                                                                                                                                                                             |
| Generic update of weight or metadata                                   | Marker retained — neither field enters the closure predicate. If a future serving ADR makes either field semantic, it must first add that field to the binding and the invalidation trigger.                                                                                                                                                                                      |

Invalidation is installed as SQLite triggers — `AFTER UPDATE OF namespace,
source_id, target_id, relation, target_backend, deleted_at` on
`graph_edges`, `AFTER UPDATE OF deleted_at` on `notes` scoped to notes
with incident governed edges, plus delete triggers — at the database
layer, because merge rewires include raw `UPDATE graph_edges` statements
that bypass handler code (`crates/khive-runtime/src/curation.rs`).
Handler-only clearing is insufficient by construction. Each trigger
performs two writes in the same statement-level transaction: delete the
marker, append the invalidation row referencing the marker's
`decision_id`. `INSERT OR REPLACE` on `graph_edges` may not become a
supported writer shortcut; every insert/upsert form must have an explicit
resurrection test before activation.

**Relationship to ADR-014.** ADR-014's public contract stands unchanged:
`delete(kind="edge")` is always hard, and edges have no soft-delete state
on the verb surface. The `graph_edges` schema nonetheless carries a
`deleted_at` column, and the natural-key upsert arm clears it
(`deleted_at = NULL` in its conflict set), so the revived-row state is
reachable at the store layer regardless of which paths write the column
today. This ADR binds invalidation to column transitions, not to any
verb: if no current path soft-deletes an edge, the edge-liveness triggers
are dormant; if a future path does, invalidation already covers it.
Nothing here introduces, requires, or permits edge soft-delete semantics
on the public surface, and this ADR does not amend ADR-014.

### 4. Closure predicate (consumed by ADR-157)

ADR-157's chain traversal may treat an edge as substitutive only when all
of the following hold in the same read snapshot:

```text
edge_governance_state.status == 'active'
AND graph_edge is live
AND graph_edge.relation == 'supersedes'
AND both endpoints are live memory notes
AND edge_governance_active.edge_id == graph_edge.id
AND active.bound namespace/source/target/relation/target_backend
    == current edge preimage
```

Expansion starts from `edge_governance_active.target_id` via the covering
index and joins `graph_edges` by its unique, indexed ID — one indexed
lookup plus one unique join per hop, no JSON parsing, no per-edge
cryptography on the recall hot path. Governance changes only the
_admissible edge set_; chain direction and cycle/branch/head rules remain
ADR-157's (per ADR-013: `new --supersedes--> old`, traversal toward the
head follows incoming edges).

### 5. In-place migration primitive (contract for the ADR-046 amendment)

ADR-046 must add a distinct, closed changeset operation:

```text
ClassifyExistingEdgeGovernance {
  edge_id,
  expected: { namespace, source_id, target_id, relation, target_backend,
              deleted_at: null },
  disposition: authorize | reject,
  reason_code?
}
```

The payload may request a classification and supply a preimage; it may
**not** name the authorizer, authority scope, review event, timestamp, or
policy revision — those come from the review/apply runtime.

The storage primitive `classify_existing_edge_governance` must, in one
transaction: (1) select the edge by UUID including current liveness;
(2) require exact equality with the full expected preimage,
`relation = 'supersedes'`, and live memory-note endpoints; (3) consume and
revalidate the internal authority receipt for an `authorize` disposition;
(4) append the decision; (5) insert the active projection only for
`authorize`. It issues **no update to `graph_edges`** — UUID, `created_at`,
`updated_at`, weight, metadata, and deletion state are preserved
byte-for-byte. A stale UUID/preimage or changed authority rolls back with
no decision and no graph mutation.

For a _newly proposed_ memory supersession, the amendment maps its reviewed
edge operation to a `governed_link` primitive only after receiving the
authority receipt; that primitive must return and stamp the actual
incumbent UUID selected by the natural-key upsert. Authority is never
inferred from the apply worker's system identity.

### 6. Write fence and cutover

This ADR owns cutover, because it owns the sidecar schema, the invalidation
triggers, and the activation receipt; ADR-157 only consumes the state. The
fence is a short single-writer transaction, not a process pause, a
long-held lock, or an epoch column:

1. Deploy schema, triggers, and governed write paths with state `inactive`.
2. Inventory and review legacy live memory-to-memory `supersedes` edges
   (measured population: 234) without holding a write lock.
3. Enter one `BEGIN IMMEDIATE` transaction through the normal single-writer
   owner.
4. Re-scan for live legacy edges without a matching decision. Record each
   late arrival as a decision with `disposition = rejected`,
   `via = cutover_fence`, reason code `late_arrival_unreviewed`, authorizer
   = the migration's system identity acting under the activation mode's
   authority (single-principal at this deployment) — or abort if the delta
   exceeds a configured bound. Like every rejection, this is reversible
   only by explicit later reauthorization (§8), never by rerunning the
   fence.
5. Verify counts plus trigger/schema identity against the trigger manifest
   (§7 — names and normalized-SQL digests, not mere table existence), set
   state `active`, commit.

File-backed khive uses WAL with one writer and concurrent readers, and
writer transactions use `BEGIN IMMEDIATE` (`crates/khive-db/src/pool.rs`,
`crates/khive-db/src/writer_task.rs`). A write committed before the fence
is present in the final delta; a write admitted after executes under the
installed post-cutover rules; a recall reader on an older WAL snapshot sees
the wholly inactive regime. No write can land in the inventory/cutover gap
unnoticed.

### 7. Activation gate and single-principal mode

`edge_governance_state` records the authority-provider mode used at
migration. Two modes exist:

- **single-principal**: the deployment attests that one principal owns all
  memory writes. Legacy-edge classification may authorize via
  `migration_review` under that single authority. This is the initial mode
  for the measured deployment (99% anonymous namespace, no owner signal).
- **multi-actor**: requires a real endpoint-authority provider. A
  deployment whose Gate can answer only "may review" or "is a different
  actor" **must not** activate in this mode — activation fails closed.

Mode is part of the activation receipt; upgrading single-principal →
multi-actor is a new migration with its own review, not a flag flip.

The activation receipt carries a **trigger manifest**: for every
invalidation trigger this ADR requires, its exact name and a digest of its
normalized SQL. Normalization is defined as: take the trigger's `sql` text
from `sqlite_master`, collapse every run of whitespace to a single space,
trim, and digest the resulting bytes (SHA-256). The expected manifest is
authored in the same versioned migration that installs the triggers, so
`schema_version` names exactly one expected manifest and the comparison
has an immutable reference on both sides.

At every boot while `edge_governance_state.status` is `active`, the
runtime MUST recompute the manifest from `sqlite_master` and compare it to
the activation receipt: a missing trigger, an unexpected name, or a digest
mismatch demotes the status to `inactive` — keeping ADR-157
canonicalization off — until a new activation review restores it. This
check is normative, not advisory.

### 8. Backward compatibility

Rejected legacy edges remain visible through `get`, `list`, `neighbors`,
`traverse`, and `query`. They are excluded only from ADR-157
canonicalization, each with a durable `rejected` decision naming the exact
preimage and reason. No graph history is soft-deleted or mutated to repair
a view (ADR-013 data/view split). A later authorized review may create a
new decision and active marker for the unchanged current preimage — that
is explicit reauthorization, not resurrection of the rejection.
Pre-activation metadata stamps remain inert and are not scrubbed.

---

## Alternatives considered

| Fork                                                                           | Outcome              | Decisive result                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------------------------------------------ | -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Metadata / reserved key (with or without endpoint binding)                     | Rejected             | Pre-activation caller bytes are indistinguishable from runtime bytes; endpoint binding does not cure original-tuple forgery.                                                                                                                                                                                                                  |
| Dedicated columns on `graph_edges`                                             | Rejected (converges) | Non-caller-writable columns solve forgery, but bare columns survive raw merge rewires and resurrection unless DB-layer invalidation is added, and immutable rejection/authority history needs a second table anyway — at which point it is this design with a worse schema.                                                                   |
| UUID-keyed sidecar (no preimage binding, no liveness triggers)                 | Rejected             | Same-key resurrection preserves UUID and `created_at`; the surviving sidecar row silently reactivates authority. Failure trace: authorize edge → soft-delete → generic re-link revives the row with the same UUID → the UUID-only sidecar joins again.                                                                                        |
| Event-derived governance                                                       | Rejected             | Link audit is best-effort, heterogeneous across write paths, and unpruned-today is not a retention guarantee; it cannot be the synchronous closure root.                                                                                                                                                                                      |
| HMAC over edge identity with a runtime-held key                                | Rejected             | Defeats pre-activation forgery only with an external key, and same-key resurrection only with a non-reusable incarnation input; adds key storage outside the store, rotation, and per-read verification. Caching a verified result into an indexed marker converges on the selected design without improving the caller-surface threat model. |
| **Bound sidecar projection + decision log + invalidation triggers (selected)** | **Selected**         | Non-caller-writable, preimage-bound, resurrection-safe, immutable authority/rejection history, one indexed join at recall, no secrets, no hot-path crypto.                                                                                                                                                                                    |

## Rationale

The selected design separates three concerns the metadata design conflated:
graph edges record _assertions_ and stay usable by every generic surface;
governance decisions record _who authorized or rejected one exact
incarnation and why_; the invalidation ledger records _which decisions
have been spent_; the active projection is a cheap serving index,
disposable and rebuildable from authorized decisions that have no
invalidation row, plus current edge state. Failure modes are explicit and fail closed: a missing marker means
no substitution; endpoint mutation cannot carry authority (trigger deletion
and bound-column equality both reject it); rejected rows stay auditable
without corrupting graph history; authority policy stays at the
Gate/ADR-046 boundary where it belongs.

## Risks

- **The authority oracle is the gating risk.** Note rows have no durable
  owner column, namespace is attribution rather than ownership (ADR-007),
  and the default Gate is permissive. This mechanism proves an internal
  path stamped an unchanged edge; it cannot by itself prove the named
  authorizer was entitled to stamp. The ADR-046 amendment must supply the
  endpoint-scoped authority receipt from a real policy source, and
  single-principal mode is the only honest posture until it does.
- **Trigger drift is safety-critical.** Same-tuple resurrection safety
  relies on the liveness trigger. Activation verifies trigger names and
  normalized SQL, and a boot/activation check that finds a dropped or
  altered trigger keeps ADR-157 inactive.
- **Fail-closed canonicalization can surface stale material.** Invalidated
  governance means recall stops substituting through that edge — safer
  than substituting on unauthorized authority, but the degradation must be
  observable, not silent.
- **The sidecar is not tamper-proof against a database owner.** Hostile
  local file mutation is out of scope; if it enters scope, revisit the
  HMAC fork with an external key and rotation protocol.

## Measurements required before acceptance

1. **Closure cost**: `EXPLAIN QUERY PLAN` plus p50/p95 latency at
   realistic governed-chain depth; accept only an indexed plan with no
   JSON scan.
2. **Trigger overhead**: singleton/bulk link, edge update/delete, and
   merge throughput plus writer-queue p95 with triggers installed.
3. **Cutover delta**: rehearse inventory-to-fence delay under
   representative write load; set the abort bound from the measured late
   rows, not intuition.

(The migration census and authority-coverage measurements are complete and
recorded in § Context.)

## Verification

- Regression matrix: UUID endpoint rewrite, same-key resurrection, entity
  merge, note merge, merge-conflict survivor, soft/hard delete, generic
  metadata forgery, generic relink, governed reauthorization,
  `target_backend` rewrite through both upsert arms, endpoint-note
  soft-delete and same-ID restore.
- Rebuild test: authorize → invalidate (any cause) → restore the original
  preimage via same-key resurrection → rebuild the projection; the spent
  decision must NOT reactivate, and a fresh governed transaction must.
- Security tests: a non-self reviewer without target authority — neither
  review approval nor the apply worker's system identity may produce an
  active marker.
- In-place migration test: the complete `graph_edges` row is byte-identical
  before and after authorization.
- Cutover race test: one writer committed before `BEGIN IMMEDIATE`, one
  blocked behind it, recall readers on both WAL snapshots; no edge may be
  active without a decision or silently cross regimes.
- Activation test: dropping or altering an invalidation trigger keeps
  canonicalization inactive.
- Query-plan and latency gates from the measurements above.

## Implementation fences

**MAY**: add versioned DDL for the four objects, indexes, and triggers;
introduce internal non-serializable `AuthorityReceipt`, `governed_link`,
and `classify_existing_edge_governance`; keep ungoverned edges visible but
inert; rebuild the active projection only from authorized decisions that
no invalidation row references, and only after rechecking current
preimages and liveness — an invalidated decision is spent forever, and a
recurring preimage (same-key resurrection) never revives it.

**MAY NOT**: read `metadata.created_by_actor` or any edge JSON as
governance evidence; accept caller-supplied authorizer, scope, policy
revision, review event, or stamp timestamp; infer endpoint authority from
"not proposer", edge namespace, note namespace alone, or a permissive Gate
in a multi-actor deployment; carry, copy, or merge an active marker across
endpoint/relation mutation, delete, resurrection, or natural-key conflict;
recreate an edge to migrate it, or change UUID/`created_at`/metadata while
stamping it; derive the serving predicate from best-effort audit events;
rebuild or backfill the projection from any decision an invalidation row
references; perform per-edge HMAC verification on recall; activate
canonicalization before schema/trigger verification and final-delta
classification commit atomically.
