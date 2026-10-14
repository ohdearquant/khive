# khive-vcs-adapters Design

## ADR Compliance

### KG Import/Export Adapters (ADR-036)

- This crate implements the format adapter layer of the two-stage KG import pipeline.
- Adapters are pure transforms: they parse a source format and produce `EntityRecord`/`EdgeRecord`
  streams with no database access; records missing an ID receive a freshly generated UUID at parse time.
- Eager structural/classification errors fail adapter construction. Per-record validation failures
  are retained as `Result::Err` items in the corresponding entity/edge iterator; callers that need
  all-or-nothing import must drain and collect both iterators before writing. Non-fatal warnings
  accumulate in `FormatAdapter::warnings()`.
- Field lookup is case-insensitive: keys are matched by ASCII-lowercase comparison, allowing
  `"Name"`, `"name"`, and `"NAME"` to all resolve to the `name` field.
- Required entity labels and edge endpoint strings are non-blank after trimming, while their
  original accepted bytes are preserved in the adapter record.
- Unknown entity keys fold into the `properties` map rather than being rejected.
- Schema mode strictness applies only to entity kinds; edge relations are always validated
  against the closed set regardless of schema mode.
- Phase P0 formats: `csv`, `tsv`, `json`, `ndjson`. BibTeX P1 import is implemented.
  Deferred to P1/P2: Turtle,
  JSON-LD, streaming JSON, GraphML, GEXF, Markdown.

### Git-Native KG Implementation (field shapes) (ADR-020)

- `EntityRecord` and `EdgeRecord` follow the wire shapes specified for the import pipeline.
- `EntityRecord` carries `id`, `kind`, `entity_type?`, `name`, `description?`, `properties`,
  `tags`, `created_at?`, and `updated_at?`.
- `EdgeRecord` carries `edge_id`, `source`, `target`, `relation`, `weight`, `properties`,
  `created_at?`, and `updated_at?`.
- The adapter layer produces these shapes; the standard `khive kg import` pipeline validates
  and loads them into `working.db`.

### ADR-001: Entity Kind Taxonomy

- `EntityRecord.kind` must pass the default 8-base-kind validation: `concept`, `document`,
  `dataset`, `project`, `person`, `org`, `artifact`, `service`. Pack-defined kinds such as
  `resource` (ADR-048) require the caller-supplied kind registry (`new_with_valid_kinds`).
- Validation uses `khive_types::EntityKind::from_str` at parse time, which also handles
  recognized aliases (e.g. `paper` → `document`).
- Unknown kinds produce `AdapterError::UnknownKind` — never silently defaulted.
- Missing `kind` is a fatal `AdapterError::MissingField`.

### Edge Ontology (ADR-002)

- `EdgeRecord.relation` must be one of the 20 canonical relations (ADR-002 non-epistemic base 18, including `links_to` via ADR-191, `located_in` via ADR-196, and `owns` via ADR-197, plus the ADR-055 epistemic pair).
- Validation uses `khive_types::EdgeRelation::from_str` at parse time.
- Unknown relations always produce `AdapterError::UnknownRelation`, regardless of schema mode.
- `EdgeRecord.weight` must be finite and in `[0.0, 1.0]`. Out-of-range values produce
  `AdapterError::InvalidField`. Default when absent: `0.7`.

## Consistency Notes

- ADR-036 §7 specifies streaming JSON parse (requiring an `impl Read` pipeline). The current
  `JsonFormatAdapter` uses eager `serde_json::from_str` — the full source is loaded before
  iteration. Streaming is deferred to P1. This is documented in `docs/api/adapter-protocol.md`.
- CSV/TSV use the eager `CsvFormatAdapter`. The `PHASE0_FORMATS` constant remains a
  historical phase list, so it does not include P1 BibTeX.
- BibTeX reads bounded frames from `BufRead` and uses pinned `serde_bibtex` for
  grammar validation. Explicit token capture keeps macro expansion under local
  byte limits. Converted records and reference tables remain buffered; source
  streaming does not imply constant memory for the whole import. See
  [BibTeX API](api/bibtex-adapter.md) for mapping, failure and limit contracts.
