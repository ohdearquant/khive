# ADR-173: Chunked blob upload — objects larger than one wire frame

- **Status**: Proposed
- **Date**: 2026-09-07
- **Relates to**: [ADR-111](ADR-111-blob-store.md) (blob store, 64 MiB object ceiling, orphan GC,
  capability-by-hash), [ADR-049](ADR-049-khived-daemon.md) and [ADR-137](ADR-137-tailnet-wire-transport.md)
  Amendment 1 (the 8 MiB frame), [ADR-138](ADR-138-blob-enumeration.md) (proposed enumeration, unaffected)

## Context

Two ceilings disagree. `blob.put` accepts objects up to 64 MiB (`MAX_OBJECT_BYTES`,
`crates/khive-pack-blob/src/handlers.rs`, matched by the S3 backend's ceiling from ADR-111 Amendment 2).
The daemon frame is 8 MiB in either direction (`MAX_FRAME_BYTES`, `crates/khive-runtime/src/daemon.rs`;
ratified as the default for the tailnet transport by ADR-137 Amendment 1), and inside the frame the
request parser refuses an `ops` string longer than 1 MiB before it parses anything (`MAX_OPS_INPUT_LEN`,
`crates/khive-request/src/types.rs`, checked in `crates/khive-request/src/parser/dispatch.rs`; the
Python client mirrors the constant in `python/khive/dsl.py` and refuses to render past it). `blob.put`
carries its bytes as base64 inside that `ops` string, so the tighter of the two caps binds: base64 grows
3 bytes into 4, and the largest object a client can actually put over the wire today is 768 KiB
(786,432 bytes), a little under that once the call's own envelope is counted. A 1 MiB object renders past
the parser cap before the connection opens. Anything between roughly 768 KiB and 64 MiB can be stored
only by a caller inside the daemon process.

Reads do not have this problem. `blob.get` already takes `range={offset, length}` and bounds each
response by the frame budget (`max_returnable_raw_bytes()`, 6,288,384 bytes), so a 64 MiB object is read
in eleven calls today. The gap is write-side only.

A client that keeps objects up to 64 MiB (a state layer whose records spill large tool results to
objects) hits this on its first large record. The alternatives it must not take are named in
Alternatives: a client-side chunk manifest would make the reference something other than the object's
hash, and a direct write into the blob root bypasses the store.

## Decision

### 1. An upload is a server-side staging object with a capability id

`blob.begin(size, content_ref=None)` opens an upload and returns `{upload_id, part_limit, next_index}`.
`upload_id` is a random 128-bit value rendered as hex; possession of it is the right to append to and
commit that upload, the same capability-by-possession model ADR-111 Amendment 4 applies to content
references. `size` is the declared total and is refused above `MAX_OBJECT_BYTES` before any bytes move.

`part_limit` is the largest raw part the server accepts, and it is a named constant beside
`max_returnable_raw_bytes()` in the blob handlers: `max_request_part_raw_bytes() =
((min(MAX_OPS_INPUT_LEN, MAX_FRAME_BYTES) - REQUEST_RESERVE) * 3) / 4`. The minimum is over both caps a
request passes through, the parser's `ops` input cap and the daemon frame, because the base64 payload
sits inside the `ops` string and the `ops` string sits inside the frame; the constant tracks whichever
is tighter, so raising either cap later moves `part_limit` without a second edit. `REQUEST_RESERVE` is
the room the call's own text needs around the payload (`upload_id`, `index`, the verb name, the ops
wrapper, and at the frame the actor and namespace fields) and is fixed at 8192 bytes, twice the
response reserve, because the request carries more fields than the response. The reserve is a bound
on the call's own text inside `ops`, which is fixed in shape; it is not a bound on the frame envelope,
whose `visible_namespaces` list has no cardinality limit. That is safe today because the parser cap
binds and the frame keeps `MAX_FRAME_BYTES - MAX_OPS_INPUT_LEN` of headroom above the whole `ops`
string, 7 MiB for the envelope; if the two caps ever converge, the reserve must be re-derived from
bounded envelope fields, and acceptance 4's formula assert is where that shows up. With the constants as
they stand the parser cap is the tighter one and `part_limit` is 780,288 bytes, so a 64 MiB object
takes 87 parts. A part of exactly `part_limit` raw bytes renders to an `ops` string under
`MAX_OPS_INPUT_LEN` and to a frame under `MAX_FRAME_BYTES`; a part one byte larger is refused by the
handler on decoded length, an `ops` string over the parser cap is refused before parsing, and a frame
that overflows the frame cap is refused by the daemon before dispatch, so a client that computes the
same formula never learns either cap by a rejected request.

When the caller already knows the object's BLAKE3 reference it may pass `content_ref`. The server then
checks existence first and, if the object is present, answers `{content_ref, size}` with no `upload_id`,
so a re-put of a known object costs one round trip and no bytes. A supplied `content_ref` is verified at
commit; a mismatch fails the commit and discards the staged bytes.

### 2. Parts are sequential, bounded, and retry-safe at the tail

`blob.put_part(upload_id, index, bytes)` appends one base64 part. Parts arrive in index order starting at
0; the server feeds each into an incremental BLAKE3 hasher and the backend's staging object, and answers
`{next_index, received_bytes}`. An `index` other than `next_index` is refused with `InvalidInput`, with
one exception for the lost acknowledgement: the server keeps the last accepted part's decoded length and
the BLAKE3 hash of its bytes, and a resend of `next_index - 1` whose length and hash both match is
acknowledged without appending. A resend of that index whose length or hash differs is refused and the
upload is aborted, because the client's picture of what it sent no longer matches the staging object
and no later part can repair that. A part that would take `received_bytes` past the declared `size`, or
past `MAX_OBJECT_BYTES`, is refused and the upload aborted. ADR-111 §5's free-space floor applies to
every part, not only to the commit. Parts are sequential by contract; parallel parts are out of scope
(§6).

### 3. Commit hashes and publishes through the one publish routine

`blob.commit(upload_id)` requires `received_bytes == size`, finalizes the hash, and publishes the
staging object under that reference through the backend's publish routine, which `blob.put` uses for
the same step. The response is `{content_ref, size}`, the same shape `blob.put` returns; there is no
`deduplicated` flag, because neither backend reports one today (`BlobStore::put` returns only the
reference) and adding it would change `blob.put`'s public result for a bit the caller can read from
`blob.stat` before it starts. Dedup is observable exactly as it is for `put`: the same reference, no
second object, and on the filesystem backend the existing object's mtime touched. After commit the
`upload_id` is dead.

Because commit and put share one publish routine, commit inherits what put has today, including the
missing directory barrier after the rename, a known defect with its own repair in flight: the file is synced, the directory is
not, so the published entry is not machine-death durable on either path. The barrier repair lands in
that one routine and covers both; a second copy of the publish step is not permitted, and the
acceptance list holds the barrier test against commit as well as put once the repair lands.

### 3b. The storage contract below the handlers

Nothing below the handlers holds upload state today: `BlobStore` exposes whole-buffer `put`, verified
reads, `exists`, `size`, `delete` and GC; the blob pack registers `put`, `get` and `stat` and its
read-only wrapper implements only those. This ADR adds the lifecycle to the trait so the handlers have
something to call and both backends have one contract to satisfy:

```rust
async fn begin_upload(&self, declared_size: u64) -> StorageResult<UploadId>;
async fn append_part(&self, id: &UploadId, bytes: Vec<u8>) -> StorageResult<u64>; // bytes staged so far
async fn commit_upload(&self, id: &UploadId, content_ref: &ContentRef) -> StorageResult<()>;
async fn abort_upload(&self, id: &UploadId) -> StorageResult<()>;
async fn sweep_uploads(&self, idle_for: Duration) -> StorageResult<u64>; // uploads removed
```

The pack layer owns the per-upload record: declared size, bytes received, the incremental hasher, the
last part's length and hash, the frame's actor, the last activity time, and the backend's `UploadId`.
That record lives in the daemon's memory, keyed by the wire `upload_id`; it is not journaled, which is
why uploads do not survive a restart (§4). Hashing stays in the pack layer so both backends see the
same reference; `commit_upload` receives the finished reference and publishes under it, and the
filesystem backend does not hash the staged file a second time. The read-only wrapper refuses all five
methods the way it refuses `put`.

Filesystem backend: `begin_upload` creates `<blob root>/.uploads/<id>`; `append_part` appends and
syncs; `commit_upload` renames that file into the `hex[0..2]/hex[2..4]/hex` shard through the publish
routine (same filesystem, one `rename`, creating the shard directory as put does), or, when the object
already exists, deletes the staging file and touches the existing object's mtime; `abort_upload`
deletes the staging file; `sweep_uploads` removes every `.uploads/` entry whose mtime is older than
`idle_for`. The object scan already skips dot-leading entries on the raw directory name
(`crates/khive-db/src/stores/blob.rs`, the `readdir` loop), so `.uploads/` is invisible to the orphan
GC without a new exclusion, and the upload sweep is a separate pass rather than a clause in that scan.

S3-compatible backend: `begin_upload` opens a multipart upload on the staging key `uploads/<id>` and
wraps it in `object_store`'s `WriteMultipart` with its fixed 5 MiB chunk, because most providers refuse
non-final parts under 5 MiB and some require equal part sizes; wire parts of any size are written into
that buffer and the provider sees only 5 MiB chunks plus a final one. `commit_upload` finishes the
multipart upload, then does the head check and a server-side copy to the sharded key with the same
create-if-absent semantics `put` uses, then deletes the staging key; the copy costs at most one object's
worth of bytes inside the store and never crosses the wire. `abort_upload` aborts the multipart upload.
`sweep_uploads` lists the `uploads/` prefix and deletes staging objects older than `idle_for`. On this
backend a staging object becomes visible only when its multipart upload finishes, so the sweep reaches
uploads that finished but never committed (a crash between finish and the copy) and cannot reach one
still open. Open multipart uploads that a daemon restart orphans cannot be listed through
`object_store`, so a deployment on this backend sets the bucket's abort-incomplete-multipart-upload
lifecycle rule, and that rule's days-after-initiation is the cleanup bound for them; the ADR names
that as a deployment requirement rather than pretending the daemon can reap them.

### 4. Abort and expiry

`blob.abort(upload_id)` discards the staging object and the pack record. An upload with no part for one
hour is expired, and expiry is enforced at two places so that neither depends on the other's timing.
At the verbs: `put_part` and `commit` on an upload whose last part is older than the idle bound abort
it, drop the record, and answer unknown upload, so a stale upload never commits whatever the sweeper is
doing. By a sweeper: the blob pack has no periodic task in this tree (its GC entry points are
administrative, ADR-111 §8), so this ADR adds one. It is daemon-role only: the upload records live in
the daemon's memory and only the daemon holds them, so the task starts where the daemon's other owned
loops start in `crates/khive-mcp/src/serve.rs` (the daemon component start that runs before the daemon
loop), never in a non-daemon serve process; the session WAL-registry sweep is the wrong neighbour for
it, because that one deliberately runs in every non-daemon process. It borrows that task's shape: a
config read from the environment (`KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS`, default 600;
`KHIVE_BLOB_UPLOAD_IDLE_SECS`, default 3600), the daemon's blob store handle, a shutdown watch channel,
and a join handle the daemon's drain awaits. Each tick aborts idle pack records and calls
`sweep_uploads(idle_for)` on the backend, which also catches staging a crash left visible: every
filesystem staging file, and on S3 the finished-but-uncommitted staging objects (an open multipart
upload is the lifecycle rule's job, §3). A tick that fails logs the backend error at warn and the next
tick retries; a failing sweeper never stops serving and never touches a committed object. The sweeper and the
transactional orphan GC of ADR-111 §8 do not coordinate because neither can see the other's objects: on
the filesystem the GC walk skips dot-leading entries, so `.uploads/` is invisible to it, and on S3 the
orphan sweep skips any key that does not parse as a shard key, so `uploads/<id>` is invisible to it
(the filesystem caller-snapshot `orphan_sweep` is disabled in this tree; the S3 one is live, and it is
safe for that reason). Uploads do not survive a daemon restart: the pack record is not journaled, and a client
whose upload id is unknown after a restart begins again. That is stated here rather than engineered
around because the client already retries from `begin` on any error class it cannot classify.

### 5. Gate and attribution

`blob.begin`, `blob.put_part`, `blob.commit` and `blob.abort` are Declaration verbs gated as `blob.put` is.
A deployment that keeps ADR-111 Amendment 4's `(namespace, ContentRef)` put ledger records the row at
commit, where the reference first exists; `begin` and `put_part` record nothing. Uploads carry the
frame's actor for audit and nothing else; they are not namespaced, because objects are not.

### 6. What does not change, and what is out of scope

`blob.put` stays as it is for objects under the frame budget; it remains the one-call path and the
conformance baseline. `blob.get` range reads are the read side for large objects and gain nothing here.
`blob.stat` is unchanged. ADR-138's proposed catalog, if it lands, indexes at commit through the same
"object store first, index second" rule it already states for `put`. Out of scope, by decision and not
by oversight: parallel or out-of-order parts (a client that wants throughput opens several uploads),
uploads that survive a daemon restart, and a dedup flag on the result.

## Alternatives considered

| Alternative                                                        | Why not                                                                                                                                                                                                                                                                       |
| ------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Raise `MAX_FRAME_BYTES` to cover 64 MiB objects                    | ADR-137 Amendment 1 ratified 8 MiB for every transport; every client and every daemon worker buffers a whole frame, one 90 MiB request would hold a socket worker and its memory for its whole transfer, and the audit, comm and events frames share the cap.                 |
| A streaming frame type in the wire protocol                        | A protocol version bump that every client must implement to keep talking to the daemon at all, for one verb's benefit.                                                                                                                                                        |
| Client-side chunking with a manifest object                        | The reference a caller stores would be the manifest's hash, not the object's; two clients chunking differently would store one object twice under two refs; the state layer that motivates this explicitly refuses it.                                                        |
| A server-local file path on `blob.put`                             | Rejected already in the blob handler; a path is not a capability and a remote client has no such path.                                                                                                                                                                        |
| Let the client compute the hash and target key up front (required) | Requires BLAKE3 in every client; the optional `content_ref` in §1 keeps the early-dedup benefit for clients that have it without making it a dependency.                                                                                                                      |
| A `deduplicated` flag on commit                                    | Neither backend reports it and `blob.put` does not; adding it changes a public result for a bit `blob.stat` already answers before the upload starts.                                                                                                                         |
| Send each wire part as one provider multipart part                 | Most providers refuse non-final parts under 5 MiB and some require equal sizes; the wire part limit is under 1 MiB and clients may choose smaller parts still. `WriteMultipart`'s fixed 5 MiB chunking absorbs the mismatch.                                                  |
| Raise or exempt `MAX_OPS_INPUT_LEN` for the blob verbs             | The cap guards the request parser for every verb and every transport; a per-verb exemption is a parser change with a wider blast radius than this ADR, and the price of keeping it is part count, not reachability. Kept; the formula in §1 follows the cap if it ever moves. |
| Acknowledge a tail resend on length alone                          | A same-length resend with different bytes would be acknowledged while the staging object holds the first bytes; the hash of the last part costs 32 bytes per upload and closes it.                                                                                            |

## Consequences

- Four verbs added to the blob pack; five methods added to `BlobStore`, implemented by both backends
  and refused by the read-only wrapper; no schema change; no wire protocol change.
- The daemon gains one periodic task, the upload sweeper, with two environment knobs; non-daemon serve
  processes gain nothing.
- The filesystem backend gains a staging directory under the blob root and its own expiry sweep; the S3
  backend gains a staging prefix, a buffered multipart writer, a server-side copy at commit, a prefix
  sweep, and a documented dependency on the bucket's incomplete-multipart lifecycle rule.
- Commit and put share the publish routine, so the directory-barrier repair covers both when it lands,
  and until then neither is machine-death durable.
- A client library can expose one `put(bytes)` that chooses `blob.put` or the chunked path by size, so
  callers see one operation with one result shape.
- The 64 MiB ceiling is now reachable over the wire, which is the point.

## Acceptance

1. A 64 MiB object uploaded in parts commits to the reference equal to `blake3` of the same bytes computed
   outside the daemon; `blob.get` with ranges reads it back byte-identical.
2. Uploading the same object twice: the second `begin` with `content_ref` answers the reference with no
   `upload_id`; the second full upload without `content_ref` commits to the same reference, no second
   file exists, the mtime is touched, the same observable as two `blob.put` calls today.
3. A part at the wrong index is refused; a resend of the last part with the same length and the same
   bytes is acknowledged without changing `received_bytes`; a resend with a different length is refused
   and the upload is gone; a resend with the same length and different bytes is refused and the upload
   is gone, and a subsequent `commit` answers unknown upload.
4. A part crossing `size` aborts the upload and leaves no staging file; `begin` above `MAX_OBJECT_BYTES`
   is refused before any part; a part of exactly `part_limit` bytes renders to an `ops` string the
   request parser accepts and is accepted by the handler, and one of `part_limit + 1` is refused on
   decoded length; the same test asserts `part_limit` equals the §1 formula evaluated over the live
   constants, so a moved cap fails the test rather than a client. The frame-fit half of that claim holds
   under the envelope precondition §1 states (the serialized request minus `ops` under
   `MAX_FRAME_BYTES - MAX_OPS_INPUT_LEN`); the test pads an envelope past that bound and asserts the
   daemon refuses the frame before dispatch, not that the part was accepted.
5. Abort leaves no file. Expiry, verb side: with no sweeper running, a `put_part` after the idle bound
   answers unknown upload and the staging file is gone. Expiry, sweeper side: with no verb call, an
   idle upload's staging file is gone after the sweeper's next tick. The transactional orphan GC run
   with a live staging file present deletes nothing it should not (the control: a committed object and
   a staging file side by side, both survive one GC pass, the staging file alone is removed by the
   sweeper after expiry); on S3 the same control runs against the S3 orphan sweep with a `uploads/<id>`
   key present.
6. Restart between `put_part` and `commit`: `commit` answers unknown upload; the client's begin-again path
   succeeds. On the filesystem the orphaned staging file, whose record died with the process, is
   removed by the sweeper's first tick after the idle bound; on S3 a staging object left visible
   (finished, uncommitted) is removed the same way, and for an open multipart upload the arm asserts
   the bucket's abort-incomplete-multipart lifecycle rule is present (the MinIO lane reads it back),
   not that the daemon reaped it. Owner control: an upload begun through the daemon by a client process
   that then exits is expired by the daemon's sweeper with no client alive, which fails if the task
   were started in the client's serve path instead.
7. Mutation: with the tail-retry hash check removed, test 3's same-length different-bytes resend is
   accepted (red). The length comparison is a pre-check the digest subsumes, so it carries no mutation
   arm of its own: a different-length resend also has a different digest and is refused either way.
   With the verb-side expiry removed, test 5's stale `put_part` is accepted (red); with the sweeper's
   `sweep_uploads` call removed, test 5's sweeper arm and test 6's filesystem and visible-S3 arms are
   red; with commit given its own copy
   of the publish step, the shared-routine assertion in test 8 is red.
8. One publish routine: a test asserts by construction that `put` and `commit_upload` call the same
   function for the rename-into-shard step, and once the directory-barrier repair lands, the barrier
   test runs against `commit` as well as `put`.
9. S3 backend: a 64 MiB upload through wire parts of `part_limit` bytes completes against a
   MinIO-compatible target (the ADR-111 Amendment 2 lane) and reads back byte-identical; abort leaves no
   staging object.
10. Where ADR-111 Amendment 4's put ledger exists: no row before commit and exactly one after.
