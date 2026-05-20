# ADR-055: KG Import/Export Format Adapters

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 establishes `.khive/kg/` NDJSON as the canonical git-tracked format for knowledge graphs.
ADR-051 defines `khive kg import` as the command that loads NDJSON files into the local SQLite
database. This pipeline works well once data is in NDJSON form — but researchers arrive with data
in whatever format their existing tools produce.

The gap is onboarding friction. A researcher with a BibTeX export from Zotero, a CSV from a
spreadsheet, or a Gephi GraphML network cannot use khive without first hand-authoring NDJSON. That
is not a realistic expectation. The "GitHub for knowledge graphs" positioning requires khive to meet
researchers where their data already lives.

The formats that matter for the initial research audience are:

- **CSV/TSV**: ubiquitous. Every spreadsheet tool exports it. Most bibliographic databases offer it.
- **JSON**: array-of-objects exports from APIs, tools, and databases.
- **BibTeX**: the universal export format for citation managers (Zotero, Mendeley, Papers, JabRef).
  Every academic paper database supports it.
- **RDF/Turtle and N-Triples**: the semantic web stack. Significant bodies of linked open data are
  published in these formats.
- **GraphML/GEXF**: the standard exchange formats for network analysis tools (Gephi, Cytoscape,
  NetworkX exports).
- **JSON-LD**: the linked data format used by Schema.org, Wikidata, and many modern APIs.
- **Markdown with wikilinks**: the format of Obsidian, Roam, Logseq, and similar PKM tools.

These formats vary significantly in expressiveness, type systems, and structural conventions. The
adapters must translate them into khive's typed NDJSON with minimal friction while preserving the
schema integrity guarantees established by ADR-048 and ADR-039.

### What changes and what does not

- ADR-048 (`init`, `export`, `import`, `validate`, `diff`, `update`): the `import` command gains a
  `--format` flag. All other ADR-048 commands are unchanged.
- ADR-039 (bulk import, conflict modes `error`/`skip`/`update`): unchanged. All adapter output
  feeds through the same import pipeline and respects these modes.
- ADR-002 (closed edge ontology): unchanged. Adapters must map source relations to canonical
  `EdgeRelation` values or reject them.
- ADR-001 (entity kind taxonomy): unchanged. Adapters must map source type fields to canonical
  entity kinds or reject unknown kinds in strict mode.
- ADR-048 NDJSON format, sort order, and field shape: unchanged. Adapters produce intermediate
  NDJSON consumed by the standard import path.

## Decision

### 1. Pipeline architecture

Adapters are pure functions in the transform stage of a two-stage pipeline:

```
source file
    ↓ adapter (pure transform)
intermediate NDJSON (entities + edges)
    ↓ khive kg import (ADR-048 + ADR-039)
local SQLite database
```

Each adapter takes one or more input files and produces two NDJSON streams: one for entities, one
for edges. Both streams follow the exact ADR-048 record shape. The adapter does not write to the
database directly — it produces NDJSON that the standard `import` command then validates and loads.

This separation has three benefits:

1. Adapters can be tested and debugged independently of the database.
2. The entire validation pipeline (schema compliance, referential integrity, sort order) from
   ADR-048 §validate runs automatically on adapter output.
3. Adding a new format requires only an adapter; no changes to the import or validation logic.

The command form is:

```
khive kg import --format <fmt> [--mapping <file>] [--infer|--strict|--map] [--on-conflict <mode>] <source>
```

When `--format` is omitted, `import` infers the format from the file extension (`.ndjson` → native;
`.csv`, `.tsv` → csv; `.bib` → bibtex; `.ttl`, `.n3` → turtle; `.nt` → ntriples; `.graphml` →
graphml; `.gexf` → gexf; `.jsonld` → jsonld; `.json` → json; `.md` → markdown). If inference is
ambiguous, an explicit `--format` is required.

### 2. Supported formats

#### P0 — Required for launch

**CSV/TSV** (`--format csv` / `--format tsv`)

The most common research format. A CSV file can describe entities, edges, or both, depending on its
columns.

Without a mapping file, `import` applies auto-detection heuristics:

- If the file has columns named `source` and `target` (case-insensitive), it is treated as an edge
  list. The `relation` column is required; `weight` is optional (defaults to `0.7`).
- Otherwise, it is treated as an entity list. The `id` column is optional (a UUID is generated if
  absent). The `name` column is required. The `kind` column is required unless a default kind is
  specified via `--default-kind`.
- Headers are always read from the first row. There is no headerless CSV mode.

With a mapping file (`--mapping import-mapping.yaml`), the file controls column-to-field mapping:

```yaml
format: csv
entities:
  id: uuid              # CSV column "uuid" → entity id (auto-generate if column absent)
  name: title           # CSV column "title" → entity name
  kind: type            # CSV column "type" → entity kind (requires kind_mapping or --infer)
  description: abstract # CSV column "abstract" → entity description (optional)
  properties:           # additional columns → entity properties
    year: year
    authors: authors
    doi: doi
edges:
  source: from_id       # CSV column "from_id" → edge source
  target: to_id         # CSV column "to_id" → edge target
  relation: rel_type    # CSV column "rel_type" → EdgeRelation
  weight: confidence    # CSV column "confidence" → weight (optional, default 0.7)
kind_mapping:           # normalize CSV values to canonical entity kinds
  "paper": concept
  "tool": project
  "author": person
  "organization": org
relation_mapping:       # normalize CSV values to canonical EdgeRelation strings
  "wrote": "introduced_by"
  "cites": "depends_on"
```

A single CSV file can contain both entity and edge rows by defining both the `entities` and `edges`
sections. The adapter reads the file twice, once per section, filtering rows by the presence of
required columns.

**JSON** (`--format json`)

Expects a JSON array of objects at the top level. Each object becomes one entity. Without a mapping
file, the adapter maps keys directly to entity fields (case-insensitive): `id`, `name`, `kind`,
`description`. All other keys are collected into `properties`. The `kind` field is required unless
`--default-kind` is specified.

With a mapping file, the `entities` section of the mapping YAML applies (same shape as CSV mapping,
with JSON key paths instead of column names).

Edge objects are detected by the presence of `source` and `target` fields. Mixed arrays (entities
and edges in the same file) are supported.

#### P1 — Required before ecosystem release

**BibTeX** (`--format bibtex`)

Each BibTeX entry becomes one entity with `kind: concept` and `properties.type: "paper"`. The
mapping from BibTeX fields to entity fields is fixed and not configurable:

| BibTeX field | Entity field |
|---|---|
| citation key | `name` (if no `title`) |
| `title` | `name` |
| `abstract` | `description` |
| `author` | `properties.authors` |
| `year` | `properties.year` |
| `journal` / `booktitle` | `properties.venue` |
| `doi` | `properties.doi` |
| `url` | `properties.source` (prefixed `url:`) |
| `eprint` (with `archivePrefix=arXiv`) | `properties.source` (prefixed `arxiv:`) |

Cross-references (`crossref` field) generate `depends_on` edges between entries. `@string`
expansions are resolved before mapping.

The adapter uses a lenient parser that accepts common BibTeX dialect variations (missing braces
around title words, unbalanced quotes in values). Parse errors are warnings, not failures — the
entry is skipped and reported in the import summary.

**RDF/Turtle and N-Triples** (`--format turtle` / `--format ntriples`)

RDF triples are mapped to entities and edges as follows:

- **Subjects** (`rdf:type` declarations): each subject with an `rdf:type` triple becomes an entity.
  The RDF class maps to entity `kind` via the `kind_mapping` section in the mapping file, or via
  `--infer` if no mapping is provided.
- **Object properties** (relations to other subjects): become edges. The RDF predicate maps to
  `EdgeRelation` via the `relation_mapping` section in the mapping file.
- **Datatype properties** (literal values): become entity `properties` entries.
- **Blank nodes**: are expanded inline — their triples are merged into the entity of the subject
  that references them. Cyclic blank node references are an error.
- **Namespace prefixes**: are resolved before mapping. A prefix mapping file (YAML) can alias
  common namespaces: `schema: http://schema.org/`.

Without a mapping file, the adapter produces entities of `kind: concept` and emits a warning for
every unmapped predicate. The `--infer` flag adds unmapped classes to `schema.yaml` as new entity
kinds (minor version bump).

**JSON-LD** (`--format jsonld`)

JSON-LD documents are first expanded to canonical RDF (using the JSON-LD 1.1 expansion algorithm)
and then processed via the Turtle/N-Triples adapter. The `@context` is resolved before expansion;
remote contexts are fetched and cached locally (in `.khive/kg/.remote-cache/jsonld-ctx/`).

#### P2 — Target for v0.6

**GraphML** (`--format graphml`)

GraphML `<node>` elements become entities. `<edge>` elements become edges. The `id` attribute on
both becomes the khive UUID (or, if not UUID-format, a deterministic UUID5 derived from the
namespace `ohdearquant/khive/graphml` and the original ID string). Node and edge `<data>` elements
map to entity properties or edge fields via the mapping file.

**GEXF** (`--format gexf`)

GEXF nodes and edges are mapped identically to GraphML. GEXF `<attributes>` declarations inform
the property schema. Dynamic GEXF (time-sliced nodes/edges) is supported by taking the final
timeslice (highest end timestamp); the `--timeslice` flag selects a specific slice.

**Markdown with wikilinks** (`--format markdown`)

Each `.md` file becomes one entity. The filename (without extension) becomes the entity name. A
`kind` property in YAML frontmatter becomes the entity kind. All other frontmatter keys become
entity properties. `[[wikilinks]]` in the document body become edges. The relation is inferred from
the section heading containing the wikilink:

| Section heading pattern | Edge relation |
|---|---|
| `## References`, `## Bibliography` | `depends_on` |
| `## See Also`, `## Related` | `competes_with` |
| `## Implements`, `## Implementation` | `implements` |
| `## Extends`, `## Based On` | `extends` |
| `## Part Of`, `## Components` | `part_of` |
| (no matching section) | `annotates` |

A `--vault` flag points to an Obsidian vault directory; all `.md` files in the vault are imported
as a batch. Wikilinks are resolved relative to the vault root. Unresolved wikilinks (no matching
file) become stub entities with `kind: concept` and a `properties.status: "stub"` marker.

### 3. Mapping files

A mapping file (`.khive/kg/import-mapping.yaml` by convention, overridable via `--mapping`) is a
YAML document that controls how source format fields map to khive entity/edge fields.

A mapping file can be generated interactively for CSV and JSON when no `--mapping` is provided and
the terminal is a TTY:

```
$ khive kg import --format csv papers.csv
No mapping file found. Detected columns: id, title, authors, year, type, abstract, from_id, to_id, rel
Auto-detected: entity columns (id→id, title→name, type→kind, abstract→description)
              edge columns (from_id→source, to_id→target, rel→relation)
Proceed with auto-detected mapping? [Y/n] Y
Save mapping as .khive/kg/import-mapping.yaml for future imports? [Y/n] Y
```

In non-TTY contexts (CI, scripted workflows), the absence of a mapping file is not an error — the
auto-detection heuristics run silently.

### 4. Schema handling

When adapter output references entity kinds or edge relations not in the current `schema.yaml`,
behavior is controlled by three flags:

- `--strict` (default): reject records with unknown kinds or relations. The import fails with a
  structured error listing every violation. This is the correct mode for maintaining a curated KG.
- `--infer`: accept unknown kinds and relations. Add new entity kinds to `schema.yaml#entity_kinds`
  and new relations to `schema.yaml#edge_relations`. The schema version minor component is
  incremented. The import summary reports every schema addition made.
- `--map <mapping-file>`: translate source types/relations to existing schema values using the
  `kind_mapping` and `relation_mapping` sections of the mapping file. Unknown values that have no
  mapping entry are still rejected. This is the mode for importing data from a different ontology
  into an existing curated schema.

Only one of `--infer` and `--map` may be specified. `--strict` is implied when neither is present.

Schema additions made by `--infer` are written to `schema.yaml` before the import proceeds. If the
import subsequently fails (validation error, referential integrity failure), the `schema.yaml`
changes are not rolled back — the added kinds/relations remain. This is intentional: the schema
expansion is a deliberate act separate from whether the data load succeeded.

### 5. Validation on import

All adapter output passes through the full `khive kg validate` pipeline before database writes:

1. **Schema compliance**: every entity `kind` appears in `schema.yaml#entity_kinds`. Every edge
   `relation` is a valid `EdgeRelation` string.
2. **Referential integrity**: every edge `source` and `target` UUID resolves to an entity UUID
   present in the adapter output or already in the local database.
3. **Duplicate detection**: duplicate UUIDs (within the adapter output) are an error. Conflicts
   with existing database records are handled by `--on-conflict` (ADR-039: `error` / `skip` /
   `update`).
4. **Endpoint validation**: every edge triple `(source_kind, relation, target_kind)` is checked
   against the same `validate_edge_relation_endpoints` path used by the runtime `link` verb
   (ADR-031, ADR-039). This closes the security hole documented in ADR-039 §context.

Import is all-or-nothing within a single adapter run: on validation failure, no records are written
and the transaction is rolled back.

An import summary is printed to stdout after every run, regardless of success or failure:

```
khive kg import: CSV → NDJSON → SQLite
  source:    papers.csv (1,247 rows)
  entities:  1,203 imported, 0 skipped, 2 errors
  edges:     389 imported, 12 skipped (unknown relation), 0 errors
  schema:    2 kinds inferred (added to schema.yaml) [--infer]
  warnings:  14 (see --verbose for details)
  time:      1.2s
```

The `--verbose` flag appends a structured list of all warnings and errors (with row numbers for
CSV/JSON, entry keys for BibTeX, subject IRIs for RDF).

### 6. Export formats

`khive kg export` gains a `--format` flag that produces adapter-specific output from the local
database. Export is the inverse of import where the format supports round-tripping:

```
khive kg export --format csv > entities.csv
khive kg export --format bibtex > refs.bib
khive kg export --format graphml > graph.graphml
khive kg export --format turtle > kg.ttl
khive kg export --format jsonld > kg.jsonld
khive kg export --format markdown --output-dir ./notes/
```

The `--format ndjson` (default) is unchanged (ADR-048). All other formats are additive.

Export format coverage:

| Format | Import | Export | Notes |
|---|---|---|---|
| NDJSON | yes (ADR-048) | yes (ADR-048) | Canonical; lossless |
| CSV | yes | yes | Entities and edges as separate files |
| JSON | yes | yes | Array-of-objects |
| BibTeX | yes | yes | Only `kind: concept` entities with `properties.type: "paper"` |
| Turtle | yes | yes | All entities and edges as RDF triples |
| N-Triples | yes | yes | Flat RDF, no prefix declarations |
| JSON-LD | yes | yes | `@context` generated from `schema.yaml` |
| GraphML | yes | yes | All entities and edges |
| GEXF | yes | yes | Static (no dynamic timeslicing on export) |
| Markdown | yes | yes | One `.md` file per entity; wikilinks for edges |

Export with `--format markdown` generates a static, browsable representation of the KG. Combined
with a static site generator, this produces human-readable KG documentation from a single command.

### 7. Performance requirements

Large file handling:

- All adapters use streaming parsers. The full source file is never loaded into memory. For CSV,
  this means row-by-row processing via a streaming CSV reader. For JSON, a streaming JSON parser
  (jq-style token stream). For BibTeX, entry-by-entry streaming.
- Database writes use batch INSERT of 50 records per transaction (consistent with ADR-048 §7).
- Progress is reported to stderr for any import that takes more than 2 seconds (entity count, edges
  count, elapsed time, estimated remaining time).

Resume support:

- `--continue` skips entities and edges whose UUID already exists in the database (equivalent to
  `--on-conflict skip` from ADR-039). This allows a large import to be interrupted and resumed
  without re-importing already-loaded records.
- `--continue` is meaningful only with `--on-conflict skip` (implied) or `--on-conflict update`.
  With `--on-conflict error`, `--continue` is rejected as contradictory.

### 8. CLI summary

New flags on `khive kg import`:

| Flag | Values | Default | Description |
|---|---|---|---|
| `--format` | See §2 | inferred from extension | Source format |
| `--mapping` | file path | `.khive/kg/import-mapping.yaml` | Column/field mapping file |
| `--strict` | — | — | Reject unknown kinds/relations (default behavior) |
| `--infer` | — | — | Add unknown kinds/relations to schema.yaml |
| `--map` | — | — | Use mapping file kind_mapping/relation_mapping |
| `--default-kind` | entity kind | — | Kind for entities with no kind column |
| `--timeslice` | datetime | latest | GEXF dynamic: which timeslice to import |
| `--vault` | directory | — | Markdown: Obsidian vault root |
| `--verbose` | — | — | Print detailed warning/error list |
| `--continue` | — | — | Skip already-imported UUIDs (resume) |

New flags on `khive kg export`:

| Flag | Values | Default | Description |
|---|---|---|---|
| `--format` | See §6 | ndjson | Output format |
| `--output-dir` | directory | — | Required for markdown format (one file per entity) |

## Rationale

### Why a pipeline of adapter → NDJSON → standard import rather than format-specific importers

A direct format-specific path (CSV → SQLite, BibTeX → SQLite) would require each adapter to
implement its own conflict resolution, referential integrity checking, endpoint validation, and
transaction management. The pipeline approach means all of that logic lives in one place — the
existing ADR-048/ADR-039 import path — and adapters are thin transforms. Adding a new format is
adding a transform function, not an importer.

### Why auto-detection heuristics for CSV rather than always requiring a mapping file

Requiring a mapping file for every CSV import creates unnecessary friction for the simple and common
case: a CSV with obvious column names (`name`, `kind`, `source`, `target`). The auto-detection
heuristics handle this case with no configuration. The mapping file is the progressive disclosure
layer for cases where the column names don't match the expected patterns.

### Why BibTeX has a fixed mapping rather than a configurable one

BibTeX has a well-established field semantics that has been stable for decades. Every BibTeX
exporter (Zotero, Mendeley, Google Scholar, arXiv) produces `title`, `author`, `year`, `journal`,
`doi` with consistent meaning. A fixed mapping means users get correct results without any
configuration. The cases where a researcher would want to remap BibTeX fields are rare enough to
defer to a later ADR.

### Why `--infer` does not roll back schema additions on import failure

Schema expansion is a deliberate act: the user chose `--infer` knowing that new kinds or relations
would be added. The failure of the subsequent data load is a separate concern. Rolling back schema
additions would create a confusing situation where the user reruns the import, `--infer` expands
the schema again, and the cycle repeats. It is cleaner to let the schema expansion stand and let
the user fix the data validation error independently.

### Why Markdown/wikilinks is P2 rather than P1

The semantic inference required for wikilinks (mapping section headings to edge relations) is
higher-ambiguity than the other formats, where the source has an explicit structure. Shipping it as
P2 allows the simpler formats to be validated with real users before committing to the section
heading heuristics. The format is high-value for PKM users but represents a different user segment
than the CSV/BibTeX/RDF users targeted at launch.

### Why export covers all import formats

Round-trip fidelity (import → export → original format) is a correctness signal and a user
expectation. Researchers need to export to share with colleagues who use other tools. Supporting
export for every import format avoids khive becoming a data sink.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| NDJSON only — require users to convert externally | Zero adapter maintenance burden | Adoption barrier; researchers won't hand-author NDJSON | Contradicts "GitHub for knowledge graphs" positioning |
| Universal import via LLM ("paste your data, AI maps it") | Zero configuration; works for any format | Non-deterministic; slow; cloud dependency for basic import; hard to audit | Deferred as a cloud feature; deterministic adapters ship first |
| Plugin-based adapters (user-installable format plugins) | Extensible; community can add formats | Plugin API surface maintenance; version compatibility overhead | Deferred; start with built-in adapters, plugin system later if needed |
| Always require a mapping file | Explicit, no ambiguity | High friction for common cases; users abandon onboarding | Auto-detection handles common cases; mapping file is progressive disclosure |
| Format-specific importers (CSV → DB, BibTeX → DB) | Fewer indirections | Duplicates validation logic; harder to test | Pipeline to NDJSON concentrates validation in one place |

## Consequences

### Positive

- Researchers with existing data in CSV, BibTeX, RDF, or GraphML can onboard without pre-processing
  their data. This removes the primary adoption barrier identified in user feedback.
- All adapter output passes through the ADR-048 validation pipeline, so the schema integrity
  guarantees are preserved regardless of input format.
- The pipeline architecture means format adapters are testable in isolation without a running
  database. Input file + expected NDJSON output is a complete test case.
- Export symmetry allows khive to participate in existing research workflows rather than requiring
  them to change.
- The `--infer` flag makes it practical to import from richer-than-expected sources (ontologies
  with more types than khive's 6 entity kinds) without losing data or requiring schema ADRs for
  every import.

### Negative

- Adapter maintenance burden: each supported format is a parser that must handle the full range of
  dialect variations in the wild. BibTeX in particular has significant real-world variation.
- The section-heading heuristic for Markdown edge relations (P2) will produce incorrect edges when
  authors use non-standard section names. The default fallback to `annotates` reduces the harm but
  does not eliminate it.
- Streaming parsers for some formats (JSON-LD with remote `@context` fetching; RDF with blank node
  expansion) require buffering expanded triples before they can be sorted into UUID order for NDJSON
  output. For large RDF graphs, this buffer can be significant.
- The `--infer` flag can pollute `schema.yaml` with source-specific types if used carelessly on
  heterogeneous imports. Users who want a curated schema should use `--map` with an explicit
  `kind_mapping` instead.

### Neutral

- Adapter output is temporary NDJSON (written to a temp directory and deleted after import). The
  intermediate files are not committed to git unless the user explicitly runs `khive kg export`
  after import.
- The `--mapping` file is optional for all formats. Its absence triggers auto-detection; its
  presence overrides auto-detection entirely (no partial merging of auto-detected and mapped fields).
- `khive kg export` behavior for the default NDJSON format is unchanged. The new `--format` flag
  is additive.

## Implementation

### Crate changes

A new crate `crates/khive-fmt/` provides the format adapters:

```
crates/khive-fmt/
├── Cargo.toml
└── src/
    ├── lib.rs           — re-exports; FormatAdapter trait
    ├── mapping.rs       — MappingFile: parse import-mapping.yaml, kind_mapping, relation_mapping
    ├── csv.rs           — CsvAdapter: streaming row parser → (EntityRecord, EdgeRecord) streams
    ├── json.rs          — JsonAdapter: streaming object parser → same streams
    ├── bibtex.rs        — BibtexAdapter: lenient entry parser → entity + crossref edges
    ├── turtle.rs        — TurtleAdapter: Turtle/N-Triples → entities + edges via blank node expansion
    ├── jsonld.rs        — JsonLdAdapter: expand → TurtleAdapter pipeline
    ├── graphml.rs       — GraphmlAdapter: SAX-style XML parser → entities + edges
    ├── gexf.rs          — GexfAdapter: SAX-style XML parser → entities + edges
    ├── markdown.rs      — MarkdownAdapter: frontmatter + wikilink extractor
    └── export/
        ├── mod.rs       — export dispatch by format
        ├── csv.rs       — entity/edge → CSV rows
        ├── bibtex.rs    — concept entities → BibTeX entries
        ├── turtle.rs    — entities + edges → RDF Turtle
        ├── jsonld.rs    — entities + edges → JSON-LD (context from schema.yaml)
        ├── graphml.rs   — entities + edges → GraphML
        ├── gexf.rs      — entities + edges → GEXF
        └── markdown.rs  — entities + edges → .md files with wikilinks
```

`khive-fmt` depends on `khive-types` for `EntityRecord`, `EdgeRecord`, and `EdgeRelation`. It does
not depend on `khive-db` or `khive-runtime`. The database write path remains in `khive-vcs`
(import.rs) which calls `khive-fmt` for the source-to-NDJSON transform.

The `FormatAdapter` trait:

```rust
pub trait FormatAdapter: Send {
    fn name(&self) -> &'static str;
    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>>;
    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>>;
}
```

Adapters are stateful (they hold the streaming parser state) but produce immutable record types
from `khive-types`. The callers in `import.rs` drive the iterators, batch-insert records, and
handle `AdapterError` by collecting warnings (non-fatal) or aborting (fatal).

### Schema inference integration

When `--infer` is active, `import.rs` passes unknown kind and relation strings to a
`SchemaInferrer` that accumulates additions and flushes them to `schema.yaml` before the first
database write. The `SchemaYaml` type in `khive-vcs/src/schema.rs` gains `add_entity_kind` and
`add_edge_relation` methods that mutate in memory and mark the struct dirty; `flush_to_disk` writes
only if dirty.

### Phasing

| Phase | Scope | Target |
|---|---|---|
| 1 | `khive-fmt` crate skeleton + `FormatAdapter` trait + `mapping.rs` + `csv.rs` + `json.rs` | v0.5 |
| 2 | `bibtex.rs` + `turtle.rs` (N-Triples subset first, full Turtle second) + export/csv + export/bibtex | v0.5 |
| 3 | `graphml.rs` + `gexf.rs` + `jsonld.rs` + corresponding exports | v0.6 |
| 4 | `markdown.rs` + export/markdown (static site output) + `--vault` flag | v0.6 |
| 5 | Interactive mapping generation (TTY auto-detect + save prompt) | v0.6 |

Phases 1 and 2 cover the primary research audience (CSV, JSON, BibTeX, basic RDF). Phases 3–5
are independent and can ship in either order based on user demand signals.

## References

- ADR-001: Entity Kind Taxonomy — entity kinds validated on import
- ADR-002: Edge Ontology — closed edge relation set enforced by adapters
- ADR-014: Curation Operations — post-import correction workflow
- ADR-022: Schema Migrations — schema.yaml version bump format
- ADR-029: Authorization Gate — namespace scoping enforced during import
- ADR-031: Pack-Extensible Edge Endpoints — endpoint validation path used by adapter pipeline
- ADR-039: Bulk Import Adapters — conflict modes (`error`/`skip`/`update`) reused by this ADR
- ADR-048: Git-Native KG Versioning — NDJSON format, field shapes, sort rules, `import` command
- ADR-051: CLI Authentication and KG Git Workflow Commands — CLI command surface context
- NDJSON specification: https://ndjson.org/
- BibTeX format reference: https://www.bibtex.org/Format/
- JSON-LD 1.1 specification: https://www.w3.org/TR/json-ld11/
- GraphML specification: http://graphml.graphdrawing.org/specification.html
- GEXF 1.3 specification: https://gexf.net/schema.html
