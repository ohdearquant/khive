---
description: Register a concept entity with optional domain and tags.
---

# Learn

Register a named concept in the knowledge graph. The concept is stored as an entity with
`kind = "concept"`. Domain is stored in `properties.domain` and also promoted to the entity's tags,
making it discoverable by both `topic` and the kg `search` verb.

## Usage

```
request(ops="knowledge.learn(name=\"LoRA\", domain=\"fine-tuning\", tags=[\"adapter\", \"peft\"])")
```

## Parameters

| Parameter     | Type            | Required | Description                                                               |
| ------------- | --------------- | -------- | ------------------------------------------------------------------------- |
| `name`        | string          | yes      | Canonical short name for the concept (e.g. `"LoRA"`, `"FlashAttention"`). |
| `description` | string          | no       | Free-text description. Included in FTS index.                             |
| `domain`      | string          | no       | Research domain (e.g. `"attention"`, `"fine-tuning"`, `"inference"`).     |
| `tags`        | list of strings | no       | Additional classification tags.                                           |

## Response shape

```json
{
  "id": "a1b2c3d4",
  "full_id": "a1b2c3d4-e5f6-...",
  "kind": "concept",
  "name": "LoRA",
  "description": "Low-Rank Adaptation of large language models",
  "domain": "fine-tuning",
  "tags": ["fine-tuning", "adapter", "peft"],
  "namespace": "default"
}
```

## Patterns

### Register a paper as a concept

Papers are best stored as `document` entities (use the kg `create` verb). Use `learn` for algorithms
and techniques, then `cite` to link them to their introducing paper.

### Batch registration

```
request(ops="[knowledge.learn(name=\"GQA\", domain=\"attention\"), knowledge.learn(name=\"MLA\", domain=\"attention\")]")
```

## Anti-patterns

- **Using `learn` for papers.** Papers are `document` entities. Use `create(kind=\"document\")`.
- **Leaving domain empty for browseable concepts.** Domain unlocks `topic` filtering.
- **Duplicate names.** `learn` creates a new entity on every call. Search first with
  `knowledge.topic(query=\"<name>\")` or `search(kind=\"concept\", query=\"...\")` before
  registering.
