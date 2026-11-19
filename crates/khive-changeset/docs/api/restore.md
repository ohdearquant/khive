# Restore a soft-deleted record

`RestoreOp` contains `target_id` and `record`, the complete live entity, note, or
edge expected after restoring a soft deletion. `record` uses the existing
`DeletePreimage` representation, tagged by `substrate`, and the same strict
embedded-record mirrors. All fields, including timestamps, status and expiry,
remain on the wire. This operation does not create a missing row.

The checked constructor and decoder require matching IDs, no `deleted_at`
tombstone, valid note salience/decay and valid edge weight. A note must have
`status: active`: the live restore operation cannot recover an archived status.
Unknown outer and embedded fields are refused. The wire tag is `op: restore`.

```json
{
  "op": "restore",
  "target_id": "00000000-0000-0000-0000-000000000001",
  "record": {
    "substrate": "entity",
    "id": "00000000-0000-0000-0000-000000000001",
    "namespace": "local",
    "created_at": 11,
    "updated_at": 29,
    "kind": "concept",
    "entity_type": null,
    "name": "Original",
    "description": null,
    "properties": {},
    "tags": [],
    "deleted_at": null
  }
}
```

This belongs to schema version 2; the envelope is the sole version carrier.
A version-1 decoder rejects the envelope before reading operations. The current
encoder also refuses a manually assigned version-1 envelope rather than emitting
a restore under the old contract.

## Inversion and lifecycle evidence

A soft delete inverts to a restore carrying its original full preimage. The
caller must supply the current tombstoned row. Content, identity, namespace,
creation time, properties, tags and expiry must still match. Entity and note
soft-deletion leave `updated_at` unchanged, so inversion checks it; edge
soft-deletion changes it, so inversion permits that difference. A deleted note
must have status `deleted`, corresponding to the captured active note.

A restore inverts to a soft delete carrying the actual current live record.
It checks all record content and creation time. Every live restore advances
`updated_at`, so that one timestamp may differ from the captured record. The
restored note must still be active. A full record is retained for review, not a
request to overwrite current storage clocks with historical timestamps.

The reverse walk rewinds content in a private working copy. When it crosses a
restore, it records the known prior tombstoned state separately: the operation
does not contain the original deletion timestamp, and inversion never invents
one. This supports a delete followed by restore of the same ID within one list.

Hard deletes and merges remain non-invertible. An archived-note soft delete also
refuses: the live surface cannot recover its old status. Missing or changed
records refuse with the original operation index; no partial inverse is returned.

This API is pure staging logic. It performs no live restoration or atomic
landing check. In particular, the domain record does not carry entity merge
provenance; the live applier must still refuse merge tombstones and enforce the
live restore's other guards. ADR-101/102's proposed restore amendment requires
approval before this format is adopted for landing.
