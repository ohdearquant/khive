# khive-pack-kg

The KG verb pack — entity/note CRUD, graph traversal, hybrid search, and
event-sourced proposals for khive's research knowledge graph substrate. This is
the first-party pack shipped with the khive binary; every other pack in this
workspace declares it as a dependency.

## Verbs

17 handlers, registered under [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md):

| Verb        | What it does                                                                    |
| ----------- | ------------------------------------------------------------------------------- |
| `create`    | Create an entity or note (singleton), or a batch of entities (bulk via `items`) |
| `get`       | Fetch any record by UUID (short hex prefix accepted, min 8 chars)               |
| `list`      | List records with optional filtering                                            |
| `update`    | Patch an entity or edge                                                         |
| `delete`    | Soft- or hard-delete a record                                                   |
| `merge`     | Merge two entities                                                              |
| `search`    | Hybrid FTS + vector search over entities or notes                               |
| `link`      | Create a typed directed edge between two entities                               |
| `neighbors` | Immediate graph neighbors of a node                                             |
| `traverse`  | Multi-hop BFS over the graph with relation/depth filters                        |
| `query`     | GQL or SPARQL pattern query compiled to SQL                                     |
| `propose`   | Create an event-sourced KG change proposal                                      |
| `review`    | Approve, reject, or comment on a proposal                                       |
| `withdraw`  | Rescind an open proposal (proposer-only)                                        |
| `verbs`     | List all MCP-callable verbs registered on the server                            |
| `stats`     | Aggregate KG substrate counts (entities, edges, notes)                          |
| `context`   | Entity-anchored graph context in one call (ADR-089)                             |

`propose`/`review`/`withdraw` implement the event-sourced proposal lifecycle from
[ADR-046](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-046-event-sourced-proposals.md).

## Vocabulary

The pack declares 9 entity kinds (`concept`, `document`, `dataset`, `project`,
`person`, `org`, `artifact`, `service`, `resource`) and 5 note kinds
(`observation`, `insight`, `question`, `decision`, `reference`) — see
`KgPack::NOTE_KINDS` / `KgPack::ENTITY_KINDS` in `src/pack.rs`.

It also extends the base edge endpoint contract ([ADR-002](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md))
with `person`/`org`-specific pairs — e.g. `part_of` and `instance_of` from a
`person` entity to an `org` entity, plus several `org`→`org` pairs
(`depends_on`, `enables`, `contains`, `part_of`, `precedes`). This is
pack-extensible per ADR-017; the edge relation enum itself stays closed.

## Usage

Packs are consumed through the MCP `request` tool
([ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)),
not called as a Rust library. A deployment wires `KgPack` onto a
`VerbRegistry` and dispatches verbs by name:

```rust
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

let mut builder = VerbRegistryBuilder::new();
builder.register(KgPack::new(runtime));
let registry = builder.build()?;

let result = registry
    .dispatch(
        "create",
        json!({"kind": "entity", "entity_kind": "concept", "name": "RoPE"}),
    )
    .await?;
```

Over MCP, the same call is issued as a DSL string:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"RoPE\")")
```

`khive-mcp` loads a default set of eleven packs: `kg`, `gtd`, `memory`, `brain`,
`comm`, `schedule`, `knowledge`, `session`, `git`, `code`, `workspace`, with `kg`
always present; `KHIVE_PACKS` / `--pack` select a subset.

## Where this sits

`khive-pack-kg` depends directly on `khive-types`, `khive-runtime`,
`khive-query`, and `khive-storage`, and is registered into the pack runtime that
`khive-mcp` serves. Every other pack in this workspace requires `kg`; the schedule pack's
`schedule.remind` verb additionally requires the registered `comm.send` delivery
capability at creation time, while the rest of the schedule pack works without `comm`.
Governing ADRs:
[ADR-001](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-001-entity-kind-taxonomy.md) (entity kinds),
[ADR-002](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md) (edge relations),
[ADR-013](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-013-note-kind-taxonomy.md) (note kinds),
[ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md) (request DSL),
[ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md) (pack standard),
[ADR-023](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-023-declarative-pack-format.md) (verb surface/visibility),
[ADR-046](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-046-event-sourced-proposals.md) (proposals).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
