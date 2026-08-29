# Attachments — role-keyed record content

`AttachmentStore` is the backend-neutral ADR-121 capability for blob references
owned by an entity or note. It stores metadata only; bytes remain in `BlobStore`.
The key is `(record_uuid, role)`, and one record may therefore carry parallel
renditions such as `content`, `pdf`, or the moodboard-owned `fann-network`.

## Types and validation

- `AttachmentSubstrate` has the stable lowercase values `entity` and `note`.
- `NewAttachment` is caller-supplied metadata. `Attachment::from_new` binds it
  to a record UUID, substrate, and creation timestamp.
- `ContentRef` is canonical by construction. A role must be non-empty and
  contain no control characters. `size_bytes`, when present, must fit a SQLite
  signed integer; it is descriptive metadata rather than read authority.

`AttachmentStore` supports upsert, exact `(record_uuid, role)` lookup, stable
role-ordered listing, and role deletion. Deleting an attachment never deletes
the referenced blob; attachment-only GC later reclaims objects with zero live
rows.

## Atomic record publication and deletion

`EntityStore::upsert_entity_with_attachments` is the storage contract for
committing an entity plus all initial attachment roles in one transaction.
Concrete hard-delete paths remove every row for the record in the same
transaction as the entity or note deletion. Soft delete retains attachments so
recoverable records continue to anchor their bytes.

`Entity.content_ref` is retained only as a compatibility response field. Reads
project attachment role `content`; ordinary entity upserts ignore that field and
cannot recreate the removed database column.

## Runtime ownership and GC

The canonical main/core database is the sole attachment and blob-liveness
authority. Pack runtimes assigned to a secondary backend must route through
`KhiveRuntime::core()`; direct `KhiveRuntime::attachments()` on a secondary is
rejected. The concrete V21 migration backfills legacy entity refs, authenticates
pack-owned roles before finalization, switches GC anti-joins and claim fences to
`attachments`, and then drops `entities.content_ref`.

The low-level trait does not hydrate bytes or authorize callers. Production
whole-object reads use the shared runtime `BlobHydrator`, while Gate and
record/role eligibility remain runtime and pack policy.
