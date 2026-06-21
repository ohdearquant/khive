---
description: Audit graph health and fix issues — orphans, low-degree nodes, duplicates, stale edges. Report before/after.
---

# Polish

Your graph needs maintenance. This skill audits health, identifies issues, and fixes them.

The MCP server exposes one tool — `request` — that takes the verb call as a string:

```text
request(ops="list(kind=\"entity\", entity_kind=\"concept\", limit=50)")
request(ops="[list(kind=\"edge\", source_id=\"<u>\"), list(kind=\"edge\", target_id=\"<u>\")]")  # parallel batch
```

The verb examples in this skill show the inner call. Wrap each one as `request(ops="…")`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`), even when the
MCP server runs with `--actor lambda:myproject`. Do NOT override the namespace for entity/edge/note
operations. The knowledge graph is cross-project by design.

## Workflow

### 1. Get the lay of the land

```
list(kind="entity", entity_kind="concept", limit=50)
list(kind="entity", entity_kind="project", limit=20)
```

### 2. Check each entity for edge count

For every entity listed:

```
neighbors(node_id="<entity-id>", direction="both")
```

Classify:

- **Orphan** (0 edges): must fix — every entity needs at least one relationship
- **Under-linked** (concepts < 4, projects < 3, documents < 2): should fix
- **Healthy** (at or above target): skip

### 3. Find duplicates

Search for entities with similar names:

```
search(kind="entity", query="<entity name>")
```

If two entities refer to the same real-world thing (e.g., "LoRA" and "Low-Rank Adaptation"):

```
merge(into_id="<keep-id>", from_id="<remove-id>")
```

`merge` deduplicates entities. Properties combine, tags union, edges rewire to the kept entity, and
the duplicate is removed. Both IDs must refer to entities.

For duplicate **notes** — use supersession instead:

```
create(kind="note", note_kind="<kind>", content="<better version>",
  annotates=["<target-entities>"])
link(source_id="<new-note-id>", target_id="<old-note-id>", relation="supersedes")
```

Supersession does NOT transfer annotations. The new note must explicitly declare its own
`annotates`. The old note stays in the store but is excluded from `search(kind="note")`.

### 4. Fix orphans and under-linked entities

For each orphan or under-linked entity, think about:

- What is it a kind of? → `instance_of`
- What does it extend? → `extends`
- Who introduced it? → `introduced_by`
- What competes with it? → `competes_with`
- What project implements it? → `implements`

Create the appropriate links. If you can't determine the relationship from context, search for
clues:

```
search(kind="note", query="<entity name>")
search(kind="entity", query="<broader category>")
```

### 5. Audit edge quality

List edges from high-value entities:

```
list(kind="edge", source_id="<entity-id>")
list(kind="edge", target_id="<entity-id>")
list(kind="edge", source_id="<entity-id>", relations=["introduced_by", "extends"])  # filter by relation type
```

Check for:

- **Wrong direction**: `introduced_by` going paper → concept (should be concept → paper)
- **Wrong relation**: `extends` used where `instance_of` fits better
- **Low weight on confident relations**: definitional relations should be weight 1.0

Fix with:

```
update(kind="edge", id="<edge-id>", relation="<correct-relation>")
update(kind="edge", id="<edge-id>", weight=1.0)
delete(kind="edge", id="<edge-id>")  # if the edge is just wrong
```

### 6. Report

Summarize:

- Entities audited: N
- Orphans fixed: N (list them)
- Under-linked fixed: N
- Duplicates merged: N
- Edges corrected: N
- Before density: X edges/entity → After density: Y edges/entity

## Density targets

| Entity kind | Minimum edges | Required relationship types                                                                                               |
| ----------- | ------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `concept`   | 4             | At least one parent (`instance_of` or `extends`), `introduced_by` if source exists, `competes_with` if alternatives exist |
| `project`   | 3             | `implements` (what concept), structural (`contains`/`part_of`), `depends_on`                                              |
| `document`  | 2             | `introduced_by` edges FROM concepts it introduced                                                                         |
| `person`    | 1             | `introduced_by` from their work                                                                                           |
| `org`       | 1             | `contains` or structural relationship                                                                                     |

## Data-vs-view principle

Supersession **keeps** the old record and marks it superseded — it never deletes, copies, or
transfers data. "Show only current" is a query concern handled by the search filter, not by data
mutation. If you want to actually remove something, the verb is `delete`.

## Stop condition

No orphans. All entities at or above minimum density. No obvious duplicates. Stale edges corrected.
Report filed.
