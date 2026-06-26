"""Contract tests: search verb property/tag filter surface.

ADR: ADR-017
section: search verb; property and tag predicates; before-truncation candidate widening

Issue #260 bullet 2: property and tag filters on the `search` verb for
entity and note kinds, including the before-truncation candidate-window
widening semantics introduced in PR #225 (entity branch) and PR #223/#225
(note branch).

Behavioral spec derived from
  crates/khive-pack-kg/src/handlers/search.rs
  crates/khive-pack-kg/src/handlers/common.rs:

  props_match  — ALL key-value pairs in the filter must appear in record
                 properties (AND semantics).
  tags_match_any — ANY single tag in the filter matches record tags;
                   comparison is case-insensitive (OR semantics).

  search_limit widening — when a property or tag filter is active the handler
  passes (limit * 50).min(FILTERED_SCAN_CAP=500) candidates to the runtime so
  that records ranked below the bare `limit` remain within the retrieval budget.
  The runtime fetches CANDIDATE_MULTIPLIER * search_limit entries before
  applying the predicate and truncating to limit.  This means a record at
  rank N can still be returned if N <= limit * 50 * CANDIDATE_MULTIPLIER.

Note on note tags: entity tags live in the top-level `tags` column; note
tags live in `properties["tags"]` (notes have no dedicated tag column).
The `search(tags=[...])` filter checks the correct field for each substrate.

Out of scope for this file (covered elsewhere in the suite):
  - create_many bulk verb (#232) — see tests/test_create_many.py
  - khive-pack-formal / EntityOfType (#231) — see tests/test_formal_pack.py
  - True N-physical-backend fan-out routing (storage-pluggability unmerged)
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {"create", "search"}


# ---------------------------------------------------------------------------
# Entity search — property filter
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_props_filter_drops_non_matching(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind=entity, properties=...) returns only entities whose properties match.

    Creates two entities in the same namespace:
    - 'matching': properties include {"category": "alpha"}
    - 'other':    properties include {"category": "beta"}

    The property filter {"category": "alpha"} must return the matching entity
    and exclude the other.  Verifies props_match AND semantics at the verb surface.
    """
    ns = temp_namespace

    matching = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfprop_match_entity",
        "description": "entity with matching property",
        "properties": {"category": "alpha"},
        "namespace": ns,
    })
    match_id = matching["id"]

    other = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfprop_other_entity",
        "description": "entity with non-matching property",
        "properties": {"category": "beta"},
        "namespace": ns,
    })
    other_id = other["id"]

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfprop",
        "properties": {"category": "alpha"},
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert match_id in hit_ids, (
        f"entity with matching property must appear in results; hit_ids={hit_ids}"
    )
    assert other_id not in hit_ids, (
        f"entity with non-matching property must be absent from results; hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Entity search — tag filter
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_tag_filter_drops_non_matching(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind=entity, tags=[...]) returns only entities carrying at least one requested tag.

    Creates two entities:
    - 'tagged':   tags=["sftag-target"]
    - 'untagged': tags=["sftag-other"]

    The tag filter ["sftag-target"] must include the tagged entity and exclude
    the untagged one.
    """
    ns = temp_namespace

    tagged = khive_session.verb("create", {
        "kind": "concept",
        "name": "sftag_tagged_entity",
        "description": "entity carrying the target tag",
        "tags": ["sftag-target"],
        "namespace": ns,
    })
    tagged_id = tagged["id"]

    untagged = khive_session.verb("create", {
        "kind": "concept",
        "name": "sftag_untagged_entity",
        "description": "entity without the target tag",
        "tags": ["sftag-other"],
        "namespace": ns,
    })
    untagged_id = untagged["id"]

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sftag",
        "tags": ["sftag-target"],
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert tagged_id in hit_ids, (
        f"entity with tag 'sftag-target' must appear in results; hit_ids={hit_ids}"
    )
    assert untagged_id not in hit_ids, (
        f"entity without tag 'sftag-target' must be absent from results; hit_ids={hit_ids}"
    )


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_tag_filter_or_semantics(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """tags filter uses OR semantics: an entity matching ANY requested tag is included.

    Creates three entities:
    - 'alpha-only':  tags=["sfor-alpha"]
    - 'beta-only':   tags=["sfor-beta"]
    - 'neither':     tags=["sfor-gamma"]

    Filter tags=["sfor-alpha", "sfor-beta"] must return both alpha-only and
    beta-only entities, and exclude neither.  Confirms tags_match_any OR behavior.
    """
    ns = temp_namespace

    alpha = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfor_alpha_entity",
        "description": "entity tagged with alpha",
        "tags": ["sfor-alpha"],
        "namespace": ns,
    })
    alpha_id = alpha["id"]

    beta = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfor_beta_entity",
        "description": "entity tagged with beta",
        "tags": ["sfor-beta"],
        "namespace": ns,
    })
    beta_id = beta["id"]

    neither = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfor_neither_entity",
        "description": "entity tagged with gamma only",
        "tags": ["sfor-gamma"],
        "namespace": ns,
    })
    neither_id = neither["id"]

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfor",
        "tags": ["sfor-alpha", "sfor-beta"],
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert alpha_id in hit_ids, (
        "alpha-tagged entity must appear with filter ['sfor-alpha','sfor-beta']; "
        f"hit_ids={hit_ids}"
    )
    assert beta_id in hit_ids, (
        "beta-tagged entity must appear with filter ['sfor-alpha','sfor-beta']; "
        f"hit_ids={hit_ids}"
    )
    assert neither_id not in hit_ids, (
        "entity with only 'sfor-gamma' must be absent from results; "
        f"hit_ids={hit_ids}"
    )


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_tag_filter_case_insensitive(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Tag matching is case-insensitive: filter "sfci-rusttag" must match stored tag "SFCI-RustTag".

    Confirms tags_match_any uses eq_ignore_ascii_case.
    """
    ns = temp_namespace

    entity = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfci_case_entity",
        "description": "entity with mixed-case tag",
        "tags": ["SFCI-RustTag"],
        "namespace": ns,
    })
    entity_id = entity["id"]

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfci_case",
        "tags": ["sfci-rusttag"],
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert entity_id in hit_ids, (
        "tag filter must be case-insensitive: 'sfci-rusttag' must match stored 'SFCI-RustTag'; "
        f"hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Entity search — before-truncation widening semantics (#225)
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_search_props_filter_before_truncation_widening(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Entity property filter is applied before result truncation (widened candidate window).

    Mirrors the Rust regression test
    handler_search_entity_props_filter_beyond_scan_cliff in
    crates/khive-pack-kg/src/handlers/dispatch.rs at the MCP verb surface.

    Setup: 51 decoys with high TF for the query terms but wrong property value;
    1 target with the required property value and lower TF.  limit=1.

    With no filter the decoys fill the top 51 ranks; the target sits at rank 52.
    Without widening (naive limit=1 → runtime fetches 1*4=4 candidates) the
    target would be invisible.  With the handler's widening
    (search_limit = min(1*50, 500) = 50 → runtime fetches 50*4=200 candidates)
    the target at rank 52 is inside the 200-candidate budget, survives the
    property filter, and is returned as the single result.
    """
    ns = temp_namespace

    # 51 decoys: high TF on the query terms, wrong property value.
    decoy_blob = "sfwidp_probe sfwidp_signal " * 10
    for i in range(51):
        khive_session.verb("create", {
            "kind": "concept",
            "name": f"{decoy_blob}decoy_{i}",
            "description": f"{decoy_blob}decoy_{i}_description",
            "properties": {"domain": "sfwidp-other"},
            "namespace": ns,
        })

    # Target: lower TF, but carries the required property.
    target = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfwidp_probe sfwidp_signal target",
        "description": "sfwidp_probe sfwidp_signal target description",
        "properties": {"domain": "sfwidp-target"},
        "namespace": ns,
    })
    target_id = target["id"]

    # Self-asserting phase: confirm the target's unfiltered rank lies in (4, 200].
    # Budget chain for limit=1:
    #   naive (no widening):  runtime candidates = 1 * 4  =   4
    #   handler widens:       search_limit = min(1*50,500) = 50
    #   runtime over-fetch:   candidates  = 50 * 4        = 200
    # Source: crates/khive-pack-kg/src/handlers/search.rs (lines ~64-70, widening formula)
    #         crates/khive-runtime/src/retrieval.rs (CANDIDATE_MULTIPLIER=4, line 67)
    # The target must be at rank > 4 (proves widening is needed) and rank <= 200
    # (proves the widened budget is sufficient).  Exact rank is NOT asserted —
    # the (4, 200] band stays robust to minor FTS scoring drift.
    unfiltered = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfwidp_probe sfwidp_signal",
        "limit": 200,
        "namespace": ns,
    })
    assert isinstance(unfiltered, list), (
        f"unfiltered search must return a list; got {type(unfiltered)}"
    )
    unfiltered_ids = [h.get("id", "") for h in unfiltered]
    assert target_id in unfiltered_ids, (
        "target must appear in unfiltered results at limit=200 (setup sanity check); "
        f"total unfiltered hits={len(unfiltered_ids)}"
    )
    target_rank = unfiltered_ids.index(target_id) + 1  # 1-indexed
    assert target_rank > 4, (
        f"target must rank below the naive budget floor (rank > 4); got rank={target_rank}. "
        "Decoy FTS signal is insufficiently dominant — check decoy_blob repetitions."
    )
    assert target_rank <= 200, (
        f"target must be within the widened budget ceiling (rank <= 200); got rank={target_rank}. "
        "Budget model is off — verify handler widening and runtime CANDIDATE_MULTIPLIER."
    )

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfwidp_probe sfwidp_signal",
        "properties": {"domain": "sfwidp-target"},
        "limit": 1,
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert len(hits) == 1, (
        "exactly one hit expected (the target); "
        f"got {len(hits)}: {hits}"
    )
    assert hits[0].get("id") == target_id, (
        f"the property-filtered target entity (unfiltered rank={target_rank}) must be returned; "
        f"got {hits[0]}"
    )


@pytest.mark.search_filters
@pytest.mark.slow
def test_entity_search_tag_filter_before_truncation_widening(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Entity tag filter is applied before result truncation (widened candidate window).

    Same cliff scenario as the props test but exercises the tag filter branch.
    51 decoys outrank the target in FTS; the target carries the required tag.
    With limit=1 and the handler's (limit*50) widening, the target is returned.
    """
    ns = temp_namespace

    decoy_blob = "sfwidt_probe sfwidt_signal " * 10
    for i in range(51):
        khive_session.verb("create", {
            "kind": "concept",
            "name": f"{decoy_blob}decoy_{i}",
            "description": f"{decoy_blob}decoy_{i}_description",
            "tags": ["sfwidt-decoy"],
            "namespace": ns,
        })

    target = khive_session.verb("create", {
        "kind": "concept",
        "name": "sfwidt_probe sfwidt_signal target",
        "description": "sfwidt_probe sfwidt_signal target description",
        "tags": ["sfwidt-target"],
        "namespace": ns,
    })
    target_id = target["id"]

    # Self-asserting phase: confirm the target's unfiltered rank lies in (4, 200].
    # Budget chain for limit=1:
    #   naive (no widening):  runtime candidates = 1 * 4  =   4
    #   handler widens:       search_limit = min(1*50,500) = 50
    #   runtime over-fetch:   candidates  = 50 * 4        = 200
    # Source: crates/khive-pack-kg/src/handlers/search.rs (lines ~64-70, widening formula)
    #         crates/khive-runtime/src/retrieval.rs (CANDIDATE_MULTIPLIER=4, line 67)
    unfiltered = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfwidt_probe sfwidt_signal",
        "limit": 200,
        "namespace": ns,
    })
    assert isinstance(unfiltered, list), (
        f"unfiltered search must return a list; got {type(unfiltered)}"
    )
    unfiltered_ids = [h.get("id", "") for h in unfiltered]
    assert target_id in unfiltered_ids, (
        "target must appear in unfiltered results at limit=200 (setup sanity check); "
        f"total unfiltered hits={len(unfiltered_ids)}"
    )
    target_rank = unfiltered_ids.index(target_id) + 1  # 1-indexed
    assert target_rank > 4, (
        f"target must rank below the naive budget floor (rank > 4); got rank={target_rank}. "
        "Decoy FTS signal is insufficiently dominant — check decoy_blob repetitions."
    )
    assert target_rank <= 200, (
        f"target must be within the widened budget ceiling (rank <= 200); got rank={target_rank}. "
        "Budget model is off — verify handler widening and runtime CANDIDATE_MULTIPLIER."
    )

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "sfwidt_probe sfwidt_signal",
        "tags": ["sfwidt-target"],
        "limit": 1,
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert len(hits) == 1, (
        "exactly one hit expected (the target); "
        f"got {len(hits)}: {hits}"
    )
    assert hits[0].get("id") == target_id, (
        f"the tag-filtered target entity (unfiltered rank={target_rank}) must be returned; "
        f"got {hits[0]}"
    )


# ---------------------------------------------------------------------------
# Note search — property filter
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_note_props_filter_drops_non_matching(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind=note, properties=...) returns only notes whose properties match.

    Note properties are the note's `properties` JSON blob.  The filter is
    applied via props_match (AND semantics, same helper as entity search).
    """
    ns = temp_namespace

    matching = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfnprop match note content for search",
        "properties": {"source": "sfnprop-match"},
        "namespace": ns,
    })
    match_id = matching["id"]

    other = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfnprop other note content for search",
        "properties": {"source": "sfnprop-other"},
        "namespace": ns,
    })
    other_id = other["id"]

    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "sfnprop",
        "properties": {"source": "sfnprop-match"},
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert match_id in hit_ids, (
        f"note with matching property must appear in results; hit_ids={hit_ids}"
    )
    assert other_id not in hit_ids, (
        f"note with non-matching property must be absent from results; hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Note search — tag filter
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_note_tag_filter_drops_non_matching(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind=note, tags=[...]) filters by tags stored in properties["tags"].

    Notes have no dedicated tag column; tags live in properties["tags"] (a JSON
    array).  The search handler reads them from there and applies tags_match_any.
    Tags must be supplied at create time via properties={"tags": [...]}.
    """
    ns = temp_namespace

    tagged = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfntag tagged note content for search",
        "properties": {"tags": ["sfntag-target"]},
        "namespace": ns,
    })
    tagged_id = tagged["id"]

    untagged = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfntag untagged note content for search",
        "properties": {"tags": ["sfntag-other"]},
        "namespace": ns,
    })
    untagged_id = untagged["id"]

    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "sfntag",
        "tags": ["sfntag-target"],
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert tagged_id in hit_ids, (
        f"note with tag 'sfntag-target' in properties['tags'] must appear; hit_ids={hit_ids}"
    )
    assert untagged_id not in hit_ids, (
        f"note without tag 'sfntag-target' must be absent from results; hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Note search — before-truncation widening semantics (#225)
# ---------------------------------------------------------------------------


@pytest.mark.search_filters
@pytest.mark.slow
def test_note_search_props_filter_before_truncation_widening(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Note property filter is applied before result truncation (widened candidate window).

    Mirrors handler_search_note_props_filter_beyond_scan_cliff from
    crates/khive-pack-kg/src/handlers/dispatch.rs at the MCP verb surface.

    51 observation notes with high TF for the query terms but wrong property;
    1 target note with the required property and lower TF.  limit=1.

    The target at FTS rank 52 must be returned because the handler widens
    search_limit to 50, the runtime fetches 200 candidates, and the property
    filter is applied on that wider window before truncation.
    """
    ns = temp_namespace

    decoy_blob = "sfnwidp_probe sfnwidp_signal " * 10
    for i in range(51):
        khive_session.verb("create", {
            "kind": "observation",
            "content": f"{decoy_blob}decoy_{i}",
            "properties": {"category": "sfnwidp-other"},
            "namespace": ns,
        })

    target = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfnwidp_probe sfnwidp_signal target note",
        "properties": {"category": "sfnwidp-target"},
        "namespace": ns,
    })
    target_id = target["id"]

    # Self-asserting phase: confirm the target's unfiltered rank lies in (4, 200].
    # Budget chain for limit=1:
    #   naive (no widening):  runtime candidates = 1 * 4  =   4
    #   handler widens:       search_limit = min(1*50,500) = 50
    #   runtime over-fetch:   candidates  = 50 * 4        = 200
    # Source: crates/khive-pack-kg/src/handlers/search.rs (lines ~171-178, note branch)
    #         crates/khive-runtime/src/operations.rs (lines ~2409-2427, candidates=limit*4)
    unfiltered = khive_session.verb("search", {
        "kind": "note",
        "query": "sfnwidp_probe sfnwidp_signal",
        "limit": 200,
        "namespace": ns,
    })
    assert isinstance(unfiltered, list), (
        f"unfiltered search must return a list; got {type(unfiltered)}"
    )
    unfiltered_ids = [h.get("id", "") for h in unfiltered]
    assert target_id in unfiltered_ids, (
        "target must appear in unfiltered results at limit=200 (setup sanity check); "
        f"total unfiltered hits={len(unfiltered_ids)}"
    )
    target_rank = unfiltered_ids.index(target_id) + 1  # 1-indexed
    assert target_rank > 4, (
        f"target must rank below the naive budget floor (rank > 4); got rank={target_rank}. "
        "Decoy FTS signal is insufficiently dominant — check decoy_blob repetitions."
    )
    assert target_rank <= 200, (
        f"target must be within the widened budget ceiling (rank <= 200); got rank={target_rank}. "
        "Budget model is off — verify handler widening and runtime CANDIDATE_MULTIPLIER."
    )

    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "sfnwidp_probe sfnwidp_signal",
        "properties": {"category": "sfnwidp-target"},
        "limit": 1,
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert len(hits) == 1, (
        "exactly one hit expected (the target note); "
        f"got {len(hits)}: {hits}"
    )
    assert hits[0].get("id") == target_id, (
        f"the property-filtered target note (unfiltered rank={target_rank}) must be returned; "
        f"got {hits[0]}"
    )


@pytest.mark.search_filters
@pytest.mark.slow
def test_note_search_tag_filter_before_truncation_widening(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Note tag filter is applied before result truncation (widened candidate window).

    Same cliff scenario as the note props test but exercises the tag filter branch.
    Note tags live in properties["tags"]; the handler reads them from there.
    51 decoy notes outrank the target in FTS; the target has the required tag.
    """
    ns = temp_namespace

    decoy_blob = "sfnwidt_probe sfnwidt_signal " * 10
    for i in range(51):
        khive_session.verb("create", {
            "kind": "observation",
            "content": f"{decoy_blob}decoy_{i}",
            "properties": {"tags": ["sfnwidt-decoy"]},
            "namespace": ns,
        })

    target = khive_session.verb("create", {
        "kind": "observation",
        "content": "sfnwidt_probe sfnwidt_signal target note",
        "properties": {"tags": ["sfnwidt-target"]},
        "namespace": ns,
    })
    target_id = target["id"]

    # Self-asserting phase: confirm the target's unfiltered rank lies in (4, 200].
    # Budget chain for limit=1:
    #   naive (no widening):  runtime candidates = 1 * 4  =   4
    #   handler widens:       search_limit = min(1*50,500) = 50
    #   runtime over-fetch:   candidates  = 50 * 4        = 200
    # Source: crates/khive-pack-kg/src/handlers/search.rs (lines ~171-178, note branch)
    #         crates/khive-runtime/src/operations.rs (lines ~2409-2427, candidates=limit*4)
    unfiltered = khive_session.verb("search", {
        "kind": "note",
        "query": "sfnwidt_probe sfnwidt_signal",
        "limit": 200,
        "namespace": ns,
    })
    assert isinstance(unfiltered, list), (
        f"unfiltered search must return a list; got {type(unfiltered)}"
    )
    unfiltered_ids = [h.get("id", "") for h in unfiltered]
    assert target_id in unfiltered_ids, (
        "target must appear in unfiltered results at limit=200 (setup sanity check); "
        f"total unfiltered hits={len(unfiltered_ids)}"
    )
    target_rank = unfiltered_ids.index(target_id) + 1  # 1-indexed
    assert target_rank > 4, (
        f"target must rank below the naive budget floor (rank > 4); got rank={target_rank}. "
        "Decoy FTS signal is insufficiently dominant — check decoy_blob repetitions."
    )
    assert target_rank <= 200, (
        f"target must be within the widened budget ceiling (rank <= 200); got rank={target_rank}. "
        "Budget model is off — verify handler widening and runtime CANDIDATE_MULTIPLIER."
    )

    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "sfnwidt_probe sfnwidt_signal",
        "tags": ["sfnwidt-target"],
        "limit": 1,
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert len(hits) == 1, (
        "exactly one hit expected (the target note); "
        f"got {len(hits)}: {hits}"
    )
    assert hits[0].get("id") == target_id, (
        f"the tag-filtered target note (unfiltered rank={target_rank}) must be returned; "
        f"got {hits[0]}"
    )
