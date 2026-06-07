---
description: Withdraw an open knowledge graph proposal with a short rationale.
---

# Withdraw

Use `withdraw` when an open proposal is obsolete, duplicated, or should no longer be reviewed.

The MCP server exposes one tool, `request`, that takes the verb call as a string:

```text
request(ops="withdraw(proposal_id=\"00000000-0000-0000-0000-000000000001\", rationale=\"Superseded by a narrower proposal.\")")
```

Required args: `proposal_id`. Optional args: `rationale`, `namespace`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`). Do NOT
override the namespace with `lambda:*` actor namespaces.

## When to withdraw

- The changeset was superseded by a better proposal
- Source evidence no longer supports the change
- The entities involved were merged or deleted before the proposal was reviewed
- The proposer identifies an error in the original changeset

## Workflow

### 1. Confirm the proposal is withdrawable

```text
request(ops="get(id=\"<proposal-id>\")")
```

`withdraw` is rejected when the proposal is already `applied`, `withdrawn`, `applying` (the apply
worker has claimed it and is executing), or `rejected`. Only proposals in `open`,
`changes_requested`, or `approved` state can be withdrawn.

### 2. Withdraw with rationale

```text
request(ops="withdraw(proposal_id=\"<id>\", rationale=\"Duplicate of proposal <other-id> which is already approved.\")")
```

The `rationale` is stored as `reason` on the `ProposalWithdrawn` event payload, not on the proposal
projection record. Retrieve it via event log replay — `get(id=<proposal-id>)` on the proposal will
not include the rationale.

## Stop condition

Proposal status is `withdrawn`. No further action needed. If you need a replacement change, open a
new proposal via `propose`.

## Known precheck/CAS divergence

The precheck at `crates/khive-pack-kg/src/handlers.rs:2852` blocks `applied | withdrawn | applying`
but does **not** block `rejected`. The CAS at `crates/khive-pack-kg/src/projection_worker.rs:632`
excludes `rejected` from the UPDATE condition, so a withdraw on a rejected proposal passes the
precheck but the CAS returns `false` to the handler — 0 rows updated, and because the event INSERT
is guarded by `changes() = 1`, no event is emitted either. The handler then converts the CAS miss
into an `InvalidInput` error (`handlers.rs:2889`), so callers receive an error response, not a
silent success. The precheck should be tightened to also block `rejected` so the two layers agree
and the error surfaces earlier. Track as a follow-up hardening item.
