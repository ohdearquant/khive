# ADR-087: Workspace Mirror — Folding `.khive/` Into the Graph Substrate

**Status**: Proposed\
**Date**: 2026-07-03\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-086 (Document/File Modeling — the `document`-entity shape this mirror
populates), ADR-080 (Session Pack — OSS Storage Mechanism, §6 session mirror — the
operational pattern this ADR reuses), ADR-002 (Edge Ontology — `supersedes`), ADR-017
(Pack Standard)\
**Related**: ADR-010 (KG Versioning — NDJSON snapshot scope; explains why `.khive/kg/*`
is excluded from this mirror's scope), ADR-021 (Memory Pack)

## Context

Ocean's request, verbatim intent: fold the `.khive/` filesystem convention (workspaces,
notes, summaries, handoffs, reports, `codex_reviews/`) into khive itself, "on record and
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
transcripts, which are recall-only raw material, not meant to be graph nodes. Ocean's ask
for `.khive/` is different in kind: the content must be "on record" in the sense of
graph-queryable (`search`, `neighbors`, `traverse`), linkable, and versioned via
`supersedes` — not merely recallable. Copying the session mirror's storage target verbatim
would satisfy the operational-pattern analogy Ocean drew but miss the actual requirement.
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
   in-place content overwrite. This is Ocean's confirmed resolution: kept means
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
notes, handoffs, and reports are exactly the kind of content Ocean's directive says
should be linkable and traversable — decisions annotate documents, documents get
superseded, agents `neighbors()` out from a report to the project it concerns. None of
that is possible if the content sits in a pack-private table the graph substrate doesn't
see. The requirement, not just the analogy, decides the storage target.

## Alternatives Considered

**A1: Copy the session mirror's auxiliary-table storage verbatim ("similar architecture"
taken literally).** Rejected. Satisfies the literal analogy Ocean drew but not the
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
2. Should `codex_reviews/` mirroring be gated behind a separate opt-in flag, given it's
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
