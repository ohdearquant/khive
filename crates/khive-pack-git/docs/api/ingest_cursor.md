# `git.ingest_cursor` — persisted ingest position

Proposed contract, 2026-09-10. See the operational rider in
[ADR-088 Amendment 1](../../../../docs/adr/ADR-088-amendment-1-git-digest.md).

```text
git.ingest_cursor(project="<full project UUID>", source_kind="issues")
```

Both arguments are required. `source_kind` accepts `commits`, `issues`, or
`pull_requests`; prefixes and alternate source-kind names refuse. The live project
anchor is read through the canonical `get` gate with the caller's actor and resolved
request namespace. Full UUID reads are namespace-agnostic under the existing runtime
contract. The namespace inside a checkpoint is continuation metadata, not a separate
access rule. Missing, deleted, non-project, or denied anchors refuse.

The token retains the namespace used at the originating Gate check separately from
its primary storage namespace. This read passes that Gate namespace into nested `get`,
including when an implicit request's storage primary is `local` but its identity or
registry default is non-local. Explicit `namespace` still wins. Existing nested ingest
writes continue using their storage-primary helper.

The response has `project_id` (canonical full UUID), `source_kind`, `cursor`, and
`checkpoint`. Each stored row has this shape:

```json
{
  "value": "2026-09-01T12:01:02+00:00",
  "updated_at": 1788264062000000,
  "value_bytes": 25,
  "truncated": false
}
```

`value` is the exact stored string, including checkpoint JSON as a string. No timestamp
normalization, JSON decoding, validity judgment, reset, or repair occurs. `updated_at`
is the stored integer Unix timestamp in microseconds; it is not reformatted. A missing
row is `null`. A present row whose value is SQL NULL is an object with `value: null`,
`value_bytes: null`, and its stored `updated_at`. Cursor and checkpoint can be absent
independently, including legacy timestamp-only state.

Values over 262,144 UTF-8 bytes per row return `value: null`, their actual `value_bytes`,
and `truncated: true`. The complete value is omitted, never partially sliced JSON.
Exactly 262,144 bytes are returned. This limits materialized raw values to 512 KiB per
pair; JSON escaping can enlarge the wire response. Invalid column types or invalid UTF-8
in a bounded value refuse with a generic diagnostic that does not copy stored data.
Bounded text is read as bytes and decoded strictly, avoiding the SQL bridge's lossy text
conversion. Omitted oversized values are not decoded. Valid text with malformed checkpoint
JSON remains inspectable.

The handler reads both rows in one SQL statement from `git_mirror_cursor`. Public
`pull_requests` selects stored `prs` and `prs_checkpoint`; the other kinds select their
same-named cursor and `_checkpoint` row. The pair shares one SQLite statement snapshot,
so an atomic producer update cannot be read halfway through. Read presentation is
`AlwaysVerbose`: full UUIDs, raw strings, and microsecond integers survive MCP output.

This verb reads existing state without starting ingestion or invoking Git, `gh`, a
remote, or anchor creation. It does not mutate cursor rows, notes, or edges. Normal
runtime authorization and audit behavior still applies. Pack schema initialization
remains a separate startup operation.

## Recovery after an ambiguous response

Use the known project UUID and the source kind to inspect persisted position. Keep both
rows: issue/PR page checkpoints carry exact timestamp-boundary and undated membership;
commit checkpoints carry a frozen tip and the last completed SHA. The cursor alone does
not describe all continuation state. These remain opaque, versioned producer records;
the next `git.digest` applies its existing validation and continuation rules.

This is the current persisted position, not a receipt for the interrupted request,
proof of `done`, or a promise that the next digest can resume. An in-flight or concurrent
digest may advance immediately after this read. Do not launch a concurrent replay just
because a transport stopped waiting. For a particular request's completed report, use
the durable audit-receipt recovery procedure in ADR-088 Amendment 1. If the original
call auto-created an unknown project, recover the anchor separately; this read does not
resolve repositories or create anchors.
