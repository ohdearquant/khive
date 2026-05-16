---
name: assign
description: Create typed notes — decisions, insights, observations, questions, references — and attach them to entities for cross-substrate navigation.
---

# Assign

Notes are the temporal layer over the entity graph. They record what you discovered, decided, questioned, or observed. A note without `annotates` is an orphan — always link it.

## Note Kinds

| Kind | Records | Salience default |
|------|---------|-----------------|
| `observation` | An empirical capture — "I noticed that X" | 0.5 |
| `insight` | Synthetic conclusion from multiple observations | 0.75 |
| `question` | An open inquiry needing investigation | 0.6 |
| `decision` | A committed choice with rationale; often irreversible | 0.85 |
| `reference` | External pointer with context (URL, paper, person) | 0.5 |

`observation` is the default if `note_kind` is omitted.

## Creation Pattern

```python
# Decision with high salience (critical invariant)
create(kind="note", note_kind="decision",
  content="Use RRF k=60 for score fusion — robust across scale differences, validated on 10K entity graphs. Do not tune k per-query.",
  salience=0.9,
  annotates=["<search-system-entity-id>"])

# Insight from research
create(kind="note", note_kind="insight",
  content="LoRA r=4 achieves 99% of full fine-tuning quality on code tasks; r=8 adds marginal gain for 2× param cost.",
  salience=0.8,
  annotates=["<LoRA-entity-id>"])

# Open question
create(kind="note", note_kind="question",
  content="Should GQL RETURN support JSON property projection for nested properties?",
  salience=0.65,
  annotates=["<query-layer-entity-id>"])

# External reference
create(kind="note", note_kind="reference",
  content="FlashAttention-3 implementation: https://github.com/Dao-AILab/flash-attention — supports H100 TMA and async softmax",
  salience=0.5,
  annotates=["<FlashAttention-3-entity-id>"])
```

## Salience Guide

```
0.9-1.0 — Critical decisions, invariants that must not be violated
0.7-0.8 — Important insights, synthesized conclusions, recurring patterns
0.5-0.6 — Standard observations, useful context, open questions
0.3-0.4 — Minor notes, ephemeral context, historical trivia
```

Salience affects search ranking: `score *= (0.5 + 0.5 * salience)`. A salience-0.9 note will surface before a salience-0.4 note even if the keyword match is slightly weaker.

## Named Notes (titled notes)

Notes accept an optional `name` field for titled notes — use when the note stands alone as a named artifact:

```python
create(kind="note", note_kind="decision", name="ADR-023 rationale",
  content="Verb-consolidated MCP surface chosen over per-kind namespacing. See docs/adr/ADR-023.",
  salience=0.9, annotates=["<mcp-surface-entity-id>"])
```

Unnamed notes are fine — content alone is sufficient for search.

## Annotates: the cross-substrate edge

`annotates` wires the note into the entity graph. Without it:
- `neighbors(node_id=<entity>)` won't return the note
- The note is only reachable via `search(kind="note")` — no graph context

With it:
- `neighbors(node_id=<entity>, direction="in", relations=["annotates"])` returns all notes on that entity
- Traversal from the entity includes the note's connections

Always use `annotates` unless the note is genuinely freestanding.

## Supersession (updating a note)

When a decision changes, don't delete the old note — supersede it:

```python
# 1. Fetch the old note to find its annotates targets
old = get(id=old_note_id)
# old.annotates contains the entity IDs this note was attached to

# 2. Create the replacement, attaching to the same entities
new = create(kind="note", note_kind="decision",
  content="New decision...", salience=0.9,
  annotates=old.annotates)

# 3. Mark the old as superseded
link(source_id=new.id, target_id=old.id, relation="supersedes")
```

Old note is now excluded from `search(kind="note")` results automatically. It's preserved for history via `get(id=old_note_id)`.

## Before Creating a Note

```python
search(kind="note", query="<topic>")
```
Check for existing notes. If a note already covers the topic with similar content, supersede it rather than duplicate.
