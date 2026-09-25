"""KG ranking fields and threshold names survive the public facade unchanged."""

from __future__ import annotations

import copy
import json

import pytest

from khive import Khive, Transport


class SearchTransport(Transport):
    def __init__(self, hits):
        self.hits = hits
        self.frames = []

    def round_trip(self, frame, timeout):
        self.frames.append(copy.deepcopy(frame))
        response = {"ok": True, "served_config_id": "search-test-config"}
        if not frame.get("metrics_only"):
            response["result"] = json.dumps(
                {"results": [{"ok": True, "tool": "search", "result": self.hits}]}
            )
        return response


@pytest.mark.parametrize("kind", ["rrf", "vector", "keyword", "weighted", "union"])
@pytest.mark.parametrize(
    "signals", [{}, {"keyword_score": 0.0}, {"vector_similarity": 0.73123456789}]
)
def test_search_preserves_ranking_evidence_and_tied_order(kind, signals):
    hits = [
        {
            "id": "first",
            "rank_score": 0.016393000001,
            "score": 0.016393000001,
            "rank_score_kind": kind,
            "signals": signals,
        },
        {
            "id": "second",
            "rank_score": 0.016393000001,
            "score": 0.016393000001,
            "rank_score_kind": kind,
            "signals": {},
        },
    ]
    transport = SearchTransport(hits)
    result = Khive(transport=transport, actor_id="search-test").search("ranking")

    assert result == hits
    assert [hit["id"] for hit in result] == ["first", "second"]
    assert set(result[0]["signals"]) == set(signals)
    assert result[1]["signals"] == {}
    for hit in result:
        assert hit["score"].hex() == hit["rank_score"].hex()


@pytest.mark.parametrize("floor_name", ["min_rank_score", "min_score"])
@pytest.mark.parametrize("floor", [0.0, 0.0123456789, 1.0])
def test_search_forwards_the_selected_floor_without_renaming(floor_name, floor):
    transport = SearchTransport([])
    db = Khive(transport=transport, actor_id="search-test")

    assert db.search("ranking", kind="note", limit=7, **{floor_name: floor}) == []

    requests = [frame for frame in transport.frames if not frame.get("metrics_only")]
    assert len(requests) == 1
    assert json.loads(requests[0]["ops"]) == [
        {
            "tool": "search",
            "args": {"kind": "note", "query": "ranking", "limit": 7, floor_name: floor},
        }
    ]


@pytest.mark.parametrize(
    "floors",
    [
        {},
        {"min_rank_score": None},
        {"min_score": None},
        {"min_rank_score": None, "min_score": None},
    ],
)
def test_search_without_a_floor_keeps_both_names_absent(floors):
    transport = SearchTransport([])
    Khive(transport=transport, actor_id="search-test").search("ranking", **floors)
    request = next(frame for frame in transport.frames if not frame.get("metrics_only"))
    assert json.loads(request["ops"])[0]["args"] == {"kind": "entity", "query": "ranking"}


@pytest.mark.parametrize("canonical,legacy", [(0.0, 0.0), (0.25, 0.25), (0.25, 0.5)])
def test_search_rejects_both_floor_names_before_any_io(canonical, legacy):
    transport = SearchTransport([])
    db = Khive(transport=transport, actor_id="search-test")
    with pytest.raises(ValueError, match="min_rank_score and min_score"):
        db.search("ranking", min_rank_score=canonical, min_score=legacy)
    assert transport.frames == []
