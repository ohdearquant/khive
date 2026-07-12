# khive-vcs-adapters Design

## ADR Compliance

### KG Import/Export Adapters (ADR-036)

- This crate implements the format adapter layer of the two-stage KG import pipeline.
- Adapters are pure transforms: they parse a source format and produce `EntityRecord`/`EdgeRecord`
  streams with no database access and no ID generation side-effects.
- Fatal errors (missing required fields, unknown kinds/relations, out-of-range weights) abort the
  iterator immediately. Non-fatal warnings accumulate in `FormatAdapter::warnings()` and are
  available after the iterator is exhausted.
- Field lookup is case-insensitive: keys are matched by ASCII-lowercase comparison, allowing
  `"Name"`, `"name"`, and `"NAME"` to all resolve to the `name` field.
- Unknown entity keys fold into the `properties` map rather than being rejected.
- Schema mode strictness applies only to entity kinds; edge relations are always validated
  against the closed set regardless of schema mode.
- Phase P0 formats: `csv`, `tsv`, `json`, `ndjson`. Deferred to P1/P2: BibTeX, Turtle,
  JSON-LD, streaming JSON, GraphML, GEXF, Markdown.

### Git-Native KG Implementation (field shapes) (ADR-020)

- `EntityRecord` and `EdgeRecord` follow the wire shapes specified for the import pipeline.
- `EntityRecord` carries `id`, `kind`, `name`, `description?`, `properties`, `tags`.
- `EdgeRecord` carries `edge_id`, `source`, `target`, `relation`, `weight`, `properties`.
- The adapter layer produces these shapes; the standard `khive kg import` pipeline validates
  and loads them into `working.db`.

### ADR-001: Entity Kind Taxonomy

- `EntityRecord.kind` must be one of the 8 canonical entity kinds: `concept`, `document`,
  `dataset`, `project`, `person`, `org`, `artifact`, `service`.
- Validation uses `khive_types::EntityKind::from_str` at parse time, which also handles
  recognized aliases (e.g. `paper` → `document`).
- Unknown kinds produce `AdapterError::UnknownKind` — never silently defaulted.
- Missing `kind` is a fatal `AdapterError::MissingField`.

### Edge Ontology (ADR-002)

- `EdgeRecord.relation` must be one of the 15 canonical edge relations.
- Validation uses `khive_types::EdgeRelation::from_str` at parse time.
- Unknown relations always produce `AdapterError::UnknownRelation`, regardless of schema mode.
- `EdgeRecord.weight` must be finite and in `[0.0, 1.0]`. Out-of-range values produce
  `AdapterError::InvalidField`. Default when absent: `0.7`.

## Consistency Notes

- ADR-036 §7 specifies streaming JSON parse (requiring an `impl Read` pipeline). The current
  `JsonFormatAdapter` uses eager `serde_json::from_str` — the full source is loaded before
  iteration. Streaming is deferred to P1. This is documented in `docs/protocol.md`.
- The `PHASE0_FORMATS` constant in `lib.rs` includes `csv` and `tsv`, but no CSV/TSV adapters
  are implemented yet. These are P0 aspirational entries.
