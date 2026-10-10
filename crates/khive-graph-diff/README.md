# khive-graph-diff

Pure graph-state diff for [ADR-101 D5](../../docs/adr/ADR-101-kg-changeset-model.md).
The library takes already-loaded NDJSON and performs no filesystem access, database
queries, clock reads or identifier generation. CLI/UI consumers own loading and rendering.

```rust
use khive_graph_diff::{diff, GraphState};

let before = GraphState::from_ndjson("", "", "")?;
let after = GraphState::from_ndjson(
    r#"{"id":"00000000-0000-0000-0000-000000000001","name":"New"}"#,
    "",
    "",
)?;
let changes = diff(&before, &after);
assert_eq!(changes.entities.added.len(), 1);
# Ok::<(), khive_graph_diff::DiffInputError>(())
```

## Input contract

`GraphState::from_ndjson(entities, edges, notes)` accepts three separate strings.
Every nonblank physical line must be a JSON object. Entities and notes require `id`;
edges require `edge_id`, matching the existing graph NDJSON representation. IDs use
`khive_types::Id128` parsing: 32 hexadecimal digits or the dashed UUID form, either
case. Output uses canonical lowercase dashed UUIDs. Duplicate normalized IDs within
one substrate refuse, including identical duplicate records. The same ID in different
substrates remains separate.

Blank lines are ignored. Typed `DiffInputError` variants distinguish malformed JSON,
non-object records, missing/non-string/invalid identities and duplicates, with substrate
and one-based physical line. JSON and UUID parsing errors retain their source. The
parser returns the first error in entity, edge, then note stream order.

All non-identity fields are retained. The parser does not validate domain kinds,
relations, namespaces, endpoint existence or property schemas. It does not fill in
timestamps, infer missing fields or silently drop records. JSON duplicate object keys
follow serde_json's existing last-value-wins behavior; duplicate _record identities_
always refuse.

## Diff contract

`diff(&before, &after)` returns entity, edge and note collections, each containing
`added`, `removed` and `modified`, ordered by UUID ascending. Added/removed records
contain canonical `id` plus their full non-identity `fields` map. The extracted input
identity is not repeated inside that map. Modified records contain `id` and field
changes ordered lexicographically by name.

Changes compare top-level fields. Nested objects and arrays are complete typed JSON
values: arrays preserve order and a nested change replaces that top-level field's value.
Every supplied field participates, including timestamps and unknown future fields.
Recursive object-key sorting makes serialized results independent of input key order,
including builds that enable serde_json's `preserve_order` feature. Values follow
serde_json's typed equality; numeric spellings are not preserved as source text.

Missing and null stay distinct. A missing side of `FieldChange` is omitted during
serialization; a present null emits `"before":null` or `"after":null`. The result DTOs
implement Serialize only: ordinary `Option<Value>` deserialization would erase this
distinction. Inputs are private validated states and are never mutated by diffing.

## Portability checks

The existing `wasm-parity` CI job builds this crate for `wasm32-unknown-unknown`, then
runs the same public integration suite natively and under `wasm32-wasip1` with the
pinned wasmtime runner. It compares nonempty test-result sets and the two actual
serialized results emitted by the public-API golden fixture. The harness prints data
on its own lines; the library itself has no I/O. CI requires exactly two data lines,
compares complete bytes, and checks that changed, missing and empty transcripts fail
the same comparator. Existing changeset parity checks remain intact.

This crate supplies graph-state comparison only. It does not apply changes, invert
operation lists, create graph records or change existing CLI/runtime consumers.
