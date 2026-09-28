"""Verbose KG search ranking/evidence contract (ADR-130 Release N).

ADR: ADR-130 (search response completeness and ranking evidence)
section: 2. Ranking and evidence fields; 3. Threshold rule
"""

from __future__ import annotations

import copy

import pytest

from khive_contract.client import KhiveMcpSession, error_text
from khive_contract.schema import assert_search_response

VERBS_UNDER_TEST = {"create", "search"}

HIT = {
    "id": "00000000-0000-4000-8000-000000000001",
    "rank_score": 0.016393000001,
    "rank_score_kind": "rrf",
    "signals": {},
    "score": 0.016393000001,
}


@pytest.mark.parametrize("kind", ["rrf", "vector", "keyword", "weighted", "union"])
@pytest.mark.parametrize("signals", [{}, {"keyword_score": 0.0}, {"vector_similarity": 0.7}])
def test_search_schema_accepts_kinds_and_only_present_evidence(kind, signals):
    hits = [{**HIT, "rank_score_kind": kind, "signals": signals}]
    before = copy.deepcopy(hits)
    assert_search_response(hits)
    assert hits == before


@pytest.mark.parametrize("missing", ["id", "rank_score", "rank_score_kind", "signals", "score"])
def test_search_schema_requires_all_compatibility_fields(missing):
    hit = dict(HIT)
    del hit[missing]
    with pytest.raises(AssertionError, match="Schema validation failed"):
        assert_search_response([hit])


@pytest.mark.parametrize(
    "patch",
    [
        {"rank_score": None},
        {"score": None},
        {"rank_score_kind": "similarity"},
        {"signals": None},
        {"signals": []},
        {"signals": {"keyword_score": None}},
        {"signals": {"vector_similarity": "0.7"}},
    ],
)
def test_search_schema_refuses_malformed_ranking_fields(patch):
    with pytest.raises(AssertionError, match="Schema validation failed"):
        assert_search_response([{**HIT, **patch}])


@pytest.mark.parametrize("rank_score,score", [(0.016393000001, 0.016393000002), (0.0, -0.0)])
def test_search_schema_refuses_inexact_deprecated_alias(rank_score, score):
    with pytest.raises(AssertionError, match="exactly equal rank_score"):
        assert_search_response([{**HIT, "rank_score": rank_score, "score": score}])


def test_search_schema_keeps_empty_results_and_equal_adjacent_ranks_valid():
    assert_search_response([])
    assert_search_response([HIT, {**HIT, "id": "00000000-0000-4000-8000-000000000002"}])


@pytest.mark.slow
@pytest.mark.parametrize("kind", ["entity", "note"])
def test_live_fts_search_retains_keyword_evidence_without_vector_evidence(
    khive_session: KhiveMcpSession, temp_namespace: str, kind: str
):
    query = "rankingcontractneedle"
    create = {"namespace": temp_namespace, "kind": "concept" if kind == "entity" else "observation"}
    if kind == "entity":
        create.update(name=query, description="ranking evidence fixture", skip_dedup_check=True)
    else:
        create["content"] = f"{query} ranking evidence fixture"
    record = khive_session.verb("create", create)
    args = {"namespace": temp_namespace, "kind": kind, "query": query}
    hits = khive_session.verb("search", args)
    assert_search_response(hits)
    assert [hit["id"] for hit in hits] == [record["id"]]
    assert hits[0]["rank_score_kind"] == "rrf"
    assert "keyword_score" in hits[0]["signals"]
    assert "vector_similarity" not in hits[0]["signals"]
    for floor_name in ["min_rank_score", "min_score"]:
        assert khive_session.verb("search", {**args, floor_name: 0.0}) == hits


@pytest.mark.slow
@pytest.mark.parametrize("kind", ["entity", "note"])
@pytest.mark.parametrize("canonical,legacy", [(0.0, 0.0), (0.25, 0.25), (0.25, 0.5)])
def test_live_search_refuses_both_threshold_names_even_when_equal(
    khive_session: KhiveMcpSession, temp_namespace: str, kind: str, canonical, legacy
):
    envelope = khive_session.request_batch(
        [
            {
                "tool": "search",
                "args": {
                    "kind": kind,
                    "query": "rankingcontractneedle",
                    "namespace": temp_namespace,
                    "min_rank_score": canonical,
                    "min_score": legacy,
                },
            }
        ]
    )
    assert len(envelope["results"]) == 1
    result = envelope["results"][0]
    assert result["ok"] is False
    assert "result" not in result
    message = error_text(result)
    assert "min_rank_score" in message and "min_score" in message
