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
