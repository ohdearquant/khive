# ADR-087: Workspace Mirror — Folding `.khive/` Into the Graph Substrate

**Status**: Accepted\
**Date**: 2026-07-03\
**Authors**: khive maintainers
**Depends on**: ADR-086 (Document/File Modeling — the `document`-entity shape this mirror
populates), ADR-080 (Session Pack — OSS Storage Mechanism, §6 session mirror — the
operational pattern this ADR reuses), ADR-002 (Edge Ontology — `supersedes`), ADR-017
(Pack Standard)\
**Related**: ADR-010 (KG Versioning — NDJSON snapshot scope; explains why `.khive/kg/*`
is excluded from this mirror's scope), ADR-021 (Memory Pack)

## Context

The design request: fold the `.khive/` filesystem convention (workspaces,
notes, summaries, handoffs, reports, review artifacts) into khive itself, "on record and
kept," explicitly drawing the analogy to `khive-pack-session`'s session mirror
architecture. `.khive/` today is the root CLAUDE.md's documented Workspace Convention — a
directory tree of markdown artifacts that exist only as files, invisible to
`search`/`neighbors`/`traverse`.

ADR-080 §6 already ships exactly this shape of thing once: a background poller that mirrors
external content (ChatGPT export files, session JSONL transcripts) into khive on a
warm()-spawned loop, with cursor-based idempotent progression and unconditional secret
masking via `crates/khive-runtime/src/secret_gate.rs`'s `mask_secrets(text: &str) ->
Cow<'_, str>` (the redact-in-place function, distinct from the hard-blocking `check(content:
&str) -> RuntimeResult<()>` that verb-driven writes use — passive ingestion of pre-existing
external content cannot reject a whole file over one matched line, so it must mask and
continue, exactly as the session mirror already does).

**The critical divergence this ADR must state explicitly.** The session mirror's actual
shipped implementation writes into pack-`khive-pack-session`-private auxiliary tables,
entirely outside the graph substrate (entities/notes/edges) — appropriate for session
transcripts, which are recall-only raw material, not meant to be graph nodes. The requirement
for `.khive/` is different in kind: the content must be "on record" in the sense of
graph-queryable (`search`, `neighbors`, `traverse`), linkable, and versioned via
`supersedes` — not merely recallable. Copying the session mirror's storage target verbatim
would satisfy the operational-pattern analogy but miss the actual requirement.
This ADR reuses the OPERATIONAL PATTERN and deliberately changes the STORAGE TARGET.

## Decision

A background mirror service — architecturally identical to ADR-080 §6's session mirror —
walks configured `.khive/` subpaths and, for each file, creates or updates a `document`
entity per ADR-086's shape (`description`=file content, `properties`={`source_uri`,
`source_type`, `checksum`}, `entity_type` set from the governed vocabulary where the path
maps cleanly, e.g. `notes/handoffs/*` → `entity_type="handoff"`).

1. **Reused operational pattern (from ADR-080 §6, unchanged):**
   - `warm()`-spawned poller, config-driven interval (`KHIVE_MIRROR_WORKSPACE_POLL_SECS`,
     validated nonzero per the PACKSESSION-AUD-002 fix precedent — reject or clamp zero,
     do not repeat the hot-loop defect).
   - Cursor-based idempotent progression via a small pack-owned tracking table (outside
     the graph substrate — this is the one piece that stays auxiliary, since a cursor is
     bookkeeping, not content): `workspace_mirror_cursor(path TEXT PRIMARY KEY, last_mtime
     INTEGER, last_hash TEXT, last_synced_at INTEGER)`.
   - Never advance the cursor on error — a failed mask/write/parse leaves the file to be
     retried on the next pass, matching the session mirror's failure posture.
   - One transaction per file pass (bounded work per commit, matching the PACKSESSION-
     AUD-003 fix precedent for bounded resource use — this mirror processes whole files,
     which are markdown/text and small by construction, so the unbounded-read concern
     that applied to JSONL transcript deltas does not recur here, but the one-txn-per-item
     discipline is kept regardless).
   - Unconditional `secret_gate::mask_secrets` on every file's content before it is written
     to `description` — never `check()`, since this is passive ingestion, not a rejectable
     agent-authored write.

2. **Diverged storage target (the actual change from ADR-080 §6):** content lands in the
   PRIMARY graph substrate — real `document` entities, created/updated through the same
   internal path an agent's `create`/`update` call would use — not a pack-private
   auxiliary table. This is what makes the mirrored content genuinely queryable.

3. **Retention follows ADR-086 exactly.** A file's content changing between polls produces
   a NEW `document` entity version + a `supersedes` edge to the prior version (matched by
   `properties.source_uri`, the stable identity key across versions) — never an
   in-place content overwrite. This is the confirmed resolution: kept means
   version-history-via-supersedes, not a single mutable row.

4. **Scope: explicit include/exclude, not "everything under `.khive/`."** Config-driven
   glob lists (`KHIVE_MIRROR_WORKSPACE_INCLUDE` / `_EXCLUDE`, matching the session mirror's
   own env-var configuration convention), with a recommended default:
   - **Include**: `.khive/notes/**`, `.khive/reports/**`, `.khive/codex_reviews/**` (local,
     gitignored, but valuable review history worth having "on record" in the local graph —
     mirroring is orthogonal to what gets committed to the public repo), workspace
     `artifacts/`/completion-report markdown under `.khive/workspaces/*/`.
   - **Exclude**: `.khive/kg/*.ndjson` and `schema.yaml` (already graph-versioned via
     ADR-010's git-native snapshot mechanism — mirroring the graph's own export back into
     itself would be circular), `.khive/scripts/` (executable code, not document content —
     belongs in the `project`/code-pack world if modeled at all), any build-cache or
     binary paths.

5. **Explicit non-goals.**
   - **Not a live sync.** Poll-based, bounded staleness is acceptable — matching the
     session mirror's own tolerance.
   - **Not write-through.** khive never writes back to `.khive/` files. One-directional:
     disk → graph, always.
   - **Not a git-history importer.** This mirror captures file content as it currently
     exists on disk at poll time, not commit-by-commit file history — that concern, for
     actual git commits, is ADR-088's job.

## Rationale

### Why reuse the session mirror's pattern instead of designing a new one

The operational hard parts of any filesystem-to-graph mirror are the same regardless of
target: safe polling intervals, idempotent resumption after a crash, and secret handling
on untrusted pre-existing content. ADR-080 §6 already solved these, including two
production-audit-confirmed defect classes (hot-loop on a misconfigured poll interval;
unbounded in-memory reads on large deltas) that a from-scratch design would be at real risk
of reintroducing. Reusing the pattern is a direct `PI_AEP` Modify-over-Create call:
the poller shape, cursor discipline, and secret-masking call are copied; only the
write target changes.

### Why the write target must not also be copied

Session transcripts are recall-only by design (ADR-080's own scope statement: the
mirror stores raw material for `session.recall`, not curated graph content). `.khive/`
notes, handoffs, and reports are exactly the kind of content the design request says
should be linkable and traversable — decisions annotate documents, documents get
superseded, agents `neighbors()` out from a report to the project it concerns. None of
that is possible if the content sits in a pack-private table the graph substrate doesn't
see. The requirement, not just the analogy, decides the storage target.

## Alternatives Considered

**A1: Copy the session mirror's auxiliary-table storage verbatim ("similar architecture"
taken literally).** Rejected. Satisfies the literal analogy but not the
"on record, kept, queryable" requirement — the whole point of folding `.khive/` in.

**A2: A new dedicated pack crate for the mirror.** Rejected. The mirror is a service that
calls ADR-086's existing `document`-entity write path; it needs no verbs, no new entity or
note kinds, and no edge rules of its own. A thin mirror module (mirroring
`khive-pack-session/src/mirror/`'s module shape) inside `khive-mcp`'s daemon warm() path,
or a small submodule of whichever crate owns the `document` pattern, is sufficient —
consistent with ADR-086 itself introducing no new pack.

**A3: Mirror everything under `.khive/` unconditionally, no include/exclude config.**
Rejected. `.khive/kg/*.ndjson` mirrored back into the graph it was exported from is
circular and wasteful; build/scratch paths add noise with no query value. Explicit scope
config, defaulting to the high-value subpaths, avoids both.

## Consequences

- `.khive/notes/`, `.khive/reports/`, and `.khive/codex_reviews/` content becomes
  queryable, linkable, and versioned the same way any agent-authored document would be.
- The mirror inherits ADR-080 §6's operational risk profile (a misconfigured poll interval
  or an unbounded read) but also inherits its already-fixed defenses — no new defect class
  is introduced by construction.
- A future consumer wanting to browse "every handoff note for project X" gets it via
  ordinary `search(kind="document", query=...)` / `traverse` — no new query mechanism.

## Open Questions

1. Exact default include/exclude glob list — proposed above as a starting point, but
   should be validated against real `.khive/` directory contents before the mirror ships,
   not fixed permanently in this ADR text.
2. Should review-artifact mirroring be gated behind a separate opt-in flag, given it is
   explicitly local-only/gitignored content, distinct in sensitivity from notes/reports?
   Recommend: mirror it (it's local-graph-only too, not re-exported anywhere), but keep it
   toggleable via the same include/exclude config.

## Implementation

- New `mirror` submodule (module shape mirrors `khive-pack-session/src/mirror/`), wired
  into the same `warm()` daemon startup path as the session mirror.
- New pack-owned `workspace_mirror_cursor` table (migration under
  `crates/khive-db/sql/`), outside the entity/note/edge substrate.
- No new pack crate; no new verbs.

## References

- ADR-080 §6 — session mirror operational pattern (poller, cursor, secret masking)
- `crates/khive-runtime/src/secret_gate.rs` — `mask_secrets` vs `check`/`check_json`
- ADR-086 — `document`-entity shape this mirror populates
- ADR-010 — NDJSON snapshot scope (why `.khive/kg/*` is excluded)
- `docs/adr/feedback-data-vs-view-not-mutation` principle (khive `docs/adr/README.md`
  "Data vs view" cross-cutting principle) — governs the supersedes-not-overwrite behavior

## Amendment 1 (2026-07-15): self-standing content convention, blob-backed binaries, durability separation

**Status**: Proposed (amendment). The base decision (Accepted 2026-07-03) is unchanged in
shape; this amendment unblocks implementation, which never started because the base text
depended on ADR-086 for the `document`-entity content shape and ADR-086 remains Proposed.
Priority context: workspace durability is the operator's most urgent standing concern —
`.khive/` trees across the fleet exist only as gitignored local files with no history of
ever being backed up, and this mirror is the mechanism that folds them into the substrate.

### A1: Inline content convention (decoupled from ADR-086's lifecycle)

For the mirror's writes, the minimal `document`-entity convention is normative HERE,
whatever becomes of ADR-086 as a whole: textual file content goes in `description`
verbatim (post-masking); `properties` carries `source_uri` (absolute path at mirror
time), `source_type` (MIME-ish string), `checksum` (BLAKE3-256 lowercase hex of the
content bytes), and `size_bytes`. If ADR-086 is later accepted with a richer shape, the
mirror migrates to it; if it is rejected, this subsection stands alone. This removes the
dependency deadlock without deciding ADR-086's fate.

### A2: Binary and oversized files go to the BlobStore (ADR-111, shipped)

The base text's scope was markdown/text only. Real `.khive/` trees also hold binary and
oversized artifacts (PDFs and rendered reports, images, archived exports). For any file
that is binary (content fails UTF-8 validation or matches a configured binary-extension
list) or whose size exceeds `KHIVE_MIRROR_INLINE_MAX_BYTES` (default 256 KiB):

- the mirror stores the bytes through the shipped `BlobStore` capability
  (`khive-storage::blob`, `FsBlobStore` implementation) and records the returned
  BLAKE3-256 hash;
- the `document` entity is still created, with `description` holding nothing but a
  one-line summary (filename + size + type), and `properties.blob_hash` carrying the
  content address; `checksum` equals `blob_hash` in this case;
- content addressing makes re-mirroring identical bytes free (same hash, no new blob),
  and version history stays `supersedes`-based at the entity layer exactly as in the
  base text — blobs themselves are immutable and never superseded, only referenced.

Secret masking applies to inline text content only; blob-routed binaries are stored
as-is (masking inside arbitrary binary formats is not meaningful) but their entities
carry `properties.masked=false` so a future export policy can treat them conservatively.

### A3: Durability separation (operator constraint, binding)

Substrate durability is the FEATURE this mirror delivers, but it must not become the only
safety net while the substrate's own backup lane (ADR-100) is still being implemented and
proven. The independent, dumb, file-level snapshot of `.khive/` trees and the home
databases (operator-managed, outside khive) REMAINS in force until substrate backup and
restore have been exercised end-to-end. This ADR explicitly must not be cited to retire
that snapshot; retiring it is a separate operator decision gated on a demonstrated
substrate restore. The system under development is never its own sole backup.

### A4: Workspace-entity anchoring

Since the base text was accepted, the `workspace` pack (zero verbs) added a `workspace`
entity kind with `contains` endpoint rules. The mirror creates or reuses one `workspace`
entity per mirrored `.khive/` root (identity: uuid5 over the canonicalized root path,
fixed namespace seed) and links `workspace contains document` for every mirrored entity
under that root. This gives every mirrored file a graph anchor — "everything in seat X's
workspace" is one `neighbors()` call — and gives the previously vocabulary-only workspace
pack its first real population. Verb-shaped intake (an explicit `workspace.ingest(path)`
for one-shot ingestion of a tree, complementing the background poller) is deferred to a
follow-up amendment once the mirror itself has usage evidence; the poller is the primary
mechanism because durability must not depend on anyone remembering to call a verb.

### A5: Default include set (supersedes base text item 4's recommendation)

Validated against real `.khive/` contents across the fleet's seats (32 trees surveyed
2026-07-15): include `notes/**`, `reports/**`, `codex_reviews/**`, `workspaces/**`
(markdown and text artifacts, plus blob-routed binaries per A2), `audits/**`, and
`loop/**` (loop cursors are exactly the operational history the operator fears losing).
Exclusions unchanged from the base text (`kg/*`, `scripts/`, caches), plus `*.db*`
(databases have their own backup lane and must never be double-ingested as blobs).

### A6: BlobStore backend contract — S3 compatibility is a requirement

The blob capability this mirror consumes MUST remain backend-portable across two named
targets:

1. **Filesystem CAS** (shipped: `FsBlobStore`) — the local default; ships in the first
   implementation slice.
2. **S3-compatible object store** (S3 / R2 / MinIO semantics) — the off-machine
   durability path. Does not ship in the first slice, but the interface contract is
   fixed NOW so the second backend never requires a trait change: callers hold only the
   opaque content address (BLAKE3-256 hex digest) plus size; how a backend maps that
   digest to physical storage (sharded directories, object keys, bucket layout) is
   backend-internal and never appears in entity properties, verb results, or exported
   metadata. Nothing in this ADR — including A2's `blob_hash` property convention — may
   assume filesystem paths.

### A7: Single-file export — `workspace.snapshot`

Blobs live in a content-addressed store beside the database, not inside it — correct
for size, dedup, and backend portability, but it means the database file alone is not
a complete copy of a workspace. Portability is therefore a named verb behavior, not an
implicit property: `workspace.snapshot(out_path, workspace_id?)` emits ONE bundle file
containing a consistent database backup plus every blob referenced by the included
entities (the git model: loose objects live, a bundle when you want one file). A
matching restore path ingests the bundle into a fresh or existing store, dedup-safe by
content address. The bundle format carries a version discriminator and per-blob digests
so a partial or corrupted bundle fails loudly at restore, never silently. This is the
answer to "one file contains everything" — the export provides the single-file
property; the live layout stays CAS-outside-db.

### A8: Note-first routing for prose artifacts — blobs are for true binaries only

The most common information loss in practice is not arbitrary files: it is prose
verdicts (review conclusions, gate decisions, issue analyses) written as loose files in
a working tree and destroyed with it. The routing rule:

- **Text verdicts and decisions are notes, born in the substrate directly** —
  `kind=decision`, tagged with the repository and the pull-request or issue number, and
  linked via `annotates` to the corresponding git-pack `pull_request`/`issue` note.
  They are never written as files first and never routed through the blob store. This
  requires zero new verbs; it is a convention over the existing note surface.
- **Blob CAS is for true binaries only**: bench profiles, PDFs, images, archives.
- The mirror (and the one-shot `workspace.ingest(path)` sweep for existing trees)
  handles the residual genuinely file-shaped content — history that already exists on
  disk, and artifact types that are legitimately files.

Dependency this creates on the git pack: the `annotates` target must exist. Today
git-pack ingestion is a batch admin path, so most PR/issue notes do not exist at the
moment a verdict is written. The git-pack lane (ingest cadence and/or outbound verbs)
must guarantee an upsert-on-reference path — creating the verdict note may create the
skeletal `pull_request`/`issue` note it annotates if ingestion has not reached it yet,
with the batch ingester later enriching rather than duplicating it (identity by
repo + number).

### A9: Review-round chains and snapshot anchoring

Review notes are not isolated: rounds on the same pull request form an explicit chain,
and every review is a review OF a specific snapshot.

- **Round chain**: review round N links `supersedes` (note→note, already legal under the
  base contract) to round N-1's note. This is semantically exact — the newer verdict
  supersedes the older for currency, the older round is preserved and queryable, and
  the view layer filters superseded rounds by default. No new endpoint rule is needed;
  `precedes` (which lacks a note→note endpoint) is deliberately not used.
- **Snapshot anchor**: each review note links `annotates` to the git-pack `commit` note
  for the head SHA the review examined. Same upsert-on-reference rule as A8: creating
  the review note may create a skeletal `commit` note (identity by repo + SHA) that
  batch ingestion later enriches.

### A10: Byte-exact content contract for stored documents

The note-first treatment generalizes beyond verdicts: ADRs, design documents, research
notes, and ordinary source/markdown files small enough for inline storage all live in
the database directly as notes or document entities. The storage contract:

- `properties.filename` preserves the original filename WITH extension;
  `properties.media_type` carries the MIME type (or extension-derived equivalent).
- Content is stored **verbatim, byte-exact**: whitespace, tab characters, CRLF vs LF
  line endings, trailing newlines, and all Unicode content must round-trip perfectly.
  No normalization, trimming, or re-encoding at any layer between ingest and export.
- Secret masking (A1) is the single sanctioned transformation; when applied, the entity
  records `properties.masked=true` so consumers know the content is not the original
  bytes, and the checksum is computed over the stored (masked) content.
- Acceptance includes a round-trip fidelity test covering CRLF, tabs, trailing
  whitespace, and multi-byte Unicode.

PDFs and true binaries remain blob-routed per A2/A8.

### A11: Institutionalized capture — completion hooks, not habits

Capture must not depend on anyone remembering a convention. The completion path of
each producing workflow stores the artifact automatically:

- Review-leg wrappers end with an auto-store step: the verdict text becomes the
  decision note (A8) with its round chain and snapshot anchor (A9) before the wrapper
  exits.
- Merging an ADR or design document triggers its ingestion as a document record (A10).
- The background mirror and the one-shot `workspace.ingest` sweep everything the hooks
  miss — the safety net, not the primary path.

Wrapper scripts are an acceptable mechanism; the contract is the trigger point
(completion), not the implementation.

### A12: Ordered backfill of surviving artifacts

Historical verdict-shaped artifacts surviving on disk (thousands across the fleet's
repositories) are backfilled by ONE canonical, idempotent ingest script:

- parse each file → decision note with provenance properties: `source_path`, original
  date where derivable, PR number and round parsed from the filename;
- link `annotates` to the PR/commit notes, upserting skeletal records as needed (A8/A9);
- idempotent by construction (stable identity from repo + PR + round + content hash) so
  re-runs never duplicate;
- the script ships ahead of this amendment's implementation — backfill notes use only
  existing verbs.

### Acceptance

1. A fixture `.khive/` tree containing text, an oversized text file, and a binary file
   mirrors into: N `document` entities (text inline, oversized + binary blob-routed),
   one `workspace` entity, N `contains` edges — verified via `neighbors()`.
2. Editing a mirrored file and re-polling produces a new entity version + `supersedes`
   edge; re-polling with no changes produces zero writes (cursor + checksum short-circuit).
3. Killing the daemon mid-pass and restarting completes the pass idempotently (cursor
   discipline), with no duplicate entities.
4. `workspace.snapshot` on the fixture workspace produces one bundle file; restoring it
   into an empty store reproduces every entity and blob (digest-verified); restoring it
   twice is idempotent.
5. Round-trip fidelity (A10): a fixture set containing CRLF line endings, tab
   characters, trailing whitespace, and multi-byte Unicode ingests and exports
   byte-identical (checksum equality against the original files).
6. Review chain (A9): two review rounds on the same fixture PR produce two decision
   notes, a `supersedes` note→note edge round-2→round-1, and `annotates` edges to the
   two commit notes; default views return only round 2.
