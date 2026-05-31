---
description: Survey the graph for strategic gaps — researched-but-unbuilt concepts, domains without implementations, decisions deferred, single-use papers. Emit a frontier ranking of where to invest next.
---

# Gap

A polished graph still has gaps. Not orphans — those are structural and `polish` handles them.
**Strategic gaps**: things the graph's shape implies should exist but don't. Concepts everyone
depends on but no one has built. Domains thick with research and thin with code. Alternatives we
compared then never chose between. Papers we read once and never extended.

This skill walks four gap categories and emits a single `gap_inventory.md` plus a frontier ranking.
The output is decision input — "what should we build next, given what the graph already says we
know."

The MCP server exposes one tool — `request` — that takes the verb call as a string:

```text
request(ops="list(kind=\"entity\", entity_kind=\"concept\", limit=200)")
request(ops="[neighbors(node_id=\"<u>\", direction=\"in\", relations=[\"depends_on\"]), neighbors(node_id=\"<u>\", direction=\"out\", relations=[\"implements\"])]")
```

The verb examples below show the inner call. Wrap each one as `request(ops="…")`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`), even when the
MCP server runs with `--actor lambda:myproject`. Do NOT override the namespace for entity/edge/note
operations. The knowledge graph is cross-project by design.

## Workflow

### 1. Snapshot the graph

```
list(kind="entity", entity_kind="concept", limit=500)
list(kind="entity", entity_kind="project", limit=200)
list(kind="entity", entity_kind="document", limit=500)
```

Collect every UUID, name, and `properties.{domain, status, repo, type}` field. You will need these
for category-2 and category-4 queries.

### 2. Category I — Roadmap gaps

What it surfaces: high-leverage work that nothing in the graph has scheduled.

For every concept with `status ∈ {"concept", "researched", "prototyped"}`:

```
neighbors(node_id="<concept-id>", direction="in",  relations=["depends_on"])
neighbors(node_id="<concept-id>", direction="out", relations=["implements"])
```

Score:

- `incoming_depends_on_count` — how many things wait on this
- `outgoing_implements_count` — has any project realized it (should be 0 to count as roadmap gap)

Flag any concept where `incoming_depends_on_count ≥ 2 AND outgoing_implements == 0`. These are the
highest-leverage unbuilt items — many downstream concepts already assume them.

Subcategory — explicit roadmap debt:

```
list(kind="entity", entity_kind="document", limit=500)
# filter for properties.type == "adr" and properties.status == "proposed"
```

For each such ADR, walk its `introduced_by` edges back to the concepts it introduces, then check
whether any project `implements` those concepts. Proposed ADR + zero implements = formal roadmap
debt.

### 3. Category II — Architectural gaps

What it surfaces: missing layers and orphan layers.

**Missing layers**: domains thick with concepts and thin with projects.

Aggregate concepts by `properties.domain`. For each domain:

- count concepts
- count projects whose `implements` edges land on concepts in that domain

Flag domains where `concept_count ≥ 5 AND project_count == 0`. The graph knows the topic deeply but
has shipped nothing in it.

**Orphan layers**: projects that don't compose with anything.

For each project:

```
neighbors(node_id="<project-id>", direction="both", relations=["depends_on", "part_of", "contains"])
```

Flag projects with zero structural edges. They exist, but the graph has no model of how they fit
into a larger system.

### 4. Category III — Feature direction gaps

What it surfaces: things we built that lead somewhere we didn't follow.

For each concept with `outgoing_enables` edges:

```
neighbors(node_id="<concept-id>", direction="out", relations=["enables"])
```

For each enabled-target, check its `status` and `outgoing_implements`. Flag patterns where the
source is implemented but the target is `status ∈ {"concept", "researched"}` and has zero
implementing project. We shipped X; the thing X is supposed to enable is still a sketch.

**Decision debt** — competes_with cliques with no chosen winner:

```
neighbors(node_id="<concept-id>", direction="both", relations=["competes_with"])
```

Build cliques (connected components under `competes_with`). For each clique of size ≥ 2, check if
any member has an `implements` edge. Clique with zero implementations = we researched alternatives,
never picked. The clique itself is the gap.

### 5. Category IV — Research direction gaps

What it surfaces: thin engagement with the literature.

For each `document` with `properties.type == "paper"`:

```
neighbors(node_id="<paper-id>", direction="in", relations=["introduced_by"])
```

Flag papers with exactly one incoming `introduced_by` AND where that concept has zero
`competes_with` AND zero `implements`. Single-use intellectual stub — we read it for one concept,
never extended it, never compared it, never built it.

**Narrow framing**: concepts in domains that should have alternatives but don't.

For each concept whose `properties.domain` is one where the graph normally tracks alternatives
(e.g., `attention`, `quantization`, `optimizer`), check `outgoing_competes_with`. Zero
`competes_with` in a multi-option domain = the graph picked one path and didn't record what it
rejected.

### 6. Frontier ranking

For every concept the categories flagged, compute:

```
score = (incoming_depends_on_count × adr_mention_weight) / penalty
```

Where:

- `incoming_depends_on_count`: how many other concepts list this as a dependency
- `adr_mention_weight`: count of ADR documents that cite this concept via `introduced_by` (use 1 if
  zero ADRs cite it, to avoid zeroing-out)
- `penalty`: 1 if `outgoing_implements > 0`, else 10

High score = "many things wait on it, our own decisions reference it, nobody has built it." This is
the top of the queue.

### 7. Emit `gap_inventory.md`

Structure:

```markdown
# Gap Inventory — <date>

## I. Roadmap gaps

- **<concept-name>** (`<8-char-id>`) — incoming_depends_on=N, status=researched, ADR-mentions=K
  - Why it matters: <brief, drawn from concept description>
  - Downstream: <list of dependent concept names>

## II. Architectural gaps

### Missing layers

- domain=<X> — N concepts, 0 projects. Sample concepts: <names>

### Orphan layers

- project=<name> (`<id>`) — 0 structural edges

## III. Feature direction gaps

### Enables-into-void

- <concept> enables <unbuilt-target>

### Decision debt

- clique={<A>, <B>, <C>} — 0 implementations

## IV. Research direction gaps

### Single-use papers

- paper=<title> — cited by 1 concept (<name>), that concept has no competes_with, no implements

### Narrow framing

- concept=<name> in domain=<X> — 0 competes_with edges in a multi-option domain

## Frontier ranking (top 20)

| Rank | Concept | Score | Reason |
| ---- | ------- | ----- | ------ |
| 1    | ...     | ...   | ...    |
```

## What is NOT a strategic gap

These are caught by `polish`, not here:

- Orphan entities (0 edges) — structural, fix via density rules
- Wrong-direction edges — structural, fix via audit
- Duplicate entities — structural, fix via merge
- Under-linked nodes that are simply newly-ingested — give them edges, not strategic weight

If a `gap` finding overlaps with a `polish` finding, prefer the polish framing. This skill assumes
the graph is already structurally healthy.

## Stop condition

`gap_inventory.md` exists with all four categories populated (use "none found" if a category is
genuinely empty) plus a frontier ranking of at least the top 10 concepts. Each flagged item carries
its UUID, the metric that flagged it, and a one-line "why it matters" drawn from the concept's
description.

## Cadence

Run after every major `digest` round and at least once before any roadmap planning. The inventory is
a snapshot — gaps close as work happens, new ones open as research advances. The frontier ranking is
most useful as a re-prioritization signal between waves of implementation work, not a one-time
artifact.
