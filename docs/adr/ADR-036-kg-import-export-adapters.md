# ADR-036: KG Import/Export Format Adapters

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

[ADR-020](ADR-020-git-native-kg-implementation.md) establishes `.khive/kg/` NDJSON as the
canonical git-tracked format for knowledge graphs and defines `khive kg import` as the command
that loads NDJSON files into the local SQLite working database. This pipeline works well once data
is in NDJSON form — but researchers arrive with data in whatever format their existing tools produce.

The gap is onboarding friction. A researcher with a BibTeX export from Zotero, a CSV from a
spreadsheet, or a Gephi GraphML network cannot use khive without first hand-authoring NDJSON.
That is not a realistic expectation. khive needs to meet researchers where their data already
lives.

The formats that matter for the initial research audience are:

- **CSV/TSV**: ubiquitous — every spreadsheet tool exports it; most bibliographic databases offer it.
- **JSON array**: array-of-objects exports from APIs, tools, and databases.
- **BibTeX**: the universal export format for citation managers (Zotero, Mendeley, Papers, JabRef).
- **RDF/Turtle and N-Triples**: the semantic web stack; significant bodies of linked open data live
  here.
- **JSON-LD**: the linked data format used by Schema.org, Wikidata, and many modern APIs.
- **GraphML/GEXF**: standard exchange formats for network analysis tools (Gephi, Cytoscape,
  NetworkX).
- **Markdown with wikilinks**: the format of Obsidian, Roam, Logseq, and similar PKM tools.

These formats vary significantly in expressiveness, type systems, and structural conventions.
Adapters must translate them into khive's typed NDJSON with minimal friction while preserving the
schema integrity guarantees established by [ADR-020](ADR-020-git-native-kg-implementation.md).

### What changes and what does not

- `khive kg import` ([ADR-020](ADR-020-git-native-kg-implementation.md) §5, §13): the command
  gains a `--format` flag and associated adapter flags. All existing import semantics
  (`--on-conflict`, transaction model, validation pipeline) are unchanged.
- [ADR-002](ADR-002-edge-ontology.md) closed edge ontology: unchanged. Adapters must map source
  relations to canonical `EdgeRelation` values or reject them.
- [ADR-001](ADR-001-entity-kind-taxonomy.md) entity kind taxonomy: unchanged. Adapters must map
  source type fields to canonical entity kinds, or reject unknown kinds under the active
  `--schema-mode`.
- NDJSON format, sort order, and field shape ([ADR-020](ADR-020-git-native-kg-implementation.md)
  §2): unchanged. Adapters produce intermediate NDJSON consumed by the standard import path.

## Decision

### 1. Pipeline architecture

Adapters are pure transforms in a two-stage pipeline:

```
source file
    | adapter (pure transform — no DB access)
intermediate NDJSON (entities + edges, in-memory or temp file)
    | khive kg import (ADR-020 §13 — validates + loads)
working.db
```

Each adapter consumes one or more input files and produces two record streams: one for entities,
one for edges. Both streams follow the exact [ADR-020](ADR-020-git-native-kg-implementation.md)
§2 record shape. The adapter writes no database state — it produces NDJSON that the standard
`import` command then validates and loads.

Benefits of this separation:

1. Adapters can be tested in isolation without a running database.
2. The full validation pipeline (schema compliance, referential integrity, sort order) from
   [ADR-020](ADR-020-git-native-kg-implementation.md) §6 runs automatically on all adapter
   output.
3. Adding a new format requires only an adapter; no changes to import or validation logic.

The shipped command form is:

```
khive kg import [--format ndjson|csv|tsv|json] [--default-kind <kind>]
                [--on-conflict error|skip|update] [--continue] [--verbose]
                <source>
```

Deferred flags are rejected with "not yet implemented": `--mapping`, `--schema-mode`,
`--vault`, and `--timeslice`.

When `--format` is omitted, the shipped CLI infers only these extensions:

| Extension | Format |
| --------- | ------ |
| `.ndjson` | ndjson |
| `.csv`    | csv    |
| `.tsv`    | tsv    |

JSON import is shipped, but `.json` is intentionally not inferred because both generic JSON
adapter input and KG archive JSON use that extension. Use `--format json` explicitly for
generic JSON arrays. BibTeX, RDF/Turtle, N-Triples, JSON-LD, GraphML, GEXF, and Markdown
are deferred.

### 2. Supported formats and phasing

#### P0 — Shipped

**CSV/TSV** (`--format csv` / `--format tsv`)

Without a mapping file, `import` applies auto-detection heuristics:

- If the file has columns named `source` and `target` (case-insensitive), it is treated as an
  edge list. The `relation` column is required; `weight` is optional (defaults to `0.7`).
- Otherwise, it is treated as an entity list. `name` is required. `kind` is required unless
  `--default-kind` is specified. `id` is optional (a UUID is generated if absent).
- Headers are always read from the first row. There is no headerless CSV mode.

With a mapping file (see §3), the `entities` and `edges` sections control column-to-field
mapping. A single CSV file can describe both entities and edges by defining both sections; the
adapter reads the file twice, filtering rows by the presence of required columns.

**JSON array** (`--format json`)

Expects a JSON array of objects at the top level. Without a mapping file, the adapter maps
keys directly to entity fields (case-insensitive): `id`, `name`, `kind`, `description`. All
other keys collect into `properties`. Edge objects are detected by the presence of `source` and
`target` fields. Mixed arrays (entities and edges in the same file) are supported.

With a mapping file, the `entities` section applies (same shape as CSV mapping, with JSON key
paths instead of column names).

#### P1 — Deferred (before ecosystem release)

**BibTeX** (`--format bibtex`)

Each BibTeX entry becomes one entity with `kind: document` and `entity_type: "paper"` (per
ADR-001's `paper → Document` mapping). The mapping from BibTeX fields to entity fields is fixed
and not configurable via mapping files:

| BibTeX field                       | Entity field                            |
| ---------------------------------- | --------------------------------------- |
| `title`                            | `name`                                  |
| citation key (when `title` absent) | `name`                                  |
| `abstract`                         | `description`                           |
| `author`                           | `properties.authors`                    |
| `year`                             | `properties.year`                       |
| `journal` / `booktitle`            | `properties.venue`                      |
| `doi`                              | `properties.doi`                        |
| `url`                              | `properties.source` (prefixed `url:`)   |
| `eprint` (archivePrefix=arXiv)     | `properties.source` (prefixed `arxiv:`) |

Cross-references (`crossref` field) generate `depends_on` edges between entries. `@string`
expansions are resolved before mapping. The parser is lenient: parse errors are warnings, not
failures — the entry is skipped and reported in the import summary.

**RDF/Turtle and N-Triples** (`--format turtle` / `--format ntriples`)

Mapping rules:

- **Subjects** with an `rdf:type` declaration become entities. The RDF class maps to entity
  `kind` via the `kind_mapping` section of the mapping file, or is passed to `--schema-mode`
  handling when no mapping is provided.
- **Object properties** (relations to other subjects) become edges. The RDF predicate maps to
  `EdgeRelation` via the `relation_mapping` section of the mapping file.
- **Datatype properties** (literal values) become entity `properties` entries.
- **Blank nodes** are expanded inline: their triples are merged into the entity of the
  referencing subject. Cyclic blank node references are an error.
- **Namespace prefixes** are resolved before mapping. A prefix mapping file (YAML) can alias
  common namespaces: `schema: http://schema.org/`.

Without a mapping file, the adapter produces entities of `kind: concept` and emits a warning
for every unmapped predicate. Unknown RDF predicates that cannot be mapped to ADR-002's 15
canonical edge relations are either (a) demoted to entity properties (when
`--predicate-mapping rdf-as-properties` is set), or (b) rejected with a warning listing the
unmappable predicate URIs. `--schema-mode force` does NOT override edge relation validation.

**JSON-LD** (`--format jsonld`)

JSON-LD documents are first expanded to canonical RDF using the JSON-LD 1.1 expansion algorithm,
then processed via the Turtle/N-Triples adapter path. The `@context` is resolved before
expansion; remote contexts are fetched and cached at `.khive/kg/.remote-cache/jsonld-ctx/`.

#### P2 — Deferred (v0.6 target)

**GraphML** (`--format graphml`)

GraphML `<node>` elements become entities; `<edge>` elements become edges. The `id` attribute
on both becomes the khive UUID, or, if not UUID-format, a deterministic UUID5 derived from the
namespace `kkernel/graphml` and the original ID string. Node and edge `<data>` elements map to
entity properties or edge fields via the mapping file.

**GEXF** (`--format gexf`)

GEXF nodes and edges are mapped identically to GraphML. GEXF `<attributes>` declarations inform
the property schema. Dynamic GEXF (time-sliced nodes and edges) is supported by taking the final
timeslice (highest end timestamp); the `--timeslice <datetime>` flag selects a specific slice.

**Markdown with wikilinks** (`--format markdown`)

Each `.md` file becomes one entity. The filename (without extension) becomes the entity name.
A `kind` key in YAML frontmatter becomes the entity kind. All other frontmatter keys become
entity properties. `[[wikilinks]]` in the document body become edges. The relation is inferred
from the section heading containing the wikilink:

| Section heading pattern              | Edge relation   |
| ------------------------------------ | --------------- |
| `## References`, `## Bibliography`   | `depends_on`    |
| `## See Also`, `## Related`          | `competes_with` |
| `## Implements`, `## Implementation` | `implements`    |
| `## Extends`, `## Based On`          | `extends`       |
| `## Part Of`, `## Components`        | `part_of`       |
| (no matching section)                | `annotates`     |

The `--vault <dir>` flag points to an Obsidian vault directory; all `.md` files in the vault
are imported as a batch. Wikilinks are resolved relative to the vault root. Unresolved wikilinks
(no matching file) produce stub entities with `kind: concept` and `properties.status: "stub"`.

### 3. Mapping file

Mapping files are deferred. The shipped CLI rejects `--mapping` with a clear
"not yet implemented" error. Current P0 import uses direct, built-in field mapping:

- CSV/TSV: auto-detect entity rows vs edge rows from header names; `--default-kind`
  supplies a kind when entity rows omit one.
- JSON: parse a top-level array of objects; objects with `source` and `target` become
  edges, other objects become entities; unrecognized keys fold into `properties`.

Interactive mapping generation and `.khive/kg/import-mapping.yaml` are not shipped.

### 4. Schema handling

`--schema-mode` is deferred. The shipped CLI rejects `--schema-mode` with a clear
"not yet implemented" error. Current import validates against existing closed taxonomies:
unknown `entity_kind`, `note_kind`, and `edge_relation` values are rejected through the
normal validation/import path. Schema inference, schema force mode, and atomic schema
publish are deferred.

### 5. Conflict and flag interaction

`--on-conflict` governs how the adapter pipeline handles UUID collisions with records already
in the working database. The three modes follow [ADR-020](ADR-020-git-native-kg-implementation.md)
§13 semantics:

- `error` (default): fail on UUID collision.
- `skip`: omit colliding records and their incident edges.
- `update`: patch existing records using `EntityPatch` semantics from
  [ADR-014](ADR-014-curation-operations.md).

`--continue` is syntactic sugar for `--on-conflict skip`. It is mutually exclusive with any
explicit `--on-conflict` value:

| Combination                       | Result                                       |
| --------------------------------- | -------------------------------------------- |
| `--continue --on-conflict error`  | rejected — contradictory                     |
| `--continue --on-conflict update` | rejected — ambiguous                         |
| `--continue --on-conflict skip`   | rejected — redundant; use `--continue` alone |

`--schema-mode` and `--on-conflict` (or `--continue`) are orthogonal and may be combined
freely. `--mapping` is applied before any schema or conflict mode logic runs.

### 6. Validation

All adapter output passes through the full `khive kg validate` pipeline before any database
write:

1. **Schema compliance**: every entity `kind` appears in `schema.yaml#entity_kinds`. Every edge
   `relation` is a valid `EdgeRelation` string (closed set, [ADR-002](ADR-002-edge-ontology.md)).

2. **Referential integrity**: every edge `source` and `target` UUID resolves to an entity UUID
   present in the adapter output or already in the working database.

3. **Duplicate detection**: duplicate UUIDs within the adapter output are an error. Conflicts
   with existing database records are handled by `--on-conflict`.

4. **Endpoint validation** (deferred to P1): checking every edge triple
   `(source_kind, relation, target_kind)` against the ADR-002 / pack-extensible endpoint rules
   is deferred. The current validate path verifies that each `relation` value is in the closed
   ADR-002 set but does not yet resolve entity kinds at validation time for the full endpoint
   check used by the runtime `link` verb.

Import is all-or-nothing within a single adapter run: on validation failure, no records are
written and the database transaction rolls back.

An import summary is printed to stdout after every run:

```
khive kg import: CSV -> NDJSON -> working.db
  source:    papers.csv (1,247 rows)
  entities:  1,203 imported, 0 skipped, 2 errors
  edges:     389 imported, 12 skipped (unknown relation), 0 errors
  schema:    2 kinds inferred (added to schema.yaml) [--schema-mode infer]
  warnings:  14 (run with --verbose for details)
  time:      1.2s
```

`--verbose` appends a structured list of all warnings and errors with row numbers for CSV/JSON,
entry keys for BibTeX, and subject IRIs for RDF.

### 7. Performance

All adapters use streaming parsers. The full source file is never loaded into memory:

- CSV: row-by-row via a streaming CSV reader.
- JSON: **P0 exception — eager parse.** The P0 `JsonFormatAdapter` uses
  `serde_json::from_str` and buffers all records into `Vec`s before iteration
  starts. This is intentional for the "ship one impl" goal of issue #366. P1
  work item: replace with `serde_json::Deserializer::from_reader` streaming
  deserialization once the CLI pipeline supplies an `impl Read` source.
- BibTeX: entry-by-entry streaming.
- RDF: triple-by-triple; blank node expansion buffers only the blank node subgraph, not the
  full file.

Database writes use a single outer transaction for the entire import. Within that transaction,
INSERT statements are batched at 50 rows per statement, consistent with [ADR-020](ADR-020-git-native-kg-implementation.md)
§13. This is 50 rows per INSERT statement within one outer transaction — not 50 separate
transactions.

Progress is reported to stderr for any import that takes more than two seconds (entity count,
edge count, elapsed time, estimated remaining time).

### 8. Export

`khive kg export` currently produces canonical NDJSON by default and supports compatibility
`--format archive` output. CSV, JSON-array, BibTeX, RDF, GraphML, GEXF, Markdown, and other
non-NDJSON/non-archive export formats are deferred.

Format coverage matrix:

| Format    | Import         | Export   | Phase | Notes                                                          |
| --------- | -------------- | -------- | ----- | -------------------------------------------------------------- |
| NDJSON    | yes            | yes      | P0    | Canonical; lossless                                            |
| Archive   | yes            | yes      | P0    | KG archive JSON envelope                                       |
| CSV       | yes (Deno CLI) | deferred | P0/P1 | Rust crate CSV module is not shipped                           |
| TSV       | yes (Deno CLI) | deferred | P0/P1 | Rust crate TSV module is not shipped                           |
| JSON      | yes            | deferred | P0/P1 | Generic top-level array import; use `--format json` explicitly |
| BibTeX    | deferred       | deferred | P1    |                                                                |
| Turtle    | deferred       | deferred | P1    |                                                                |
| N-Triples | deferred       | deferred | P1    |                                                                |
| JSON-LD   | deferred       | deferred | P1    |                                                                |
| GraphML   | deferred       | deferred | P2    |                                                                |
| GEXF      | deferred       | deferred | P2    |                                                                |
| Markdown  | deferred       | deferred | P2    |                                                                |

### 9. CLI flag reference

Flags on `khive kg import` (P0 — shipped):

| Flag             | Values                   | Default                                     | Description                                                                                   |
| ---------------- | ------------------------ | ------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `--format`       | `ndjson\|csv\|tsv\|json` | inferred for `.ndjson`, `.csv`, `.tsv` only | Source format                                                                                 |
| `--default-kind` | entity kind string       | —                                           | Kind assigned when source has no kind                                                         |
| `--on-conflict`  | `error\|skip\|update`    | `error`                                     | UUID collision handling; legacy aliases `replace` and `merge` are accepted by the current CLI |
| `--continue`     | flag                     | off                                         | Sugar for `--on-conflict skip`; mutually exclusive with explicit `--on-conflict`              |
| `--verbose`      | flag                     | off                                         | Print detailed warning/error list                                                             |

Flags on `khive kg import` (deferred — CLI rejects with "not yet implemented"):

| Flag            | Values                 | Deferred to | Description                |
| --------------- | ---------------------- | ----------- | -------------------------- |
| `--mapping`     | file path              | P1          | Column/field mapping file  |
| `--schema-mode` | `strict\|infer\|force` | P1          | Schema validation behavior |
| `--timeslice`   | datetime               | P2          | GEXF dynamic import slice  |
| `--vault`       | directory              | P2          | Markdown vault root        |

Flags on `khive kg export` (deferred):

| Flag           | Values   | Default | Description                           |
| -------------- | -------- | ------- | ------------------------------------- |
| `--format`     | see §8   | ndjson  | Output format (non-ndjson deferred)   |
| `--output-dir` | dir path | —       | Required for `--format markdown` (P2) |

## Rationale

### 1. Why adapter → intermediate NDJSON → standard import (vs. format-specific importers)

A direct format-specific path (CSV → SQLite, BibTeX → SQLite) would require each adapter to
implement its own conflict resolution, referential integrity checking, endpoint validation, and
transaction management. The pipeline approach concentrates all of that logic in one place — the
existing [ADR-020](ADR-020-git-native-kg-implementation.md) import path — and adapters are thin
transforms. Adding a new format is adding a transform function, not a full importer.

### 2. Why auto-detection heuristics for CSV (vs. always requiring a mapping file)

Requiring a mapping file for every CSV import creates unnecessary friction for the common case:
a CSV with obvious column names (`name`, `kind`, `source`, `target`). Auto-detection handles
this case with no configuration. The mapping file is the progressive-disclosure layer for cases
where column names do not match the expected patterns.

### 3. Why BibTeX has a fixed mapping (vs. a configurable one)

BibTeX has stable field semantics that every major exporter (Zotero, Mendeley, Google Scholar,
arXiv) honors consistently. A fixed mapping means users get correct results without any
configuration. Cases where a researcher would want to remap BibTeX fields are rare enough to
defer to a later ADR.

### 4. Why schema additions from `--schema-mode infer` are not rolled back on import failure

Schema expansion is a deliberate act: the user chose `--schema-mode infer` knowing that new
entity kinds would be added to `schema.yaml`. The failure of the subsequent data load is a
separate concern. Rolling back schema additions would force the user to rerun `--schema-mode
infer` on the next attempt, trigger the same additions, and cycle again. It is cleaner to let
the schema expansion stand and let the user fix the data validation error independently. Note
that edge relations are never added even in `infer` mode — the non-rollback applies only to
entity kind additions.

### 5. Why Markdown/wikilinks is P2 (vs. P1)

The semantic inference required for wikilinks (mapping section headings to edge relations) is
higher-ambiguity than the other formats, which have explicit structure. P2 allows simpler formats
to be validated with real users before committing to the section-heading heuristics. The format
is high-value for PKM users but represents a different user segment than the CSV/BibTeX/RDF
audience targeted at launch.

### 6. Why export is deferred

Export symmetry (import and export for every format) is a long-term goal but is not required for
the initial research audience. The current NDJSON export from [ADR-020](ADR-020-git-native-kg-implementation.md)
covers the primary exchange use case. Non-NDJSON export is deferred to P1/P2 to avoid premature
investment before usage patterns are confirmed.

## Alternatives Considered

| Alternative                                              | Why rejected                                                                        |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| NDJSON only — require users to convert externally        | Onboarding friction blocks the research use case                                    |
| Universal import via LLM ("paste your data, AI maps it") | Non-deterministic; slow; hard to audit; out of scope for core CLI                   |
| Plugin-based adapters (user-installable format plugins)  | Plugin API surface maintenance deferred; start with built-in adapters               |
| Always require a mapping file                            | High friction for obvious column-name cases; users abandon onboarding               |
| Format-specific importers (CSV → DB directly)            | Duplicates validation logic; harder to test; pipeline concentrates correctness      |
| Export parity with import at P0                          | Premature investment before usage patterns known; NDJSON export covers initial need |

## Consequences

### Positive

- Researchers with data in CSV, BibTeX, RDF, or GraphML can onboard without pre-processing their
  data. This removes the primary adoption barrier identified in early user feedback.
- All adapter output passes through the [ADR-020](ADR-020-git-native-kg-implementation.md)
  validation pipeline, so schema integrity guarantees hold regardless of input format.
- Format adapters are testable in isolation: input file + expected NDJSON output is a complete
  test case with no database required.
- `--schema-mode infer` makes it practical to import from richer-than-expected sources without
  losing data or requiring an ADR for every exploratory import.

### Negative

- Adapter maintenance burden: each supported format is a parser that must handle the full
  dialect variation found in the wild. BibTeX in particular has significant real-world variation.
- The section-heading heuristic for Markdown edge relations (P2) will produce incorrect edges
  when authors use non-standard section names. The `annotates` fallback limits but does not
  eliminate the harm.
- Streaming parsers for JSON-LD (remote `@context` fetching) and RDF (blank node expansion)
  require buffering the expanded subgraph before the records can be sorted into UUID order for
  NDJSON output. For large RDF graphs, this buffer can be significant.
- `--schema-mode infer` can pollute `schema.yaml` with source-specific entity kinds if used
  carelessly on heterogeneous imports. Users who want a curated schema should use `strict` with
  an explicit `kind_mapping` in the mapping file.

### Neutral

- Adapter output is intermediate NDJSON written to a temp directory and deleted after import.
  The intermediate files are not committed to git unless the user explicitly runs
  `khive kg export` afterward.
- When `--mapping` is provided, it overrides auto-detection entirely. There is no partial
  merging of auto-detected and mapped fields.
- The default NDJSON export behavior from [ADR-020](ADR-020-git-native-kg-implementation.md)
  is unchanged. The `--format` flag on `export` is additive.

## Implementation

### Crate structure

The shipped Rust crate `crates/khive-vcs-adapters/` currently contains the adapter trait,
record/error types, and a JSON adapter:

```
crates/khive-vcs-adapters/
  Cargo.toml
  src/
    lib.rs
    adapter.rs       -- FormatAdapter trait
    error.rs         -- AdapterError
    record.rs        -- EntityRecord / EdgeRecord
    json_adapter.rs  -- JsonFormatAdapter
```

CSV/TSV import is shipped in the Deno CLI (`cli/lib/importers/csv.ts`), not as Rust
`csv.rs` / `tsv.rs` modules in `khive-vcs-adapters`. Mapping, schema inference,
BibTeX, Turtle/N-Triples, JSON-LD, GraphML, GEXF, Markdown, and non-NDJSON export
modules are deferred.

### CLI integration

The user-facing Deno `khive kg import` path handles CSV/TSV/JSON adapter inputs directly,
converts adapter records to a `KgArchive`, and delegates to the standard import path.
`kkernel kg import` accepts archive/json/ndjson and validates/converts records for runtime
import. It does not instantiate CSV/TSV/BibTeX/RDF/GraphML/GEXF/Markdown Rust adapters.

### Schema inference integration

Schema inference is deferred. The shipped CLI rejects `--schema-mode`; unknown closed
taxonomy values are rejected through validation.

### Phasing

| State    | Scope                                                                                                                                                   |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Shipped  | Deno CSV/TSV/JSON import; Rust `FormatAdapter`/record/error types; Rust `JsonFormatAdapter`; `kkernel kg import` archive/json/ndjson                    |
| Deferred | mapping files, schema modes, Rust CSV/TSV modules, BibTeX/RDF/JSON-LD/GraphML/GEXF/Markdown, non-NDJSON export formats other than archive compatibility |

## Format-v2 migration UX (deferred to ADR-048)

The current export format (NDJSON, ADR-020 §canonical) is format-v1. A future
format-v2 will [TBD: rationale]. Migration UX for users with format-v1 archives:

- `khive kg import --format-v1 path/` continues to work; the importer detects v1
  by file header / schema absence and applies the canonical v1→v2 transform
  inline before validation.
- `khive kg export --format=v1` remains supported for one minor-version cycle
  after v2 ships, with a deprecation warning.
- No `migrate` subcommand is needed; round-trip import-then-export effectively
  upgrades archives.

Resolution: deferred to ADR-048 (Format-v2 Migration UX). ADR-047 was claimed by
the Knowledge Pack (knowledge pack verb surface). For v1, only format-v1 is
supported on both import and export sides.

## Open Questions

1. **Blank node expansion memory bound for large RDF graphs.** The current design buffers the
   expanded blank node subgraph. A bound or streaming expansion strategy may be needed for
   graphs with deeply nested blank node structures before Phase 2 ships.

2. **JSON-LD remote context caching policy.** The cache at `.khive/kg/.remote-cache/jsonld-ctx/`
   has no defined eviction policy. Whether to invalidate based on ETag, Content-Hash, or TTL
   should be resolved before Phase 3 ships.

3. **Endpoint validation in the adapter pipeline.** §6 defers full
   `(source_kind, relation, target_kind)` endpoint checking to P1. The open question is whether
   the kind-resolution index should live in `khive-vcs` (alongside the NDJSON validate path) or
   be delegated to `khive-vcs-adapters` as an optional post-pass.

4. **Section-heading heuristic coverage for Markdown.** The six patterns in §2 cover English
   Obsidian conventions. Multi-language vault support (non-English heading names) would require
   either a configurable mapping or a language-detection pass. Deferred to P2 design phase.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): Entity Kind Taxonomy — kinds validated by adapters
- [ADR-002](ADR-002-edge-ontology.md): Edge Ontology — closed edge relation set enforced in all
  modes; edge relations never added by `--schema-mode infer`
- [ADR-014](ADR-014-curation-operations.md): Curation Operations — `EntityPatch` semantics used
  by `--on-conflict update`; post-import correction workflow
- [ADR-015](ADR-015-schema-migrations.md): Schema Migrations — `ontology_version` minor bump
  mechanism used by `--schema-mode infer`
- [ADR-018](ADR-018-authorization-gate.md): Authorization Gate — namespace scoping enforced
  during import by the standard `khive kg import` path this ADR extends
- [ADR-020](ADR-020-git-native-kg-implementation.md): Git-Native KG Implementation — NDJSON
  record shapes, sort rules, directory layout, and `khive kg import` command this ADR extends
- NDJSON specification: <https://ndjson.org/>
- BibTeX format reference: <https://www.bibtex.org/Format/>
- JSON-LD 1.1 specification: <https://www.w3.org/TR/json-ld11/>
- GraphML specification: <http://graphml.graphdrawing.org/specification.html>
- GEXF 1.3 specification: <https://gexf.net/schema.html>
