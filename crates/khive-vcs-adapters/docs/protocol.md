# khive-vcs-adapters: Adapter Protocol

**Scope:** Format adapters for the two-stage KG import pipeline (ADR-036).
**ADR links:** [ADR-020](../../../docs/adr/ADR-020-git-native-kg-implementation.md) | [ADR-036](../../../docs/adr/ADR-036-kg-import-export-adapters.md)
**Modules:** `adapter` (trait) | `json_adapter` (JSON array impl) | `record` (wire shapes) | `error` (error enum)
**Tests:** `tests/json_adapter_tests.rs`
**Last reviewed:** 2026-06-06

## Pipeline

```text
source file
    | adapter (pure transform — no DB access, no ID generation)
intermediate NDJSON (EntityRecord + EdgeRecord, in-memory)
    | khive kg import (validates + loads into working.db)
working.db
```

Adapters are stateless between records. Fatal errors (missing required fields, unknown
kinds/relations, out-of-range weights) abort the iterator. Non-fatal warnings accumulate
in `FormatAdapter::warnings()`.

## Taxonomy Invariants

- `EntityRecord.kind` — must be one of the 8 ADR-001 canonical kinds; validated via
  `khive_types::EntityKind::from_str` at parse time. Unknown kinds return `AdapterError::UnknownKind`.
- `EdgeRecord.relation` — must be one of the 15 ADR-002 canonical relations; validated via
  `khive_types::EdgeRelation::from_str` at parse time. Unknown relations return `AdapterError::UnknownRelation`.
- `EdgeRecord.weight` — must be finite and in `[0.0, 1.0]`. Out-of-range values return
  `AdapterError::InvalidField`. Default when absent: `0.7`.

## JSON Array Format (P0)

The `JsonFormatAdapter` accepts a JSON array of objects. Dispatch:

- Object with `source`/`from` **and** `target`/`to` keys → edge record
- All other objects → entity record

Field lookup is case-insensitive (ADR-036 §2). Unknown entity keys fold into `properties`.

**Parse strategy:** Eager `serde_json::from_str` — full source loaded before iteration.
ADR-036 §7 calls for streaming; deferred to P1 (requires `impl Read` pipeline wiring).

## Deferred Formats

| Priority | Format                                                  |
| -------- | ------------------------------------------------------- |
| P1       | BibTeX, Turtle/N-Triples, JSON-LD; streaming JSON parse |
| P2       | GraphML, GEXF, Markdown                                 |
