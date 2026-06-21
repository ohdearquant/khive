---
description: Self-expansion. Take a strategic gap and grow the graph to close it — create the missing entity, draft the project proposal, resolve the deferred decision, bridge the disconnected domains.
---

# Expand

`gap` finds what's missing. `expand` builds it.

This is the most aggressive skill in the plugin: it adds NEW entities and edges based on what the
graph already knows it's missing. Use it when a gap-inventory item is concrete enough to act on — a
researched-but-unbuilt concept with downstream waiters, a single-use paper that should connect to
more, a domain rich with concepts but no implementing crate.

Self-expansion is high-leverage but easy to corrupt. **The rule**: never create an entity you cannot
back with at least one citation — a paper, an ADR, an existing implementation, a quote from the
source the gap came from. Speculative entities become noise that the next polish pass has to clean
up.

The MCP server exposes one tool — `request` — that takes the verb call as a string:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"…\")")
request(ops="[create(kind=\"entity\", entity_kind=\"project\", name=\"lora-tools\"), link(source_id=\"<project-id>\", target_id=\"<concept-id>\", relation=\"implements\"), link(source_id=\"<project-id>\", target_id=\"<dependency-id>\", relation=\"depends_on\")]")  # parallel batch
```

The verb examples below show the inner call. Wrap each one as `request(ops="…")`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`), even when the
MCP server runs with `--actor lambda:myproject`. Do NOT override the namespace for entity/edge/note
operations. The knowledge graph is cross-project by design.

## Input

A single gap to expand. Either:

- A concept UUID flagged by `gap` (roadmap, feature direction, research direction)
- A `properties.domain` value flagged as an architectural gap
- A set of UUIDs forming a `competes_with` clique flagged as decision debt

If the input isn't a structured gap from `gap_inventory.md`, run `gap` first. Working from a fresh
inventory keeps expansion grounded.

## Mode selection

Pick the mode that matches the gap's category. One expansion = one mode. Do not chain modes inside a
single skill invocation.

| Gap type from `gap`                   | Mode to run |
| ------------------------------------- | ----------- |
| Roadmap (researched-unbuilt)          | **Promote** |
| Architectural (domain orphan)         | **Bridge**  |
| Feature direction (enables→void)      | **Extend**  |
| Feature direction (decision debt)     | **Resolve** |
| Research direction (single-use paper) | **Extend**  |
| Research direction (narrow framing)   | **Extend**  |

### Mode: Promote — roadmap gap → project proposal

Trigger: a concept `C` with `status ∈ {"researched", "concept"}`, ≥2 incoming `depends_on`, zero
`implements`.

Workflow:

1. Pull `C`'s description, properties, and full neighborhood:

```
neighbors(node_id="<C-id>", direction="both")
```

2. Search for prior art in the same domain:

```
search(kind="entity", query="<C.domain> <C.name>")
list(kind="entity", entity_kind="project", limit=50)
```

3. Determine the smallest viable project that could `implements` C. Anchor it to existing
   infrastructure crates via `depends_on`. If no obvious crate name exists, propose one based on the
   dominant ADR or repo in C's properties.

4. Create the project entity (status: `"proposed"`):

```
create(kind="entity", entity_kind="project", name="<crate-name>",
  description="Proposed: implements <C.name>. Triggered by gap-inventory <date>.",
  properties={"status": "proposed", "domain": "<C.domain>", "repo": "<C.repo or repo:TBD>"})
```

5. Wire the edges:

```
link(source_id="<new-project-id>", target_id="<C-id>", relation="implements", weight=0.8)
# For each existing depends_on of C, add depends_on from new project to that prerequisite's project
```

6. File a `decision` note recording why this project was promoted:

```
create(kind="note", note_kind="decision",
  content="Promoted <C.name> from researched → proposed implementation. <N> concepts depend_on it. Source: gap-inventory <date>.",
  annotates=["<new-project-id>", "<C-id>"])
```

### Mode: Bridge — domain orphan → architectural crate proposal

Trigger: a `properties.domain` value with ≥5 concept entities and zero project entities whose
`implements` edges land in that domain.

Workflow:

1. List the concepts in the domain:

```
list(kind="entity", entity_kind="concept", limit=100)
# filter by properties.domain == "<target-domain>"
```

2. Pick the concept with the highest `incoming_depends_on` count as the architectural anchor. Apply
   **Promote** to that anchor (above).

3. After Promote completes, walk the rest of the domain's concepts and add `implements` edges from
   the new project to each whose description is architecturally compatible (read each description to
   verify; do NOT bulk-link).

### Mode: Extend — single-use paper / narrow framing → adjacent concepts

Trigger: a paper cited by exactly one concept, or a concept in a multi-option domain with zero
`competes_with` edges.

Workflow:

1. Pull the source concept/paper + its neighborhood:

```
neighbors(node_id="<source-id>", direction="both")
```

2. Read the source's description. Identify named alternatives, variants, or parent techniques
   mentioned in the text.

3. For each identified neighbor that **does not exist yet** in the graph (verify with `search`),
   create it as a concept:

```
search(kind="entity", query="<candidate-name>")
# If no match found:
create(kind="entity", entity_kind="concept", name="<short-canonical-name>",
  description="<one-sentence definition from the source>",
  properties={"domain": "<source.domain>", "status": "concept", "type": "technique"})
```

4. Link the new concept to the source with the appropriate relation: `competes_with` for
   alternatives, `extends`/`variant_of` for derivatives, `instance_of` for specializations.

5. Hard ceiling: **max 5 new entities per Extend invocation**. Self-expansion that creates more than
   5 entities is no longer grounded — it's hallucination. If you need more, run Extend again with a
   different source.

### Mode: Resolve — competes_with clique → decision

Trigger: a clique of ≥2 concepts under `competes_with` with zero `implements` edges across all
members.

Workflow:

1. For each member, pull description + properties:

```
request(ops="[get(id=\"<m1-id>\"), get(id=\"<m2-id>\")]")
```

2. Identify the comparison axes that matter (drawn from descriptions, properties, the domain's
   conventions). Typical axes: performance, memory, complexity, compatibility with existing infra.

3. **Resolve does not pick winners autonomously.** It writes a `decision` note that:
   - Lays out the comparison
   - Names which axis matters most for this codebase (anchored to existing project entities and
     their constraints)
   - Recommends a member, with stated assumptions

```
create(kind="note", note_kind="decision",
  content="Comparison of <clique-members>. Axes: <axes>. Recommended: <member>. Assumptions: <list>.",
  annotates=["<member-1-uuid>", "<member-2-uuid>"])
```

4. If the recommendation is strong (cited by an existing ADR or backed by benchmark properties on
   the entities), also add an `implements` edge from the relevant project to the chosen member.

## Verification

After expansion, for every new entity created:

```
neighbors(node_id="<new-id>", direction="both")
```

Each new entity must meet the kind's minimum density (concept ≥ 4, project ≥ 3, document ≥ 2). If a
new entity is below threshold and you've exhausted the visible context, file a `question` note
recording what edge is missing:

```
create(kind="note", note_kind="question",
  content="<new-entity> needs <relation> to <unknown-target>. Source for follow-up: <…>",
  annotates=["<new-entity-id>"])
```

A question note is honest debt — it tells the next agent what's missing without fabricating an edge.

## Report

State for the record:

- Which gap was the input (cite gap_inventory entry or source UUIDs)
- Which mode ran
- New entities created (UUIDs + names)
- New edges added (count + relations)
- Notes filed (decision / question)
- Density verification per new entity
- Any speculative work avoided — what you considered creating but didn't

## Safety rules (mandatory)

1. **One mode per invocation.** Do not chain.
2. **Max 5 new entities per Extend.** Promote/Bridge create at most one project. Resolve creates
   only notes (or one edge if recommendation is strong).
3. **Every new entity needs a citation.** A description sentence sourced from an existing entity, a
   paper, an ADR, or a code reference. No source = no create. File a question note instead.
4. **Do not create entities outside the closed taxonomies.** 8 kinds, 15 relations, 5 note kinds. If
   your expansion needs a new kind, that is an ADR, not a skill invocation — stop and surface to a
   human.
5. **Re-read the source after expansion** to verify you didn't drift. A common failure: the gap
   inventory pointed at X, but mid-expansion you ended up creating entities about Y. Stop and revert
   if so.

## Stop condition

The input gap is closed (the entity exists with min density and a relation back to the source) OR a
`question` note explicitly records why the gap cannot be closed without external input. Either is an
acceptable terminal state. Leaving the gap silently unaddressed is not.

## Cadence

Run after each `gap` survey. Process gaps in frontier-rank order — highest- leverage first. Re-run
`gap` between expansions to track which gaps the expansion actually closed and which new gaps it
opened (every expansion opens at least one new edge of unexplored knowledge).
