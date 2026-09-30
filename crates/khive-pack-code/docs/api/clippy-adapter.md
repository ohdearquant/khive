# Clippy findings adapter

`ingest_clippy_json_lines` accepts the JSON-lines stream from `cargo clippy
--message-format=json`, explicit repository provenance, and `CodeIngestOptions`.
It parses the complete stream before returning a `CodeIngestBatch`; it never opens
a database or writes records. The existing `ingest_findings_json` validator and
record mapper remain the final boundary.

The producer ID is `cargo-clippy/json/v1`. Only `compiler-message` records with a
`clippy::` lint code produce findings. Cargo artifact, build-script, and
future-incompat records are ignored. The terminal `build-finished` record is
validated and its outcome is carried in the project entity's
`audit_extra.clippy_build_outcome` property:

- `finished_ok`: Cargo emitted `success: true`; this stream has a successful build attestation.
- `finished_failed`: Cargo emitted `success: false`; findings remain usable, but this could be an
  ordinary deny-level lint under `-D warnings` or an early compile failure, so absence of findings
  does not prove every crate was scanned.
- `no_marker`: no terminal record was present. The adapter returns partial diagnostics for
  investigation, but the stream may be truncated and has no completeness attestation.

A `build-finished` record requires a boolean `success` and must be the final record. The outcome
uses the existing tolerated audit-extension field of `findings.json`; it does not change finding
identity. A caller that persists a batch can inspect the project properties before writing it and
must not use a non-success outcome as evidence that the scan is complete. The generic code-ingest
path consumes deterministic project IDs once, so a later run does not revise an already persisted
project entity's prior outcome.

Unknown record reasons, malformed JSON,
missing lint fields, ambiguous primary spans, and paths outside the repository are
reported with the input line number. The caller must supply repo, branch, commit,
and scope strings. On Unix, a literal backslash in a filename remains a backslash;
on Windows, native backslash separators become `/`. A colon is a valid Unix
filename character; on Windows, any colon in a span path is refused, including
drive-relative paths and stream suffixes. An initial Windows absolute drive
prefix such as `C:/` or `C:\foo` is refused on either host. A run without an
explicit `source_run` uses the producer ID and commit, independent of observation date.

Severity mapping is `error` → `high`, `warning` → `medium`, and
`note`/`help`/`failure-note` → `info`; other levels are refused. The primary span
provides repository-relative path, start/end lines, and start/end columns in
evidence. The stable producer fingerprint uses producer ID, repository, normalized path,
lint code, diagnostic message, verbatim primary-span source text, and columns.
It excludes line numbers, so a uniform line shift does not change it; edits within
the source snippet, including literal whitespace, do. When otherwise identical
diagnostics occur at distinct spans, the first in source-position order retains
that base fingerprint. Each later occurrence gets a `clippy-occurrence/v1`
fingerprint derived from the base and its one-based ordinal, independent of
Cargo record order and absolute line numbers. Inserting or removing an earlier
identical occurrence can renumber later occurrences. The existing finding note
ID remains content-versioned and can change when the evidence line changes;
the fingerprint in `finding_id` and `raw.fingerprint` is the cross-run correlation
key. Identical duplicate records collapse; conflicting records at one primary
span refuse with the input line.

The adapter test builds a miniature JSON-lines stream from synthetic records in
code. It contains only repository-relative example paths and no host paths.
