# ADR-121: Attachments — Role-Keyed Blob Renditions as a First-Class Substrate Property

**Status**: accepted\
**Date**: 2026-07-23\
**Authors**: khive maintainers\
**Amended by**: [ADR-160](ADR-160-shared-pack-infrastructure.md) (accepted 2026-08-16), whose moodboard migration
consumes this accepted role-keyed desired state, makes the canonical main backend the sole
attachment/GC-liveness authority, and specifies a two-release GC-compatibility/deployment gate plus
a boot-gated two-stage cutover rather than extending legacy `entities.content_ref`; and by its own
Amendment 1 below (initial version accepted 2026-09-25; revised text pending sign-off), which schedules the attachment orphan sweep in the daemon,
adds the dry-run `blob.sweep` verb and puts on-demand deletion in the admin CLI.\
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

## Amendment 1 (2026-09-25): the orphan sweep runs on a schedule and on demand

**Status: Revised text pending sign-off (initial version accepted 2026-09-25).** Refs #3038, #3178. This amendment also amends one sentence of
[ADR-111](ADR-111-blob-store.md) §8, named in item 3.

### Why

§5 and §6 rest on the ADR-111 sweep running. A hard delete removes a record's attachment rows in its
own transaction, and "with their rows gone they become sweep-eligible orphans"; `detach` likewise
says the blob "is reclaimed by the sweep once no references remain". Nothing in a serving process runs that sweep: every
caller of `BlobStore::transactional_orphan_sweep` is a test. So every blob freed by a hard delete or a
`detach` stays in the store indefinitely.

### Decision

1. **Scheduled sweep, daemon only.** The warm daemon (`kkernel mcp --daemon`) runs
   `transactional_orphan_sweep` against the main backend on a fixed cadence, set by
   `KHIVE_BLOB_SWEEP_INTERVAL_SECS` (default 86400, one day; `0` disables the schedule). A
   session-mode process never schedules it. The first run starts one interval after the daemon starts,
   and each later run starts one interval after the previous run ended. A daemon restarted more often
   than its interval never reaches a scheduled run; the admin command in item 3 covers that case, and
   a deployment that restarts often sets a shorter interval.
2. **Dry run first, live only by opt-in.** A scheduled run is a dry run unless `KHIVE_BLOB_SWEEP_LIVE=1`
   is set. A deployment therefore starts in dry-run mode, and its first scheduled cycle reports
   `would_delete` and deletes nothing. An operator enables live mode only after reading the counters of
   at least one dry-run cycle, from the log line in item 4 or from the `blob.sweep` verb. The daemon
   never switches modes on its own. The switch governs the scheduled pass only: no MCP request can
   make a pass delete (item 3).
3. **On demand: a dry-run verb and an admin command.** `blob.sweep()` runs one dry-run pass on demand
   and returns the four counters of `BlobOrphanSweepResult` (`scanned`, `would_delete`, `deleted`,
   `grace_period_skipped`; `deleted` is always 0) and the mode it ran in. The verb has no live mode.
   No argument or switch makes an MCP request delete. A process-wide switch set for the schedule
   would otherwise let any caller the Gate (ADR-018) admits for writes delete on demand, so on-demand
   deletion sits behind the operator's own command instead of behind a Gate policy that every
   deployment would have to narrow correctly. It reaches the main backend only. It is classified `Write` in the [ADR-129](ADR-129-fail-closed-gate-default.md) Amendment 3
   operation table, as `gtd.repair` is, because a pass holds the blob store's write lock (item 4) and
   blocks uploads for its length; a `deny_writes_for` restriction therefore denies it.
   On-demand deletion is an operator action: `kkernel blob sweep [--live]` in the admin CLI, which
   ADR-003 keeps apart from the MCP surface and ADR-109's gateway mode never exposes. It resolves the
   database and configuration from the operator's selected profile, but refuses both modes unless
   the resolved database is that profile's canonical main backend; an arbitrary SQLite secondary is
   never a GC-liveness authority, even when its schema is current. If the command cannot prove main
   backend identity, it refuses before walking the blob root. It runs the same
   `transactional_orphan_sweep`, is a dry run unless `--live` is given, and prints the four counters
   and the mode. `--live` is the operator's opt-in for that pass;
   `KHIVE_BLOB_SWEEP_LIVE` does not apply to it. ADR-111 §8 says the orphan sweep is "an admin-side
   operation, not an MCP verb"; that sentence is amended to apply to the caller-snapshot
   `orphan_sweep` only, which stays admin-side and, on the filesystem backend, disabled. Either pass,
   verb or admin command, that finds another sweep holding ownership waits for it, bounded by its
   deadline (the verb's caller deadline; the admin command's `--timeout`), and the verb returns the
   retryable timeout error. A scheduled pass likewise uses a finite ownership-wait deadline, no
   longer than 30 seconds, and cancels that wait on daemon shutdown; expiration or shutdown skips
   that cycle without acquiring the lock later or deleting after the caller has gone. Ownership
   includes ADR-111 §8's cross-process advisory lock, so an admin pass and a scheduled pass in the
   daemon never run at once. A pass abandoned at its deadline stops
   there: it must not acquire ownership later and run on with no caller to report to. An ADR-111 §8
   epoch refusal is returned as the error unchanged, and a backend without a transactional sweep
   returns its `Unsupported` error.
4. **The counters are the artifact.** Every scheduled run logs one line with its mode and the four
   counters, or the error when the sweep refuses. `deleted` is the reclaimed-object count; before the
   correction in item 5, ADR-191 Amendment 1 asked a scheduled sweep to report it. A pass holds the
   blob store's per-root write lock across its whole walk and every claim batch, dry run included, and
   `blob.put` takes the same lock, so uploads wait for the length of a pass. The log line therefore also
   reports the pass duration; bounding the walk is follow-up work if that wait matters.
5. **Out of scope: rows whose record is gone.** The sweep counts every attachment row as live, whether
   or not its record still exists. It therefore cannot reclaim a blob whose attachment row outlived its
   record, which is the case ADR-191 A1.2 describes for an interrupted cross-backend hard delete.
   Removing those rows is #3178. ADR-191 A1.2's sentence saying that scheduling this sweep bounds that
   leak is corrected in the same change.
6. **Admit the current schema epoch before enabling any pass.** ADR-160's Phase-4a gate admits only
   the exact completed V21 migration ledger. The serving binary on current main applies migrations
   through V40, so that gate refuses dry runs as well as deletion on every current database. The
   implementation of items 1–4 must extend `blob_gc_fencing_complete` in the same change: explicitly
   admit the reviewed, fully migrated current epoch (V40 as of this revision), while preserving the
   completed cutover marker, attachment-only schema, absent legacy reference column, functional
   attachment claim fences, and canonical migration names and contiguous ledger. Review migrations
   V22–V40 for changes to blob references, attachment liveness, and claim fencing; their current DDL
   does not add another blob-reference authority, but the implementation must prove that with a
   full-chain migration fixture and a live-object retention control. Do not replace the exact-V21
   check with `version >= 21` or automatically accept the latest compiled migration: an unknown or
   ahead-of-reviewed epoch remains `Unsupported` before root locking, filesystem walking, or claim
   cleanup. Each future migration that advances the admitted epoch must repeat the liveness review
   and update the gate and its tests in the same change. ADR-160's exact-V21 rule remains the
   historical Phase-4a rollout contract; this is a later, separately reviewed extension.

### Acceptance

- A database migrated through the real complete current migration chain (V40 at this revision),
  without hand-editing `_schema_migrations`, admits both dry-run and live transactional sweeps. A
  referenced object under every attachment role survives both; an older unreferenced object is
  reported in dry run and reclaimed only in live mode. A missing/corrupt ledger entry or an
  unreviewed later migration refuses both modes before root locking or a filesystem walk. A missing
  or nonfunctional claim fence refuses before new claims, claim cleanup, or deletion. The historical
  exact-V21 control still passes.
- A daemon with a short interval and an orphan older than the grace period runs a dry-run cycle that
  logs `would_delete=1`, `deleted=0` and leaves the object in place. With `KHIVE_BLOB_SWEEP_LIVE=1` the
  next cycle deletes it and logs `deleted=1`.
- A session-mode process, and a daemon with `KHIVE_BLOB_SWEEP_INTERVAL_SECS=0`, start no schedule.
- `blob.sweep()` deletes nothing, with or without `KHIVE_BLOB_SWEEP_LIVE=1` set on the serving
  process, and its `would_delete` equals the `deleted` of a live pass over the same store. A request
  carrying a live flag is rejected as an unknown argument. A caller under a `deny_writes_for`
  restriction is denied `blob.sweep`.
- `kkernel blob sweep` without `--live` deletes nothing; with `--live` it deletes an orphan older than
  the grace period and prints `deleted=1`, whether or not `KHIVE_BLOB_SWEEP_LIVE` is set. A configured
  secondary SQLite database with an empty attachments table is refused in both modes before the blob
  root is walked; an unconfigured target is refused likewise.
- `kkernel blob sweep --live` started while a scheduled run in the daemon holds ownership either
  completes after it or exits with the timeout error when `--timeout` passes first. Neither deletes an
  object whose attachment row committed while it waited, and a pass that timed out performs no
  deletion afterwards.
- A scheduled pass waiting for ownership skips the cycle within 30 seconds, or sooner on daemon
  shutdown, logs why it skipped and the elapsed wait, and never runs later after that cancellation.
