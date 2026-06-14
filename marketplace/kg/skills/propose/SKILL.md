---
description: Draft event-sourced knowledge graph changes for proposal review before mutation.
---

# Propose

Use `propose` when a KG change should be reviewed before it is applied. The verb creates an open
proposal; it does not directly mutate entities, notes, or edges.

The MCP server exposes one tool, `request`, that takes the verb call as a string:

```text
request(ops="propose(title=\"Add implementation edge\", description=\"Project X implements concept Y based on reviewed source evidence.\", changeset={\"kind\":\"add_edge\",\"source\":\"00000000-0000-0000-0000-000000000001\",\"target\":\"00000000-0000-0000-0000-000000000002\",\"relation\":\"implements\",\"weight\":0.8}, reviewers=[\"critic\"])")
```

Required args: `title`, `description`, `changeset`. Optional args: `reviewers`, `expiry`,
`parent_id`, `namespace`.

Valid `changeset.kind` values: `add_entity`, `update_entity`, `add_edge`, `add_note`,
`merge_entities`, `supersede_entity`, `compound`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`), even when the
MCP server runs with `--actor lambda:myproject`. Do NOT override the namespace for entity/edge/note
operations. The knowledge graph is cross-project by design.

## Workflow

### 1. Draft the proposal

Identify the specific change and its rationale before calling `propose`. The description should cite
the source evidence that justifies the change.

```text
request(ops="propose(title=\"Add LoRA extends Attention edge\", description=\"LoRA is a parameter-efficient variant that extends the standard attention mechanism. Source: Hu et al. 2021 (LoRA paper, entity id: <paper-id>).\", changeset={\"kind\":\"add_edge\",\"source\":\"<lora-concept-id>\",\"target\":\"<attention-concept-id>\",\"relation\":\"extends\",\"weight\":0.9})")
```

### 2. Add reviewers (optional)

Pass `reviewers` as a list of agent names or identities who should assess the proposal. If omitted,
any agent with access may review.

```text
request(ops="propose(title=\"Merge duplicate FlashAttention entities\", description=\"Two entities represent the same concept: ids <a> and <b>. Keeping <a> — it has higher edge count and more complete properties.\", changeset={\"kind\":\"merge_entities\",\"into\":\"<a>\",\"from\":\"<b>\"}, reviewers=[\"polisher\", \"researcher\"])")
```

### 3. Check proposal status

`propose` returns `id`. Use `$prev.id` to chain:

```text
# propose returns: {"id": "<uuid>", "status": "open", "proposer": "...", "title": "..."}
request(ops="get(id=\"<id-from-response>\")")
request(ops="review(id=\"<id-from-response>\", decision=\"approve\")")
```

Or list open proposals:

```text
request(ops="list(kind=\"proposal\", status=\"open\")")
```

### 4. Amend after request_changes

If a reviewer calls `review(decision="request_changes")`, create a new proposal referencing the
original via `parent_id`. The runtime validates that the referenced `parent_id` exists and belongs
to the namespace before accepting the new proposal:

```text
request(ops="propose(title=\"Add LoRA extends Attention edge (revised)\", description=\"Corrected weight to 0.9 per reviewer feedback.\", changeset={\"kind\":\"add_edge\",\"source\":\"<lora-id>\",\"target\":\"<attention-id>\",\"relation\":\"extends\",\"weight\":0.9}, parent_id=\"<original-proposal-id>\")")
```

## Lifecycle

```
open ──► approved ──► applying ──► applied
  │
  ├──► changes_requested (reviewer requests_changes)
  ├──► rejected           (reviewer rejects)
  └──► withdrawn          (proposer withdraws)
```

`applying` is a transient in-flight state held by the apply worker. A `withdraw` call that arrives
while a proposal is `applying` will be rejected — the apply completes and the proposal moves to
`applied`.

## Stop condition

Proposal is open and assigned to reviewers. Do not apply the change manually — wait for `review`
approval. If the proposal is rejected, revise the `changeset` or description based on reviewer
feedback and re-propose with a `parent_id` referencing the rejected proposal.
