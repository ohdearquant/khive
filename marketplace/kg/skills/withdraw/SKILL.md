---
description: Withdraw an open knowledge graph proposal with a short rationale.
---

# Withdraw

Use `withdraw` when an open proposal is obsolete, duplicated, or should no longer be reviewed.

The MCP server exposes one tool, `request`, that takes the verb call as a string:

```text
request(ops="withdraw(proposal_id=\"00000000-0000-0000-0000-000000000001\", rationale=\"Superseded by a narrower proposal.\")")
```

Required args: `proposal_id`.
Optional args: `rationale`, `namespace`.

## When to withdraw

- The changeset was superseded by a better proposal
- Source evidence no longer supports the change
- The entities involved were merged or deleted before the proposal was reviewed
- The proposer identifies an error in the original changeset

## Workflow

### 1. Confirm the proposal is open

```text
request(ops="get(id=\"<proposal-id>\")")
```

Only open proposals can be withdrawn. Approved or rejected proposals are immutable records.

### 2. Withdraw with rationale

```text
request(ops="withdraw(proposal_id=\"<id>\", rationale=\"Duplicate of proposal <other-id> which is already approved.\")")
```

The `rationale` is stored with the proposal record so reviewers understand why it was closed without a decision.

## Stop condition

Proposal status is `withdrawn`. No further action needed. If you need a replacement change, open a new proposal via `propose`.
