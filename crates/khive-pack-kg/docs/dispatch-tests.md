# dispatch.rs test-module rationale

Source: `crates/khive-pack-kg/src/dispatch.rs`, `mod tests`. These are `#[cfg(test)]`
regression tests — they never render on docs.rs. This guide preserves the rationale that
used to live in their doc comments; the source now carries a one-line summary per test.

## Namespace (ADR-007) regression suite

### `kg_create_entity_honors_caller_namespace`

PR-A1 (ADR-007): by-ID `get` returns a record regardless of the caller's namespace.
Namespace on the returned record must still reflect the creator's namespace. list/search
still filter by namespace (PR-B scope). Verifies: entity created under `tenant-a` is
namespaced correctly; by-ID get is namespace-agnostic — tenant-b can fetch tenant-a's
entity by UUID, and the namespace on the fetched record is still `tenant-a` (never
rewritten to the caller's namespace).

### `kg_oss_default_namespace_entities_colocate`

Two creates with no explicit namespace land in the same `local` namespace.

### `dispatch_list_honors_configured_visible_namespaces_adr007_rev4`

ADR-007 Rev 4, Rule 3b: default read scope honors configured visible set. The dispatch
token minted by VerbRegistry on the default (no explicit `namespace=`) path widens the
read scope to `['local'] ∪ visible_namespaces`. Both the 'local' entity AND the entity
written to a configured extra namespace must appear in a registry-dispatched list without
any explicit `namespace=` parameter. This test verifies:

1. Records written to "local" are visible in the list — the shared-brain property.
2. A record written to a configured visible namespace (via a directly-minted token) ALSO
   appears in the registry-dispatched list — the Rev 4 read-scope widening.

### `dispatch_list_empty_visible_namespaces_scopes_to_local_only_adr007_rev4`

ADR-007 Rev 4: backward-compat — with visible_namespaces UNSET, default read scope =
['local'] only. A registry with no `visible_namespaces` configured has the same behavior
as Rev 3: list returns only records in 'local'. A record written to a different namespace
via a directly-minted token does NOT appear in the registry list.

### `dispatch_list_local_always_included_when_visible_ns_set_adr007_rev4`

ADR-007 Rev 4: 'local' is always included in the default read scope, even when
visible_namespaces does not explicitly list it. Configuring visible_namespaces =
["other-ns"] (without "local") must still return records from BOTH 'local' and
'other-ns' in the registry list.

### `dispatch_explicit_namespace_param_is_precise_not_widened_adr007_rev4`

ADR-007 Rev 4: explicit namespace= param is a precise single-namespace escape, NOT
widened. With visible_namespaces=["other-ns"] configured, a list(namespace="other-ns")
call scopes to EXACTLY ["other-ns"] and does NOT include 'local' or the union set. This
preserves the ability to read a single named set precisely.

### `non_local_actor_config_does_not_route_storage_adr007_rule0`

ADR-007 Rev 4, Rule 0 regression: non-local actor config does NOT route WRITE storage.
Builds a VerbRegistry whose `default_namespace` is `"lambda:leo"` (simulating `[actor]
id = "lambda:leo"` or `--actor lambda:leo`). Dispatches `create` and `list` through
`VerbRegistry::dispatch` (the real MCP path — not `pack.dispatch`). Asserts:

1. The created entity lands in `"local"`, not `"lambda:leo"`.
2. A subsequent `list` via the registry returns the entity, proving write+read both
   operate on `"local"` regardless of the non-local actor configuration.
3. A direct-token `list` scoped to `"lambda:leo"` returns an empty set, proving the
   storage was never written to the actor namespace.

## Predicate-pushdown scan-cliff regressions (issue #225)

### `handler_search_entity_tag_filter_beyond_scan_cliff`

Handler-level regression for issue #225, entity branch, tag filter. 51 decoys rank above
the target in FTS but lack the required tag. The target carries the required tag and sits
at rank 52. With predicate pushdown the handler returns the target despite the cliff.

### `handler_search_entity_props_filter_beyond_scan_cliff`

Handler-level regression for issue #225, entity branch, properties filter. 51 decoys rank
above the target in FTS but have the wrong property value. The target has the required
property and sits at rank 52.

### `handler_search_note_tag_filter_beyond_scan_cliff`

Handler-level regression for issue #225, note branch, tag filter. 51 observation notes
rank above the target in FTS but lack the required tag. The target carries the required
tag and sits at rank 52.

### `handler_search_note_props_filter_beyond_scan_cliff`

Handler-level regression for issue #225, note branch, properties filter. 51 observation
notes rank above the target in FTS but have the wrong property. The target has the
required property value and sits at rank 52.

### `handler_search_note_sanitizes_fts5_metacharacters`

No extra rationale beyond the name — FTS5 metacharacter sanitization regression.

## `resolve` verb regression suite

### `resolve_id_string_passthrough_through_dispatch`

A UUID ref resolves through the id-string passthrough stage, regardless of whether the
entity was ever touched through this registry's ring — exercised end to end through verb
dispatch.

### `resolve_via_recently_referenced_ring_after_create`

The dispatch-boundary ring admits `create`'s returned id under its name; a later
`resolve(refs=[name])` call by the SAME actor resolves it via the ring stage without ever
running hybrid search over a matching name. Proves the ring, not just the id-string
passthrough.

### `resolve_ambiguous_on_duplicate_ring_names`

Two entities admitted to the ring under the exact same name resolve as `Ambiguous`, never
a silent pick (F7 of the unified-verb draft ADR).

### `resolve_not_found_when_nothing_matches`

A ref that matches nothing in the ring or hybrid search is `NotFound`.

### `resolve_search_result_sets_never_populate_the_ring`

`search` result-sets never admit to the ring (gate condition, 2026-07-09): an entity that
only ever went through `search` (never `create`/`get`/`update`/`delete`/`merge`/`link` on
THIS registry) must not resolve via the ring's high-confidence exact-match stage. It is
created directly on the runtime (bypassing dispatch, hence bypassing admission entirely)
so the only way `resolve` can find it via the ring is stage 2 — proven by a confidence
that never equals the ring's fixed 0.95 exact-match / 0.7 substring-match bands (it
instead comes from the stage-3 exact-name storage lookup, #849, since the ref is this
entity's exact name).

### `resolve_via_ring_survives_non_local_default_namespace`

Regression for the namespace-key mismatch: ring admission used the gate-resolved `ns`
(which is the configured `default_namespace`, e.g. `"lambda:leo"`, on the default
dispatch path) while `resolve_reference`'s ring lookup used `token.namespace()` (always
`"local"` on that same path, per ADR-007 Rule 0/3b). With a non-local `default_namespace`,
a `create` followed by a same-actor `resolve` on the same registry must still hit the
ring — proving admission and lookup are keyed identically.

### `resolve_id_string_passthrough_is_entity_only`

Id-string passthrough is entity-scoped, identically for full UUIDs and short prefixes: a
note's id-string is `NotFound` through `resolve`, even though `get` on the same id would
succeed.

### `resolve_appears_in_verbs_introspection`

`resolve` is registered as a public verb and appears in `verbs()`.

### `resolve_exact_name_hit_resolves_high_confidence`

An entity that already exists but was never referenced through this registry's ring
resolves via the new exact-name storage lookup, at `EXACT_NAME_CONFIDENCE` (0.98) — above
the ring's bands, below the absolute certainty of an id-string passthrough (1.0). Also
regression coverage for the literal #849 repro: `kind="entity"` is the bare substrate
label (no filter), not a literal `entities.kind` value.

### `resolve_exact_name_handles_cjk_and_spaces`

Exact-name storage lookup is Unicode-safe and does not tokenize names. CJK and embedded
spaces therefore resolve with the same confidence as an ASCII single-token name.

### `resolve_case_variant_uses_hybrid_fallback`

The storage exact-name tier is case-sensitive. A case-only variant can still resolve
through case-insensitive hybrid search, but it must not receive the exact tier's 0.98
confidence.

### `resolve_exact_name_ambiguous_on_duplicate_names`

Two entities sharing the exact same name resolve as `Ambiguous`, never a silent pick,
mirroring the ring's duplicate-name contract.

### `resolve_exact_name_miss_falls_through_to_hybrid_search`

A ref with no exact-name storage match still falls through to the hybrid-search fallback
(existing stage-4 behavior preserved): a partial-phrase query that cannot exact-match any
name resolves via search, at a confidence below the exact-name stage's 0.98.

### `resolve_exact_name_soft_deleted_entity_not_matched`

A soft-deleted entity is invisible to the exact-name storage lookup (`deleted_at IS NULL`
is baked into `query_entities`), matching the rest of the KG surface's soft-delete
contract.

### `resolve_exact_name_respects_kind_filter`

A granular `kind` filter narrows the exact-name lookup: two entities with the exact same
name but different entity kinds resolve deterministically to the one matching `kind`,
instead of `Ambiguous`.

### `resolve_kind_rejects_non_entity_kind`

`resolve`'s `kind` param is entity-only, matching the id-string passthrough and ring
stages: a note kind is rejected with a clear error rather than silently over-filtering to
zero matches.

## `warm()` telemetry

### `kg_warm_emits_exactly_one_phase_started_and_one_terminal_event`

Verifies the ADR-103 Amendment 1 Part 2 phase-span contract described in
`KgPack::warm`'s doc comment: exactly one `PhaseStarted` and one terminal
(`PhaseCompleted`/`PhaseCancelled`) event per `warm()` call.

## Implementation notes (moved from inline `//` comments)

### `warm-token-minting`

ADR-103 Amendment 1 Part 2: `warm()` runs at daemon construction / first pack install,
outside `dispatch()` entirely — there is no caller-supplied token. Mint one the same way
`khive-pack-memory`'s ANN background-rebuild task does (`rt.authorize(Namespace::local())`),
so this daemon-startup embedder warmup is attributed to the daemon principal instead of
remaining invisible on the event plane. A mint failure only removes this pass's telemetry
— the warmup itself still runs.

### `entity-type-validator-installation`

Installs the validator on the runtime this pack OWNS, not on the caller-supplied runtime.
In a multi-backend deployment the pack is constructed with a per-pack runtime (see
`PackRegistry::register_packs_with_runtimes`); `self.runtime` is that runtime. In a
single-backend deployment `self.runtime` IS the single runtime, so behaviour is identical
to the previous call-through. The validator is composed once from every loaded pack's
`ENTITY_TYPES` (`VerbRegistry::all_entity_types`, threaded in by
`call_register_entity_type_validators`) layered over the builtin registry, so
pack-declared subtypes (e.g. git's `adr` Document subtype) validate here in addition to
`EntityTypeRegistry::global()`'s builtin-only set.

### `issue-225-scan-cliff-mechanics`

These tests operate at the `pack.dispatch("search", ...)` level and prove the REAL
handler cliff: with `limit=1` the handler sets `search_limit = (1 * 50).min(500) = 50`,
which means only 50 candidates enter the runtime. To push the target BEYOND the pre-fix
cliff the tests insert 51 non-matching decoys so that the target sits at rank 52 in the
unfiltered FTS ordering.

Without predicate pushdown into `hybrid_search` / `search_notes` the runtime received
`search_limit = 50` candidates, all decoys, and never saw the target. With the fix the
runtime applies the filter BEFORE truncation over all 200 candidates (50 ×
CANDIDATE_MULTIPLIER = 4) and surfaces the target.

Budget constants (from `handlers/search.rs` and `retrieval.rs`):

- handler `search_limit = (limit * 50).min(500)` → limit=1 → 50
- runtime candidates = `search_limit * 4` → 200
- old cliff: rank 51 (beyond handler 50-record scan)
- new cliff: rank 201 (beyond runtime 200-candidate budget)

Corpus: 51 decoys (ranks 1-51 in FTS) + 1 target (rank 52). Target has the discriminating
tag/property; decoys do not. `limit=1`.

### `resolve-ring-admission-boundary`

Ring admission happens at the `VerbRegistry::dispatch_with_identity` boundary (see
`pack.rs`), not inside `KgPack::dispatch` — these tests go through
`registry.dispatch(verb, params)` (not the direct `pack.dispatch(verb, params, &registry,
&tok)` bypass most other tests in this module use) specifically so the admission hook
fires.

### `exact-name-storage-lookup-fixtures`

These entities are created directly on the runtime (bypassing `registry.dispatch`, hence
bypassing ring admission entirely — same technique as
`resolve_search_result_sets_never_populate_the_ring`), so the only way `resolve` can find
them is the exact-name storage lookup (or, for the fallback-preserved case, hybrid
search).
