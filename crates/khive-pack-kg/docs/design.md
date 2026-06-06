# khive-pack-kg Design Notes

This document captures design rationale and invariant proofs that are too long to live inline
in source code. Source files reference this document with short pointers.

---

## Verb Visibility Audit (Issue #497)

All 16 KG handlers are `Visibility::Verb` (agent-callable via the MCP `request` DSL). There are
no `Visibility::Subhandler` entries in this pack. Rationale:

- The KG pack has no operator-only introspection or internal plumbing that needs to be hidden from
  the agent surface. Every verb is a first-class operation that agents are expected to invoke
  directly.
- `propose` / `review` / `withdraw` (ADR-046) are deliberate agent-facing verbs: proposals flow
  from agents who must be able to call all three steps. If a future requirement introduces an
  operator-only "admin_apply" or "force_reject" escape hatch it should be added as
  `Visibility::Subhandler` at that point.
- The `verbs` introspection verb is agent-callable by design: it implements the self-describing
  verb catalog (ue-help-introspection H5) and explicitly filters out `Subhandler` entries before
  responding, so adding any future `Subhandler` entries here would automatically hide them from
  that output.

Packs with confirmed `Subhandler` entries as of this audit:

- `pack-memory`: `recall_embed`, `recall_candidates`, `recall_fuse`, `recall_rerank`,
  `recall_score`
- `pack-brain`: `brain.state`, `brain.config`, `brain.events`, `brain.emit` (deprecated)

*Source pointer*: `src/lib.rs` — the comment block above `static KG_HANDLERS` summarises this.

---

## Proposal Projection CAS / Event-Insert Proof (`reviewed_and_emit`)

*Source pointer*: `src/projection_worker.rs` — `ProposalsProjectionWorker::reviewed_and_emit`.

`reviewed_and_emit` atomically runs the CAS UPDATE on `proposals_open` and a conditional
`ProposalReviewed` event INSERT in a single `BEGIN IMMEDIATE` / `COMMIT` transaction via
`execute_batch`.

### CAS guard: `changes() = 1`

`changes()` returns the row count from the immediately-preceding statement on the same connection.
Since `execute_batch` runs both statements on the same connection with no intervening operations,
`changes()` at INSERT time is exactly the UPDATE's row count.

- If the UPDATE matched 1 row (this connection won the CAS): `changes() = 1` is true → the INSERT
  runs.
- If the UPDATE matched 0 rows (CAS lost): `changes() = 0` → the INSERT is skipped.

This replaces the round-3 `updated_at = <now>` subquery guard, which was unsafe under
same-microsecond concurrent calls: two callers can compute identical `now` values before either
holds the writer lock, so the loser's guard could match the winner's committed `updated_at` and
insert a duplicate event. `changes()` is connection-local and requires no timestamp uniqueness
assumption.

Return value: `Ok((cas_hit, event_id))`.

- `cas_hit = true`: projection row was updated AND the event was inserted (total_rows == 2 for
  state-changing decisions).
- `cas_hit = false`: no projection update, no event written.
- For `Comment` decisions (no state change), `cas_hit` is always true.
