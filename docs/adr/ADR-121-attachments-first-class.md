# ADR-121: Attachments — Role-Keyed Blob Renditions as a First-Class Substrate Property

**Status**: accepted\
**Date**: 2026-07-23\
**Authors**: khive maintainers\
**Amended by**: proposed [ADR-160](ADR-160-shared-pack-infrastructure.md), whose moodboard migration
consumes this accepted role-keyed desired state, makes the canonical main backend the sole
attachment/GC-liveness authority, and specifies a two-release GC-compatibility/deployment gate plus
a boot-gated two-stage cutover rather than extending legacy `entities.content_ref`.\
**Depends on**:

- [ADR-111](ADR-111-blob-store.md) — BlobStore (the content-addressed storage capability,
  `ContentRef`, `FsBlobStore`, the S3 backend of Amendment 2, and the capability-by-hash
  authorization model of Amendment 4 — this ADR builds the substrate integration on top of it)
- [ADR-001](ADR-001-entity-kind-taxonomy.md) — Entity kinds (the records attachments hang from)
- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note kinds (extended here as the second attachment substrate)
- [ADR-002](ADR-002-edge-ontology.md) — Edge relations (`supersedes` carries version history;
  attachments deliberately do not)
- [ADR-017](ADR-017-pack-standard.md) — Packs (the `kg` pack owns the agent-facing verb surface)
- [ADR-015](ADR-015-schema-migrations.md) — Schema migrations (the `attachments` table lands as a
  versioned migration)

---

## Context

ADR-111 gave khive a content-addressed blob capability: a `BlobStore` trait beside the other
storage capabilities, `ContentRef` (BLAKE3) as the opaque key, a filesystem implementation, an
S3-compatible implementation, and a transactional orphan sweep. What it deliberately did not
decide is how blobs surface in the substrate's data model. Today that surface is minimal and
lopsided:

- Entities carry a single nullable `content_ref` column (migration V10). One blob per entity,
  no role, no media type, no notion of multiple formats.
- Notes carry nothing. A record whose payload is inherently non-textual — a voice message
  arriving over a channel transport, a photo shared in a conversation, a screenshot attached to
  an observation — has nowhere to put its bytes.
- The agent-facing verb surface is a low-level triplet (`blob.get` / `blob.put` / `blob.stat`)
  that moves base64 bytes by hash and knows nothing about records. Publishing content is a
  client-driven two-step: `blob.put`, then a separate record write that commits the
  `content_ref`. Because no lock or transaction spans the two steps, the orphan sweep had to
  grow a grace-window heuristic to avoid deleting just-published blobs whose reference had not
  landed yet — a patch over a seam that exists only because the two writes are separate
  operations.

Meanwhile the roadmap keeps producing consumers that want richer shapes:

- **Documents with heterogeneous content.** A paper entity may hold a PDF, an HTML rendering,
  an extracted-text form — the same information in several formats — while another paper is
  only an external link with no bytes held at all.
- **Multimodal records.** Voice messages and images are records whose _own content_ is
  non-textual. The text field holds the searchable rendition (a transcript, a caption); the raw
  modality data needs a home on the same record. Planned multimodal retrieval (image/audio
  embedding over stored media) assumes exactly this: original bytes retained in the CAS store,
  text sidecar in the record, embeddings fanned out later.
- **Sealed artifacts.** Session transcripts, generated reports, checkpoints: immutable-once-
  sealed byte payloads whose identity and history belong in the graph.

The design question is where content lives in the data model, answered by analogy with the one
multi-valued record property the substrate already has: embeddings. An entity's embeddings are
keyed by model — several parallel representations of the same record, maintained by the
substrate, invisible to the graph. Content renditions have the same nature: several parallel
formats of the same record's content, keyed by role.

---

## Decision

### 1. Attachments: a role-keyed map on both substrates

A record — entity **or** note — may carry zero or more **attachments**: `role → ContentRef`,
plus per-attachment metadata. Roles are short caller-chosen strings (`"pdf"`, `"html"`,
`"image"`, `"audio"`, `"transcript-raw"`); at most one attachment per role per record. This
mirrors embeddings-keyed-by-model: attachments are renditions of the record's own content, not
relationships between records. Relationships stay in the graph.

Storage is a new table (one versioned migration):

```sql
CREATE TABLE attachments (
    record_uuid  TEXT NOT NULL,          -- entity or note UUID
    substrate    TEXT NOT NULL CHECK (substrate IN ('entity', 'note')),
    role         TEXT NOT NULL,
    content_ref  TEXT NOT NULL,          -- ADR-111 ContentRef (BLAKE3 hex)
    media_type   TEXT,                   -- caller-declared MIME type
    size_bytes   INTEGER,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (record_uuid, role)
);
CREATE INDEX idx_attachments_content_ref ON attachments(content_ref);
```

The migration backfills every non-null `entities.content_ref` into `attachments` with role
`"content"` and drops the `entities.content_ref` column. After the migration there is exactly
one reference source for blob garbage collection. ADR-160 Phase 4 implements this as a
boot-gated, resumable V21 state machine: the stage transaction retains the legacy column and
claim fences while pack-owned roles are authenticated; one final transaction swaps the GC
anti-join and claim triggers, drops the legacy column, and records completion before serving.

### 2. The rendition rule: what is an attachment, what is not

Two boundaries make the model predictable. Both are normative.

**Version vs. rendition.** Different information is a new record; the same information in a
different format is a different role on the same record. A report whose v3 content differs from
v2 is two records joined by a `supersedes` edge (ADR-002) — version history is graph structure,
traversable and annotatable. The PDF and HTML forms of v3 are two roles on the v3 record.
Because each version is a record, a note that references a specific version simply `annotates`
that record — version-precise reference falls out of the model with no attachment-level
addressing needed.

**Utterance vs. thing (the note boundary).** A note attachment is the note's _own content_ in a
non-text modality: the raw audio of a voice message (text field: transcript), the pixels of a
shared photo (text field: caption). A note attachment is never a _thing_ — a report, a dataset,
a paper delivered through a conversation is content worth naming, which makes it an entity; the
message note `annotates` it. The test: "is this byte payload another form of what this record
itself says, or an independent thing?" Text that fits a text column stays in the text column;
attachments are for bytes that do not belong there. The dividing line is modality and size,
not significance.

External references are not attachments. A URL (an arXiv link, a web page) lives in record
properties as it does today; the attachment map holds only content the store physically holds.
The upgrade path from reference to held content — fetch, `put`, attach — is a deliberate,
explicit ingestion action (deferred; see Consequences).

### 3. Verb surface: attachment ops on the `kg` pack

The agent-facing surface lives on the `kg` pack, beside the record verbs it extends:

- `create(kind=…, …, attach={role: <source>, …})` — create and attach in one op.
- `attach(id, role, fp=… | bytes=…, media_type=…)` — add or replace one rendition.
- `detach(id, role)` — remove a rendition (the blob itself is reclaimed by the sweep once no
  references remain).
- `get(id, hydrate=[role, …])` — fetch a record with selected renditions inlined (base64);
  default responses return attachment metadata only (role, media type, size, ref), never bytes.
- `export(id, role, to=fp)` — write a rendition to a local file path.

Byte sources: `bytes=` (base64 inline) works on every deployment. `fp=` (a local file path) is
accepted only on local stdio deployments, where client and server share a filesystem; a
non-local deployment rejects `fp=` with an error naming the constraint. Server-side upload
negotiation for remote deployments (presigned-URL flow against the ADR-111 Amendment 2 S3
backend) is out of scope here and lands as a follow-up amendment when the remote surface needs
it.

The existing `blob.get` / `blob.put` / `blob.stat` verbs remain as the low-level
administrative surface (hash-addressed, record-agnostic). They stop being the recommended
agent path for record content.

### 4. Atomic publication

The generic `attach` and `create(..., attach=...)` wire behavior below belongs to rollout step 2
and remains deferred after ADR-160 Phase 4. The shipped internal
`create_entity_with_attachments` seam already enforces the same database atomicity for current
consumers.

`attach` (and `create` with `attach=`) performs blob write and reference commit as one
operation behind the verb boundary: the blob is written to the store, then the attachment row
is committed in the same database transaction as the record write. A failure on either side
surfaces as a single verb error with nothing half-published. This closes, structurally, the
client-driven put-then-reference gap for every consumer that uses the record surface; the
ADR-111 sweep's grace window remains as defense in depth for crash debris and for direct users
of the low-level surface.

### 5. Garbage collection

Orphan definition after this ADR: a stored object whose `ContentRef` has zero rows in
`attachments`. The transactional sweep semantics of ADR-111 (locking, counters, grace window,
dry run) are unchanged; only the reference-counting query changes, and it now reads from a
single indexed table across both substrates.

### 6. Record deletion

`record_uuid` is polymorphic across two substrates, so the `attachments` table declares no
foreign key — nothing at the SQLite layer enforces cleanup, and `PRAGMA foreign_keys` covers
only declared constraints. Cleanup is therefore an explicit contract of the delete path:

- **Hard delete** of a record removes its `attachments` rows in the same transaction that
  removes the record (alongside the existing edge cascade). The blobs themselves are not
  touched inline; with their rows gone they become sweep-eligible orphans.
- **Soft delete** leaves `attachments` rows in place, exactly as it leaves edges — the record
  is recoverable, so its content must remain anchored.

Without this cascade, a hard-deleted record's rows would pin its blobs forever: the sweep's
orphan definition (§5, zero rows in `attachments`) would read them as live indefinitely.
Delete-cascade reclamation is a tested contract (see Rollout).

### 7. Promotion is a metadata operation

Content addressing makes "casual payload becomes named thing" free: a photo that arrived as a
message-note attachment and later proves worth keeping is promoted by creating the entity and
attaching the _same_ `ContentRef` — no byte copy, no re-upload. The message note remains
exactly what it was; the record of what happened is not rewritten to serve the new view.

---

## Consumers

The first consumer named by this decision was **the channel message components**. Inbound media
messages — a voice message, a shared photo — land as message notes whose text field carries
the searchable rendition (transcript, caption) and whose raw payload attaches under a media
role. This is the note-attachment case of §2 verbatim, and it is the consumer that makes the
capability observable end to end: a media message arrives over a channel transport, is stored
with both renditions, and is retrievable through the record surface.

Follow-on consumers, in expected order: document ingestion (paper entities holding PDF/HTML
renditions) and multimodal retrieval (embedding fan-out over stored media renditions), each
under its own design record.

ADR-160 Phase 4 changes implementation order: moodboard visual assets and preference-model
artifacts are the first live consumers of the internal attachment substrate. Channel ingestion and
the agent-facing attachment verbs remain follow-on work; this ordering change does not alter the
utterance-versus-thing rule above.

---

## Consequences

**Positive.**

- One model answers documents-with-formats, multimodal messages, sealed artifacts, and future
  media retrieval, with a two-clause rule (version vs. rendition; utterance vs. thing) that
  keeps the graph free of unnamed byte-carrier records.
- The dangling-reference race class is closed at the surface where agents actually publish
  content, rather than mitigated by timing heuristics.
- GC gains a single reference source; the sweep's correctness argument gets shorter.
- Text search and embedding pipelines are untouched: they continue to operate on the text
  fields, which the model now explicitly designates as the searchable rendition.

**Negative / accepted costs.**

- A schema migration with a column drop (backfill `entities.content_ref` → `attachments`).
  Single-writer migration discipline per ADR-015 applies.
- The sweep and any existing consumer of `entities.content_ref` must move to the
  `attachments` table in the same change set — a coordinated, not incremental, landing.
- Note records gain a byte-bearing surface, which grows the storage footprint of
  conversational data. Size caps and per-deployment quotas are a policy concern for the gate,
  not schema; this ADR sets no limit.

**Deferred (named, not designed here).**

- Pin-from-URL ingestion (fetch an external reference into the store and attach it).
- Presigned-upload negotiation for remote deployments (ADR-111 Amendment 2 backend).
- Embedding fan-out over non-text renditions (multimodal retrieval consumes this model; its
  design is its own record).
- Streaming/chunked hydration for large objects; `get(hydrate=…)` is whole-object base64 in v1.

---

## Alternatives considered

1. **Entity-only attachments (notes excluded).** Rejected: it forces every voice message and
   shared photo to mint a carrier entity nobody will name, link, or traverse — graph pollution
   that inverts the "worth naming → entity" rule. The utterance-vs-thing boundary keeps note
   attachments disciplined without banning them.
2. **One blob per record (keep the single `content_ref` column).** Rejected: real documents
   have parallel formats; overloading one slot forces either lossy choices or carrier-record
   proliferation. The role key is the smallest structure that fits the observed shapes.
3. **Versioning via roles (`"v1"`, `"v2"` on one record).** Rejected: version history is
   information-bearing structure — it belongs in the graph (`supersedes`), where it can be
   traversed, annotated, and filtered by the view layer. Roles carry format, never history.
4. **URLs as attachment values (`role → ContentRef | URL`).** Rejected: it destroys the GC
   invariant (a ref either counts or does not), conflates held content with external
   reference, and duplicates what properties already express.
5. **Extending the low-level `blob.*` pack instead of the record surface.** Rejected: the
   published-bytes-then-reference seam is exactly the race the substrate should own; a
   hash-addressed surface cannot make record+content publication atomic.

---

## Rollout

1. Migration: `attachments` table + backfill + `entities.content_ref` drop; sweep re-pointed
   at the new table in the same change.
2. `kg` pack verbs: `attach` / `detach` / `export`, `create(attach=)`, `get(hydrate=)`,
   `fp=` deployment gating.
3. Conformance tests: rendition rule enforcement is conventions-and-docs (roles are free
   strings); atomicity, GC single-source, delete-cascade reclamation (hard delete removes
   attachment rows in-transaction and the freed blobs become sweep-eligible; soft delete
   retains them), promotion-by-ref, and hydration behavior are tested contracts.

### Implementation state after ADR-160 Phase 4

Rollout step 1 uses two releases. Phase 4a changes no schema or data and ships only the exact-V21
transactional-GC epoch gate. After fleet convergence, old-binary drain, and quiescence of every
Phase-4a application reader/writer, Phase 4b implements one coordinated schema/consumer cutover:
typed storage and SQLite attachment stores, legacy `"content"` backfill, current entity/moodboard
reader and writer migration, transactional hard-delete cleanup, attachment-only blob liveness and
claim fences, authenticated `"fann-network"` reconstruction, and removal of
`entities.content_ref`. A Phase-4a GC-only worker is narrowly safe on exact completed V21, but is
not a schema-compatible entity server; the Phase-4b fleet starts only after exact-current topology
validation. The canonical main/core database is the only attachment and GC-liveness authority;
secondary runtime handles must route through `KhiveRuntime::core()` and direct attachment access on
a secondary is rejected.

The agent-facing `kg` verbs in rollout step 2 (`attach`, `detach`, `export`, create-with-attach,
and selective hydration) remain deferred. Phase 4 adds internal runtime/storage publication seams
for existing consumers; it does not claim the complete ADR-121 public verb rollout.
