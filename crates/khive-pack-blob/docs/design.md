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
- `blob.begin(size, content_ref?)` opens staging and returns `{upload_id, part_limit, next_index}`,
  or returns `{content_ref, size}` for an existing reference without staging.
- `blob.put_part(upload_id, index, bytes)` appends sequential base64 parts and returns
  `{next_index, received_bytes}`; an identical last-part retry returns the same counters.
- `blob.commit(upload_id)` checks the complete length and optional expected reference, publishes
  the object, and returns `{content_ref, size}` while consuming the upload id.
- `blob.abort(upload_id)` discards staging, consumes the upload id, and returns `{aborted: true}`.

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
- `blob.put` and the four upload verbs are unavailable on a read-only runtime. Reads remain
  available when a store is installed.
- Deleting committed objects and sweeping unreferenced committed objects remain administrator-only
  operations. Upload abort and expiry remove only staging.

## Staged uploads

`blob.begin(size, content_ref?)` returns `{upload_id, part_limit, next_index}`. If the
optional reference already exists, it returns `{content_ref, size}` without creating
staging; that response uses the stored object's size. `size` is a required non-negative
integer at most 64 MiB, including zero. Content references are 64-character lowercase-hex
strings and upload ids are 32-character lowercase-hex strings, not UUID spellings or paths.
`blob.put_part(upload_id, index, bytes)` requires a non-negative integer index and a
base64 string, accepts parts in order starting at zero, and returns integer
`{next_index, received_bytes}` counters. Empty parts are valid. The returned integer
`part_limit` derives from the live request-parser and frame caps, minus an 8192-byte
request reserve, scaled by 3/4; it currently equals 780,288 decoded bytes.

An identical resend of the last part is acknowledged without changing bytes, hash,
index or activity time. An altered tail retry aborts the upload. Other out-of-order
indices are refused with `InvalidInput` without advancing it. A next part crossing
declared size also aborts; exceeding only `part_limit` refuses the part while retaining
the upload. Invalid base64 is refused before appending. A successful append records
activity from before backend I/O, so a slow sync cannot make the pack clock newer than
an already expiring stage.
Cancelled or failed backend writes invalidate the record; cleanup failures retain
an unusable record for the next sweep to retry.

`blob.commit(upload_id)` requires exactly the declared length, verifies an optional
expected reference, and calls the backend's shared publication routine. It returns
`{content_ref, size}` and consumes the id. An incomplete commit is refused without
discarding the upload; a reference mismatch aborts it. `blob.abort(upload_id)` removes
staging and returns `{aborted: true}`; unknown or consumed ids report unknown upload.

The four upload verbs are Declaration verbs; existing `blob.put` remains Commissive,
and `blob.get` / `blob.stat` remain Assertive.
All mutations refuse on a read-only runtime. Upload ids are capabilities: the
originating actor is retained for attribution and admission limits, not an ownership restriction.
There is no upload journal and a restarted daemon answers unknown upload.

Each loaded upload manager admits at most 128 staged uploads in total and 16 per
originating actor. `KHIVE_BLOB_UPLOAD_MAX_ACTIVE` and
`KHIVE_BLOB_UPLOAD_MAX_PER_ACTOR` override those ceilings with positive integers;
invalid values warn and use the defaults. A full ceiling refuses `blob.begin` with
`InvalidInput` naming the total or per-actor ceiling. The known-reference shortcut
needs no slot. Reservations count pending backend creation and remain occupied
until successful commit or cleanup, including when cleanup must be retried. Once
creation is admitted, it finishes registering its record even if the request is
cancelled, so the reservation and stage remain tracked for expiry. Admission and
reservation happen under one short lock; backend I/O holds no admission lock.
These are process-local concurrency bounds, not disk quotas or request rate limits.
Staging orphaned by a process restart is still reclaimed by backend sweeping.

Only the daemon starts the upload sweep component. Each tick expires pack records
and calls backend `sweep_uploads` for orphan staging, using the same idle policy as
the verbs. `put_part` and `commit` also enforce expiry directly: once begin or the last
accepted new part is at least the idle bound old, they discard the upload and report
unknown upload. Failures warn and retry on the next tick. The component joins the existing
daemon cancellation and drain path. `KHIVE_BLOB_UPLOAD_IDLE_SECS` defaults to 3600;
`KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS` defaults to 600. Both accept positive integer
seconds; invalid values use the default with a warning. The first tick is delayed
by the interval. The filesystem stages below `.uploads/`, which object GC ignores.
The current staged-upload backend is `FsBlobStore`. `S3BlobStore` retains whole-object
put/get/stat support but inherits Unsupported for staging methods; its known-reference
begin shortcut can still return an existing object without staging.

## Filesystem platform limits

On Windows and other non-Unix systems, staged filesystem operations validate paths
and then resolve them again for creation, append, publication and removal. These
checks do not prevent a local writer from swapping a directory, junction or reparse
point between validation and use, redirecting an operation outside the blob root.
Deploy with the blob root, its contents and its ancestor directories writable only
by trusted processes. An untrusted process running as the same service account is
also outside this containment boundary. Upload IDs constrain input names but do
not close the filesystem race.

The Unix implementation uses anchored directory handles and no-follow operations.
A Windows handle-based replacement remains follow-up work: it needs native
Windows coverage of concurrent swaps during begin, append, commit, abort and sweep,
including both the publication source and destination. A compile-only check does
not establish those runtime guarantees.

Filesystem writes currently share a per-root mutex across staged operations,
ordinary publication and object GC. The guard stays held through blocking writes
and synchronization, including after cancellation of the async caller. Concurrent
uploads can be admitted, but their filesystem writes to the same root are serialized.
Changing that lock requires preserving cancellation ownership, capacity checks,
publication and GC exclusion; it is separate follow-up work.

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
`--binary` and a fresh `--out` evidence directory outside the checkout. Its default
`--case all` runs six controls: the four ADR-173 mutation operators (tail digest, verb
expiry, backend sweep, copied publisher), daemon ownership and the total upload ceiling.
`--case cap` removes only the total ceiling while the per-actor limit remains above
the test's total limit; accepting another `blob.begin` must fail the assertion.
`--case owner` selects
only the ownership control, suppressing the entire daemon sweep call while preserving its
timer and heartbeat. It requires one baseline pass, exactly one failure reporting retained
staging, exact source restoration and one restored pass. Compile errors and unrelated
test failures do not satisfy a mutation control. All commands, counts and logs are retained.
