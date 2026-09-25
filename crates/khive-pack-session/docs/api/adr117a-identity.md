# Session mirror identity and search scope (ADR-117a)

The mirror stores provider identifiers unchanged. A mirrored session is identified by
`(namespace, source, provider_session_id)`, and a message by
`(namespace, source, session_id, id)`. `source` is one of `claude_code`, `codex`,
`chatgpt_export`, or `claude_ai_export` for new ingests. These tuples are the
operative uniqueness keys; neither bare `sessions.id` nor bare
`session_messages.id` is globally unique. Two sources or two namespaces can
therefore use the same provider session and event identifiers without dropping
each other's rows.

The line-tail mirror remains a local, single-principal writer. Source comes
from the parser selected by the ingest path, never from the shape of an
identifier or from transcript content. Codex still synthesizes an event ID as
`{session_id}:{byte_offset}`. Re-reading the same file with the same source
inserts no second event, while an equal ID from another source is distinct.
`session_messages.content_hash` records parsed text plus raw content for
integrity and future embedding backfill; it does not participate in identity
and is not unique. If the same scoped event ID replays with different content,
the insert-once row remains intact and the pass reports the difference in
`MirrorStats.replay_mismatches`; it still advances the cursor so migration
recovery cannot stall behind an edited legacy event.

`session_messages.mirror_rowid` is an explicit integer primary key used only
as a stable FTS5 external-content address. The scoped unique tuple remains
the event's operative identity; the integer key survives SQLite `VACUUM`.

## Existing database migration

The global SQLite migration runner rebuilds the pack-owned mirror tables in a
versioned step. It replaces the old bare-ID primary keys, backfills `NULL` namespaces
to `local`, and makes tenant scope non-null. Existing `sessions.source` values
are retained. Each existing message receives its parent's source by joining
`session_messages` to `sessions` on `(namespace, session_id)` after namespace
backfill.

If a message has no parent session, migration retains it, reports the orphan
count, and sets `session_messages.source` to the reserved value `unknown`.
Only migration writes `unknown`; no ingest parser emits it. An orphan can be
audited by explicitly filtering for `source="unknown"`. Search without a
source filter excludes it. Rows lost to an old bare-ID collision cannot be
reconstructed by migration; replaying the original source file restores them
under the new key. Migration resets existing `session_mirror_cursor.byte_offset`
values to zero so the line-tail service re-reads its files on the next pass.
Whole-file exports are re-parsed through their normal ingest path.

## Search contract and availability

ADR-117a defines `session.search(query, limit?, since?, source?, cwd?)` over
mirrored message text. Results identify their source alongside the provider
session identifier, and include a score and matching snippets. `since`,
`source`, and `cwd` narrow results; none selects a tenant. Search binds every
row query to the request's resolved `NamespaceToken` scope. A request without
a positive scope returns `PermissionDenied`, even if the pre-dispatch Gate
allows the request. The caller cannot provide a `namespace` or `account`
argument to widen this search.

`source="unknown"` explicitly searches migrated orphan messages. The
default search excludes them. A lookup by provider session identifier alone
can become ambiguous when two sources use it in the same namespace; a future
single-session resolver must return a typed ambiguity error listing the
matching sources rather than choose one.

The public `session.search` surface remains gated until ADR-117b deletion and
ADR-117c resume/export continuity are live. Serving it to multiple connection
principals also requires ADR-096's authenticated connection identity. This
identity and scope contract does not itself enable hosted search.

See [ADR-117a](../../../../docs/adr/ADR-117a-session-identity-tenant-isolation.md)
for the accepted decision and [mirror ingest](mirror-ingest.md) for the file
tail and replay mechanics.
