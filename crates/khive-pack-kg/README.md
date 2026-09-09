# khive-pack-kg

The KG verb pack — entity/note CRUD, graph traversal, hybrid search, and
event-sourced proposals for khive's research knowledge graph substrate. This is
the first-party pack shipped with the khive binary; every other pack in this
workspace declares it as a dependency.

## Verbs

23 handlers, registered under [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md):

| Verb             | What it does                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create`         | Create an entity or note (singleton), or a batch of entities (bulk via `items`)                                                                                                                                                                                                                                                                                                                       |
| `get`            | Fetch any record by UUID (short hex prefix accepted, min 8 chars)                                                                                                                                                                                                                                                                                                                                     |
| `list`           | List records with optional filtering                                                                                                                                                                                                                                                                                                                                                                  |
| `update`         | Patch an entity or edge                                                                                                                                                                                                                                                                                                                                                                               |
| `delete`         | Soft- or hard-delete a record                                                                                                                                                                                                                                                                                                                                                                         |
| `merge`          | Merge two entities                                                                                                                                                                                                                                                                                                                                                                                    |
| `search`         | Hybrid FTS + vector search over entities or notes                                                                                                                                                                                                                                                                                                                                                     |
| `link`           | Create a typed directed edge between two entities                                                                                                                                                                                                                                                                                                                                                     |
| `neighbors`      | Immediate graph neighbors of a node                                                                                                                                                                                                                                                                                                                                                                   |
| `traverse`       | Multi-hop BFS over the graph with relation/depth filters                                                                                                                                                                                                                                                                                                                                              |
| `query`          | GQL or SPARQL pattern query; GQL supports deterministic `SKIP` paging                                                                                                                                                                                                                                                                                                                                 |
| `propose`        | Create an event-sourced KG change proposal                                                                                                                                                                                                                                                                                                                                                            |
| `review`         | Approve, reject, or comment on a proposal                                                                                                                                                                                                                                                                                                                                                             |
| `withdraw`       | Rescind an open proposal (proposer-only)                                                                                                                                                                                                                                                                                                                                                              |
| `verbs`          | List all MCP-callable verbs registered on the server                                                                                                                                                                                                                                                                                                                                                  |
| `stats`          | Aggregate KG substrate counts (entities, edges, notes)                                                                                                                                                                                                                                                                                                                                                |
| `context`        | Entity-anchored graph context in one call (ADR-089)                                                                                                                                                                                                                                                                                                                                                   |
| `resolve`        | Resolve natural-language references to record ids                                                                                                                                                                                                                                                                                                                                                     |
| `whoami`         | Report the caller identity this request resolved to                                                                                                                                                                                                                                                                                                                                                   |
| `db_diagnostics` | Reader/writer contention, graph-edge integrity, and WAL/checkpoint diagnostics: reader admission capacity/availability, pooled checkouts, separately attributed standalone opens, timeouts and hold lifecycle; aggregate plus pooled/standalone/writer-task writer acquisitions, writer-task failures, swallowed audit failures, duplicate edge-ID/list-ledger counts, checkpoint counters, PASSIVE probe, WAL size, and qualified holder census (probe may backfill WAL frames; never TRUNCATE or create/delete files) |

| `stream.append` | Append immutable JSON with a dense sequence and optional expected_seq precondition |
| `stream.read` | Read an ordered page, head_seq and next_after from one snapshot |
| `stream.stat` | Count entries and read head_seq from one snapshot |

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

`khive-mcp` loads a default set of fourteen packs: `kg`, `gtd`, `memory`, `brain`,
`comm`, `schedule`, `knowledge`, `session`, `tool`, `exec`, `git`, `code`,
`workspace`, `blob`,
with `kg` always present; `KHIVE_PACKS` / `--pack` select a subset.

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

Apache-2.0.


## Ordered streams

`stream.append(stream, record, expected_seq=None, note_kind="observation", tags=None)`
stores any JSON value, including a scalar or null, as an immutable note. Stream
names are namespace scoped and at most 512 UTF-8 bytes, with no U+0000. Each stream
starts at sequence 1. `expected_seq` is the number the new entry must receive;
a mismatch returns `conflict` with string-valued `details.reason="seq_conflict"`,
`stream`, `expected_seq` and `next_seq`, without inserting a note or ledger row.
The result is `{seq, id, created_at}`.

`stream.read(stream, after=0, limit=1000)` returns `{entries, head_seq, next_after}`.
Entries carry `{seq, id, record, created_at}` and are ordered by sequence, strictly
after the supplied cursor. `next_after` is the last returned sequence when more
entries remain, otherwise null. An unknown stream is an empty page with head 0.
`stream.stat(stream)` returns `{head_seq, count}` from a single snapshot; count
and head are read independently to expose any density defect.

Entry content, properties (including tags), kind and namespace are immutable;
soft and hard deletion are refused with `details.reason="stream_member"` and
string-valued `id`, `stream` and `seq`. Display name, salience and decay factor
remain editable. Write a separate annotation note to add information.

To reconcile a lost reply, retry the same `expected_seq`. On conflict, read
`after=expected_seq-1, limit=1` and compare your distinguishable record. A request
chain (`|`) preserves caller order; a request array gives dense numbers in writer
admission order, which need not be array order.

A supplied `fence`, including null, is refused until the later slice with
versioned leases. This slice adds no lease fence, batch verb, retention, drop,
subscription or protocol version change.

```text
request(ops='stream.append(stream="run", record={"step":1}, expected_seq=1)', presentation="verbose")
kkernel exec 'stream.read(stream="run", after=0, limit=1000)' --presentation verbose
```

```python
from khive import Khive, op

client = Khive()
client.batch([op("stream.append", stream="run", record=None, expected_seq=1)])
```

Use verbose JSON for full identifiers and canonical timestamps. Agent presentation
also preserves each stream record exactly and keeps empty pages and terminal cursors.
