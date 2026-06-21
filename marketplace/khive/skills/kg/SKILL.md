---
description: Work the knowledge graph as typed entities and edges — search before you create, model new things as entities wired with the closed relation set, explore what is known by traversing, and route reviewable changes through propose/review. Use whenever you add a concept, paper, or project to the graph, link relationships, discover what the graph knows about a topic, or propose a change that should be reviewed before it mutates.
---

# Work the knowledge graph

The kg pack is the shared, cross-project knowledge graph: typed entities (9 kinds), a closed set
of edge relations (17), and notes. Sixteen verbs — `create`, `get`, `list`, `search`, `update`,
`delete`, `merge`, `link`, `neighbors`, `traverse`, `query`, `stats`, `propose`, `review`,
`withdraw`, `verbs` — but the thing worth learning is the *graph discipline*, not the verb list.
Per-verb param detail is one call away: `request(ops="create(help=true)")`.

**Namespace (ADR-007).** kg ops always use the shared `local` namespace, even when the server
runs as `--actor lambda:<you>`. Do not override the namespace for entity/edge/note ops — the
graph is cross-project by design.

## The pattern

### 1. Search before you create

The graph is shared and long-lived; a duplicate entity is worse than a missing one. Look first:

```
request(ops="search(kind=\"entity\", query=\"<the thing>\")")
```

If it exists, skip to enriching its edges (step 3). If it is a close-but-different match (a
variant), create it and link `variant_of` / `extends` back to the original. Only create when the
search comes up empty.

### 2. Model it as a typed entity, then link

```
request(ops="create(kind=\"concept\", name=\"<short canonical name>\", description=\"<what it is>\")")
```

Pick the right kind from the nine: `concept` `document` `dataset` `project` `person` `org`
`artifact` `service` `resource`. Names are short and canonical (`LoRA`, not the paper title);
facts like year or language are **properties**, not edges.

Then wire it in with `link` — direction matters:

| Relation        | FROM → TO             | Reads as                              |
| --------------- | --------------------- | ------------------------------------- |
| `instance_of`   | specific → general    | GQA instance_of grouped-attention     |
| `extends`       | child → parent        | QLoRA extends LoRA                    |
| `introduced_by` | concept → paper       | LoRA introduced_by Hu-2021            |
| `implements`    | code → concept        | lattice-inference implements GQA      |
| `depends_on`    | consumer → dependency | quantization depends_on calibration   |

Reach minimum density before you stop: concepts ≥ 4 edges, projects ≥ 3, documents ≥ 2. If the
relation you want is not one of the 17, it is probably a property.

### 3. Explore to ground yourself

Before adding to an area, see what is already there:

```
request(ops="neighbors(node_id=\"<id>\", direction=\"both\")")   # immediate connections — pass both; default is outgoing-only
request(ops="traverse(roots=[\"<id>\"], max_depth=3, relations=[\"extends\",\"instance_of\"])")  # lineage
request(ops="query(query=\"MATCH (a:concept)-[:extends]->(b:concept) WHERE b.name = 'LoRA' RETURN a.name LIMIT 20\")")  # structural patterns (GQL)
```

`search` finds by content, `neighbors` / `traverse` walk structure, `query` matches patterns.
Narrate what connects, and flag the gaps (concepts mentioned but absent, nodes under density) as
follow-up work.

### 4. Route reviewable changes through propose → review

When a change should be reviewed before it mutates the graph (a contested edge, a merge), do not
`link` / `merge` directly — open a proposal (event-sourced, ADR-046):

```
request(ops="[{\"tool\":\"propose\",\"args\":{\"title\":\"...\",\"description\":\"<cite the evidence>\",\"changeset\":{\"kind\":\"add_edge\", ...}}}]")
```

`propose` is the one verb that needs the JSON request form, not function-call form — its
`changeset` holds nested objects the function-call DSL cannot express.

A reviewer reads it and decides — `review(id, decision="approve"|"reject"|"request_changes")` —
and the runtime applies an approved changeset asynchronously (`approved → applying → applied`).
`withdraw(id, rationale=...)` rescinds your own open proposal; on `request_changes`, re-`propose`
with `parent_id` pointing at the original. Only `open` / `changes_requested` proposals are
reviewable.

## Bulk and sweep work → the agents, not by hand

Batch ingestion, gap analysis, and hygiene have dedicated agents with their own workflow skills —
reach for them instead of hand-rolling:

- **digester** (`digest`) — turn a body of source material into entities + edges + notes
- **gap-analyst** (`gap`) + **expander** (`expand`) — survey structural gaps, then grow the graph to close one
- **polisher** (`polish`) — fix orphans, under-linked nodes, duplicates, wrong-direction edges

## Anti-patterns

- **Creating before searching.** A duplicate is the cardinal sin in a shared graph. Search first; link or enrich what is there.
- **Reversed edge direction.** `introduced_by` is concept → paper, never paper → concept. Check the table.
- **Forcing a fact into an edge.** "published 2021", "written in Rust" are properties, not relations.
- **`neighbors` without `direction="both"`.** It defaults to outgoing-only and you miss half the graph.
- **Overriding the namespace with `lambda:*`.** kg is shared `local`; attribution lives elsewhere.
- **Hand-rolling a bulk ingest or a polish sweep.** That is what the digester / polisher agents are for.
