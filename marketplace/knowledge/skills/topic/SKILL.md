---
description: List or search concept entities, optionally filtered by domain.
---

# Topic

Browse concept entities in the current namespace, optionally filtered by domain or a free-text
query. Without filters, returns all concepts up to `limit`. With `domain`, returns only concepts
whose tags include that domain string. With `query`, runs a hybrid FTS + vector search, then applies
the domain filter post-retrieval.

## Usage

```
# All concepts
request(ops="knowledge.topic()")

# By domain
request(ops="knowledge.topic(domain=\"attention\", limit=20)")

# Free-text search within a domain
request(ops="knowledge.topic(query=\"linear attention state space\", domain=\"attention\")")
```

## Parameters

| Parameter | Type   | Required | Description                                   |
| --------- | ------ | -------- | --------------------------------------------- |
| `domain`  | string | no       | Filter to concepts tagged with this domain.   |
| `query`   | string | no       | Free-text search query (hybrid FTS + vector). |
| `limit`   | int    | no       | Maximum results (default: 20, max: 100).      |

## Response shape

```json
{
  "items": [
    {
      "id": "a1b2c3d4",
      "full_id": "a1b2c3d4-...",
      "name": "GQA",
      "description": "Grouped Query Attention",
      "tags": ["attention", "transformer"]
    }
  ],
  "total": 1
}
```

When `query` is provided, each item also carries `score` and optionally `snippet` from the search
result.

### `total` semantics

The meaning of `total` differs between the two execution paths:

- **Listing path** (`query` absent): `total` is the true pre-limit count of all matching entities in
  the namespace. Use it as a pagination signal — if `total > items.length`, there are more results.
- **Search path** (`query` present): `total` is the post-filter count of the candidate window
  fetched from the search index (`limit * 4` candidates). It is bounded by that window and **does
  not** reflect the full corpus count. Use it as a relevance indicator, not a pagination total. When
  `domain` narrows results heavily, `total` may be much smaller than the actual number of matching
  concepts in the corpus.

## Patterns

### Check if a concept already exists before `learn`

```
request(ops="knowledge.topic(query=\"LoRA\", limit=5)")
```

If the result includes a matching item, link it instead of calling `learn` again.

### Browse all concepts in a research area

```
request(ops="knowledge.topic(domain=\"inference\", limit=50)")
```

## Anti-patterns

- **Using `topic` for non-concept entities.** `topic` only returns entities with `kind = "concept"`.
  Use `search(kind=\"entity\", ...)` for broader searches.
- **Expecting domain filter without tagged domain.** Domain filtering works via tags. Concepts
  registered without a `domain` in `learn` will not appear in domain-filtered results.
