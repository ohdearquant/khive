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
ratified as the default for the tailnet transport by ADR-137 Amendment 1). `blob.put` carries its bytes
as base64 inside one frame, so the largest object a client can actually put over the wire is just under
6 MiB: base64 grows 3 bytes into 4, and the request envelope needs room too. A 6 MiB object encodes past
the cap before the connection opens. Anything between about 6 MiB and 64 MiB can be stored only by a
caller inside the daemon process.

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
`part_limit` is the largest raw part the server will accept, derived from the frame budget the same way
`max_returnable_raw_bytes()` is, so a client never has to guess the base64 arithmetic.

When the caller already knows the object's BLAKE3 reference it may pass `content_ref`. The server then
checks existence first and, if the object is present, answers `{content_ref, size, deduplicated: true}`
with no `upload_id`, so a re-put of a known object costs one round trip and no bytes. A supplied
`content_ref` is verified at commit; a mismatch fails the commit and discards the staged bytes.

### 2. Parts are sequential, bounded, and retry-safe at the tail

`blob.put_part(upload_id, index, bytes)` appends one base64 part. Parts arrive in index order starting at
0; the server feeds each into an incremental BLAKE3 hasher and the staging file, and answers
`{next_index, received_bytes}`. An `index` other than `next_index` is refused with `InvalidInput`, with
one exception: a resend of `next_index - 1` whose decoded length equals that part's recorded length is
acknowledged without appending, so a client that lost the acknowledgement can retry the last part. A part
that would take `received_bytes` past the declared `size`, or past `MAX_OBJECT_BYTES`, is refused and the
upload aborted. ADR-111 §5's free-space floor applies to every part, not only to the commit.

### 3. Commit hashes, persists, deduplicates

`blob.commit(upload_id)` requires `received_bytes == size`, finalizes the hash, and persists through the
backend's own put path: on the filesystem backend the staged temp file is fsynced and renamed into the
`hex[0..2]/hex[2..4]/hex` shard exactly as `put` does today; if the object already exists the staged
bytes are discarded and the existing object's mtime is touched, again as `put` does. The response is
`{content_ref, size, deduplicated}`, byte-identical in shape to `blob.put`'s. After commit the
`upload_id` is dead.

On the S3-compatible backend the parts go to a staging key `uploads/<upload_id>` through the object
store's multipart API, and commit copies server-side to the sharded key after the head check, then
deletes the staging key. The copy costs at most one object's worth of bytes inside the store and never
crosses the wire.

### 4. Abort and expiry

`blob.abort(upload_id)` discards the staging object. An upload with no part for one hour is discarded by
the same sweep that runs ADR-111 §8's orphan GC; the staging directory (`.uploads/` under the blob root,
or the `uploads/` key prefix) is excluded from that GC's object scan, so a half-finished upload is never
mistaken for an orphan and a committed object is never mistaken for staging. Uploads do not survive a
daemon restart: staging is not journaled, and a client whose upload id is unknown after a restart begins
again. That is stated here rather than engineered around because the client already retries from
`begin` on any error class it cannot classify.

### 5. Gate and attribution

`blob.begin`, `blob.put_part`, `blob.commit` and `blob.abort` are Declaration verbs gated as `blob.put` is.
In a hosted deployment ADR-111 Amendment 4's put ledger records the `(namespace, ContentRef)` row at
commit, where the reference first exists; `begin` and `put_part` record nothing. Uploads carry the
frame's actor for audit and nothing else; they are not namespaced, because objects are not.

### 6. What does not change

`blob.put` stays as it is for objects under the frame budget; it remains the one-call path and the
conformance baseline. `blob.get` range reads are the read side for large objects and gain nothing here.
`blob.stat` is unchanged. ADR-138's proposed catalog, if it lands, indexes at commit through the same
"object store first, index second" rule it already states for `put`.

## Alternatives considered

| Alternative | Why not |
|---|---|
| Raise `MAX_FRAME_BYTES` to cover 64 MiB objects | ADR-137 Amendment 1 ratified 8 MiB for every transport; every client and every daemon worker buffers a whole frame, one 90 MiB request would hold a socket worker and its memory for its whole transfer, and the audit, comm and events frames share the cap. |
| A streaming frame type in the wire protocol | A protocol version bump that every client must implement to keep talking to the daemon at all, for one verb's benefit. |
| Client-side chunking with a manifest object | The reference a caller stores would be the manifest's hash, not the object's; two clients chunking differently would store one object twice under two refs; the state layer that motivates this explicitly refuses it. |
| A server-local file path on `blob.put` | Rejected already in the blob handler; a path is not a capability and a remote client has no such path. |
| Let the client compute the hash and target key up front (required) | Requires BLAKE3 in every client; the optional `content_ref` in §1 keeps the early-dedup benefit for clients that have it without making it a dependency. |

## Consequences

- Four verbs added to the blob pack; no schema change; no wire protocol change.
- The filesystem backend gains a staging directory under the blob root and a sweep clause in its GC; the
  S3 backend gains a staging prefix and a server-side copy at commit.
- A client library can expose one `put(bytes)` that chooses `blob.put` or the chunked path by size, so
  callers see one operation with one result shape.
- The 64 MiB ceiling is now reachable over the wire, which is the point.

## Acceptance

1. A 64 MiB object uploaded in parts commits to the reference equal to `blake3` of the same bytes computed
   outside the daemon; `blob.get` with ranges reads it back byte-identical.
2. Uploading the same object twice: the second `begin` with `content_ref` deduplicates without an upload;
   the second full upload without `content_ref` commits as `deduplicated: true`, no second file, mtime
   touched, the same observable as two `blob.put` calls today.
3. A part at the wrong index is refused; a resend of the last part with the same length is acknowledged
   without changing `received_bytes`; a resend with a different length is refused.
4. A part crossing `size` aborts the upload and leaves no staging file; `begin` above `MAX_OBJECT_BYTES`
   is refused before any part.
5. Abort leaves no file; an upload idle past the expiry is gone after the sweep; the sweep's orphan scan
   run with a live staging file present deletes nothing it should not (the control: a committed object
   and a staging file side by side, both survive one sweep, the staging file alone is removed after expiry).
6. Restart between `put_part` and `commit`: `commit` answers unknown upload; the client's begin-again path
   succeeds.
7. Mutation: with the tail-retry length check removed, test 3's different-length resend is accepted (red);
   with the staging exclusion removed from the GC scan, test 5's control is red.
8. Hosted mode: the put ledger has no row before commit and exactly one after.
