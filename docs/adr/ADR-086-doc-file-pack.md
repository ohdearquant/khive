# ADR-086: Document/File Modeling — Content on the Existing `document` Entity Kind

**Status**: Proposed\
**Date**: 2026-07-03\
**Authors**: khive maintainers
**Depends on**: ADR-001 (Entity Kind Taxonomy — `document` is a base kind; `entity_type`
subtype registration), ADR-002 (Edge Ontology — `annotates`, `supersedes`), ADR-013 (Note
Kind Taxonomy), ADR-017 (Pack Standard — `EntityTypeRegistry`, `KindHook`), ADR-021 (Memory
Pack — decay/salience precedent for pack-scoped auxiliary data)\
**Related**: ADR-010 (KG Versioning — NDJSON snapshot scope), ADR-080 (Session Pack — OSS
storage mechanism; sibling background-ingestion precedent for ADR-087), ADR-085 (Code
Pack — `EntityTypeRegistry` Modify precedent, `KindHook` validation precedent)

## Context

Design request: documents and files (specs, research notes, meeting artifacts, reports)
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
- `fts_entities` (`crates/khive-db/sql/023-fts-record-kind.sql`) is one shared table
  with a `namespace` column (corrected by Amendment 1, Decision 5). It indexes a
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
   and optionally `checksum` / `size_bytes` for dedup and integrity checks (defined by
   ADR-087 Amendment 1; see Amendment 1, Decision 5). This mirrors
   `knowledge_atoms.source_uri` / `source_type` exactly, without importing that table.

3. **`entity_type` governed vocabulary (Modify, not Create).** Register a small subtype
   token set for `document` through the composed `EntityTypeRegistry`, with additional tokens
   declared by `khive-pack-kg` — the same mechanism ADR-085 used for code declaration
   subtypes, and an explicit Modify per `PI_AEP`, not a new pack (corrected by Amendment 1,
   Decision 5):

   ```text
   entity_type ∈ {spec, note, summary, handoff, report, reference, adr, transcript, other}
   ```

   (Token set narrowed by Amendment 1, Decision 1: `khive-pack-kg` registers only `note`, `summary`, `handoff`,
   `transcript` and `other`.)

   These map directly onto the existing `.khive/` workspace convention (root CLAUDE.md
   Workspace Convention: `notes/summaries/`, `notes/handoffs/`, `reports/`, etc.), so
   `entity_type` becomes a queryable filter (`entity_type="handoff"`) instead of a tag
   convention only. This registration is optional at the KindHook-validation level — an
   entity may omit `entity_type` — but recommended for anything created via the
   workspace mirror (ADR-087). (An unknown value is refused; Amendment 1, Decision 2.)

4. **Retention: soft-delete + `supersedes`, never mutate-in-place for a new version.**
   A document that changes (a spec gets revised, a report gets a v2) is a NEW `document`
   entity linked to the prior one via `supersedes` (document→document, already legal in
   the ADR-002 base endpoint contract — no new relation). The old entity is never deleted,
   overwritten, or content-transferred; view-layer queries filter superseded records
   (root CLAUDE.md's `feedback-data-vs-view-not-mutation` principle, and khive's own
   `docs/adr/README.md` "Data vs view" cross-cutting principle). Hard-delete stays
   available via the existing `delete(id, hard=true)` curation verb for deliberate removal
   only (ADR-014), never as part of normal versioning. The existing `search(kind="note")`
   filter is note-only; the document filter remains unbuilt (Amendment 1, Decision 4).

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
ADR does not add one. (Corrected by Amendment 1, Decision 5: the enum now has ten variants,
including `Blob` and `Attachments`, and ADR-111 is the blob capability this paragraph
deferred.) Binary or very large content stays out of `description` and is
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
- `entity_type` registration adds only the unclaimed document subtype tokens in
  `khive-pack-kg`; the registry is defined in `khive-types` (corrected by Amendment 1,
  Decision 5).
- The workspace mirror (ADR-087) and the git-lifecycle pack (ADR-088) both build directly
  on this ADR: ADR-087 populates `document` entities using exactly this shape; ADR-088
  deliberately does NOT use this shape (commit/issue are note kinds, not `document`
  entities) — see ADR-088 Rationale for why that's the better fit for that content class.
  (Corrected by Amendment 1, Decision 5: ADR-088's v0 implementation amendment links commit
  notes to document entities by `properties.source_uri` or filename.)

## Open Questions

1. Should very large inline `description` values (multi-MB text) get a soft warning or
   rejection at the `KindHook` layer, or is the `properties.source_uri` guideline
   sufficient as pure convention? Deferred to implementation experience — no consumer has
   hit this yet.
2. Is a `prepare_create` `KindHook` for `document` (validating `entity_type` against the
   governed vocabulary, defaulting `source_type` if omitted) worth adding in v1, or should
   the vocabulary be advisory-only until a real validation need appears? Recommend
   advisory-only for v1, matching this ADR's minimal-machinery posture; add a hook only if
   ingesters start writing invalid `entity_type` values in practice (superseded by Amendment 1,
   Decision 2).

## Implementation

- `crates/khive-types/src/entity_type.rs`: built-in subtype registry; additional document subtypes are
  composed from pack declarations, including `khive-pack-git` (corrected by Amendment 1, Decision 5).
- No migration required — `description` and `properties` already exist on every entity.
- No new crate.

## References

- `crates/khive-types/src/entity.rs` — `Entity` struct (no content field; `description:
  Option<String>`, `properties: BTreeMap<String, PropertyValue>`)
- `crates/khive-db/sql/023-fts-record-kind.sql` — shared `fts_entities` FTS5 table
  with a `namespace` column (corrected by Amendment 1, Decision 5).
- `crates/khive-storage/src/capability.rs` — `StorageCapability` (10 variants, including
  `Blob` and `Attachments`; corrected by Amendment 1, Decision 5).
- `crates/khive-db/sql/schema.sql` — `knowledge_atoms` (contrast: separate pack-owned
  table, not reused here)
- ADR-085 §D4, §Rationale ("Why a new pack crate at all") — the `EntityTypeRegistry` Modify
  precedent this ADR follows

## Amendment 1 (2026-10-23) -- Document subtype ownership and version identity

**Status**: Proposed 2026-10-23. This draft requires specification approval before dependent implementation lands. It
does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#4724](https://github.com/ohdearquant/khive/issues/4724).

**Trigger.** The base subtype list includes tokens already owned by built-ins, the git pack, and base note kinds
(`crates/khive-types/src/entity_type.rs:61-85`, `crates/khive-pack-git/src/vocab.rs:62-66`,
`crates/khive-pack-kg/src/vocab.rs:123-131`). Registry construction rejects canonical and alias collisions and
entity subtype collisions with note kinds (`crates/khive-runtime/src/pack/builder.rs:464-467,781-825`). The base
text also recommends advisory-only validation at lines 174-178 while request validation returns an error for an
unknown subtype (`crates/khive-types/src/entity_type.rs:641-646`,
`crates/khive-pack-kg/src/handlers/common.rs:160-176`). The versioning text does not define an identity key;
ADR-087 defines `source_uri` as stable across versions (`docs/adr/ADR-087-workspace-mirror.md:73-76`).

### Decision 1 -- Register only unclaimed document subtypes

Register only `note`, `summary`, `handoff`, `transcript`, and `other` on `document` from `khive-pack-kg`.
`report` remains owned by the built-in registry, and `spec` remains an alias for built-in `specification`
(`crates/khive-types/src/entity_type.rs:61-85`). `adr` remains owned by `khive-pack-git`
(`crates/khive-pack-git/src/vocab.rs:60-66`). Exclude `reference`, which is a base note kind
(`crates/khive-pack-kg/src/vocab.rs:123-131`). The registry rejects duplicate canonical names and aliases, then rejects
subtype/note-kind collisions at build time (`crates/khive-types/src/entity_type.rs:535-575`,
`crates/khive-runtime/src/pack/builder.rs:781-825`). The registry definition is in `khive-types`, while pack
declarations contribute additional values (`crates/khive-types/src/entity_type.rs:455-459`,
`crates/khive-runtime/src/pack/builder.rs:781-786`).

### Decision 2 -- Refuse unknown document subtypes

Unknown `entity_type` values are refused at create, update, and search. Omitted `entity_type` remains valid.
`EntityTypeRegistry::resolve` returns `UnknownType` when lookup fails, and create/update validation propagates
that error (`crates/khive-types/src/entity_type.rs:598-646`,
`crates/khive-pack-kg/src/handlers/common.rs:160-176`,
`crates/khive-pack-kg/src/handlers/update.rs:261-269`). Search validates an explicit entity type before running
the entity query (`crates/khive-pack-kg/src/handlers/search.rs:225-248`). The recommendation in Open Question 2
(lines 174-178) is superseded because refusal is the current request-validation behavior and issue #4724 asks for
unknown values to be rejected.

### Decision 3 -- Identify document versions by source URI

Use `properties.source_uri` as the document version identity key, consistent with ADR-087
(`docs/adr/ADR-087-workspace-mirror.md:73-76`). An exact source URI match outranks a suffix match. Use `name` as a
fallback only when the candidate document has no `source_uri`. The existing git ingester query matches exact
source URI, basename, or suffix, but its basename branch does not exclude rows that already have a different
source URI (`crates/khive-pack-git/src/sql/entity_rows.rs:73-84`). The explanatory comment and API description
currently describe matching by source URI or name and exact-before-suffix ordering
(`crates/khive-pack-git/src/ingest/commit_history.rs:121-125`,
`crates/khive-pack-git/docs/api/ingest.md:196-209`); align both with this decision.

### Decision 4 -- Keep document supersession filtering explicit

The `supersedes` relation remains legal from document to document (`crates/khive-types/src/edge_rules.rs:113-118`).
The existing `include_superseded` filter applies to notes only, and the entity search path does not use it
(`crates/khive-pack-kg/src/handler_defs.rs:914-918`,
`crates/khive-pack-kg/src/handlers/search.rs:425-455,566-586`). Document supersession filtering remains an
unbuilt view-layer behavior. This amendment records that implementation gap and does not define a document-search
parameter or query contract.

### Decision 5 -- Correct substrate facts and citations

The storage capability enum has ten variants, including `Blob` and `Attachments`
(`crates/khive-storage/src/capability.rs:5-18`). ADR-111 defines and accepts the BlobStore capability
(`docs/adr/ADR-111-blob-store.md:3-7,38-45`). Entity full-text search uses one shared `fts_entities` table with a
`namespace` column, as described by runtime retrieval and migration V23
(`crates/khive-runtime/src/retrieval.rs:728-732`,
`crates/khive-db/sql/023-fts-record-kind.sql:6-17,40-46`).
ADR-088's v0 implementation amendment links commit notes to document entities by
`properties.source_uri` or filename
(`docs/adr/ADR-088-git-lifecycle-pack.md:245-247`). The document subtype registry
definition is in `khive-types` (`crates/khive-types/src/entity_type.rs:455-459`). For
mirrored document properties, ADR-087 A1 defines `checksum` as lowercase hexadecimal
BLAKE3-256 and sets `size_bytes`; that definition governs this shape
(`docs/adr/ADR-087-workspace-mirror.md:188-196`).

### Acceptance

| ID | Statement                                                                                                                      | Evidence                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| -- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A1 | The five declared `khive-pack-kg` document subtype tokens compose without built-in, alias, or note-kind collisions.            | `crates/khive-runtime/src/pack_tests.rs::overlapping_pack_declared_entity_types_reject_at_boot` covers collision rejection; new test `crates/khive-runtime/src/pack_tests.rs::document_subtypes_compose_without_collisions` (to add).                                                                                                                                                                                                                                    |
| A2 | Create, update, and search refuse an unknown document `entity_type`; each path also accepts an omitted `entity_type`.          | `crates/khive-pack-kg/tests/entity_type_filters.rs::entity_type_filters_refuse_unknown_values_with_and_without_kind`; new tests `crates/khive-pack-kg/src/handlers/tests.rs::create_refuses_unknown_entity_type_and_accepts_omission`, `crates/khive-pack-kg/src/handlers/tests.rs::update_refuses_unknown_entity_type_and_accepts_omission`, and `crates/khive-pack-kg/tests/entity_type_filters.rs::search_refuses_unknown_entity_type_and_accepts_omission` (to add). |
| A3 | Exact `source_uri` match wins, suffix match follows exact match, and basename fallback matches only rows without `source_uri`. | `crates/khive-pack-git/src/ingest/find_document_for_path_tests.rs::exact_match_wins_over_wildcard_broadened_candidate`; new test `crates/khive-pack-git/src/ingest/find_document_for_path_tests.rs::basename_fallback_matches_only_documents_without_source_uri` (to add); query at `crates/khive-pack-git/src/sql/entity_rows.rs:73-84`.                                                                                                                                |
| A4 | Superseded-document filtering is identified as unbuilt, and the current `include_superseded` contract is note-only.            | `crates/khive-pack-kg/src/handler_defs.rs:914-918`; entity and note search paths in `crates/khive-pack-kg/src/handlers/search.rs:425-455,566-586`.                                                                                                                                                                                                                                                                                                                       |
| A5 | `checksum` and `size_bytes` use the definitions in ADR-087 Amendment 1.                                                        | `docs/adr/ADR-087-workspace-mirror.md:188-196`.                                                                                                                                                                                                                                                                                                                                                                                                                          |
