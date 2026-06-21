---
description: Ingest research material into the knowledge graph — papers, concepts, implementations. Extract entities, link them, verify density.
---

# Digest

You have material to add to the knowledge graph. This skill walks you through a complete ingestion:
extract → create → link → annotate → verify.

The MCP server exposes one tool — `request` — that takes the verb call as a string:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\")")
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\"), create(kind=\"entity\", entity_kind=\"document\", name=\"LoRA paper\"), link(source_id=\"<concept-id>\", target_id=\"<paper-id>\", relation=\"introduced_by\")]")   # parallel batch
```

The verb examples in this skill show the inner call. Wrap each one as `request(ops="…")`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`), even when the
MCP server runs with `--actor lambda:myproject`. Do NOT override the namespace for entity/edge/note
operations. The knowledge graph is cross-project by design.

## Workflow

### 1. Check what already exists

```
search(kind="entity", query="<key terms from the material>")
```

Never create duplicates. If the entity exists, skip to linking or enriching it.

### 2. Create entities

For each nameable concept, paper, person, project, dataset, or org in the material:

```
create(kind="entity", entity_kind="<kind>", name="<short canonical name>",
  description="<1-2 sentence summary>",
  properties={"domain": "...", "type": "...", "year": "..."})
```

**9 entity kinds** (closed — pick the best fit, don't invent):

| Kind       | Use for                                                      |
| ---------- | ------------------------------------------------------------ |
| `concept`  | Algorithms, techniques, architectures, models, research gaps |
| `document` | Papers, preprints, reports, blog posts                       |
| `dataset`  | Benchmarks, corpora, evaluation sets                         |
| `project`  | Codebases, libraries, tools, frameworks                      |
| `person`   | Researchers, engineers, authors                              |
| `org`      | Labs, companies, institutions                                |
| `artifact` | Generated files, model artifacts, build outputs              |
| `service`  | Long-running services, APIs, deployed systems                |
| `resource` | Knowledge atoms, domains, skills, tools                       |

**Naming**: short canonical name people actually say. `LoRA` not
`Low-Rank Adaptation of Large Language Models`. Full titles go in `properties`.

### 3. Link entities

For each relationship you identified in the material:

```
link(source_id="<from>", target_id="<to>", relation="<relation>", weight=<0.4-1.0>)
```

**17 relations** (closed — map to these, don't invent):

| Category       | Relation        | Direction              | When                      |
| -------------- | --------------- | ---------------------- | ------------------------- |
| Structure      | `contains`      | parent → child         | System has component      |
| Structure      | `part_of`       | child → parent         | Inverse of contains       |
| Structure      | `instance_of`   | specific → general     | X is a case of Y          |
| Derivation     | `extends`       | child → parent         | Builds on, generalizes    |
| Derivation     | `variant_of`    | variant → original     | Modified version          |
| Derivation     | `introduced_by` | concept → paper/person | First described in        |
| Derivation     | `supersedes`    | new → old              | Replaces entirely         |
| Provenance     | `derived_from`  | derived → source       | Data/artifact lineage     |
| Temporal       | `precedes`      | earlier → later        | Ordering over time        |
| Dependency     | `depends_on`    | consumer → dep         | Hard requirement          |
| Dependency     | `enables`       | prerequisite → outcome | Makes possible            |
| Implementation | `implements`    | code → concept         | Code realizes algorithm   |
| Lateral        | `competes_with` | A ↔ B                  | Alternative approaches    |
| Lateral        | `composed_with` | A ↔ B                  | Used together             |
| Annotation     | `annotates`     | note → any substrate   | Note observes/comments on |
| Epistemic      | `supports`      | evidence → claim       | Evidence for a claim      |
| Epistemic      | `refutes`       | evidence → claim       | Evidence against a claim  |

**Direction matters.** `introduced_by` goes FROM the concept TO the paper (the concept was
introduced by the paper). If you get direction wrong, the traversal breaks.

**Weight**: 1.0 = definitional, 0.7-0.9 = strong, 0.4-0.6 = plausible.

### 4. Create notes (observations, insights, decisions)

For anything worth recording that isn't a nameable entity:

```
create(kind="note", note_kind="<kind>", content="<the observation>",
  salience=<0.0-1.0>, annotates=["<entity-uuid>"])
```

**5 note kinds**: `observation` (I noticed), `insight` (I concluded), `question` (I wonder),
`decision` (I chose), `reference` (I read).

Always use `annotates` to attach notes to the entities they're about.

### 5. Verify density

After ingestion, check each new entity:

```
neighbors(node_id="<entity-id>", direction="both")
```

**Minimum edges**: concepts ≥ 4, projects ≥ 3, documents ≥ 2. If below target, add edges — every
concept should have at least `instance_of` or `extends` (parent), `introduced_by` (if a paper
exists), and `competes_with` (if alternatives exist).

### 6. Report

Summarize: what entities were created, what edges link them, what notes captured, and what gaps
remain (questions filed as `question` notes).

## Stop condition

Material exhausted. Every entity above minimum density. No orphans (0-edge nodes). Gaps identified
as `question` notes for follow-up.

## Error handling

If a tool returns an error, read the message — it lists valid values. Common cases:

- Invalid `entity_kind` or `note_kind` → the error says which values are valid
- Invalid `relation` → use only the 17 above
- ID not found → check the UUID; use `search` to find the correct one
