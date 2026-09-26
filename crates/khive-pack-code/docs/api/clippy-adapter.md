# Clippy findings adapter

`ingest_clippy_json_lines` accepts the JSON-lines stream from `cargo clippy
--message-format=json`, explicit repository provenance, and `CodeIngestOptions`.
It parses the complete stream before returning a `CodeIngestBatch`; it never opens
a database or writes records. The existing `ingest_findings_json` validator and
record mapper remain the final boundary.

The producer ID is `cargo-clippy/json/v1`. Only `compiler-message` records with a
`clippy::` lint code produce findings. Cargo artifact, build-script, build-finished,
and future-incompat records are ignored. Unknown record reasons, malformed JSON,
missing lint fields, ambiguous primary spans, and paths outside the repository are
reported with the input line number. The caller must supply repo, branch, commit,
and scope strings. A run without an explicit `source_run` uses the producer ID and
commit, independent of observation date.

Severity mapping is `error` → `high`, `warning` → `medium`, and
`note`/`help`/`failure-note` → `info`; other levels are refused. The primary span
provides repository-relative path, start/end lines, and start/end columns in
evidence. The stable producer fingerprint uses producer ID, repository, normalized path,
lint code, diagnostic message, source snippet, and columns. It excludes line
numbers, so lines inserted elsewhere in the file do not change it. The existing
finding note ID remains content-versioned and can change when the evidence line
changes; the fingerprint in `finding_id` and `raw.fingerprint` is the stable
cross-run correlation key. Identical duplicate diagnostic records are collapsed;
different spans that collide on the stable fingerprint are refused.

The committed `tests/fixtures/clippy-mini.jsonl` is a hand-checked miniature
stream. It contains only repository-relative example paths and no host paths.
