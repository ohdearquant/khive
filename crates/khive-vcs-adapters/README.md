# khive-vcs-adapters

Format adapters for the KG import pipeline — parse a source format into the wire
records `khive kg import` validates and loads.

An adapter is a pure, stateless transform: it reads a source and yields `EntityRecord`
/ `EdgeRecord` values. It writes no database state and generates no IDs beyond filling
in a missing UUID. Fatal errors (missing/blank required fields, ambiguous record shapes,
malformed timestamps, unknown entity kind or edge relation, out-of-range weight) abort the corresponding iterator item; non-fatal issues
accumulate as strings retrievable via `FormatAdapter::warnings`.

## Usage

```rust
use khive_vcs_adapters::{FormatAdapter, JsonFormatAdapter};

let input = r#"[
    {"name": "LoRA", "kind": "concept", "tags": ["peft"]},
    {"source": "LoRA", "target": "FullFineTuning", "relation": "competes_with"}
]"#;

let mut adapter = JsonFormatAdapter::new(input).expect("valid JSON array");
for entity in adapter.entities() {
    let entity = entity.expect("no missing/invalid fields in this example");
    println!("{} ({})", entity.name, entity.kind);
}
for edge in adapter.edges() {
    let edge = edge.expect("no missing/invalid fields in this example");
    println!("{} --{}--> {}", edge.source, edge.relation, edge.target);
}
assert!(adapter.warnings().is_empty());
```

`JsonFormatAdapter::new` parses eagerly and dispatches each array element: an object
carrying canonical `source` **and** `target` keys (case-insensitive) becomes an
`EdgeRecord`; every other object becomes an `EntityRecord`. `from`/`to` are ordinary
entity metadata. A complete `kind`+`name` entity signature combined with a complete
`source`+`target` edge signature is rejected as ambiguous. Unknown entity keys fold into
the record's `properties`.

## Taxonomy validation

- `EntityRecord.kind` is validated against `khive_types::EntityKind` at parse time —
  an unrecognized kind returns `AdapterError::UnknownKind`, never a silent default.
- `EdgeRecord.relation` is validated against `khive_types::EdgeRelation` — an
  unrecognized relation returns `AdapterError::UnknownRelation`. This check applies
  regardless of any schema-mode setting elsewhere in the pipeline.
- `EdgeRecord.weight` must be finite and in `[0.0, 1.0]`; deserialization enforces this
  at the JSON boundary as well as via the `JsonFormatAdapter`'s own `extract_weight`
  path. Absent, it defaults to `0.7`.
- Present `created_at`/`updated_at` values must be RFC 3339 strings. Missing timestamps
  remain optional; malformed values are never replaced with import time.

## Format support

JSON arrays (`JsonFormatAdapter`), CSV/TSV (`CsvFormatAdapter`), and
[BibTeX](docs/api/bibtex-adapter.md) (`BibtexFormatAdapter`) are implemented.
BibTeX accepts an `impl BufRead`, frames one entry at a time, expands bounded
`@string` definitions, and resolves crossrefs before returning converted records.
`kkernel kg import` accepts `--format bibtex` and infers `.bib` without case sensitivity.

`PHASE0_FORMATS` (`"csv"`, `"tsv"`, `"json"`, `"ndjson"`) retains the original P0
format list; BibTeX is a P1 format. `AdapterError::NotYetImplemented` remains the
error path for deferred formats: Turtle/N-Triples, JSON-LD, GraphML, GEXF and
Markdown. See `docs/api/adapter-protocol.md` in this crate.

## Where this sits

`khive-vcs-adapters` has no dependency on `khive-storage` or `khive-runtime`. Its output feeds the
standard `khive kg import` pipeline, which is what performs validation and loading
into `working.db`; the adapter itself never touches a database.

Governed by [ADR-036](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-036-kg-import-export-adapters.md).

## License

Apache-2.0.
