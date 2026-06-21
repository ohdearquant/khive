---
description: Work with the knowledge corpus — search atoms and domains, compose context briefings, register concepts (learn), link concepts to their sources (cite), browse concept clusters (topic), and grow or curate the corpus. Use whenever you need retrieved context before implementing, want to record a technique, or need to attribute a concept to its paper or author.
---

# Work with the knowledge corpus

khive knowledge is the domain-corpus layer: atoms (markdown chunks), domains (thematic
groupings), and concept entities. The verbs fall into retrieval (`search`, `suggest`,
`compose`), growth (`learn`, `cite`, `topic`), and corpus maintenance (`upsert_atoms`,
`edit`, `import`, `index`). Per-verb param detail is one call away:
`request(ops="knowledge.search(help=true)")`.

## The pattern

### 1. Search first

Before composing a briefing or registering anything new, search to see what is already
in the corpus.

```
request(ops="knowledge.search(query=\"attention sink quantization\", rerank=true)")
```

`rerank=true` normalizes scores to [0,1] so results are directly comparable. For queries
that span multiple concepts, add `decompose=true` to split the query before retrieval.

```
request(ops="knowledge.search(query=\"RoPE positional encoding and flash attention\", rerank=true, decompose=true)")
```

### 2. Suggest domains, then compose a briefing

When you need a richer briefing assembled from multiple atoms, use `suggest` to identify
relevant domains, then pass the returned IDs to `compose`.

```
request(ops="knowledge.suggest(query=\"token-efficient attention for long context inference\", role=\"implementer\", limit=8)")
```

Run `suggest` two or three times with varied phrasing to get broader coverage before
calling `compose`. Then:

```
request(ops="knowledge.compose(domain_ids=[\"<id1>\", \"<id2>\"], query=\"rerank for long-context attention\")")
```

`compose` accepts `domain_ids`, `atom_ids`, or both. The `query` parameter re-ranks
the assembled content toward your actual question.

### 3. Register a concept with learn

`knowledge.learn` creates a **concept entity** (not an atom). Use it for algorithms,
techniques, and ideas worth tracking in the knowledge graph.

```
request(ops="knowledge.learn(name=\"GQA\", domain=\"attention\", tags=[\"transformer\", \"kv-cache\"])")
```

Papers are `document` entities, not concepts. Use `create(kind=\"document\")` for papers,
then link them with `cite`.

### 4. Cite the source

`knowledge.cite` creates an `introduced_by` edge from a concept to the document or person
that first introduced it. Both IDs must be full UUIDs.

```
request(ops="knowledge.cite(concept_id=\"<concept_uuid>\", source_id=\"<document_uuid>\")")
```

The typical chain is: `learn` to register the concept, `create(kind="document")` to
register the paper, then `cite` to link them.

### 5. Browse concept clusters with topic

Use `knowledge.topic` to check for duplicates before calling `learn`, or to explore what
the corpus knows in a research area.

```
request(ops="knowledge.topic(query=\"LoRA\", limit=5)")
request(ops="knowledge.topic(domain=\"fine-tuning\", limit=20)")
```

## Anti-patterns

- **Composing without suggesting first.** `suggest` tells you which domains are relevant.
  Skipping it means guessing domain IDs or missing coverage.
- **Calling `learn` without checking topic first.** `learn` always creates a new entity.
  If a concept already exists you get a duplicate. Search or topic-check before registering.
- **Using `learn` for papers.** Papers are `document` entities. Use `learn` for algorithms
  and techniques, `create(kind="document")` for papers, then `cite` to connect them.
- **Citing with note IDs.** The `source_id` in `cite` must be a `document` or `person`
  entity. Notes are not valid targets for `introduced_by`.
- **Omitting `domain` in `learn`.** Without a domain tag, the concept will not surface in
  `topic(domain="...")` filtered queries.
- **Skipping `rerank=true` in search.** Raw RRF scores are around 0.016 and hard to
  compare. Always pass `rerank=true` for meaningful relevance ordering.
