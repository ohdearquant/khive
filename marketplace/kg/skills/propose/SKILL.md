---
description: Draft event-sourced knowledge graph changes for proposal review before mutation.
---

# Propose

Use `propose` when a KG change should be reviewed before it is applied. The verb creates an open proposal; it does not directly mutate entities, notes, or edges.

The MCP server exposes one tool, `request`, that takes the verb call as a string:

```text
request(ops="propose(title=\"Add implementation edge\", description=\"Project X implements concept Y based on reviewed source evidence.\", changeset={\"kind\":\"add_edge\",\"source\":\"00000000-0000-0000-0000-000000000001\",\"target\":\"00000000-0000-0000-0000-000000000002\",\"relation\":\"implements\",\"weight\":0.8}, reviewers=[\"critic\"])")
```

Required args: `title`, `description`, `changeset`.
Optional args: `reviewers`, `expiry`, `parent_id`, `namespace`.

Valid `changeset.kind` values: `add_entity`, `update_entity`, `add_edge`, `add_note`, `merge_entities`, `supersede_entity`, `compound`.

## Workflow

### 1. Draft the proposal

Identify the specific change and its rationale before calling `propose`. The description should cite the source evidence that justifies the change.

```text
request(ops="propose(title=\"Add LoRA extends Attention edge\", description=\"LoRA is a parameter-efficient variant that extends the standard attention mechanism. Source: Hu et al. 2021 (LoRA paper, entity id: <paper-id>).\", changeset={\"kind\":\"add_edge\",\"source\":\"<lora-concept-id>\",\"target\":\"<attention-concept-id>\",\"relation\":\"extends\",\"weight\":0.9})")
```

### 2. Add reviewers (optional)

Pass `reviewers` as a list of agent names or identities who should assess the proposal. If omitted, any agent with access may review.

```text
request(ops="propose(title=\"Merge duplicate FlashAttention entities\", description=\"Two entities represent the same concept: ids <a> and <b>. Keeping <a> — it has higher edge count and more complete properties.\", changeset={\"kind\":\"merge_entities\",\"into_id\":\"<a>\",\"from_id\":\"<b>\"}, reviewers=[\"polisher\", \"researcher\"])")
```

### 3. Check proposal status

After submission, retrieve the proposal with `get`:

```text
request(ops="get(id=\"<proposal-id>\")")
```

Or list open proposals:

```text
request(ops="list(kind=\"proposal\", status=\"open\")")
```

## Stop condition

Proposal is open and assigned to reviewers. Do not apply the change manually — wait for `review` approval. If the proposal is rejected, revise the `changeset` or description based on reviewer feedback and re-propose.
