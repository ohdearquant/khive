# BibTeX import

`BibtexFormatAdapter::from_reader(impl BufRead)` reads BibTeX entries without loading
the complete raw source into a string. It is a pure transform with no database access.
It implements `FormatAdapter`; `stats()` returns `BibtexImportStats` before or after
draining the entity and edge iterators.

```rust
use std::io::Cursor;
use khive_vcs_adapters::{BibtexFormatAdapter, FormatAdapter};

let mut adapter = BibtexFormatAdapter::from_reader(Cursor::new(
    b"@article{paper, title={An example}, author={Ada and Grace}, year=2026}",
))?;
assert_eq!(adapter.stats().entries, 1);
let paper = adapter.entities().next().unwrap()?;
assert_eq!(paper.kind, "document");
assert_eq!(paper.entity_type.as_deref(), Some("paper"));
# Ok::<(), khive_vcs_adapters::AdapterError>(())
```

## Mapping

Every regular entry produces one `document` with `entity_type="paper"` and a fresh
UUID. Citation keys are case-sensitive import-local references; they are not persistent
IDs. Re-importing a file creates fresh IDs.

| BibTeX field                                | Record field                                                     |
| ------------------------------------------- | ---------------------------------------------------------------- |
| `title`                                     | `name`; absent or blank title uses the citation key              |
| `abstract`                                  | `description`                                                    |
| `author`                                    | `properties.authors`, as one expanded string                     |
| `year`                                      | `properties.year`, as an expanded string                         |
| `journal`, otherwise `booktitle`            | `properties.venue`; a nonempty journal wins                      |
| `doi`                                       | `properties.doi`                                                 |
| `url`                                       | `properties.source="url:<url>"`                                  |
| `archivePrefix=arxiv` and nonempty `eprint` | `properties.source="arxiv:<eprint>"`, taking precedence over URL |
| `crossref`                                  | `depends_on` edge to the referenced imported entry, weight `0.7` |

Field names and `archivePrefix`'s arXiv value are compared without ASCII case.
Authors are retained as a property, without creating person entities. Other fields
are ignored after value expansion. TeX markup, interior whitespace and expanded
field text are retained; there is no TeX rendering or author-name normalization.
The existing shared record validators enforce nonblank names and the closed kind
and relation vocabularies.

Both forward and backward crossrefs resolve after all entries have been read. A
duplicate parsed citation key or a crossref to a missing/skipped entry refuses the
whole source; no stub is created and no edge is silently dropped.

## Macros and framing

The pinned `serde_bibtex` parser validates one complete frame at a time. The reader
tracks braced and quoted values, nested braces, comments, and both entry delimiters.
An `@` inside a value cannot start another entry. Backslashes follow the parser's
literal brace-balancing rules.

`@string` names use the parser's case-insensitive variable identity. Definitions
expand against earlier successful definitions; `#` concatenates strings and numeric
tokens remain text. No predefined month macros are installed. A successful
redefinition replaces the old value. An undefined macro skips the regular entry or
macro definition with a warning; a rejected definition leaves the earlier value
intact. Comments and preambles produce no graph records.

The source reader returns each frame before reading the next. Converted records,
citation-key tables, pending crossrefs and warnings remain buffered for validation
and forward references; total import memory therefore still grows with the output.

## Failures and limits

A complete, balanced frame with invalid syntax is skipped with a warning, and
parsing continues at the next frame. An unfinished frame at EOF produces one
warning and skips the remaining tail. The reader never heuristically resumes at an
embedded `@`, including an `@` at the start of a line in an unfinished field.

IO errors, invalid UTF-8 anywhere in the source, duplicate keys, unresolved
crossrefs and resource-limit violations are fatal. These construction errors reach
the CLI before its target database opens. Limits are inclusive:

| Resource                                               | Maximum |
| ------------------------------------------------------ | ------- |
| Raw bytes of one entry, including delimiters           | 16 MiB  |
| Sum of expanded field-value bytes in one regular entry | 16 MiB  |
| Retained macro-name and expanded macro-value bytes     | 16 MiB  |
| Brace nesting, including the outer entry delimiter     | 256     |

Expansion checks its remaining byte allowance before appending each token. Replaced
macro definitions release their previous name and value allowance. Unused fields
still count toward the expanded-entry limit. Limits cannot be disabled by CLI flags.

## CLI

```text
kkernel kg import --format bibtex --db papers.db papers.bib
kkernel kg import --db papers.db papers.BIB --verbose
```

`.bib` is inferred without case sensitivity. An explicit `--format` takes precedence;
`.json` keeps its existing archive interpretation unless `--format json` is supplied.
`--default-kind` remains restricted to CSV/TSV; BibTeX always maps to document/paper.

Successful BibTeX imports extend the existing JSON import summary with:

```json
{ "adapter": { "format": "bibtex", "entries": 10, "skipped": 1, "warnings": 1 } }
```

`entries` counts encountered regular-entry frames, including malformed ones;
`skipped` counts regular frames rejected with a warning. Macro/comment/preamble
warnings contribute only to `warnings`. `--verbose` prints warning details to stderr,
including the one-based source line and zero-based byte offset. The ordinary import
validation and transaction path consumes the converted records unchanged.
