---
description: Link a concept to the paper or person that introduced it.
---

# Cite

Create a provenance-tracked `introduced_by` edge from a concept entity to the document or person
that first introduced it. This follows the ADR-002 base endpoint contract:
`concept -[introduced_by]-> document` or `concept -[introduced_by]-> person`.

## Usage

```
request(ops="knowledge.cite(concept_id=\"<concept_uuid>\", source_id=\"<document_uuid>\")")
```

## Parameters

| Parameter    | Type   | Required | Description                                                        |
| ------------ | ------ | -------- | ------------------------------------------------------------------ |
| `concept_id` | string | yes      | Full UUID of the concept entity.                                   |
| `source_id`  | string | yes      | Full UUID of the source entity; must be kind `document` or `person`. |
| `weight`     | float  | no       | Edge weight in `[0.0, 1.0]`. Default: `1.0` (definitional).        |

## Response shape

```json
{
  "id": "a1b2c3d4",
  "full_id": "a1b2c3d4-...",
  "relation": "introduced_by",
  "concept_id": "<concept-uuid>",
  "source_id": "<document-uuid>",
  "weight": 1.0
}
```

## Workflow

```
# 1. Register the concept
request(ops="knowledge.learn(name=\"LoRA\", domain=\"fine-tuning\")")

# 2. Create the paper as a document entity
request(ops="create(kind=\"document\", name=\"Hu et al. 2021\", description=\"LoRA: Low-Rank Adaptation\")")

# 3. Link them
request(ops="knowledge.cite(concept_id=\"<concept_full_id>\", source_id=\"<paper_entity_id>\")")
```

## Anti-patterns

- **Citing concept → concept.** `introduced_by` requires the target to be a `document` or `person`.
  Use the kg `link` verb with a different relation (e.g. `extends`) for concept→concept.
- **Using a note ID as source.** Notes are not valid targets for `introduced_by`. The source must be
  a `document` or `person` _entity_.
