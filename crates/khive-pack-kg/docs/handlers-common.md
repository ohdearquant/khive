# handlers/common.rs — internal rationale

Source: `crates/khive-pack-kg/src/handlers/common.rs`. Most items here are `pub(crate)`
(internal helpers, never rendered on docs.rs); a handful are re-exported as real `pub`
paths from `handlers/mod.rs` for kkernel's `--atomic` seam (ADR-099 B3) and keep a full
doc-comment contract in the source. This guide holds the non-public rationale.

## ADR-099 B3 pub-widening rationale

`KindSpec`, `resolve_kind_spec`, `resolve_uuid_unfiltered`,
`resolve_uuid_unfiltered_including_deleted`, and `normalize_entity_timestamps` were
widened from `pub(crate)` to `pub` under ADR-099 finding B3: `kkernel`'s `--atomic`
validation/resolution seam reuses these exact functions so a caller-supplied
`delete`/`update`/`link` request resolves kinds, ids, and result payloads through the
SAME canonical logic the verb handlers use, instead of re-deriving or duplicating that
logic. `kkernel` already depends on this crate directly, so this is not a crate-graph
inversion.

`resolve_uuid_unfiltered` implements the ADR-007 Rev 6 by-ID contract: UUID resolution
for get/update/delete/merge is namespace-agnostic — the Gate is the authz seam, not
storage-layer filtering. Full-UUID inputs were already unfiltered (`resolve_by_id`); this
function closes the gap for the *prefix* form, which previously fell through to the
primary-namespace-only `resolve_prefix` and was invisible for any row stamped with a
non-primary namespace (#391 §3). It is an exact copy of `resolve_uuid_async` except the
prefix-resolution branch.

`resolve_uuid_unfiltered_including_deleted` is the same function again, but also matching
soft-deleted rows — used by the hard-delete by-ID path (#391 §3).

## `validate_entity_type`

The composed registry is built from the builtin registry plus every loaded pack's
declared `ENTITY_TYPES` (ADR-017 additive composition, mirroring `EDGE_RULES`) — NOT
`EntityTypeRegistry::global()`, which only knows builtin subtypes and is blind to
pack-declared extras (e.g. git's `adr` Document subtype).

## `valid_relations_for_entity_pair`

Derives valid relations for a `(src_kind, src_entity_type, tgt_kind, tgt_entity_type)`
entity pair from the SAME sources `validate_edge_relation_endpoints` consults when
accepting or rejecting a link (issue #543): the base entity endpoint allowlist
(`khive_runtime::operations::base_entity_endpoint_rules`) plus the runtime's live
composed pack `EDGE_RULES`, matched through
`khive_runtime::operations::accepted_pack_relations_for_entities` — the same
`endpoint_matches` semantics `pack_rule_allows` applies internally (`EntityOfKind`,
`EntityOfType`, `NoteOfKind`). There is no separate hand-authored table and no local
re-filter of endpoint kinds here: a hint can no longer diverge from what the validator
itself accepts, including pack rules scoped to a granular `entity_type` (e.g.
`khive-pack-formal`'s typed `theorem -> definition` `depends_on` rule).

Note-scoped pack rules (e.g. GTD's `task` -> `task` `depends_on`, declared as
`NoteOfKind`) cannot match here regardless of the shared matcher, because this function
is only ever reached (via `enrich_allowlist_error`) after both endpoints have already
been resolved as entities — a note/note mismatch produces a different validation error
entirely ("must be an entity for relation ..."), not the base-allowlist error this
function enriches.

## `merge_note_tags`

Merges the top-level `tags` create-param into `properties["tags"]` for a note. Notes have
no dedicated tags column (see `search.rs`'s `tag_filter` handling) — `properties["tags"]`
is the storage convention already used by `memory.remember`
(`khive-pack-memory/src/handlers/remember.rs`) and by this pack's own `search`/`list`
note-tag filters. Without this merge, `create(kind=note, tags=[...])` silently dropped
the tags (#747).

Precedence: an empty/absent `tags` param leaves `properties` untouched. A non-empty
`tags` param always WINS over any `properties["tags"]` the caller also supplied — the
top-level, typed param is the more explicit signal, so it overwrites rather than merges
with a same-named nested key.

## `reject_inapplicable_fields` (handlers/update.rs)

Field applicability guard — authoritative field sets per substrate. Source of truth:
`handler_defs.rs:241-243` + `EntityPatch`/`NotePatch`/`EdgePatch` in
`crates/khive-runtime/src/curation.rs`:

- Entity: `name`, `description`, `tags`, `properties`
- Note: `name`, `content`, `salience`, `decay_factor`, `properties` (notes have NO
  top-level tags column; tags live in `properties["tags"]`)
- Edge: `relation`, `weight`, `properties`

Any present-but-inapplicable field is rejected with a fail-loud error naming the
offending field and listing the substrate's valid set. This function MUST be updated
whenever `UpdateParams` or a patch struct changes.

## `resolve` handler (handlers/resolve.rs)

Thin and read-only: deserializes params, calls the runtime's `resolve_reference`
capability once per ref, and renders each `ReferenceResolution` to its wire shape. All
resolution logic (id-string passthrough, ring lookup, hybrid-search fallback) lives in
`khive_runtime::reference_resolution` — this handler performs no mutation and no side
effect beyond the ring reads/admissions `resolve_reference` and the dispatch boundary
already make.

`resolve`'s pipeline (id-string passthrough, ring, exact-name storage lookup, hybrid
search) is entity-only, so `kind` here follows the same substrate-or-granular
discriminant as `create`/`list`/`search` (see `resolve_kind_spec`): the bare substrate
label `"entity"` means "no kind filter", not a literal `entities.kind` value. Forwarding
the raw string as-is would filter every real match out (#849) — `entities.kind` only ever
holds a granular value like `"concept"`, never `"entity"`.

## `FILTERED_SCAN_CAP` and the scan-cliff widening (handlers/search.rs)

Maximum candidate window used when property/tag filters are active. Predicates are
applied BEFORE result truncation inside the runtime's candidate budget (`search_limit ×
CANDIDATE_MULTIPLIER`). This constant widens the handler's initial `search_limit` so that
sparse matches ranked just below the bare `limit` remain within the candidate window.
Matches ranked beyond the overall budget may still be missed — use specific query text to
keep target records near the top of the ranking.

The entity and note search branches both widen the candidate window the same way so
sparse matches ranked below the bare `limit` remain within the runtime's retrieval
budget. Predicates are applied BEFORE result truncation (entity tags: SQL-level via
`EntityFilter`; entity/note properties and note tags: Rust-level in the alive-set loop,
since notes have no dedicated tag column — tags live in `properties["tags"]`). The cap
bounds worst-case scan cost.
