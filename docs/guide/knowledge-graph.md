# Knowledge Graph Modeling

The knowledge graph is for keeping a durable, queryable model of a body of
work: the things that exist, the claims and decisions made about them, and the
relationships between them. Use it when a fact should remain useful after the
session that produced it.

You work with two kinds of record directly:

- **Entities** are independently identifiable things: a paper, method,
  repository, person, or dataset. They give the graph its stable structure.
- **Notes** capture time-bound work about those things: an observation, a
  conclusion, a question, a decision, or a reference. Notes can be linked back
  to their subject.

An edge is a typed relationship within the entity graph layer, not a record you
create on its own. Entities and notes are two of khive's three storage
substrates; the third, events, is an immutable audit log the system writes and
you query, not something you author directly. The entity, note, and edge
vocabularies are all closed sets. Model distinctions that do not belong in a
kind or relation as properties instead of inventing a new label.

Full verb signatures and return shapes live in the [KG pack rustdoc](https://docs.rs/khive-pack-kg/latest/khive_pack_kg/) and the
[`request` tool rustdoc](https://docs.rs/khive-mcp/latest/khive_mcp/tools/request/struct.RequestParams.html).
The [API reference](api-reference.md) is the local wire reference.

## Choose the right record

Create an entity when it has a stable name and would make sense outside the
current session. Create a note when it records what was learned, decided, or
asked during work. A useful pattern is to create the entity first, then add
notes that annotate it.

| Entity kind | Use for                                                                   |
| ----------- | ------------------------------------------------------------------------- |
| `concept`   | Methods, algorithms, architectures, theories, and models                  |
| `document`  | Papers, reports, posts, and books                                         |
| `dataset`   | Benchmarks, corpora, and evaluation sets                                  |
| `project`   | Repositories, libraries, tools, and frameworks                            |
| `person`    | Authors, researchers, and engineers                                       |
| `org`       | Companies, labs, and institutions                                         |
| `artifact`  | Checkpoints, packages, images, and binaries                               |
| `service`   | APIs, endpoints, and hosted products                                      |
| `resource`  | Reusable operational content such as atoms, skills, prompts, and runbooks |

These nine entity kinds are closed. Use `entity_type`, tags, and properties for
finer distinctions; for example, a `document` can have `entity_type="paper"`.

| Note kind     | Use for                                    |
| ------------- | ------------------------------------------ |
| `observation` | A factual capture or finding               |
| `insight`     | A conclusion synthesized from observations |
| `question`    | An unresolved inquiry                      |
| `decision`    | A choice and its rationale                 |
| `reference`   | A pointer to an external source            |

These five note kinds are also closed. `observation` is the default for a KG
note when no note kind is supplied.

## Model relationships deliberately

Edges are directed unless noted otherwise. Their 17 relation names are grouped
by purpose below; the set is closed.

| Group          | Relations                                              | Typical reading                                    |
| -------------- | ------------------------------------------------------ | -------------------------------------------------- |
| Structure      | `contains`, `part_of`, `instance_of`                   | parent → child, child → parent, specific → general |
| Derivation     | `extends`, `variant_of`, `introduced_by`, `supersedes` | newer or derived idea → its predecessor or source  |
| Provenance     | `derived_from`                                         | output → input                                     |
| Temporal       | `precedes`                                             | earlier → later                                    |
| Dependency     | `depends_on`, `enables`                                | consumer → dependency; prerequisite → outcome      |
| Implementation | `implements`                                           | code or project → concept                          |
| Lateral        | `competes_with`, `composed_with`                       | peer relationship; both are symmetric              |
| Annotation     | `annotates`                                            | note → its subject                                 |
| Epistemic      | `supports`, `refutes`                                  | evidence → claim                                   |

The endpoint rules are part of the model, not suggestions. `annotates` is the
cross-substrate relation. `supersedes`, `supports`, and `refutes` are
same-substrate only: entity → entity or note → note. The source of a
`supports` or `refutes` edge is evidence; the target is the claim. The remaining
base relations are entity → entity, subject to their specific allowlist.

## Work through the request DSL

All KG verbs are called through the single `request` MCP tool. A `|` chain runs
left to right and lets the next operation read the preceding result through
`$prev`. A batch in `[...]` runs independently and has no ordering guarantee.

Create a concept and connect it to an existing document in one chain. Replace
the document UUID with an ID returned by `search` or `resolve`.

```
request(ops="create(kind=\"concept\", name=\"Grouped-query attention\") | link(source_id=$prev.id, target_id=\"<document-uuid>\", relation=\"introduced_by\")")
```

Find the best matching entity, then inspect all of its immediate neighbors.

```
request(ops="search(kind=\"entity\", query=\"grouped-query attention\", limit=1) | neighbors(node_id=$prev[0].id, direction=\"both\")")
```

Explore a small, relation-filtered neighborhood from a known root UUID.

```
request(ops="traverse(roots=[\"<root-uuid>\"], max_depth=2, direction=\"both\", relations=[\"extends\", \"variant_of\", \"introduced_by\"])")
```

For discovery, start with `search` for content, `resolve` for a name or other
human reference, and `neighbors` or `traverse` for graph structure. Use
`verbs(pack="kg")` to inspect the available KG surface. `context` is useful when
an agent needs a compact, bounded 1–2-hop view around selected entities. Use
`list` and `stats` for structured browsing and graph health; use `query` for
read-only GQL or SPARQL pattern matching. `propose`, `review`, and `withdraw`
provide a reviewed change workflow when direct mutation is not appropriate.

## Behavior worth knowing

- `create`, `list`, and `search` need a `kind`. Supply a substrate such as
  `entity` or `note`, or a granular kind such as `concept` or `decision`.
- `get`, `update`, and `delete` are by-ID operations. Use a UUID rather than a
  natural-language name; use `resolve` when you have a human reference.
- `neighbors` returns edges in both directions by default. Pass
  `direction="outgoing"` or `direction="incoming"` to restrict to one direction.
- `merge` returns `kept_id`, not `id`; a following chain step must use
  `$prev.kept_id`.
- `query` is read-only. Use `create`, `link`, `update`, `merge`, or `delete` to
  mutate the graph.

## Gotchas

- Do not create a second entity for a spelling variant or alternate title.
  Search first; if it is a duplicate, use `merge` rather than splitting its
  edges across records.
- `supersedes` preserves the old record and marks the replacement relationship;
  it does not delete history.
- Direction matters: a concept is `introduced_by` a document, while a project
  `implements` a concept. Read each relation as source → target before linking.
- An edge can be rejected even with a valid relation name when its source and
  target kinds are not an allowed pair. Do not substitute an ad-hoc relation to
  work around that validation.
- `delete` is soft by default. A hard delete permanently removes the record and
  cascades its edges.
- `$prev` refers only to the immediately preceding operation. Split work into
  separate requests if a later step needs an earlier result that the intervening
  step did not return.

For the complete domain vocabulary, see the [core type rustdoc](https://docs.rs/khive-types/latest/khive_types/). For full request
examples and every verb parameter, use the [API reference](api-reference.md).
