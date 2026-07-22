# Entity kind and edge-endpoint validation

Technical reference for `handlers/common.rs`'s entity-kind validator and the
`valid_relations_for_entity_pair` hint, and the ADR-099 B3 pub-widening that exposes
resolution helpers to `kkernel`.

## ADR-099 B3 pub-widening rationale

`KindSpec`, `resolve_kind_spec`, `resolve_uuid_unfiltered`,
`resolve_uuid_unfiltered_including_deleted`, and `normalize_entity_timestamps` were widened
from `pub(crate)` to `pub` under ADR-099 finding B3: `kkernel`'s `--atomic` validation/
resolution seam reuses these exact functions so a caller-supplied `delete`/`update`/`link`
request resolves kinds, ids, and result payloads through the SAME canonical logic the verb
handlers use, instead of re-deriving or duplicating that logic. `kkernel` already depends on
this crate directly, so this is not a crate-graph inversion.

`resolve_uuid_unfiltered` implements the ADR-007 Rev 6 by-ID contract: UUID resolution for
get/update/delete/merge is namespace-agnostic — the Gate is the authz seam, not storage-layer
filtering. Full-UUID inputs were already unfiltered (`resolve_by_id`); this function closes
the gap for the _prefix_ form, which previously fell through to the primary-namespace-only
`resolve_prefix` and was invisible for any row stamped with a non-primary namespace (#391
§3). It is an exact copy of `resolve_uuid_async` except the prefix-resolution branch.

`resolve_uuid_unfiltered_including_deleted` is the same function again, but also matching
soft-deleted rows — used by the hard-delete by-ID path (#391 §3).

## `validate_entity_type`

The composed registry is built from the builtin registry plus every loaded pack's declared
`ENTITY_TYPES` (ADR-017 additive composition, mirroring `EDGE_RULES`) — NOT
`EntityTypeRegistry::global()`, which only knows builtin subtypes and is blind to
pack-declared extras (e.g. git's `adr` Document subtype). See
[pack-lifecycle.md](pack-lifecycle.md) for where this composed registry gets installed.

## `valid_relations_for_entity_pair`

Derives valid relations for a `(src_kind, src_entity_type, tgt_kind, tgt_entity_type)` entity
pair from the SAME sources `validate_edge_relation_endpoints` consults when accepting or
rejecting a link (issue #543): the base entity endpoint allowlist
(`khive_runtime::operations::base_entity_endpoint_rules`) plus the runtime's live composed
pack `EDGE_RULES`, matched through
`khive_runtime::operations::accepted_pack_relations_for_entities` — the same
`endpoint_matches` semantics `pack_rule_allows` applies internally (`EntityOfKind`,
`EntityOfType`, `NoteOfKind`). There is no separate hand-authored table and no local
re-filter of endpoint kinds here: a hint can no longer diverge from what the validator itself
accepts, including pack rules scoped to a granular `entity_type` via `EntityOfType`.

Note-scoped pack rules (e.g. GTD's `task` -> `task` `depends_on`, declared as `NoteOfKind`)
cannot match here regardless of the shared matcher, because this function is only ever
reached (via `enrich_allowlist_error`) after both endpoints have already been resolved as
entities — a note/note mismatch produces a different validation error entirely ("must be an
entity for relation ..."), not the base-allowlist error this function enriches.

## Test coverage: hint/validator divergence sweep (issue #543)

Hints must equal the validator's own acceptance set, not a separate hand-authored table (the
person→project gap, issue #60, was exactly this divergence class).

`valid_relations_hint_matches_real_validator_acceptance_across_all_entity_kind_pairs` is a
generative cross-check: for EVERY `(source_kind, target_kind)` entity pair in the 9x9 closed
entity-kind taxonomy, the hint set returned by `valid_relations_for_entity_pair` must equal
the set of relations the REAL production validator (`KhiveRuntime::link` ->
`validate_edge_relation_endpoints`) actually accepts for that pair. This calls production code
on both sides — it never re-implements the rule check inline.

Issue #621 flagged that a five-pair spot check missed an `EntityOfType`-scoped divergence;
this test sweeps the full closed kind space so a future divergence at any pair fails loudly
rather than only at hand-picked pairs.

`annotates` requires a note source (never an entity), so it can never be accepted for an
entity→entity pair regardless of kind and is skipped (matches the hint function's own scope,
which never emits "annotates"). `supersedes`/`supports`/`refutes` DO validate entity→entity
pairs against the same base allowlist the hint function reads, so they stay in scope.
