# ADR-121: Attachments — Role-Keyed Blob Renditions as a First-Class Substrate Property

**Status**: accepted\
**Date**: 2026-07-23\
**Authors**: khive maintainers\
**Amended by**: [ADR-160](ADR-160-shared-pack-infrastructure.md) (accepted 2026-08-16), whose moodboard migration
consumes this accepted role-keyed desired state, makes the canonical main backend the sole
attachment/GC-liveness authority, and specifies a two-release GC-compatibility/deployment gate plus
a boot-gated two-stage cutover rather than extending legacy `entities.content_ref`; and by its own
Amendment 1 below (proposed), which specifies a gated attachment orphan sweep in the daemon,
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

**Status: Proposed.** Refs #3038, #3178. This amendment also proposes to amend one sentence of
[ADR-111](ADR-111-blob-store.md) §8, named in item 3.

### Why

§5 and §6 rest on the ADR-111 sweep running. A hard delete removes a record's attachment rows in its
own transaction, and "with their rows gone they become sweep-eligible orphans"; the deferred `detach`
verb likewise specifies reclamation after its last reference is removed. Nothing in a serving process runs that sweep: every
caller of `BlobStore::transactional_orphan_sweep` is a test. So every blob freed by a hard delete or a
`detach` stays in the store indefinitely.

### Decision

1. **Scheduled sweep, daemon only.** After the admission checks in items 6–8, the warm daemon
   (`kkernel mcp --daemon`) runs `transactional_orphan_sweep` against the canonical main backend
   and its bound blob root on a fixed cadence, set by
   `KHIVE_BLOB_SWEEP_INTERVAL_SECS` (default 86400, one day; `0` disables the schedule). A
   session-mode process never schedules it. The first run starts one interval after the daemon starts,
   and each later run starts one interval after the previous run ended. A daemon restarted more often
   than its interval never reaches a scheduled run; the admin command in item 3 covers that case, and
   a deployment that restarts often sets a shorter interval.
2. **Dry run first, live only by opt-in.** A scheduled run is a dry run unless `KHIVE_BLOB_SWEEP_LIVE=1`
   is set. Dry runs obey the same epoch, liveness-completeness and store-binding gates as live runs;
   an incomplete inventory must refuse rather than report live objects as `would_delete`. A deployment
   therefore starts in dry-run mode, and its first admitted scheduled cycle reports
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
   deployment would have to narrow correctly. It uses the canonical main backend even when
   `[packs.blob].backend` routes ordinary blob verbs to a secondary. It is classified `Write` in the [ADR-129](ADR-129-fail-closed-gate-default.md) Amendment 3
   operation table, as `gtd.repair` is, because a pass holds the blob store's write lock (item 4) and
   blocks uploads for its length; a `deny_writes_for` restriction therefore denies it. Adding this
   explicit classification must bump the versioned classifier identity used in nonempty Gate policy
   fingerprints.
   On-demand deletion is an operator action: `kkernel blob sweep [--live]` in the admin CLI, which
   ADR-003 keeps apart from the MCP surface. It resolves the effective configuration (the loaded
   `khive.toml`, if any, plus CLI and environment database/root overrides) and requires the selected
   database and root to pass the durable main-owner binding in item 8. An arbitrary SQLite secondary
   is never a GC-liveness authority, even when its schema is current. An unbound database/store pair
   means either side lacks that matching durable binding; `--db` alone, including with no declared
   `[[backends]]`, never proves main ownership. The command refuses an unbound pair before acquiring
   any database/root ownership lock or starting a filesystem walk. It runs the same
   `transactional_orphan_sweep`, is a dry run unless `--live` is given, and prints the four counters
   and the mode. `--live` is the operator's opt-in for that pass;
   `KHIVE_BLOB_SWEEP_LIVE` does not apply to it. ADR-111 §8 says the orphan sweep is "an admin-side
   operation, not an MCP verb"; that sentence is amended to apply to the caller-snapshot
   `orphan_sweep` only, which stays admin-side and, on the filesystem backend, disabled. Either pass,
   verb or admin command, that finds another sweep holding ownership waits for it, bounded by its
   deadline (the verb's caller deadline; the admin command's `--timeout`), and the verb returns the
   retryable timeout error. A scheduled pass likewise uses a finite ownership-wait deadline, no
   longer than 30 seconds. Daemon shutdown cancels an interval timer or ownership wait, so that
   cycle cannot acquire the lock later. Filesystem enumeration is made cancellable in bounded
   chunks; a pass already walking checks shutdown between chunks, and a pass deleting finishes and
   releases its current bounded claim batch. Neither starts another chunk or batch after observing
   cancellation. The supervisor waits for that cooperative stop and ownership release rather than
   assuming aborting an async task cancels its blocking filesystem work. Ownership includes
   ADR-111 §8's cross-process advisory lock, so an admin pass and a scheduled pass in the daemon
   never run at once. A pass abandoned at its deadline stops
   there: it must not acquire ownership later and run on with no caller to report to. An ADR-111 §8
   epoch refusal is returned as the error unchanged, and a backend without a transactional sweep
   returns its `Unsupported` error.
4. **The counters are the artifact.** Every scheduled run logs one line with its mode and the four
   counters, or the error when the sweep refuses. `deleted` is the reclaimed-object count; the
   earlier ADR-191 Amendment 1 asked a scheduled sweep to report a count of rows reclaimed, which
   is a different quantity. A pass holds the blob store's per-root write lock across its whole walk
   and every claim batch, dry run included, and
   `blob.put` takes the same lock, so uploads wait for the length of a pass. The log line therefore also
   reports the pass duration. The filesystem sweep's default publish grace is one hour; test orphans
   must have an observed age greater than the configured grace, rather than relying on a fresh put.
   Bounding each walk and its upload wait remains follow-up work.
5. **Out of scope: rows whose record is gone.** The sweep counts every attachment row as live, whether
   or not its record still exists. It therefore cannot reclaim a blob whose attachment row outlived its
   record, which is the case ADR-191 A1.2 describes for an interrupted cross-backend hard delete.
   Removing those rows is #3178. A dated correction proposed alongside this amendment records why
   ADR-191 A1.2's scheduling claim does not bound that leak.
6. **Admit only a reviewed schema epoch.** ADR-160's Phase-4a gate admits only the exact completed
   V21 migration ledger. Define one explicit `REVIEWED_SCHEMA_EPOCH = 41` for this amendment's
   core-schema review, matching the terminal core migration on main when this amendment was written.
   The implementing change must use that named constant in the exact-epoch predicate and its
   fixtures, rather than repeat a version literal or derive admission from the latest compiled
   migration. It must review the complete
   V22-through-`REVIEWED_SCHEMA_EPOCH` core migration chain, including `sender_transport`, together
   with pack-owned schemas and blob-writing paths. V40 is not admitted separately. The predicate
   retains the historical gate's completed cutover marker, absent legacy reference column,
   functional attachment claim fences and contiguous ledger, and adds canonical migration-name
   validation for every row below the reviewed terminal version; the current gate checks only the
   V21 name and leaves below-terminal name validation to boot. The liveness ownership rows and store
   binding in items 7 and 8 must also pass before a full sweep. The admission key is the reviewed
   schema epoch **and** the recorded store identity, never an epoch alone. Pack DDL outside
   `_schema_migrations` and references inside blob manifests cannot be inferred from the core
   ledger. A full-chain migration fixture and live-object retention controls must prove the gate.
   The implementing change must compare the compiled migration tip with the named reviewed epoch so
   adding a migration fails the gate until that migration is reviewed and the constant is updated in
   the same change. Do not replace the exact-V21 check with `version >= 21`. An unknown or
   ahead-of-reviewed epoch refuses both modes before root locking, filesystem walking, or claim
   cleanup. A new migration for items 7 or 8 also requires review through its new terminal epoch
   and an update to the single named constant; it inherits no earlier full-sweep admission. Every
   change to core or pack-owned liveness schema, producer code or manifest format repeats that review
   and updates the gate and tests in the same change.
   ADR-160's exact-V21 rule remains the historical Phase-4a rollout contract.
7. **Use registered ownership as the complete liveness authority.** Choose option (a): every
   production path in any crate linked into `kkernel` that writes to the shared runtime blob store
   and persists or returns a ref as durable must register each such root in the canonical main
   database, either as an `attachments.content_ref` row or in a new versioned
   `blob_pack_owners` table. That table records at least `(store_id, content_ref, owner_pack,
   owner_kind, owner_id, format_version)` with a unique owner/ref key; `owner_pack` also identifies
   non-pack producers such as `khive-mcp`. It is the ownership row, not an ad hoc query over
   producer-specific JSON, that authorizes retention and release. This lets the
   sweep and all pack writers use one fenced SQL authority even when pack records live on secondary
   backends. Only raw `blob.put`/`blob.commit` output that no production path persists or returns as a
   durable ref is an explicitly unowned candidate after the grace period. Every path that persists
   or returns such a ref as durable must register it before publication. The sweep's atomic
   candidate-claim anti-join reads every attachment role, every live ownership row for the bound
   `store_id`, conservative legacy pins and transitive manifest closure. Registration (including
   idempotent put of an older unowned ref) and
   release serialize with claim/delete, so a newly published owner never points to deleted bytes.

   Exec registers run receipts (`tree_in`, `tree_out`, stdout, stderr, sandbox profile and
   changed/base refs) and receiptless `exec.tree`/`exec.tree_put` outputs; git registers input trees,
   checkout trees and diffs; persisted web derived-text bodies and the existing moodboard and
   network bodies use main-backend attachments. The `khive-mcp` channel quarantine path registers
   each original as a main-backend attachment on its quarantine message before publishing
   `quarantine_content_ref`, even though it writes through `registry.dispatch("blob.put", ...)` rather
   than a direct `BlobStore` call. The web pack must root a derived-text resource body as an
   attachment under ADR-191 A1.2; a `properties.blob_ref` alone does not keep it.
   Every live `khive-tree/v1` manifest keeps its entry refs live recursively: registration
   materializes a checked child-owner row for each transitive ref before publishing the root.
   ADR-181's exec
   receipts and ADR-182's git tree refs remain readable through `blob.get` while their owning
   receipts are live; a returned receiptless tree and its entries stay pinned until a separately
   approved release rule exists. An owner row is removed only after its source record is no longer
   live under that pack's retention contract; an unknown release rule retains the row.

   A test-time census covers production source in every crate linked into the `kkernel` binary,
   including non-pack crates and packs absent from a particular test configuration. For direct store
   calls it keys on the resolved receiver type, not helper names, file paths or grep patterns: every
   call resolving to a `BlobStore` write method (`put`, `begin_upload`, `append_part`,
   `commit_upload` or any future write method) and every function that reaches such a call
   transitively is in scope. A second arm covers string-keyed verb dispatch to `blob.put`,
   `blob.begin`, `blob.put_part` or `blob.commit` from any linked crate, including aliases or
   constructed verb names that can resolve to those writes. The trace follows
   `KhiveRuntime::blob_store()`, pack accessors such as `tree::blob_store`, wrappers taking
   `&KhiveRuntime` or `&dyn BlobStore`, paths that return an existing ref, and verb-dispatch callers.
   Each reachable write path is classified as main-backend attachment registration,
   `blob_pack_owners` registration or an explicitly unowned producer only if no durable ref is
   persisted or returned; read-only paths are classified separately and cannot justify a write.
   Adding an unclassified writer fails the gate. Must-fail controls plant production-shaped paths
   in a linked-crate fixture: a newly named `f(rt: &KhiveRuntime)` that writes through
   `rt.blob_store()?.put(...)`, `g(store: &dyn BlobStore)` that calls `store.put(...)`, and a
   quarantine-shaped caller that persists the result of `registry.dispatch("blob.put", ...)` in
   message properties. Each alone must trip the census; a staged-upload control using
   `registry.dispatch("blob.commit", ...)` must also trip it. Backfill also inventories all
   historically co-resident pack backends, source
   tables and manifest formats; neither the core
   migration ledger nor an attachment-only scan proves this coverage. The registered-row design
   is chosen over querying each pack's evolving receipt/property schema at sweep time because a
   missing or rerouted pack query would silently turn its live refs into orphans.

   An existing root needs a one-time exhaustive backfill from every backend that wrote to it,
   followed by a durable completion marker tied to that root, the backend roster and the producer
   versions. The scan and marker commit run under a writer barrier: every pre-protocol writer is
   drained and prevented from restarting, and every continuing writer validates the binding and
   registers new durable refs in the main ownership authority before the marker can become valid.
   A put racing the backfill is either included in the inventory or occurs through that protocol
   after the marker; there is no unobserved interval. The backfill conservatively pins preexisting
   objects whose provenance cannot be
   established, including unrecorded `exec.tree` manifests and their child refs; such pins may leak
   space until a separately reviewed retention rule exists. It never classifies an unknown old
   object as collectible solely because its attachment row is absent. A missing or malformed
   reachable manifest, unknown co-resident backend, incomplete backfill or unregistered blob writer
   makes both modes `Unsupported` before sweep ownership or walk. In dry run these objects must not
   appear as `would_delete`. New durable refs are registered on the main backend before a receipt,
   entity property or returned durable tree ref exposes them; a failed registration fails that
   publication. Deletion removes the owning record before its root, so a crash can retain bytes but
   cannot expose a live record to collection. Every blob writer admitted to a bound root validates
   its owner binding; a second database or pre-protocol binary cannot publish a durable ref there
   outside the main ownership authority. Every owner-registration write shares the claim fence
   (or stronger serialization) with the sweep, including writes from a pack routed to a secondary.
   This item extends ADR-111 §8 and ADR-160 Phase 4's attachment-only liveness rule for these pack
   objects while keeping the canonical main backend as the sole SQL authority.
8. **Bind the root to its main database at cutover.** A filesystem blob root has one durable owning
   main database. For a provably new empty root, a new attachment cutover first records a pending
   store ID in the main database, then durably writes and verifies the matching anchored marker in
   the canonical root. A populated root instead requires a separately invoked verified adoption
   that establishes this same pending ID and marker before cutover can complete; daemon boot never
   claims it automatically. Only then does one database transaction mark both the binding and
   cutover complete with that same ID.
   A crash before this final transaction leaves the cutover incomplete and sweeps refuse. Recovery
   reuses and verifies the pending ID and any existing matching root marker; it never mints a new ID
   for that attempt or overwrites a different owner's marker. A cutover completed under this rule implies a durable
   verified root marker. A database already cut over before this rule obtains its store ID only
   through the separate verified adoption action below; the
   old V21 completion marker alone cannot mint one. The database binding and root marker identify
   the same `store_id`, root and owner, including a durable database identity and canonical
   database file identity; a copied database at another path cannot inherit ownership by copying
   its rows. A completed attachment cutover, empty `attachments`, a database path supplied by
   `--db`, or the path-derived GC claim key is not binding proof. `FsBlobStore` checks the binding
   against the supplied `SqlAccess` for _both_ sweep modes before any database/root ownership lock
   or filesystem walk, then rechecks after holding both the database GC owner and root write lock,
   immediately before walking or deleting. A reviewed epoch with a missing or different `store_id`
   is `Unsupported` just like an unreviewed epoch. Missing, corrupt, stale or mismatched evidence
   returns a typed refusal with no fallback. An in-memory database cannot own a
   durable filesystem root under this rule. Binding or rebinding a populated root is a separate
   verified adoption action, never an automatic side effect of a sweep or boot. Adoption records a
   pending database store ID, durably writes and verifies the root marker under the same owner/root
   locks, and records binding completion only after they match. Recovery reuses that pending ID;
   adoption cannot infer an ID from a path or silently overwrite another owner's marker.
   Provisioning proves the selected database is the effective topology's canonical main (`KhiveRuntime::core()` in a
   daemon); with no declared `[[backends]]`, a selected `--db` becomes main only through an existing
   valid binding or this explicit provisioning action. It takes the affected
   database GC owners in a deterministic order and then the root write lock, following the sweep's
   database-before-root order, and holds them through the durable update and verification. The scheduler,
   `blob.sweep` and the admin command all supply the canonical main backend irrespective of the
   blob pack's routing, and all three refuse an unbound pair. Root resolution still follows
   `KHIVE_BLOB_ROOT`, configured root, then `<db_dir>/blobs`; the resolved _canonical_ root is what
   the binding names. A root shared with a second fully migrated database is refused for that
   database, even if its attachment table is empty or its database UUID was copied.

### Acceptance

- A database migrated through the real complete chain to `REVIEWED_SCHEMA_EPOCH`, without
  hand-editing `_schema_migrations`, passes the explicit current core-schema predicate but refuses a
  full sweep until the ownership rows and binding in items 7–8 are complete. The separate migration
  that adds `blob_pack_owners` needs review through its new terminal epoch and an update to the
  named constant; the earlier core-schema predicate does not grandfather it. At that fully reviewed
  epoch, a bound store ID admits dry-run and live transactional sweeps. A referenced object under
  every attachment role survives both; an object
  absent from attachments, registered roots, legacy pins and manifest closure, created after the
  inventory cutover and older than the configured grace (one hour by default) is reported in dry run
  and reclaimed only in live mode. Test objects are backdated beyond that grace. A missing/corrupt
  ledger entry or any migration ahead of the
  named reviewed epoch refuses both modes before root locking or a filesystem walk. A missing or
  nonfunctional claim fence refuses before new claims, claim cleanup or deletion. The historical
  exact-V21 control still passes its epoch predicate; without the inventory and binding, even a V21 full
  sweep refuses.
- A live exec receipt's input and output trees, every tree entry, stdout, stderr, sandbox profile,
  and changed/base refs remain readable through `exec.receipt`, `exec.tree_get` and `blob.get`
  after an older-than-grace live scheduled pass and live admin pass. A receiptless `exec.tree` or
  `exec.tree_put` manifest, including an edited entry and a pre-cutover tree with its child refs,
  also survives both. A
  live git checkout receipt's tree and entries and a git diff receipt's blob likewise remain
  readable through the git receipt and `blob.get`. A persisted web extracted-text resource has a
  main-backend content attachment and survives; a pre-cutover derived resource whose only old root
  was `properties.blob_ref` is retained by backfill as well. Moodboard originals, model bundles and network
  bodies survive under every existing attachment role. A live channel-quarantine message's original,
  written through `blob.put` and referenced by `quarantine_content_ref`, has a main-backend
  attachment and survives an older-than-grace live scheduled pass and a live admin pass;
  `blob.get` returns its original bytes exactly after both. The same objects are absent from
  `would_delete` in dry runs.
- A test-time census fails if any crate linked into `kkernel` adds a direct `BlobStore` write or blob
  write-verb dispatch without a main-backend attachment, `blob_pack_owners` or a justified explicit
  unowned classification.
  It resolves receiver types, enumerates blob verb dispatches and follows runtime/pack accessors
  and transitive callers, regardless of names or which packs a test enables. Synthetic writers
  through `f(rt: &KhiveRuntime)`, `g(store: &dyn BlobStore)`, and a quarantine-shaped
  `registry.dispatch("blob.put", ...)` each fail the census until classified; so does a
  `registry.dispatch("blob.commit", ...)` upload completion. A durable-ref producer cannot pass as
  explicitly unowned.
  A planted older-than-grace exec tree and git checkout survive dry run and live sweep via their
  ownership rows, even when their source receipts are stored on secondary pack backends. Removing
  either row after the source becomes nonlive makes the ref eligible only under its approved release
  rule; an unclassified source never becomes eligible by default.
- A populated root with no completed producer/backfill inventory, an unknown historically
  co-resident backend, an unregistered writer, or an unreadable reachable tree manifest refuses
  _both_ modes before ownership or walk. Backfill from all known pack backends pins old unknown
  objects rather than deleting them by age. A new durable reference published concurrently with a
  candidate claim either fences the claim or waits; it never returns a reference to a deleted blob.
  A put during backfill is included or registered after the barrier, never lost between scan and
  completion. A second database cannot publish a durable reference into the bound root and then
  have that object collected by the owner. Rebinding races with an active pass without changing
  ownership underneath its walk or deletion.
- A daemon with a short interval and an orphan older than the grace period runs a dry-run cycle that
  logs `would_delete=1`, `deleted=0` and leaves the object in place. With `KHIVE_BLOB_SWEEP_LIVE=1` the
  next cycle deletes it and logs `deleted=1`.
- A session-mode process, and a daemon with `KHIVE_BLOB_SWEEP_INTERVAL_SECS=0`, start no schedule.
- `blob.sweep()` deletes nothing, with or without `KHIVE_BLOB_SWEEP_LIVE=1` set on the serving
  process, and its `would_delete` equals the `deleted` of a live pass over the same store. A request
  carrying a live flag is rejected as an unknown argument. A caller under a `deny_writes_for`
  restriction is denied `blob.sweep`.
- `kkernel blob sweep` without `--live` deletes nothing; with `--live` it deletes an orphan older than
  the grace period and prints `deleted=1`, whether or not `KHIVE_BLOB_SWEEP_LIVE` is set. In a
  two-backend daemon with `[packs.blob].backend` selecting a secondary, the schedule and
  `blob.sweep` still use the bound main database; a direct sweep handed the fully migrated
  secondary's empty-attachment SQL refuses in both modes before a lock or walk. A second migrated
  database sharing that root, once through the default same-directory root and once through
  `KHIVE_BLOB_ROOT`, refuses through `--db` and the direct API in both modes, even when it has a
  copied database UUID. Crashes before the root marker, after that marker but before database
  completion, and after completion are replayed: both sweep modes refuse before any lock or walk
  until the final state has matching durable IDs, and replay preserves the pending `store_id`
  without overwriting another owner's marker. A populated-root cutover without explicit adoption
  also refuses; an already-completed cutover never silently mints or replaces an ID. The same
  reviewed epoch with an absent or mismatched cutover `store_id`
  refuses before a lock or walk. Missing, corrupt and stale binding markers, root relocation and an unbound
  database/store pair refuse likewise. No command treats an arbitrary `--db` as proof of ownership.
- `kkernel blob sweep --live` started while a scheduled run in the daemon holds ownership either
  completes after it or exits with the timeout error when `--timeout` passes first. Neither deletes an
  object whose attachment row committed while it waited, and a pass that timed out performs no
  deletion afterwards.
- A scheduled pass waiting for its interval or ownership skips the cycle within 30 seconds of an
  ownership wait, or sooner on daemon shutdown, logs why it skipped and the elapsed wait, and never
  runs later after cancellation. Shutdown during a walk or delete finishes at most the current
  bounded enumeration chunk or claim/delete/release unit; no later unit starts, all guards are released, and a
  replacement process can acquire ownership without inheriting an active blocking task.

## Amendment 2 (2026-09-25): the issues Amendment 1 item 7 answers, and the blob writer census population

**Status: Accepted (2026-09-25).** Refs #3327, #3273, #3344, #3038, #3178. A follow-up to Amendment 1; it does not
change Amendment 1's text or status, and it binds only together with Amendment 1.

### Why

Amendment 1 cites #3038 and #3178. Its item 7 also answers two filed defects it does not cite, and its
census leaves one population question open.

- #3327: exec run receipts (stdout, stderr and profile refs), `khive-tree/v1` manifests and their
  entries, and git checkout trees and diffs are referenced only from pack receipts and manifests, so an
  attachment-only liveness query treats them as orphans. Item 7's `blob_pack_owners` rows and
  transitive manifest closure are the resolution.
- #3273: the channel quarantine path in `crates/khive-mcp/src/serve.rs`
  (`quarantine_channel_ingest_failure`) stores an inbound original through `dispatch("blob.put", ...)`
  and keeps the ref only in the quarantine message's `quarantine_content_ref` property. Item 7 requires
  that original to be a main-backend attachment on the quarantine message, and its census covers
  string-keyed dispatch.
- #3344: item 7's census covers "every crate linked into the `kkernel` binary" but does not say under
  which cargo features. Several writers are feature-gated. `khive-pack-moodboard` is optional in
  `crates/kkernel/Cargo.toml` and `crates/khive-mcp/Cargo.toml`; the channel crates are optional in
  `crates/khive-mcp/Cargo.toml`; `quarantine_channel_ingest_failure` is compiled only under
  `cfg(any(feature = "channel-email", feature = "channel-telegram"))`; `kkernel` declares no `default`
  feature; and the serving artifact workflow (`.github/workflows/serving-artifact.yml`) builds
  `kkernel` with `--features pack-formal` only. A census run on the default build cannot see the
  moodboard or quarantine writers, and a future writer behind a non-default feature would pass it.

### Decision

1. **The census population is the union over `kkernel`'s features.** The census runs with every feature
   `kkernel` declares enabled at once, and it reads the declared feature list from cargo metadata and
   refuses to run if the enabled set is not all of it, so a newly added feature cannot be left out
   silently. Because the census keys on resolved receiver types, source that no enabled feature
   compiles cannot be classified; under the all-features rule no linked production source is in that
   state, and if features ever become mutually exclusive, the census runs once per exclusive
   combination and the union of the runs is the result.
2. **A must-fail control behind a feature.** Beside item 7's controls, a production-shaped writer
   placed in a linked-crate fixture behind a feature that is off by default must trip the census.
3. **The survival arms run where the writers are compiled.** The acceptance arms that keep a
   quarantined original and moodboard bodies alive through a live sweep run in a CI job built with
   `channel-email` or `channel-telegram` and `pack-moodboard`; the default matrix alone does not satisfy
   them.

### Alternatives considered

- _Census the default build only._ Rejected: the quarantine and moodboard writers are not compiled
  there, which is the gap #3344 reports.
- _Scan `cfg`-gated source textually and fail closed on anything the enabled set cannot resolve._
  Rejected as the primary rule: item 7 forbids keying on names and grep patterns because they miss
  aliases and wrappers, and a textual scan of uncompiled source is that kind of check. It remains a
  possible second arm if a feature can never be enabled together with the others.
- _Census the serving artifact's feature set._ Rejected: a deployment can build other features, and
  the ownership invariant has to hold for every binary a root can be served by.

### Consequences

- The census job needs an all-features build of the `kkernel` dependency graph; the repository's lint
  pass already builds the workspace with `--all-features`.
- Acceptance adds the arms in items 2 and 3 to Amendment 1's list.
