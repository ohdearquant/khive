# khive-pack-blob Design

## Purpose

`khive-pack-blob` exposes the runtime's installed content-addressed blob service through seven MCP
verbs: `blob.put`, `blob.get`, `blob.stat`, `blob.begin`, `blob.put_part`, `blob.commit` and
`blob.abort`. It adapts `BlobStore` and
`BlobHydrator`; it does not implement a storage backend, schema, or graph vocabulary.

## Key types and modules

- `BlobPack` holds the runtime and one shared upload manager. The daemon obtains that same
  manager from the registered pack instance; constructing another pack would lose the upload map.
- `pack.rs` declares the seven agent-visible verbs, inventory-registers the factory, and dispatches
  calls to `handlers.rs`.
- `handlers.rs` validates base64 payloads, strict content references and optional ranges, enforces
  memory/wire bounds, and shapes verb responses.
- `uploads.rs` owns sequential part accounting, incremental hashing, tail retries and expiry.
- `ContentRef` is the canonical lowercase-hex BLAKE3 identity supplied by `khive-storage`.
- `vocab.rs` contributes no entity or note kinds; typed artifact/reference modeling is a separate
  layer.

## Verb contracts

- `blob.put(bytes)` decodes base64, stores the bytes, and returns `{content_ref, size}`. Content
  addressing makes identical puts idempotent.
- `blob.get(content_ref, range?)` verifies and hydrates the complete object through the runtime's
  shared admission controller, then optionally slices it and returns base64 bytes.
- `blob.stat(content_ref)` reports existence and size through metadata only; it neither hydrates
  bytes nor implies a lease or reservation.

## Invariants

- The verb surface accepts bytes, never a server-local file path. This prevents a remote caller
  from turning `blob.put` into host-file exfiltration.
- Put and whole-object hydration share ADR-111's 64 MiB object ceiling, giving filesystem and S3
  backends the same externally visible acceptance limit.
- A `blob.get` response must also fit the daemon frame limit after base64 expansion. Callers use a
  smaller range when a whole object cannot fit on the wire, even though range slicing currently
  happens after full verified hydration.
- `blob.get` uses digest-verified hydration; `blob.stat` deliberately does not claim digest
  verification because it never reads the content.
- `blob.put` is unavailable on a read-only runtime. Reads remain available when a store is
  installed.
- Deleting committed objects and sweeping unreferenced committed objects remain administrator-only
  operations. Upload abort and expiry remove only staging.

## Staged uploads

`blob.begin(size, content_ref?)` returns `{upload_id, part_limit, next_index}`. If the
optional reference already exists, it returns `{content_ref, size}` without creating
staging. `blob.put_part(upload_id, index, bytes)` accepts base64 parts in order and
returns `{next_index, received_bytes}`. The returned `part_limit` derives from the
live request-parser and frame caps, minus an 8192-byte request reserve, scaled by 3/4.

An identical resend of the last part is acknowledged without changing bytes, hash,
index or activity time. An altered tail retry aborts the upload. Crossing declared
size also aborts. A successful append records activity from before backend I/O,
so a slow sync cannot make the pack clock newer than an already expiring stage.
Cancelled or failed backend writes invalidate the record; cleanup failures retain
an unusable record for the next sweep to retry.

`blob.commit(upload_id)` requires exactly the declared length, verifies an optional
expected reference, and calls the backend's shared publication routine. It returns
`{content_ref, size}` and consumes the id. `blob.abort(upload_id)` removes staging
and returns `{aborted: true}`; unknown or consumed ids report unknown upload.

The four upload verbs are Declaration verbs; existing `blob.put` remains Commissive.
All mutations refuse on a read-only runtime. Upload ids are capabilities: the
originating actor is retained for attribution, not an ownership restriction.
There is no upload journal and a restarted daemon answers unknown upload.

Only the daemon starts the upload sweep component. Each tick expires pack records
and calls backend `sweep_uploads` for orphan staging, using the same idle policy as
the verbs. Failures warn and retry on the next tick. The component joins the existing
daemon cancellation and drain path. `KHIVE_BLOB_UPLOAD_IDLE_SECS` defaults to 3600;
`KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS` defaults to 600. Both accept positive integer
seconds; invalid values use the default with a warning. The first tick is delayed
by the interval. The filesystem stages below `.uploads/`, which object GC ignores.
Backends without staged-upload support return Unsupported from the storage contract.

## Expiry and ownership controls

`python/tests/test_blob_upload_wire_integration.py` contains real daemon controls for
[ADR-173](../../../docs/adr/ADR-173-blob-chunked-upload.md) acceptance items 5 and 6.
The tests require an explicit `KKERNEL` executable and the Python client test dependencies.

| Acceptance          | Wire arm                                             | Discriminating failure                                                                            |
| ------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| 5: verb expiry      | `verb_expiry_without_sweeper[put_part]`              | Removing the verb idle guard acknowledges the stale part instead of refusing it.                  |
| 5: daemon expiry    | `daemon_expiry_without_verbs_keeps_committed_object` | Removing the whole daemon sweep leaves staging present; the committed object must survive.        |
| 6: restart          | `restart_orphan_sweep_and_begin_again`               | Removing backend sweeping leaves the old process's staging present despite its unknown upload id. |
| 6: daemon ownership | `daemon_owns_expiry_after_mcp_client_exit`           | Removing the whole daemon sweep leaves staging present after the client has exited.               |

The owner arm proves the file exists after the client exits, then polls only filesystem
state and daemon liveness until removal. It sends no further verb, and fixture cleanup runs
after the assertion. This prevents verb-side expiry or teardown from satisfying the control.
Removing only backend `sweep_uploads` leaves live-record expiry effective through
`abort_upload`, so the live-record control correctly remains green under that mutation.

`scripts/test-blob-upload-mutations.py` builds baseline, mutated and restored executables in
an isolated clean checkout with an explicit `CARGO_TARGET_DIR`. Supply `--root`, `--head`,
`--binary` and a fresh `--out` evidence directory outside the checkout. Its default `--case all` runs the four
ADR-173 mutation operators (tail digest, verb expiry, backend sweep, copied publisher).
The additional `--case owner` suppresses the entire daemon sweep call while preserving its
timer and heartbeat. It requires one baseline pass, exactly one failure reporting retained
staging, exact source restoration and one restored pass. Compile errors and unrelated
test failures do not satisfy a mutation control. All commands, counts and logs are retained.
