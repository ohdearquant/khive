# ADR-086: Document/File Modeling — Content on the Existing `document` Entity Kind

**Status**: Proposed\
**Date**: 2026-07-03\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-001 (Entity Kind Taxonomy — `document` is a base kind; `entity_type`
subtype registration), ADR-002 (Edge Ontology — `annotates`, `supersedes`), ADR-013 (Note
Kind Taxonomy), ADR-017 (Pack Standard — `EntityTypeRegistry`, `KindHook`), ADR-021 (Memory
Pack — decay/salience precedent for pack-scoped auxiliary data)\
**Related**: ADR-010 (KG Versioning — NDJSON snapshot scope), ADR-080 (Session Pack — OSS
storage mechanism; sibling background-ingestion precedent for ADR-087), ADR-085 (Code
Pack — `EntityTypeRegistry` Modify precedent, `KindHook` validation precedent)

## Context

Ocean's request: documents and files (specs, research notes, meeting artifacts, reports)
should be "on record and kept" — first-class, graph-queryable, versioned — rather than
opaque files agents happen to read from disk. The `document` entity kind has existed since
ADR-001 as one of the 8 base `EntityKind` variants, but nothing in the codebase specifies
how document _content_ is meant to be stored, retained, or versioned. In practice, agents
either paste excerpts into `description` ad hoc or don't model documents in the graph at
all.

Two facts about the current substrate shape drive this ADR's design directly:

- `Entity` (`crates/khive-types/src/entity.rs`) has `name: String`, `description:
  Option<String>`, and `properties: BTreeMap<String, PropertyValue>` — no dedicated content
  field, but `description` is an unconstrained free-text field with no length limit at the
  SQLite layer.
- `fts_entities` (`crates/khive-db/sql/004-fts-consolidation.sql`) already indexes a
  `title`/`body` pair (name/description) via FTS5 trigram tokenization for every entity,
  regardless of kind. Any text placed in `description` is automatically part of the
  existing hybrid FTS5 + vector search fusion — no new indexing path required.

This is a different shape than the knowledge pack's `knowledge_atoms` table (`content TEXT
NOT NULL DEFAULT ''`, its own primary key, its own FTS5 table `fts_knowledge`) — atoms are
a wholly separate pack-owned corpus, not KG entities. Modeling document/file content is not
the same problem as modeling knowledge atoms, and this ADR does not touch `knowledge_atoms`.

## Decision

Documents and files are modeled as `document`-kind entities using the substrate that
already exists. No new pack, no new table, no new verb surface ships in v1.

1. **Content placement.** `description` holds the document's textual body (markdown,
   plain text, or extracted text) directly, inline. This is the only storage decision this
   ADR makes for text content — it is a usage convention, not a schema change.

2. **Metadata placement.** `properties` carries `source_uri` (where the canonical
   bytes live, if not fully inline — a local path, an object-store URI, or a location
   under the Global Asset Store per the root CLAUDE.md's Resource & Scope Governance
   section), `source_type` (a MIME-ish string, e.g. `text/markdown`, `application/pdf`),
   and optionally `checksum` / `size_bytes` for dedup and integrity checks. This mirrors
   `knowledge_atoms.source_uri` / `source_type` exactly, without importing that table.

3. **`entity_type` governed vocabulary (Modify, not Create).** Register a small subtype
   token set for `document` in `khive-pack-kg`'s existing `EntityTypeRegistry` — the same
   mechanism ADR-085 used for code declaration subtypes, and an explicit Modify per
   `PI_AEP`, not a new pack:

   ```text
   entity_type ∈ {spec, note, summary, handoff, report, reference, adr, transcript, other}
   ```

   These map directly onto the existing `.khive/` workspace convention (root CLAUDE.md
   Workspace Convention: `notes/summaries/`, `notes/handoffs/`, `reports/`, etc.), so
   `entity_type` becomes a queryable filter (`entity_type="handoff"`) instead of a tag
   convention only. This registration is optional at the KindHook-validation level — an
   entity may omit `entity_type` — but recommended for anything created via the
   workspace mirror (ADR-087).

4. **Retention: soft-delete + `supersedes`, never mutate-in-place for a new version.**
   A document that changes (a spec gets revised, a report gets a v2) is a NEW `document`
   entity linked to the prior one via `supersedes` (document→document, already legal in
   the ADR-002 base endpoint contract — no new relation). The old entity is never deleted,
   overwritten, or content-transferred; view-layer queries filter superseded records
   (root CLAUDE.md's `feedback-data-vs-view-not-mutation` principle, and khive's own
   `docs/adr/README.md` "Data vs view" cross-cutting principle). Hard-delete stays
   available via the existing `delete(id, hard=true)` curation verb for deliberate removal
   only (ADR-014), never as part of normal versioning.

5. **Decisions/annotations use existing note kinds.** A `decision` note (or `observation`,
   `insight`, `question`, `reference`) `annotates` a `document` entity to record review
   outcomes, corrections, or commentary. `annotates` is note→any-substrate-UUID and already
   universally legal (ADR-002) — no new relation, matching the same pattern ADR-085 D4 used
   for `finding`→`project`.

6. **No new verb surface.** `create(kind="document", name=..., description=..., properties=
   {...})`, `update`, `get`, `search(kind="document", query=...)`, `link` all already do the
   job. There is no `doc.*` verb family in v1.

## Rationale

### Why not a new pack + content table

The obvious alternative — a dedicated `khive-pack-doc` crate with its own content table,
modeled on `knowledge_atoms` — was the original framing this ADR started from. It is
strictly more machinery than the actual requirement needs: `Entity.description` is already
unconstrained free text, already FTS5-indexed, already versionable via `supersedes`, and
already annotatable via existing notes. `PI_AEP` (`FindExisting > Modify(≤5 files, ≤100
LOC) > Create`) resolves cleanly to Modify — one `EntityTypeRegistry` addition — once the
substrate is read carefully rather than assumed. A new pack crate would duplicate
`fts_entities` coverage in a second FTS5 table for no query benefit, and would need its own
`get`-path wiring to make content visible at all, which is exactly the new-verb-surface
outcome this ADR avoids.

### Why this does not collide with the knowledge pack

`knowledge_atoms` serves a different consumer: curated, rerank-composed corpus content
(`knowledge.compose`), sectioned, dispute/adjudication-tracked, with its own domain
taxonomy. Document/file entities serve "this specific artifact is on record" — a
project-history and provenance concern, not a composable-corpus concern. Nothing here
proposes migrating knowledge atoms to entities, or vice versa; the two remain separate,
matching the "Atoms canonical, not project inventory" standing distinction already in
practice.

### The blob-ready seam, deliberately not built

`StorageCapability` (`crates/khive-storage/src/capability.rs`) is a closed 8-variant enum
(`Sql, Notes, Entities, Graph, Events, Vectors, Sparse, Text`) with no blob/binary variant.
There is currently no storage capability anywhere in khive for large binary content. This
ADR does not add one. Binary or very large content stays out of `description` and is
referenced only via `properties.source_uri` pointing at wherever the bytes actually live
(local disk, the Global Asset Store on LaCie per the Resource & Scope Governance directive,
or an eventual object store). If a real consumer needs inline binary storage, that is a
`StorageCapability::Blob` v2 amendment with its own ADR — the same "defer until a real
consumer needs it" discipline ADR-085's A7 alternative used for commit/PR entities.

As a soft operational guideline (not a hard technical limit — SQLite TEXT columns have no
practical size ceiling here): prefer inline `description` for content under roughly 200KB;
larger text should live at `properties.source_uri` with an excerpt or summary inline, so
that entity list/search responses stay a reasonable size. This is guidance for ingesters
(notably ADR-087's workspace mirror), not a validated constraint.

## Alternatives Considered

**A1: New `khive-pack-doc` crate with a dedicated content table.** Rejected — see
Rationale above. More machinery than the requirement needs; `description` already covers
the v1 need with zero schema change.

**A2: Store content in `properties.content` instead of `description`.** Rejected.
`properties` values are `PropertyValue` (structured metadata), not indexed by
`fts_entities`'s `body` column — using it for large text content would silently lose
search coverage. `description` is the field the existing FTS5/hybrid-search machinery
already treats as the entity's body text.

**A3: A `doc.*` verb family (`doc.write`, `doc.read`) mirroring `knowledge.*`.** Rejected
for v1. Generic `create`/`update`/`get` already suffice for a plain-text-in-`description`
model; a bespoke verb family would only be justified once binary/blob content exists,
which this ADR explicitly defers.

## Consequences

- Documents become queryable via the same `search`/`get`/`neighbors`/`traverse` verbs as
  every other entity, with zero new indexing infrastructure.
- Version history is a linked chain of `supersedes` edges, consistent with how `document`
  already behaves per the base ADR-002 endpoint contract — no new consumer-side logic
  needed to understand "this document has prior versions."
- `entity_type` registration is a small, additive change to `khive-pack-kg`'s
  `EntityTypeRegistry`, reviewed and shipped like any other subtype-token addition.
- The workspace mirror (ADR-087) and the git-lifecycle pack (ADR-088) both build directly
  on this ADR: ADR-087 populates `document` entities using exactly this shape; ADR-088
  deliberately does NOT use this shape (commit/issue are note kinds, not `document`
  entities) — see ADR-088 Rationale for why that's the better fit for that content class.

## Open Questions

1. Should very large inline `description` values (multi-MB text) get a soft warning or
   rejection at the `KindHook` layer, or is the `properties.source_uri` guideline
   sufficient as pure convention? Deferred to implementation experience — no consumer has
   hit this yet.
2. Is a `prepare_create` `KindHook` for `document` (validating `entity_type` against the
   governed vocabulary, defaulting `source_type` if omitted) worth adding in v1, or should
   the vocabulary be advisory-only until a real validation need appears? Recommend
   advisory-only for v1, matching this ADR's minimal-machinery posture; add a hook only if
   ingesters start writing invalid `entity_type` values in practice.

## Implementation

- `crates/khive-pack-kg/src/vocab.rs` (or wherever `EntityTypeRegistry` tokens are
  declared): add the `document` subtype token set.
- No migration required — `description` and `properties` already exist on every entity.
- No new crate.

## References

- `crates/khive-types/src/entity.rs` — `Entity` struct (no content field; `description:
  Option<String>`, `properties: BTreeMap<String, PropertyValue>`)
- `crates/khive-db/sql/004-fts-consolidation.sql` — `fts_entities` (title/body FTS5,
  trigram tokenizer)
- `crates/khive-storage/src/capability.rs` — `StorageCapability` (8 variants, no blob)
- `crates/khive-db/sql/schema.sql` — `knowledge_atoms` (contrast: separate pack-owned
  table, not reused here)
- ADR-085 §D4, §Rationale ("Why a new pack crate at all") — the `EntityTypeRegistry` Modify
  precedent this ADR follows
