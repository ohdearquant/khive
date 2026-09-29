# ADR-121: Attachments — Role-Keyed Blob Renditions as a First-Class Substrate Property

**Status**: accepted\
**Date**: 2026-07-23\
**Authors**: khive maintainers\
**Amended by**: [ADR-160](ADR-160-shared-pack-infrastructure.md) (accepted 2026-08-16), whose moodboard migration
consumes this accepted role-keyed desired state, makes the canonical main backend the sole
attachment/GC-liveness authority, and specifies a two-release GC-compatibility/deployment gate plus
a boot-gated two-stage cutover rather than extending legacy `entities.content_ref`; and by its own
Amendment 1 below (accepted 2026-09-25), which specifies a gated attachment orphan sweep in the daemon,
adds the dry-run `blob.sweep` verb and puts on-demand deletion in the admin CLI; Amendment 2 below
(accepted 2026-09-25, binding only with Amendment 1), which names the issues Amendment 1 item 7
answers and the blob writer census population; Amendment 3 below (accepted 2026-09-26), which ships a
read-only report of attachment rows whose record was not found and states what any code that removes
such rows must first supply; and Amendments 4 and 5 below (both accepted 2026-09-28), which record
the reviewed core-schema epochs for blob GC.\
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

**Status: Accepted (2026-09-25).** Refs #3038, #3178. This amendment also proposes to amend one sentence of
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
   Amendment 4 supersedes the historical epoch value with the reviewed V22–V42 snapshot.
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
8. **Bind the root to its main database after cutover.** A filesystem blob root has one durable
   owning main database. The migration that adds binding state and the attachment cutover do not
   mint a store ID, bind a root or write its marker. On any daemon boot while the main database holds
   no completed binding, the daemon may bind its configured canonical root only when attachment
   cutover is complete, the main database has no pending or completed binding and holds no live blob
   reference (no attachment rows and no registered pack-owned refs), and it proves the root empty:
   no blob objects and no root ownership marker of any owner. It checks this under database GC ownership and the
   root write lock. This is the only automatic fresh-root bind. The absence of live refs in main is
   a precondition for this automatic bind, not ownership proof for a sweep. The daemon records the
   pending store ID with a conditional write in one main-database transaction that fails if any
   pending or completed binding row exists; a failed write ends that boot's attempt without touching
   the root. It then durably writes and verifies the matching anchored marker in the canonical root,
   then uses one main-database transaction to mark binding complete with that same ID. The completed
   cutover state is unchanged by that transaction. A populated root, even with one object, or a
   root with a preexisting marker cannot enter this fresh path; neither boot nor sweep claims it
   automatically.

   A boot that dies before the pending ID is recorded leaves no state, so the next boot is a fresh
   attempt. A crash after recording the pending ID but before the completing transaction leaves
   binding pending and both sweep modes refuse. Recovery reuses the pending ID, rechecks that no
   blob objects appeared and verifies its own matching root marker if one was already written. It
   never mints a replacement ID, overwrites a foreign or mismatched marker, or completes an
   automatic bind for a populated root. A marker without a matching pending ID is not a recoverable
   fresh-root attempt. A completed
   binding implies a durable verified root marker. A database cut over before this rule may take
   the fresh-root path only for a provably empty configured root; a pre-rule completed root with
   objects, and every populated root, requires separately invoked verified adoption. The old V21
   completion marker alone cannot mint a store ID or change ADR-160's historical exact-V21
   predicate. The database binding and root marker identify the same `store_id`, root and owner,
   including a durable database identity and canonical database file identity; a copied database at
   another path cannot inherit ownership by copying its rows. A completed attachment cutover, empty
   `attachments`, a database path supplied by `--db`, or the path-derived GC claim key is not
   binding proof. `FsBlobStore` checks the binding
   against the supplied `SqlAccess` for _both_ sweep modes before any database/root ownership lock
   or filesystem walk, then rechecks after holding both the database GC owner and root write lock,
   immediately before walking or deleting. A reviewed epoch with a missing or different `store_id`
   is `Unsupported` just like an unreviewed epoch. Missing, corrupt, stale or mismatched evidence
   returns a typed refusal with no fallback. An in-memory database cannot own a durable filesystem
   root under this rule. A live sweep also requires daemon/GC exclusion keyed by the opened main
   database file identity across different `HOME` values or path spellings, as in
   #3069 or an equivalent fix; a socket or path-derived lock alone does not satisfy that gate.
   Binding or rebinding a populated root is a separate verified adoption action, never an automatic
   side effect of a sweep or boot. Adoption records a pending database store ID, durably writes and
   verifies the root marker under the same owner/root locks, and records binding completion only
   after they match. Recovery reuses that pending ID;
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
  copied database UUID. A completed cutover with no binding and a configured root containing no
  objects and no marker binds on a daemon boot while no binding is completed: the test observes the
  pending ID, verified marker and one completing main-database transaction. After the completing
  transaction, a sweep in either mode is admitted on that root. A pre-rule database with one live
  attachment row and an empty configured root refuses the automatic path, binds nothing, and leaves
  the attachment row and the object it references, in its original root, untouched; a mutant that removes the main live-reference check fails this arm.
  Two boots of one database through different path spellings, both with empty roots, race to bind:
  exactly one pending ID lands and the other boot refuses without a second marker; a mutant that
  removes the conditional pending-ID write fails this arm. A root with exactly one object refuses
  that automatic path, leaving the object untouched, and sweeps stay refused; a foreign
  marker likewise refuses and remains untouched, and sweeps stay refused. A mutant that removes
  the emptiness check fails the one-object control. Crashes before the first-boot marker, after that
  marker but before binding completion, and after completion are
  replayed: both sweep modes refuse before any lock or walk until the final state has matching
  durable IDs, and replay preserves the pending `store_id` without overwriting another owner's
  marker or claiming a root that gained an object. A populated-root cutover without explicit
  adoption also refuses; a pre-rule completed root with objects never silently mints or replaces
  an ID. A second daemon under a different `HOME` cannot serve the same opened main database while
  live sweeps are enabled. The same reviewed epoch with an absent or mismatched cutover `store_id`
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

## Amendment 3 (2026-09-25): rows whose record is gone are reported now, and no scan removes them until a reviewed design exists

**Status: Accepted (2026-09-26).** Refs #3178. A follow-up to Amendment 1 that addresses part of the case its item 5 leaves out of
scope: it ships a read-only report of attachment rows whose record was not found, and it states what any code that
removes such rows must first supply. It changes no text of Amendment 1 or Amendment 2. Item 1 only reads and does not
depend on Amendment 1 being accepted; the requirements in item 3 build on Amendment 1 items 6 to 8 and bind only
together with Amendment 1. This amendment does not by itself reclaim the space #3178 describes. Every code and document
reference below is to commit `18b1113a3bb760caa4fa3e29f986eae15586c9b0`.

### Why

[ADR-191](ADR-191-web-pack-ontology-and-operations.md) A1.2 makes the hard delete of a `page` or `resource` routed to
its own backend one verb invocation with two commits: the record's backend deletes the record, then the canonical main
backend deletes the attachment rows that named it (`crates/khive-pack-kg/src/handlers/update.rs:463-472`,
`crates/khive-runtime/src/pack.rs:1951-1967`). A crash or a failed write between the two commits leaves rows on the
main backend for a record that exists nowhere. Amendment 1 item 5 records why the blob sweep cannot reclaim their
blobs: its claim anti-join reads only `attachments` (`crates/khive-db/src/stores/blob.rs:2133-2141`), so a row counts as
live whether or not its record exists.

Nothing else removes such a row. The production statements that delete attachment rows name their rows by record id
(`crates/khive-db/src/stores/attachment.rs:71-95`) or by the ids of the probe rows the sweep's fence probe inserted in
the same unit (`blob.rs:1901-1915`). The second commit runs again only when a caller repeats `delete(hard=true)` with
the record's id (`update.rs:397-408`, `update.rs:431-442`). No path finds these rows without their id.

The blob sweep itself has no production caller at this commit. Every call of `transactional_orphan_sweep`, and every
call of a blob store's `orphan_sweep`, is test code: the `#[cfg(test)]` modules of `crates/khive-db/src/stores/blob.rs`
(from line 3366), `crates/khive-db/src/stores/blob_s3.rs` (from 705), `crates/khive-mcp/src/attachment_cutover.rs`
(from 422), `crates/khive-mcp/src/serve.rs` (from 4556) and `crates/khive-pack-moodboard/src/preference_handlers.rs`
(from 1666), and the test sources `crates/khive-db/tests/blob_conformance.rs` and
`crates/khive-pack-blob/src/uploads/tests.rs`. The two calls of a method named `orphan_sweep` outside test code
(`crates/khive-runtime/src/retrieval.rs:1370`, `crates/kkernel/src/vector.rs:225`) are the vector store's sweep of
[ADR-044](ADR-044-vector-store-extensions.md). So no serving process reclaims any blob today, whether or not its row has
a record, and removing an ownerless row frees its blob only once Amendment 1's sweep runs live. A report can size and
locate the leak now. It cannot shrink it.

Locating the rows is a read. Removing them is a decision about several databases that other processes may be writing
while it is made, and five situations stand in the way of any remover that runs beside those writers:

1. **A publication in flight.** Under the order Amendment 1 item 7 requires, a new durable ref is registered on the
   main backend before a record exposes it, so a routed record's row commits before the record, and until the record
   commits the row looks exactly like a leftover. A producer can stall there for any length of time, and no elapsed
   time proves that it will not commit.
2. **A re-creation under the same id.** Web ids are derived from the URL
   (`crates/khive-pack-web/src/identity.rs:70-78`), so a later fetch re-creates a deleted page under its old id. A HEAD
   fetch re-creates the entity and writes no row (`crates/khive-pack-web/src/fetch.rs:641-650`, `674-675`, `706-716`),
   so a re-creation need not change the row a remover last saw.
3. **A hard delete running beside the remover.** The retry decides absence and deletes rows in two separate steps
   (`pack.rs:1959-1966`).
4. **A change to the set of backends or to a backend file.** A backend is added, dropped from configuration while it
   still holds records, or replaced or restored. `kkernel sync` renames a rebuilt database over its target
   (`crates/khive-vcs/src/sync.rs:846-883`).
5. **A record published again without its row.** Archive import restores each entity's supplied id and properties
   through `upsert_entity` with `content_ref: None` (`crates/khive-runtime/src/portability.rs:275-300`), a statement
   that writes only the entity (`crates/khive-db/src/stores/entity.rs:731-740`). The archive carries no attachments
   (`portability.rs:24-52`), and `kkernel kg import` runs this path (`crates/kkernel/src/kg/archive.rs:121-134`). Sync
   writes records the same way (`sync.rs:976-1010`). A web page exposes its body through `properties.blob_ref`
   (`fetch.rs:578-598`, `690-705`), which Amendment 1 item 7 does not count as a root. A correctly removed row can
   therefore be followed by an import that exposes its body again with no row, and a sweep may then collect bytes a
   live record names.

At this commit nothing excludes those writers. The daemon is unique per rendezvous, not per database: the pairing check
accepts a daemon given its own socket and PID file (`crates/khive-runtime/src/daemon.rs:147-190`), and the lock its boot
takes lives under the khive home directory (`daemon.rs:192-207`, `daemon.rs:371-374`). A client that finds no daemon
socket dispatches locally unless `KHIVE_DAEMON_STRICT=1` is set, and one run with `KHIVE_NO_DAEMON` always does
(`crates/khive-mcp/src/daemon.rs:2491-2500`), and
[ADR-100](ADR-100-store-backup-replication.md) records the result: "there is no quiescent origin state to capture
against short of a real maintenance window" (`docs/adr/ADR-100-store-backup-replication.md:229-231`). The pool's
exclusive writer is a per-process mutex (`crates/khive-db/src/pool.rs:602-607`), so a second process's writer contends
only on SQLite's own lock. [ADR-150](ADR-150-single-write-owner-topology.md), which makes one process the only writer,
is proposed and not implemented (`docs/adr/ADR-150-single-write-owner-topology.md:3`). So no timing rule, recheck or
documented maintenance window makes a removal safe at this commit.

### Decision

1. **A read-only report ships now.** `kkernel blob ownerless-rows` is an admin command. It resolves the effective
   configuration as Amendment 1 item 3 describes for `kkernel blob sweep`, and lists the attachment rows of the
   canonical main backend whose record it did not find on any member of a stated roster. It never deletes and has no
   removal mode. It runs only against quiesced database members or frozen snapshot sets, not against members with live
   writers.
   - **Roster.** The main backend, every backend declared in the effective configuration's `[[backends]]` whether or
     not a selected pack is assigned to it, and every database the operator adds with `--with-db <path>` (a backend that
     once held records and is no longer configured). Members that resolve to the same file are probed once. The roster
     is fixed when the report starts. The report never uses the backends a serving process has open: that set follows
     pack selection, because pack runtimes are built only for the selected packs
     (`crates/khive-mcp/src/serve.rs:3027-3059`) and the shared resolver holds the default runtime and those runtimes
     (`crates/khive-runtime/src/kg_read.rs:14-28`, installed at `pack.rs:4822-4825`). The output header names each
     member (how it was named, its canonical path, its device and inode, its schema version) and states that the
     roster's completeness is not verified.
   - **Opening and failing closed.** Every member opens through `KhiveRuntime::new_readonly`, read-only and query-only,
     at this build's current schema (`crates/khive-runtime/src/runtime.rs:369-377`,
     `crates/khive-db/src/backend.rs:446-449`, `pool.rs:2822-2831`). Every member is a quiesced database or a frozen
     snapshot set consisting of the database file and its matching `-wal` and read-only `-shm` sidecars when WAL frames
     must be read. A writable `-shm` at open ends the report with a nonzero exit and no row list; the error names the
     member and tells the operator to close every writer or take a frozen snapshot. A nonempty `-wal` without a frozen
     read-only `-shm` also refuses, because immutable mode would omit committed WAL frames (`pool.rs:2623-2710`). A
     read-only `-shm` without its matching `-wal` refuses as an inconsistent snapshot. The pool treats an `-shm` with
     any write permission bits, including a copy left at `0644`, as writable. The `-shm` permission check is an
     open-time heuristic, not proof that no writer can appear later. A rollback-journal member has no WAL `-shm` signal;
     its normal read-only open cannot establish quiescence, so the operator must quiesce it before the report. The
     operator must keep every member quiesced or frozen until the report exits. A member that cannot be opened, is not
     at the current schema, is an in-memory database, or returns an error on any probe also stops the report with a
     nonzero exit and a message naming the member and the error. No row list is printed, because a partial list would
     show that member's records as ownerless.
   - **Walk.** Rows are read from main in pages of at most 128, ordered by `(record_uuid, role)` under SQLite's `BINARY`
     collation (the key columns declare no other, `crates/khive-db/sql/021-attachments-a-stage.sql:8-29`). Each page
     continues strictly after the last key read, `(record_uuid, role) > (?, ?)`, never by offset. Each page read and
     each member probe runs in its own read transaction, and none is held from one page to the next, as ADR-150
     component 3 asks of readers outside the owner (`docs/adr/ADR-150-single-write-owner-topology.md:140-142`).
   - **Probe.** For each page, every member is asked for the page's ids in its `entities` and `notes` tables with no
     `deleted_at` predicate and no namespace predicate, the shape of the existing reads that include tombstones
     (`crates/khive-db/src/stores/entity.rs:1111-1125`, `crates/khive-db/src/stores/note.rs:1623-1639`). A row is owned
     when any member holds its id in either table. A soft-deleted record, the tombstone of a merged entity and a record
     moved to another namespace therefore own their rows, as §6 requires. ADR-044's vector sweep treats a soft-deleted
     subject as orphaned (`docs/adr/ADR-044-vector-store-extensions.md:350-353`); a vector can be rebuilt from its
     record, and a body cannot.
   - **Output.** For each row not found on any member: `record_uuid`, `substrate`, `role`, `content_ref`, `media_type`,
     `size_bytes`, and `created_at` labelled as producer-supplied (the web pack stamps the time it writes the row,
     `fetch.rs:539`, while `create_entity_with_attachments` uses the record's creation time,
     `crates/khive-runtime/src/operations.rs:1589-1597`); the time the row was read; and for each member, `absent` with
     the time that member was probed. Counters: `members`, `scanned`, `owned`, `owned_deleted` (present only
     soft-deleted or as a tombstone), `owned_multiple` (on more than one member) and `ownerless`, with the walk's start
     and end times.

2. **The report is a best-effort interval report, and a listed row is only a candidate.** Its output says so, and says:
   - the roster is probed as of its quiesce or freeze. A record committed after that snapshot is not visible to this
     report;
   - a row present on main for the whole walk is read exactly once. A row inserted behind the cursor during the walk
     is not read, and a row changed by an upsert (`attachment.rs:44-52`) or deleted after its page was read is reported
     as it was read;
   - the counters describe observations, each made at its recorded time, not a total at one instant;
   - each member probe is its own read, so the probes for one row are not a simultaneous snapshot of several
     databases. A record moved from one member to another between their probes reads absent on both;
   - a listed row's record may exist on a probed member by the time the output appears, may be committed by a
     publication in flight, may live on a backend outside the roster, or may come back when a backend is restored from
     an older copy. The row's age decides none of these.

   The report is input to an operator's investigation. It is never an instruction to delete and never a deletion
   manifest: any future removal computes its own set under the conditions of item 3.

3. **No code removes rows by scanning for ownerless ones until a separately reviewed design supplies at least the
   following,** and none is added behind a flag in the meantime.
   - **(a) Writer exclusion per database file.** Every writer-capable open of a file holds a shared side for the life
     of the open, whether it comes from a daemon on any rendezvous, local dispatch, a migration, a maintenance command,
     the blob sweep or cutover, or a direct connection. Replacing or restoring the file, and removing rows, need the
     exclusive side, which is granted only when no other holder exists. The design also supplies: validation that the
     SQLite handles a removal uses belong to the file incarnation its lock protects, checked again after any wait; a
     replacement handoff that covers both the old and the new incarnation, including processes that hold either open
     read-only; coordination of SQLite handles with the file's WAL and SHM sidecars; hard-link aliases resolved to one
     supported filename or refused; and the supported filesystems stated, with network storage refused. A lock file
     beside the database path is not by itself such a design. A census over the population Amendment 2 item 1 defines
     finds every production open of a database for writing, and a planted open that skips the exclusion fails it.
     ADR-150's topology meets this requirement only if its single owner is enforced per database file: under ADR-150
     other processes reach the owner through the daemon socket
     (`docs/adr/ADR-150-single-write-owner-topology.md:86-95`), and two rendezvous can name one database.
   - **(b) A durable backend roster.** Each member is named by a durable database identity stored in the member, of the
     kind Amendment 1 item 8 defines for main, with its canonical path. A backend is enrolled on main's roster before it
     serves a write under that main, and the roster shrinks only through an explicit retirement action. A member that
     is gone stays on the roster, and removal refuses until it is retired. A backend with no durable identity, such as
     an in-memory one, cannot be enrolled. Backends that held records before enrollment existed are covered exactly as
     far as Amendment 1 item 7's inventory covers them.
   - **(c) No record publication without its row.** Every path that publishes or re-creates a record able to expose a
     durable body reference either registers and validates its attachment row before the record exposes the reference,
     fails before publication, or is explicitly unsupported. That explicitly includes archive import and sync
     (situation 5 above). Holding the shared side of (a) does not satisfy this: a record-only path that waits for a
     removal to end and then publishes still exposes a body with no row. The census extends Amendment 1 item 7's census
     to every record publication and re-creation path, including paths that call no attachment or blob writer, over
     the population Amendment 2 item 1 defines, and a planted record-only replay must fail it.

   That design carries its own acceptance, showing that none of the five situations above can remove a row while its
   record exists or while a pending publication could expose a body using that row; a later re-creation of the same id
   must satisfy (c). If it bounds how many rows one pass may remove, the bound refuses an oversized set before
   deleting anything. A pass interrupted after it starts deleting may have committed a prefix, and the next pass starts
   from a fresh observation, never from an earlier list.

   The report writes nothing, starts no grace period and marks no row. There is
   no pending-removal state, nothing is scheduled, and no time is promised by which a row will be removed or its blob
   reclaimed.

4. **Retrying a hard delete.** Repeating a previously authorized `delete(id=<record_uuid>, hard=true)` remains possible,
   but it is not a safe way to clear report candidates: it decides absence only over the backends the calling process
   built runtimes for (`pack.rs:1935-1948`), it hard-deletes whatever record currently holds that id, including one
   re-created after the report was read (`update.rs:369-372`, `update.rs:424-472`), and its absence check and its
   delete are two steps under no exclusion (`pack.rs:1959-1966`).

5. **Claims and time.** The report neither reads nor writes `blob_gc_claims` and does not refuse because claims exist.
   A claim abandoned by a crashed live sweep is cleared by the next admitted live sweep and never by a dry run
   (`blob.rs:2921-2928`). Deleting an attachment row is not fenced by a claim, because the claim triggers fire on an
   insert and on an update of `content_ref` only (`crates/khive-db/sql/021-attachments-b-claim-fences.sql:4-20`); a
   removal design accounts for that itself. Once a row is gone, its blob is reclaimed only by an admitted live sweep
   under Amendment 1, subject to that sweep's publish grace and cadence (Amendment 1 item 1).

6. **Surfaces and schema.** The report adds one admin command, no MCP verb and no migration, and leaves the
   `blob.sweep` response of Amendment 1 item 3 unchanged. The roster and per-member identity of item 3(b) are new
   schema, reviewed through their new terminal epoch with `REVIEWED_SCHEMA_EPOCH` updated in the same change, as
   Amendment 1 item 6 requires.

### Alternatives considered

- _Open a live database through a new read-only pool mode._ Rejected for this amendment. A live-capable open changes
  pool snapshot semantics: immutable mode can omit committed WAL frames, while ordinary read-only SQLite may create or
  mutate `-shm`. Its correctness argument belongs in a separate proposal if operators need reports against a serving
  daemon.
- _Specify the removal pass in this amendment, conditional on its prerequisites._ Rejected for now. The pass's safety
  argument rests on the file-lifecycle protocol of item 3(a) and the publication rule of item 3(c), and neither is
  designed yet. A command specified ahead of them would be approved on an argument its prerequisites might not support.
- _Remove online after two observations a grace period apart, restoring a row whose record reappears._ Rejected on three
  counterexamples. A producer stalled between its row and its record for longer than the grace loses its row, then
  commits a record whose body a later sweep collects. A record re-created between the last probe and the delete without
  writing a row, for example by a HEAD fetch, leaves the delete matching the old row, and a crash before the restore
  leaves a live record with no row. A restore decided on a positive recheck can commit after a concurrent hard delete
  finished both commits, so the record is gone and its row is back.
- _Remove after stopping the daemon, in a documented maintenance window._ Rejected: nothing enforces the window.
  Stopping the daemon does not stop writes, and commands that already ask for stopped writers do not check for them
  (`crates/kkernel/src/entity_type_backfill.rs:33`, `docs/adr/ADR-111-blob-store.md:290`).
- _Reuse an existing lock._ Rejected. The recovery lock lives under the khive home directory (`daemon.rs:192-207`), and
  a non-daemon process holds it only while it builds its server (`serve.rs:161-179`). The database GC owner is shared by
  the sweep and the V21 cutover (`blob.rs:2307-2318`) and taken by migrations (`backend.rs:462-471`). None is held for
  the life of an ordinary writer.
- _Fence each record's publication with a per-record lease on main._ Deferred, not rejected. It would let removal run
  beside writers, but every record writer on every backend must take the lease, a whole-file replacement cannot take
  one, and an abandoned lease needs its own recovery rule. It can replace item 3(a) later without changing the report.
- _Clear candidates by repeating the hard delete._ Rejected for the reasons in item 4.
- _Take a removal roster from the effective configuration, or from the backends one process has open._ The report does
  this, says so, and deletes nothing. Rejected for removal: configuration and pack selection change, and a backend
  dropped from configuration still holds records that return when it is configured again.
- _Store the owning backend on each row, or log an intent before a routed delete's first commit._ Rejected: every
  producer changes, earlier rows carry no value, and an intent log misses rows left before it ships or by other causes.
- _Leave the rows._ Rejected as an end state, because ADR-191 A1.2 records the leak as an obligation. Reporting now and
  gating removal behind named requirements neither removes a live row nor hides the leak.

### Acceptance

- **Interrupted routed delete.** In the fixture of `routed_hard_delete_interrupted_cleanup_leaves_only_orphan_attachments`
  (`crates/khive-pack-web/tests/backend_routing.rs:408-410`) with the second commit failed, the report lists exactly the
  page's rows, each `absent` on main and on the web backend, with probe times. Control: without the injected failure it
  lists nothing.
- **Deleted but present.** A soft-deleted routed page, an entity merged into another and a record moved to another
  namespace are counted as owned and are not listed. Must fail if the probe filters on `deleted_at` or on namespace.
- **Failing closed.** A configured member whose file is missing, a member at another schema version, an in-memory
  member, and a probe error injected on one member at the second page each end the report with a nonzero exit and no
  row list. Control: with the member present, the report lists the leftover row.
- **Roster from configuration, not packs.** A page on a backend declared in `[[backends]]` while the web pack is not
  selected is owned. With the declaration removed, its rows are listed and the header no longer shows the member.
  Adding the member with `--with-db` makes the page owned again.
- **Walk through the CLI.** On quiesced or frozen members, with a page boundary between two roles of one
  `record_uuid`, every row is reported once.
- **Walk function under test internals.** On a writable fixture backend, delete an already-read row between two page
  reads. The keyset walk still returns the unread row; an offset-paging control skips it. This mutation belongs in the
  walk function test, not in a successful run of the read-only CLI.
- **Read-only, no held transaction.** Every member opens read-only and query-only, and every database's contents are
  unchanged after the report. A member with a nonempty `-wal` beside a frozen read-only `-shm` reports a row committed
  only in the WAL. Control: a test-only immutable open of the same database and WAL without the `-shm` misses that row,
  which is why the pool refuses that open for the report.
- **In flight.** With a row committed on main and its record held uncommitted by a live writer that keeps main's `-shm`
  writable, the command exits nonzero with no row list. Its error names main and directs the operator to close every
  writer or take a frozen snapshot. Control: after the writer aborts and closes, checkpoint the same database and
  provide frozen sidecars; the report lists the row as a candidate.
- **No removal.** The command accepts no argument that deletes, and a log of every statement it issues on every
  database shows reads only.

### Consequences

- Until a design satisfying item 3 ships, these rows and their blobs stay. The cost is space, not data. This amendment
  does not reclaim the space #3178 describes, and #3178 stays open.
- Removing rows frees blobs only when Amendment 1's sweep runs live; at this commit no serving process calls it.
- The report reads one page of main and probes each member once per page. Its output carries its duration.
- Operators quiesce every member or supply frozen copies before running the report. A serving writer's database is not
  an accepted input.
- The exclusion of item 3(a) is shared infrastructure. Amendment 1 item 7 also requires a writer barrier for its
  backfill, and this exclusion is one way to provide it; that choice belongs to Amendment 1.

### Out of scope

- The second commit of a routed hard delete and its retry are unchanged.
- A record re-created under a leftover row's id without writing a row of its own (a HEAD fetch) owns that row. The
  report answers only whether a record with that id exists.
- `blob_pack_owners` rows of Amendment 1 item 7 whose source record is gone follow their pack's release rule.
- Rows of soft-deleted records keep their blobs until the record is hard-deleted or restored, as §6 intends.
- The removal command and its acceptance, retiring a roster member, removing rows while writers run, and restoring one
  backend independently of main.

## Amendment 4 (2026-09-28): reviewed core-schema epoch for blob GC

**Status: Accepted (2026-09-28).** This amendment supplies the core migration review required by Amendment 1 item 6.
It supersedes that item's historical `REVIEWED_SCHEMA_EPOCH = 41` value for this reviewed snapshot;
it does not remove the ownership, store-binding, or pack-schema review obligations in items 6–8.

The reviewed core migration chain is V22–V42 at snapshot
`e6b34cb5e255d2e6572d20a979f45746484356c2`. The terminal version is V42 because the
final entry of `MIGRATIONS` is version 42 (`crates/khive-db/src/migrations.rs:444-448`).
“Tables touched” includes schema and index changes and data rewrites; views and virtual tables
are named explicitly. A read-only backing table is identified as such. “None” means the
migration neither adds a durable `BlobStore` producer nor changes the shape or authority of a
blob reference.

| Migration                                         | Tables touched                                                                                                                                  | Blob-liveness effect | Source                                                                                                                                                                                                                                                                                                                |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- | -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| V22 `notes_unread_probe_recipient`                | `notes` index                                                                                                                                   | None                 | `crates/khive-db/sql/022-notes-unread-probe-recipient.sql:7-15`                                                                                                                                                                                                                                                       |
| V23 `fts_record_kind`                             | Rebuilds `fts_entities`, `fts_notes`; reads `entities`, `notes`                                                                                 | None                 | `crates/khive-db/sql/023-fts-record-kind.sql:6-88`                                                                                                                                                                                                                                                                    |
| V24 `fts_rowid_map`                               | `fts_entities`, `fts_notes`; their rowid maps and state tables; reads `entities`, `notes`                                                       | None                 | `crates/khive-db/sql/024-fts-rowid-map.sql:41-134`                                                                                                                                                                                                                                                                    |
| V25 `notes_unread_probe_recipient_direction`      | `notes` index                                                                                                                                   | None                 | `crates/khive-db/sql/025-notes-unread-probe-recipient-direction.sql:24-33`                                                                                                                                                                                                                                            |
| V26 `knowledge_fts_repair`                        | `fts_knowledge`, `fts_sections`, `knowledge_atoms_fts_content` view; reads `knowledge_atoms`, `knowledge_sections`                              | None                 | `crates/khive-db/sql/026-knowledge-fts-repair.sql:14-63`; `crates/khive-db/sql/schema.sql:197-205`                                                                                                                                                                                                                    |
| V27 `notes_hot_property_indexes`                  | `notes` indexes                                                                                                                                 | None                 | `crates/khive-db/sql/027-notes-hot-property-indexes.sql:45-55`                                                                                                                                                                                                                                                        |
| V28 `notes_key`                                   | `notes` column and index                                                                                                                        | None                 | `crates/khive-db/sql/028-notes-key.sql:2-6`                                                                                                                                                                                                                                                                           |
| V29 `note_streams`                                | `note_streams` table; `notes` delete/update guards                                                                                              | None                 | `crates/khive-db/sql/029-note-streams.sql:2-52`                                                                                                                                                                                                                                                                       |
| V30 `tool_source_mounts`                          | `tool_source_mounts` table                                                                                                                      | None                 | `crates/khive-db/sql/030-tool-source-mounts.sql:1-5`                                                                                                                                                                                                                                                                  |
| V31 `note_versions`                               | `notes` column and update trigger                                                                                                               | None                 | `crates/khive-db/sql/031-note-versions.sql:1-8`                                                                                                                                                                                                                                                                       |
| V32 `knowledge_count_indexes`                     | `events`, `knowledge_atoms` indexes                                                                                                             | None                 | `crates/khive-db/sql/032-knowledge-count-indexes.sql:3-8`                                                                                                                                                                                                                                                             |
| V33 `notes_message_recipient_direction`           | `notes` indexes                                                                                                                                 | None                 | `crates/khive-db/sql/033-notes-message-recipient-direction.sql:4-22`                                                                                                                                                                                                                                                  |
| V34 `notes_namespace_created`                     | `notes` index                                                                                                                                   | None                 | `crates/khive-db/sql/034-notes-namespace-created.sql:2-4`                                                                                                                                                                                                                                                             |
| V35 `notes_unread_probe_recipient_type_direction` | `notes` index                                                                                                                                   | None                 | `crates/khive-db/sql/035-notes-unread-probe-recipient-type-direction.sql:8-16`                                                                                                                                                                                                                                        |
| V36 `events_operation_attribution`                | `events` columns                                                                                                                                | None                 | `crates/khive-db/sql/036-events-operation-attribution.sql:2-7`                                                                                                                                                                                                                                                        |
| V37 `entity_versions`                             | `entities` column and triggers                                                                                                                  | None                 | `crates/khive-db/sql/037-entity-versions.sql:1-18`                                                                                                                                                                                                                                                                    |
| V38 `entities_legacy_type_index`                  | `entities` index                                                                                                                                | None                 | `crates/khive-db/sql/038-entities-legacy-type-index.sql:1-3`                                                                                                                                                                                                                                                          |
| V39 `knowledge_cursor_indexes`                    | `knowledge_atoms`, `knowledge_domains` indexes                                                                                                  | None                 | `crates/khive-db/sql/039-knowledge-cursor-indexes.sql:1-6`                                                                                                                                                                                                                                                            |
| V40 `session_source_scoped_identity`              | `session_mirror_migration_audit`, `sessions`, `session_messages`, `session_messages_fts`, staging tables; conditionally `session_mirror_cursor` | None                 | `crates/khive-db/sql/040-session-source-scope.sql:4-11`; `crates/khive-db/sql/040a-session-source-scope-stage.sql:4-34`; `crates/khive-db/sql/040b-session-source-scope-swap.sql:3-6`; `crates/khive-db/sql/040c-session-source-scope-finalize.sql:3-59`; `crates/khive-db/src/session_identity_migration.rs:231-232` |
| V41 `sender_transport`                            | `comm_sender_transport` table and index                                                                                                         | None                 | `crates/khive-db/sql/041-sender-transport.sql:1-45`                                                                                                                                                                                                                                                                   |
| V42 `comm_external_id_channel_scope`              | `notes` index                                                                                                                                   | None                 | `crates/khive-db/sql/042-comm-external-id-channel-scope.sql:5-16`                                                                                                                                                                                                                                                     |

V29 can prevent deletion of a stream member note, but it does not introduce a new blob owner
or alter the `attachments` reference. V40 keeps session mirror content inline; V41 keeps its
encrypted envelope inline in `comm_sender_transport`, and `credential_ref` names an external
credential, not a `ContentRef`. No migration in the table adds a producer or changes the blob
reference shape. The separate pack-owned schema and blob-writer census required by Amendment 1
remain independent proof obligations; the core migration ledger cannot prove them.

For this reviewed snapshot, `REVIEWED_SCHEMA_EPOCH = 42`. The implementation of
`blob_gc_fencing_complete` replaces its historical exact-V21 admission with **exactly** that
named epoch, never `>=` that value. The compiled `latest_schema_version()` must equal the same
named epoch before either sweep mode can run. The persisted ledger must be contiguous and
canonically named through that exact terminal version, and retain the completed V21 cutover
marker and functional attachment fences. Unknown and newer versions refuse before root locking,
walking, or claim cleanup. The ownership and store-binding checks required by Amendment 1
items 7 and 8 remain mandatory in addition to this schema predicate.

Before the collector is admitted on a database past V21, pre-existing slugless channel
quarantine notes must be repaired as #3497 requires. Each still-published original referenced
only by `quarantine_content_ref` must gain a `quarantine-original` attachment row, or its
note must expire under the normal quarantine rule. The repair must report counts and be
idempotent. The schema epoch alone does not establish that those originals have a live owner.

A migration added after this table was reviewed invalidates the review, even if it appears
unrelated to blob liveness. Re-read the complete chain from V22 through the new `MIGRATIONS` tip,
add a row for every new version, advance `REVIEWED_SCHEMA_EPOCH` to that tip, and record the new
snapshot SHA before the collector accepts that epoch. Changes to pack-owned liveness schema,
blob producers, or manifests independently repeat the review and gate updates required by
Amendment 1 item 6.

## Amendment 5 (2026-09-28): reviewed core-schema epoch 43 for blob GC

**Status: Accepted (2026-09-28).** Amendment 4 requires this review whenever a migration lands after its table
("a migration added after this table was reviewed invalidates the review"). The review covers V43.

The reviewed core migration chain is V22–V43 at snapshot `e6f90dd41fc526c0ae78ea841b6fdbe4df48111e`. The
terminal version is V43 because the final entry of `MIGRATIONS` is version 43
(`crates/khive-db/src/migrations.rs:454-458`).

The chain was re-read from V22. Between Amendment 4's snapshot `e6b34cb5e255d2e6572d20a979f45746484356c2` and
this snapshot, the only changes under `crates/khive-db/sql/`, `crates/khive-db/src/migrations.rs` and
`crates/khive-db/src/session_identity_migration.rs` are:

- the added `crates/khive-db/sql/043-vector-provenance.sql`;
- the `migrations.rs` lines that register it.

Amendment 4's rows for V22–V42 therefore stand unchanged. One row is added:

| Migration               | Tables touched                  | Blob-liveness effect | Source                                               |
| ----------------------- | ------------------------------- | -------------------- | ---------------------------------------------------- |
| V43 `vector_provenance` | `vector_provenance` table (new) | None                 | `crates/khive-db/sql/043-vector-provenance.sql:3-21` |

V43 adds a sidecar that records, for each stored embedding:

- the model key, subject, namespace and write time;
- `embedding_digest`, a BLAKE3 hex digest of the stored embedding bytes (`crates/khive-db/src/stores/vectors.rs:622`);
- optionally `text_fingerprint`, a BLAKE3 digest of the exact prepared input text.

`text_fingerprint` is carried as a `ContentRef` value, but it names no stored object. Nothing is written to a
`BlobStore` under it, and nothing dereferences it through one. The files that write or clear the sidecar make no
`BlobStore` call:

- `crates/khive-db/src/stores/vectors.rs`
- `crates/khive-db/src/namespace_move.rs`
- `crates/khive-runtime/src/note_write.rs`
- `crates/khive-runtime/src/atomic_message.rs`
- `crates/khive-runtime/src/atomic_prepare.rs`
- `crates/khive-runtime/src/curation.rs`
- `crates/kkernel/src/reindex.rs`

The same search finds `BlobStore` calls in `crates/khive-pack-blob/src/uploads/tests.rs`, which serves as its
positive control. V43 thus adds no producer and changes no blob reference. The collector does not read
`vector_provenance`: `crates/khive-db/src/stores/blob.rs`, which holds `blob_gc_fencing_complete`, names it
nowhere, while it names `attachments` throughout. A matching digest there keeps no blob alive.

For this reviewed snapshot, `REVIEWED_SCHEMA_EPOCH = 43`. This value supersedes Amendment 4's
`REVIEWED_SCHEMA_EPOCH = 42`. Every other sentence of Amendment 4 stands:

- admission at exactly the named epoch;
- `latest_schema_version()` equal to it;
- a contiguous, canonically named ledger;
- refusal of unknown and newer versions before root locking;
- the ownership and store-binding checks of Amendment 1 items 7 and 8;
- the #3497 quarantine-original repair.

At this snapshot the named epoch is not yet in code. `blob_gc_fencing_complete` still admits exactly the
completed V21 epoch (`crates/khive-db/src/stores/blob.rs:1576-1627`), and `REVIEWED_SCHEMA_EPOCH` appears only in ADR
text (this ADR and ADR-160), not in code. A collector naming 42 would refuse every store migrated to V43, so it must not ship at 42. The change
that implements the named-epoch admission names the migration tip at its own landing.

A later migration invalidates this review, as Amendment 4 already requires. In particular, the blob pack-owner
and blob root-binding migrations for Amendment 1 items 7 and 8 add blob owners and a store binding. They take
the next free versions when they land, and they need their own reviewed rows, with their liveness effect
stated, in the change that adds them. This row set does not cover them.

## Amendment 6 (2026-09-29): staged uploads count as root content

**Status: Proposed (2026-09-29).** This amendment changes two places in Amendment 1 item 8 and nothing else
in Amendment 1: the sentence that defines an empty root, "no blob objects and no root ownership marker of
any owner", and the acceptance entry for a root with exactly one object. It concerns the filesystem blob
root. It binds the implementing change, because the fresh-root bind and item 7's owner validation are not
in code yet. Line references are at commit `2d30ab6d249a6db524c22d578e9df2af35f17f3b`, where the migration
chain ends at V43 `vector_provenance` (`crates/khive-db/src/migrations.rs:454-458`) and `blob_pack_owners`,
`store_binding` and `REVIEWED_SCHEMA_EPOCH` appear in no crate.

### Why

Item 8 lets a daemon boot bind a configured root to its main database when it proves the root empty. A
staged multipart upload is neither a blob object nor a root marker. `blob.begin` creates one regular file at
`<root>/.uploads/<id>`, and each `blob.put_part` appends to that file and syncs it
(`crates/khive-db/src/stores/blob_uploads.rs:27`, `157-192`, `236-240`). The object walk skips every
dot-leading name (`crates/khive-db/src/stores/blob.rs:1385-1392`, walk at `1433`), so an emptiness proof
built on that walk reads a root whose only content is one database's open upload as empty. A second database
could then bind that root. Item 7 refuses the first database's next part write, and a refused append aborts
the upload and removes its staged file (`crates/khive-pack-blob/src/uploads.rs:345-357`, `194-203`). An
upload that was progressing normally is lost.

### Decision

1. **Open uploads are root content.** The empty-root predicate in item 8 becomes: no blob objects, no entry
   in `<root>/.uploads`, and no root ownership marker of any owner. A root whose only content is staged
   uploads is not empty. The automatic fresh-root bind refuses it, binds nothing, and does not open, modify,
   move or remove a staged file. An absent or empty `.uploads` directory is not content. A `.uploads` that
   cannot be listed as a real directory is content, so the bind refuses as item 8 already does for missing
   or corrupt evidence. An entry counts whatever its name. The expiry path in clause 3 removes only entries
   named as upload ids (`blob_uploads.rs:406-408`), so a stray entry keeps the root out of the fresh path
   until an operator removes it. Every use of "empty", "populated" and "no blob objects" in item 8 reads
   with this predicate, including the recovery recheck and the rule for a database cut over before item 8.
2. **The orphan collector never removes staged uploads.** The collector does not enter `.uploads` today. It
   lists the root through `read_dir_names_no_follow`, which drops every dot-leading name
   (`blob.rs:1385-1392`), and `.uploads` is one. [ADR-173](ADR-173-blob-chunked-upload.md) §3 and §4 already
   record this. This clause pins the existing behavior: a transactional sweep in either mode reads and
   deletes nothing under `.uploads`, and a change that makes the walk enter dot-leading directories must
   exclude `.uploads` explicitly.
3. **Abandoned uploads leave through the upload expiry path.** Upload ids do not survive a process restart
   (`uploads.rs:142`), so a staged file left by an earlier daemon process has no live state. The daemon's
   upload sweeper removes it. `UploadManager::sweep` discards records idle past the bound and calls
   `sweep_uploads`, which unlinks every id-named file under `.uploads` whose modification time is at least
   the bound old (`uploads.rs:437-476`, `blob_uploads.rs:385-447`). The bound is
   `KHIVE_BLOB_UPLOAD_IDLE_SECS`, default 3600 seconds (`uploads.rs:56`). The sweeper ticks every
   `KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS`, default 600 seconds, and its first tick comes one interval after
   daemon start (`uploads.rs:57`, `crates/khive-mcp/src/components.rs:172-192`). The bind and the orphan
   collector never remove a staged file. A root holding an abandoned upload younger than the bound refuses
   the bind on that boot, and the first boot after the sweeper has removed the file can bind it. The
   expiry pass itself is not yet limited to abandoned uploads: `sweep_uploads` removes any id-named file
   older than the sweeping process's own bound with no owner check (`blob_uploads.rs:385-447`), so on a
   root shared by two daemons with different bounds it can remove the other daemon's open upload, a gap
   this amendment does not close and #3643 tracks.
4. **An upload begin cannot land inside the bind.** Item 8 says the emptiness check runs "under database GC
   ownership and the root write lock". That sentence keeps the check itself apart from any upload begin,
   because `begin_upload` takes the root write lock: the store's per-root mutex and the
   `.khive-blob-write.lock` file lock (`blob_uploads.rs:42`, `67-68`; `blob.rs:40`, `745-783`), the same pair
   the orphan sweep takes (`blob.rs:2976`, `2989`). It does not say the bind keeps those locks through the
   pending-ID write, the marker write and the completing transaction. Item 8 says that only of adoption
   ("holds them through the durable update and verification"). `begin_upload` also takes no database handle
   and validates no binding (`blob_uploads.rs:157`, `uploads.rs:255`). Two rules close the window.
   - The fresh bind holds the database GC owner and the root write lock from the emptiness check until the
     completing transaction commits or the attempt ends. The recovery recheck takes the same locks.
   - Under that root write lock, and before it creates a staging file, `begin_upload` reads the binding
     state and takes one of three outcomes. With no binding and none in progress it proceeds, and its entry
     then counts as content for any later fresh bind under clause 1. With a completed binding it validates
     against that binding as item 7 requires. With a pending binding it refuses with a retryable error and
     stages nothing.

   A begin therefore either runs before the check, where the check sees its staged file and refuses, or runs
   after the bind has ended, where it finds the completed binding or none.

### Acceptance

These entries join the item 8 entries in Amendment 1's list. Its sentence "A mutant that removes the
emptiness check fails the one-object control" stands, and entry (a) is its counterpart for `.uploads`.

- **(a) Uploads-only root.** A database cut over with no live references, and a configured root whose only
  content is one open upload's staged file (`blob.begin`, then one `blob.put_part`), refuses the automatic
  fresh bind on daemon boot. It records no pending ID and no marker, and the main database is unchanged. The
  staged file keeps its bytes and modification time, and the upload accepts its next part and commits, with
  `blob.get` returning the whole object. A mutant whose emptiness check skips `.uploads` binds the root and
  fails this entry.
- **(b) Abandoned upload control.** The same root, with the staged file aged past
  `KHIVE_BLOB_UPLOAD_IDLE_SECS` and no live upload record, refuses the bind on one boot and leaves the file
  in place. One expiry pass removes the file. The next boot binds the root with the pending ID, verified
  marker and single completing transaction that Amendment 1's empty-root entry requires.
- **(c) Sweeps leave open uploads alone.** On a bound root with one open upload, a transactional sweep in
  dry-run mode and another in live mode each leave the staged file with its bytes and modification time,
  count it in no counter, and the upload then commits. The test
  `transactional_gc_preserves_committed_and_staging_then_upload_sweep_only_reaps_staging`
  (`crates/khive-pack-blob/src/uploads/tests.rs:1426-1515`) covers the live-mode sweep before any binding
  exists. This entry adds the dry-run sweep and runs both on a bound root.
- **(d) A begin cannot land inside the bind.** With a test hook holding the bind between its emptiness check
  and its completing transaction, a `blob.begin` on the same root creates no staging file until the bind
  ends, and then follows the outcome for the binding state it finds. A begin that completed before the check
  is seen by the check, which refuses. A begin against a root whose binding is pending refuses and creates
  no file. A mutant that releases the root write lock after the check lets the begin stage a file inside
  the window and fails this entry.
