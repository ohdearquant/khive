# Source-input observations

`code.ingest` with L1.5 records literal source inputs alongside import facts in
the same dedicated code map. L1-only and L2-only calls do not run this scan.
No additional verb, manifest parser, database or language dependency is needed.

Rust observations cover literal `include!`, `include_str!`, `include_bytes!`,
top-level external-module `#[path]` attributes, and string literals beginning
with `crates/` or `scripts/`. Python records repository-prefixed plain/raw
string literals with a comment/string-aware lexical scan. A literal is an
observation, not proof that a runtime call reads it.

Each scanned module carries `source_read_scan`, with `scanner_version: 1`,
`status: "lexical"`, source path, revision, content hash, `observations` and
`unresolved` reasons. The persisted scan is independent of the invocation clock;
sweep time remains in current-row matching and edge fingerprints. Observations preserve their kind,
literal path and base (`file` or `repository`), plus resolution status and the
normalized target path when available. Dynamic includes, opaque macros and
attributes, known filesystem calls, parse failures, unsupported languages,
unsafe/glob paths and ambiguous ownership remain explicit unknowns. Python
always reports its unverified lexical coverage. Nested inline-module path
attributes are unresolved because their directory semantics differ.

A source input resolves only to an unambiguous module admitted by this same
L1.5 invocation and revision. File-relative parent components may stay within
the repository; absolute paths and repository escapes cannot resolve. Reads
reuse the existing contained, regular-file, 2 MiB source admission and Rust
parser safety limits. No target file is opened by the resolver. Missing files,
non-module inputs such as SQL/assets, and files outside the selected source
tree remain unresolved rather than creating phantom nodes.

Resolved inputs produce module-to-module `depends_on` edges with dependency
kind `source_read` and build scope. Imports and source reads for the same
ordered pair share one edge. Its `source_read_observations` qualify both
reader and input by path, revision, content hash and sweep time. The complete
module observation passes the runtime secret gate before any corresponding
edge is written; endpoint stamps are checked again before writing an edge.
Endpoint and edge writes are separate guarded operations. A concurrent sweep
can supersede an endpoint after that check; the edge then remains qualified
historical evidence, rather than an atomic assertion of endpoint currentness.

Re-ingest replaces current module observations, preserving old edge evidence
as revision-qualified history. Consumers must compare both endpoint
fingerprints and the current module observation; edge existence alone does
not establish a current read. Content hashes are the pack's existing FNV-1a
change fingerprints, not security attestations. The revision labels observed
HEAD and does not certify a clean tree: a protected consumer must independently
bind its producer receipt to the exact checked-out tree.

The optional report object `source_reads` counts admitted file observations,
literal observations, persisted resolved reads, unresolved observations/reasons
and missed coverage stamps. Missed stamps and other ingest warnings invalidate
any completeness assumption. An absent scan never means zero readers.

This feature does not prove a complete filesystem read graph, expand macros,
resolve arbitrary calls, classify cfg/features, identify Cargo test targets,
or authorize skipping CI. CI selection must remain FULL until a separately
validated consumer can establish its coverage and target-role obligations.
