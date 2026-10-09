# CSV and TSV adapter

`CsvFormatAdapter::new(source, format, default_kind, extra_valid_kinds)` is an eager, pure
transform. `DelimitedFormat::Csv` selects commas; `Tsv` selects tabs. It uses the `csv` reader
for quoted separators, escaped quotes, CRLF and multiline fields. Rows must have the same
number of fields as the header. Empty input, empty headers and case-insensitive duplicate
headers are refused. Header-only input is accepted when its columns satisfy the required shape.

Headers are trimmed. Nonblank textual cells retain their original bytes, including quoted
surrounding whitespace and newlines. Whitespace-only cells are treated as absent; numeric
weights accept surrounding whitespace. Field names are case-insensitive; property names
retain their trimmed header spelling. Both `source` and `target` columns select an edge list, which also
requires `relation`. Otherwise the file is an entity list and requires `name` and either `kind`
or a default kind. A blank kind cell also uses the supplied default.

Entity and edge IDs are validated when present and generated when absent or blank. Kinds,
relations, weights and timestamps use the same validation as the JSON adapter. Blank optional
cells are omitted; edge weight defaults to `0.7`. Nonblank weights must parse completely as
finite numbers in `[0, 1]`. `properties` cells contain JSON objects, and entity `tags` cells
contain JSON arrays of strings. Other columns become string-valued properties; a year such
as `2026` remains a string. Nonblank invalid typed cells are errors, not discarded metadata.

Structural errors fail construction. Record errors remain in the entity/edge iterators, so a
consumer must collect both streams successfully before writing anything. `kkernel kg import`
uses this boundary before opening the target database. Adapter tests cover quoting, field
mapping, taxonomy validation and row errors; CLI tests cover explicit CSV, inferred CSV/TSV,
format precedence and refusal without a partial import.

```text
kkernel kg import records.csv --db private.db --default-kind concept
kkernel kg import records.tsv --db private.db --default-kind concept
kkernel kg import records.data --format csv --db private.db --default-kind concept
```

An existing Rust CLI limitation remains: adapter edge endpoints must refer to entities in the
same import. Consequently, a standalone CSV/TSV edge list is parsed by the adapter but refused
by the CLI, even if the target database already contains the endpoints. This is an existing gap
against ADR-036 §6, not a new endpoint policy. Mapping files and schema modes remain separate work.
