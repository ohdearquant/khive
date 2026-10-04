# khive-pack-blob Design

## Purpose

`khive-pack-blob` exposes the runtime's installed content-addressed blob service through nine MCP
verbs: `blob.put`, `blob.get`, `blob.stat`, `blob.begin`, `blob.put_part`, `blob.commit` and
`blob.abort`, plus confined server file transfers `blob.import` and `blob.export`. It adapts `BlobStore` and
`BlobHydrator`; it does not implement a storage backend, schema, or graph vocabulary.

## Key types and modules

- `BlobPack` holds the runtime and one shared upload manager. The daemon obtains that same
  manager from the registered pack instance; constructing another pack would lose the upload map.
- `pack.rs` declares the nine agent-visible verbs, inventory-registers the factory, and dispatches
  calls to `handlers.rs`.
- `handlers.rs` validates base64 payloads, strict content references and optional ranges, enforces
  memory/wire bounds, and shapes verb responses.
- `uploads.rs` owns sequential part accounting, incremental hashing, tail retries and expiry.
- `file_handlers.rs` uses the shared runtime file policy for confined imports and atomic exports.
- `ContentRef` is the canonical lowercase-hex BLAKE3 identity supplied by `khive-storage`.
- `vocab.rs` contributes no entity or note kinds; typed artifact/reference modeling is a separate
  layer.

## Verb contracts

Each verb deserializes a closed typed argument object before accessing blob or upload state.
Unknown fields are rejected by name with the allowed fields. The optional `blob.get` range
is also closed: only `offset` and `length` are accepted. An omitted offset defaults to zero;
an explicit null offset is invalid. An omitted or null range means the whole object, and an
omitted or null length means through the end. Existing size, base64 and capability checks
still apply after argument parsing.

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
- `blob.import(path, media_type?)` streams a confined server file through staged upload and returns
  reference and size. `blob.export(content_ref, path)` atomically writes a verified blob into the
  export root and returns path and size. Neither result contains bytes. See
  [server file transfers](api/file-transfers.md) for deployment and path requirements.

## Invariants

- `blob.put` accepts base64 bytes only. The separate file-transfer verbs confine paths to disjoint
  operator-configured import and export roots; multitenant deployments must not expose them.
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

For filesystem uploads, an identical resend of the last part renews the lease and
activity time without changing bytes, hash, index or stage mtime. A backend without
lease support keeps its previous retry activity semantics. An altered tail retry
aborts the upload. Other out-of-order indices are refused with `InvalidInput`
without advancing it. A next part crossing declared size also aborts; exceeding
only `part_limit` refuses the part while retaining the upload. Invalid base64 is
refused before appending. A successful append, or an accepted filesystem tail
renewal, records activity from before backend I/O, so a slow sync cannot make the
pack clock newer than an already expiring stage.
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
and calls backend `sweep_uploads` for staging recovery. Filesystem expiry uses each
lease's recorded owner bound; other backends retain their existing idle policy.
`put_part` and `commit` also enforce the pack's idle bound directly: once begin or
the last accepted new part or filesystem tail renewal is at least that bound old,
they discard the upload and report unknown upload. Failures warn and retry on the
next tick. The component joins the existing daemon cancellation and drain path.
`KHIVE_BLOB_UPLOAD_IDLE_SECS` defaults to 3600;
`KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS` defaults to 600. Both accept positive integer
seconds; invalid values use the default with a warning. The first tick is delayed
by the interval. Filesystem stages are below `.uploads/`, which object GC ignores.
The current staged-upload backend is `FsBlobStore`. `S3BlobStore` retains whole-object
put/get/stat support but inherits Unsupported for staging methods; its known-reference
begin shortcut can still return an existing object without staging.

## Shared-root lease activation (ADR-173 Amendment 1)

Before using a shared filesystem root, drain uploads from older producers and
upgrade every daemon allowed to sweep it. Ownerless filesystem begin is
Unsupported; append and renewal require valid leases. The fixed no-lease expiry
floor handles crash leftovers, not a second active-upload protocol. Do not run
mixed old and lease-aware sweepers.

With filesystem staging, `blob.begin` and `blob.import` require durable MAIN;
an in-memory MAIN, or one without an available durable identity, reports
Unconfigured naming the identity failure before creating an upload.

The host conveys a validated durable store binding identity, or effective MAIN's
installed durable identity before binding exists, and the owning daemon's positive
whole-second bound. No caller actor, PID or random fallback supplies that owner.
The fixed bound is capped at 21,600 seconds for FS only; larger configured values
warn with KHIVE_BLOB_UPLOAD_IDLE_SECS, the original value and the clamp. S3 policy
and its staged Unsupported behavior remain unchanged. Known-content begin needs
no staging, reservation or lease owner.

Begin creates/syncs the stage and a complete four-field sibling `<id>.lease`
(owner, idle_secs, renew_seq=0, renewed_at). The timestamp is Unix milliseconds;
validation requires a non-nil owner UUID and
a representable checked millisecond expiry deadline. A u64 sequence may reach its
maximum, but its next renewal refuses without wrapping.
New parts sync bytes then atomically renew the same owner/bound with checked next
sequence; accepted identical tails renew without appending. Rejected calls renew
neither lease nor local clock. Failed renewal never ACKs and follows existing
abort/terminal cleanup. Lease publication writes a unique temporary sibling,
syncs it, replaces the lease and on Unix syncs the directory; begin also persists
the `.uploads` root entry. Root write ownership spans append, renewal and sweep,
including cancellation. Commit/abort remove the lease only after staging is gone;
cleanup failures remain visible/retryable. Lease writes obey the capacity floor.

Each retained sweeper store has an instance-local observation of (owner, renew_seq)
and its own monotonic first-seen time. Changed pairs restart it. Any sweeper may
reap unknown owners after that lease's own idle_secs + 300 unchanged seconds.
Restart/host sleep delays observation. The wall backstop is renewed_at + idle_secs
+ 24 hours; a no-lease leftover uses a fixed 24-hour stage-mtime floor. Malformed
present leases retain/report rather than fall back. Future timestamps >300 seconds
report a clock fault without changing expiry. Backward wall movement delays wall
arms; it cannot shorten monotonic observation. Remove vanished-stage observations
and recognized orphan lease files under the root lock; never follow symlinks or
sweep arbitrary temporary names. Temporary publication cleanup is best effort;
a failed unlink reports the retained name for repair. Failed begin cleanup also
reports its failure while returning the original begin error. A bad lease is
retained and reported while the sweep continues processing healthy siblings; the
call still returns an error if any entry failed.

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
