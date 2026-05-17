---
name: curate
description: Curate the knowledge graph — merge duplicates, supersede stale notes, clean up orphans, verify edge cascades.
---

# Curate

The graph accumulates duplicates over time (parallel ingestion, synonym drift, abbreviation variations). Curation keeps it queryable.

## Dedup Detection

### By name similarity
```python
search(kind="entity", query="Low-Rank Adaptation")
search(kind="entity", query="LoRA")
search(kind="entity", query="low rank fine tuning")
```
If multiple results describe the same concept, merge them.

### By listing and grouping
```python
# Multi-pattern GQL is not yet supported — use list + client-side grouping
concepts = list(kind="entity", entity_kind="concept", limit=500)
# Group by normalized name; inspect same-name groups with get/neighbors before merge
```

### After parallel batch ingestion
Always run a name-collision check after spawning multiple ingest agents. Each agent independently searches before creating, but race conditions and semantic near-misses still produce duplicates.

## Merge Workflow

```python
# 1. Identify the canonical entity (more edges = keep as into_id)
neighbors(node_id=<candidate_a>)
neighbors(node_id=<candidate_b>)

# 2. Merge — the from_id record is removed, all its edges rewire to into_id
merge(into_id=<canonical>, from_id=<duplicate>,
      strategy="union")      # safe default: combines properties from both

# strategy options:
# "prefer_into"   — canonical's properties win on conflict
# "prefer_from"   — duplicate's properties win on conflict
# "union"         — merge properties, neither wins (safest when uncertain)
```

`merge` is entity-only in v0.1. Note deduplication requires the supersession pattern below.

## Supersession Pattern (notes and concepts)

When a note is replaced by a better version, don't delete — supersede:

```python
# Step 1: create the new note (with its OWN annotations — never inherited)
new_note = create(kind="note", note_kind="decision",
  content="Updated decision with new evidence...", salience=0.9,
  annotates=["<entity-uuid>"])

# Step 2: link new → old
link(source_id=new_note.id, target_id="<old-note-id>", relation="supersedes")
```

Supersession does NOT transfer annotations. The new note annotates only what you explicitly
declare — the old note's `annotates` edges stay on the old note (ADR-019).

**Effect**: `search(kind="note")` automatically excludes notes targeted by `supersedes` edges.
The old note remains accessible via `get(id=old_note_id)` for history.

For concepts (same pattern — explicit link):
```python
link(source_id=new_concept_id, target_id=old_concept_id, relation="supersedes")
```
Note: concept supersession is structural only — search does NOT auto-hide superseded entities.

## Edge Cascade on Hard Delete

Hard entity delete cascades to all incident edges:

```python
delete(id="<entity-uuid>", hard=True)
# → entity removed
# → all edges where source_id or target_id == entity removed
# → no dangling references
```

Soft delete (default) removes the entity from queries but leaves edges in place:

```python
delete(id="<entity-uuid>")          # soft = default
# → entity hidden from search/list (deleted_at set)
# → edges remain (may point to invisible nodes)
```

**Use hard delete for genuine errors** (wrong entity created). Use soft delete when you might want history.

## Orphan Cleanup

After any merge or delete, check for orphans using a multi-step procedure (GQL does not support `WHERE NOT`):

```python
# 1. List all concept entities
concepts = list(kind="entity", entity_kind="concept", limit=50)

# 2. For each entity, check neighbor count
for entity in concepts:
    result = neighbors(node_id=entity.id, direction="both")
    if len(result) == 0:
        # Orphan — add minimum edges or hard-delete if stale artifact
        print(f"Orphan: {entity.name} ({entity.id})")
```

Repeat for `entity_kind="project"`. For each orphan: either add minimum edges or hard-delete if it was a stale artifact.

## Edge Quality Pass

Low-weight edges are speculative linkages. Audit periodically:

```python
list(kind="edge", min_weight=0.0, max_weight=0.4)
```

For each low-weight edge: `get(id=<edge-id>)` → decide: update weight (if you now have evidence), delete (if speculative link was wrong), or leave as-is.

## Post-Curate Verification

```python
# 1. Density check (multi-step: aggregate queries are not supported in GQL)
all_edges = list(kind="edge", limit=500)
all_nodes = list(kind="entity", limit=500)
# → len(all_edges) / len(all_nodes) should be ≥ 5

# 2. Orphan check
concepts = list(kind="entity", entity_kind="concept", limit=50)
for entity in concepts:
    nbrs = neighbors(node_id=entity.id, direction="both")
    if len(nbrs) == 0:
        print(f"Orphan: {entity.name}")
# → should find 0 orphans

# 3. Low-degree check
for entity in concepts:
    nbrs = neighbors(node_id=entity.id, direction="both")
    if len(nbrs) < 4:
        print(f"Under-linked: {entity.name} ({len(nbrs)} edges)")
# → all concepts should have ≥ 4 edges
```
