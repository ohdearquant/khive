# Graph Store (Edges)

`SqlGraphStore` (`crates/khive-db/src/stores/graph.rs`) implements the
`GraphStore` capability trait over the `graph_edges` table. This is the
function-specific technical reference for its write-routing and the
endpoint-existence guards that back `link`'s atomic-unit safety.

## `with_writer` — WriterTask routing (ADR-067 Component A, Fork C slice 2)

See `crates/khive-db/src/stores/graph.rs` — private method `with_writer`.

Resolves the pool-wide `WriterTask` at write time and routes through it when
available; a handle missed by construction outside Tokio is refreshed here.
Strict routing fails closed when no handle is available. Compatibility mode
falls back to the legacy standalone-connection / pool-mutex path and emits a
`direct_route:graph_general_write` violation when the file-backed queue is
enabled. This is the ONE routing
point for every `with_writer` caller in this store (`upsert_edge`,
`delete_edge`, `purge_incident_edges`). `f` must be DML-only — on the
flag-on path it runs inside the WriterTask's own transaction, so a bare
`BEGIN IMMEDIATE` would violate SQLite's nested-transaction rule.
`upsert_edges` and guarded batch/transaction paths perform the same write-time
lookup before falling through this helper. A non-strict `None` records only at
the actual fallback seam; strict mode never reaches it.

## Observed edge upserts and tombstone policy

`upsert_edge_observed` and the guarded singleton/batch variants determine the
natural-key preimage and apply the write on the same write connection. Their
typed result distinguishes:

- `Created`: no natural-key row existed;
- `Updated`: a live row existed; weight, metadata, revision, and target-backend
  state are replaced while the original row ID and creation time survive;
- `Resurrected`: a tombstone existed and the request explicitly set
  `resurrect=true`.

A tombstone conflict with `resurrect=false` is a typed refusal and performs no
write. Compatibility wrappers (`upsert_edge`, `upsert_edges`, and their older
guarded results) retain their signatures but default to that non-resurrecting
policy. Batch observed writes preflight every endpoint and tombstone policy
inside one transaction before changing any row, so refusal remains
all-or-nothing.

## Atomic `link` statement builders (ADR-099 §B3)

The atomic plan uses two guarded shapes instead of a blind upsert:

- `edge_insert_new_guarded_by_endpoints_statement` inserts only when both
  endpoints still exist and no ID or natural-key row appeared after prepare;
- `edge_link_replace_if_unchanged_and_endpoints_exist_statement` replaces or
  resurrects only when the row's ID, `updated_at`, and deletion marker still
  match the prepare snapshot and both endpoints remain live.

Both statements carry an affected-row guard. This closes the endpoint race and
also guarantees that the prepare-time `created`/`updated`/`resurrected`
disposition used by the response and event payload is still true at commit.

## Shared Batch Writes and Endpoint Pre-check (#769)

`observed_edge_batch_upsert` pre-checks every endpoint and tombstone policy on
the write connection before issuing any write. `observed_edge_upsert` uses the
same `edge_endpoints_exist` probe for guarded singleton requests. Missing endpoints
are therefore transaction-local facts, not reconstructions from a later reader.
Both WriterTask and compatibility routing retain one transaction for the preflight
and writes; an `Ok(refusal)` can commit safely because no row was changed.

After preflight, batch and singleton DML share `observed_edge_upsert` and
`edge_upsert_statement_with_resurrection`. The compatibility
`edge_upsert_statement` selects the same builder with resurrection disabled.
The natural-key conflict arms and `bind_params` conversion are shared, not copied
into a separate batch SQL loop (ADR-099 §B3).

The legacy `upsert_edges_guarded` adapter preserves the original ordered batch,
the refusing entry's index and `MissingEndpoints`, and returns
`GuardedBatchOutcome::refused` with `affected: 0`. Its `record_failure` path
classifies the culprit as `InvalidInput/Permanent` and every sibling as
`BatchAborted/Unknown`, with the full original first-error diagnostic. A legacy
tombstone refusal remains `StorageError::Conflict`; observed APIs retain the
explicit `ResurrectionRequired` result.

### Enumerating Refused Writes Beyond the Sample (#2375)

Storage callers can retain the original ordered `Vec<Edge>` and use
`GuardedBatchOutcome::refusal_page(&original, class, PageRequest { offset, limit })`
after one guarded batch call. No write is resubmitted and no endpoint is re-read.
The default `BatchWriteSummary`, its 128-detail sample, and legacy `first_error`
remain unchanged. Pages use that same 128-detail cap and message bound.

`class=None` enumerates every refused write in original input order. Only the
first guard-refused entry is `InvalidInput/Permanent`; all siblings are
`BatchAborted/Unknown`. Later siblings were not necessarily examined, and paging
does not claim they all have missing endpoints. The initial summary and pages
share the same classification and detail formatter.

An optional class filter is applied before offset and limit, and `Page.total`
is the complete matching population. Add `items.len()` to the offset to continue
until that total is reached. A requested limit above 128 is clamped to 128; zero
returns count metadata only and is not a progressing enumeration request.
Success, an empty successful batch, and offsets at/beyond the filtered population
return empty pages. Skipped/filtered entries are never formatted.

The supplied slice length must equal `summary.attempted`, and a refusal index
must be within that slice. The caller must retain the exact batch contents and
order: length validation cannot detect a substituted same-length batch. The
original `first_error` supplies the refusing entry's diagnostic unchanged.

This is a synchronous, in-memory storage API, not a new MCP/runtime endpoint or
durable cursor. Runtime `link_many` currently discards the summary in favor of
its first guarded-write failure; this change does not expand that wire response.
