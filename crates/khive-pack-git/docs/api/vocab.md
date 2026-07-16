# Git pack vocabulary — design notes

Long-form rationale extracted from `crates/khive-pack-git/src/vocab.rs` doc-comments.
All items in that module are `pub(crate)` (the `vocab` module itself is
`pub(crate)` in `lib.rs`), so none of this renders on docs.rs — it is kept
here for future maintainers instead of inline.

## Module overview

ADR-088 v0 shipped with no `HANDLERS` and no `EDGE_RULES`: zero new verbs,
relying exclusively on the base `annotates` contract (note -> any substrate)
for provenance edges. ADR-088 Amendment 1 adds exactly one verb
(`git.digest`) and one endpoint extension (`precedes` commit→commit, for
parent→child commit lineage). See `crates/khive-pack-git/src/pack.rs` for
how this vocabulary is wired into `GitPack`.

## `GIT_LIFECYCLE`

Lifecycle declaration shared by `issue` and `pull_request` — both track an
open/closed state with the same posture as ADR-088's `finding` precedent:
declared for introspection, not yet enforced by the runtime (Phase 1).

## `GIT_NOTE_KIND_SPECS`

`commit` deliberately has no entry: commits are immutable and carry no
lifecycle field.

## `GIT_SCHEMA_PLAN_STMTS`

Pack-auxiliary schema: the git-ingest cursor table (ADR-088 §5, ADR-087
operational pattern reused).

Shape is intentionally generic across git record kinds within a project —
`kind` distinguishes `commits` / `issues` / `prs` cursors so a follow-up pack
(e.g. a code-review pack) can reuse this exact table for its own cursor rows
without a schema change, keyed by its own `project_id`/`kind` pair.
Idempotent (`CREATE TABLE IF NOT EXISTS`), applied once at pack registration
time; not part of the core versioned migration chain.

## `GIT_EDGE_RULES`

ADR-088 Amendment 1 ingest enrichment: parent→child commit lineage as
`precedes` edges. The base endpoint contract only allows `precedes` between
five entity kinds (`document`, `dataset`, `artifact`, `service`, `project` —
see `khive-runtime::operations::BASE_ENTITY_ENDPOINT_RULES`); it has no
note→note case at all. This is the same additive-extension mechanism
`khive-pack-gtd` uses for `depends_on` task→task.

## `GIT_ENTITY_TYPES`

Pack-declared `Document` entity-type subtype: Architecture Decision Records.

`find_document_for_path` (`src/ingest.rs`) resolves pre-existing `document`
entities by git-tracked file path (`properties.source_uri`) so commits/PRs
can `annotates`-link to them; this pack never creates those document
entities itself ("v0 never creates documents on the ingester's behalf"). The
single most common git-tracked document kind ingesting agents attach to a
repo's `document` entities is its ADR corpus (`docs/adr/*.md`) — but
`EntityTypeRegistry::BUILTIN_DEFS` has no `adr` Document subtype, so any
caller attempting `entity_type="adr"` was rejected at the handler layer, and
callers omitted `entity_type` entirely rather than retry against an unknown
value: a schema gap, not a data gap. Declaring it here (composed at boot via
`VerbRegistry::all_entity_types`, ADR-017's additive pack-vocabulary
pattern) makes ADR documents representable without editing the builtin
registry.

## `GIT_HANDLERS`

Illocutionary classification (Searle 1976): `git.digest` commits data to the
graph (ingests notes and edges), so it is `Commissive` — the same category
`create`/`link`/`remember` use.
