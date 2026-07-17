# Format adapter protocol

Format adapters are pure transforms from external data into the intermediate entity and edge
records consumed by khive's two-stage KG import pipeline. The protocol follows
[ADR-020](../../../../docs/adr/ADR-020-git-native-kg-implementation.md) and
[ADR-036](../../../../docs/adr/ADR-036-kg-import-export-adapters.md).

## Pipeline

```text
source file
    | adapter (pure transform — no DB access; missing IDs are minted fresh)
intermediate NDJSON (EntityRecord + EdgeRecord, in-memory)
    | khive kg import (validates + loads into working.db)
working.db
```

## `FormatAdapter`

An adapter reports its short format `name`, exposes separate `entities` and `edges` iterators,
and accumulates non-fatal issues in `warnings`. Implementations write no database state. IDs are accepted when supplied and validated as UUIDs; when an entity omits `id` or an edge omits `edge_id`/`id`, the adapter mints a fresh `Uuid::new_v4()` for that record, so ID-free input is not deterministic across parses. Record-level fatal errors remain in the iterator as `AdapterError` values.

The current modules divide responsibilities as follows: `adapter` defines the trait,
`json_adapter` implements JSON-array parsing, `record` defines the intermediate wire shapes, and
`error` defines the failure taxonomy. Integration coverage lives in
`tests/json_adapter_tests.rs`.

## Taxonomy invariants

- `EntityRecord.kind` — the default adapter validates the 8 base `EntityKind` values (and their
  aliases) via `khive_types::EntityKind::from_str` at parse time. Pack-defined kinds, including
  the ADR-048 `resource` kind, are accepted only when the caller supplies the installed kind
  registry through `new_with_valid_kinds`. Unknown kinds return `AdapterError::UnknownKind`.
- `EdgeRecord.relation` — must be one of the 17 canonical relations (ADR-002 base 15 plus the ADR-055 epistemic pair `supports`/`refutes`); validated via
  `khive_types::EdgeRelation::from_str` at parse time. Unknown relations return `AdapterError::UnknownRelation`.
- `EdgeRecord.weight` — must be finite and in `[0.0, 1.0]`. Out-of-range values return
  `AdapterError::InvalidField`. Default when absent: `0.7`.

## JSON array format

The `JsonFormatAdapter` accepts a JSON array of objects. Dispatch:

- Object with `source`/`from` **and** `target`/`to` keys → edge record
- All other objects → entity record

Field lookup is case-insensitive (ADR-036 §2). Unknown entity keys fold into `properties`.

Parsing is eager through `serde_json::from_str`, so the complete source is loaded before
iteration. ADR-036 section 7 calls for a streaming implementation; that remains P1 work because it
requires `impl Read` pipeline wiring.

## Error boundary

Missing fields, invalid values, structural parse failures, unknown entity kinds, and unknown edge
relations are fatal. Deferred formats return `AdapterError::NotYetImplemented`. Malformed optional
reserved fields (e.g. a non-string `created_at`) produce warnings; absent optional fields are
accepted silently, and unknown non-reserved keys are folded into `properties` without a warning.
Callers inspect `warnings()` after draining the streams.

## Deferred formats

| Priority | Format                                                  |
| -------- | ------------------------------------------------------- |
| P1       | BibTeX, Turtle/N-Triples, JSON-LD; streaming JSON parse |
| P2       | GraphML, GEXF, Markdown                                 |

The protocol and its JSON implementation were last reviewed on 2026-06-06.
