"""Typed recall outcomes: the envelope class survives the client boundary.

Scripted transport, no daemon. The server answers `memory.recall` with a bare
row array, or with an object envelope when an empty answer needs a reason
(`degraded`, `truncated`, or both). The control at the end is the red
mutation the packet names: a rows-only conversion makes a degraded empty
recall equal to a clean empty one, and that assertion fails.
"""

from __future__ import annotations

import copy
import json

import pytest

from khive import RecallHit, RecallOutcome, Session, Transport
from khive.errors import OperationError, TransportError

HIT_ID = "00000000-0000-4000-8000-000000000011"
CLEAN_ROW = {
    "id": HIT_ID,
    "full_id": HIT_ID,
    "score": 0.81,
    "rank_score": 0.64,
    "raw_score": 0.81,
    "content": "a remembered line",
    "salience": 0.5,
    "decay_factor": 0.01,
    "memory_type": "semantic",
    "created_at": "2026-09-08T12:00:00Z",
    "serve_attribution": "default",
}
DEGRADED_ROW = CLEAN_ROW | {
    "degraded": "ann_unavailable",
    "degraded_reason": "ann index unavailable for model test-model",
}
TRUNCATED_ROW = CLEAN_ROW | {"truncated": True}
CLEAN_EMPTY: list = []
DEGRADED_EMPTY = {
    "results": [],
    "degraded": True,
    "degraded_reason": "ann index unavailable for model test-model",
}
TRUNCATED_EMPTY = {"results": [], "truncated": True}
BOTH_EMPTY = {
    "results": [],
    "truncated": True,
    "degraded": True,
    "degraded_reason": "ann index unavailable for model test-model",
}


class ScriptedTransport(Transport):
    def __init__(self, responses):
        self.responses = list(responses)
        self.dispatches = []

    def round_trip(self, frame, timeout):
        if frame.get("metrics_only"):
            return {"ok": True, "served_config_id": "test-config"}
        self.dispatches.append((copy.deepcopy(frame), timeout))
        ops = json.loads(frame["ops"])
        assert len(ops) == 1 and ops[0]["tool"] == "memory.recall"
        assert self.responses, "unexpected additional operation dispatch"
        response = self.responses.pop(0)
        return {"ok": True, "result": json.dumps({"results": [response]})}


def _entry(result):
    return {"ok": True, "tool": "memory.recall", "result": result}


def _session(*results) -> tuple[Session, ScriptedTransport]:
    transport = ScriptedTransport([_entry(r) for r in results])
    return Session(transport, namespace="frame-only", actor_id="test-client"), transport


@pytest.mark.parametrize(
    ("payload", "expected"),
    [
        (CLEAN_EMPTY, ("clean_empty", False, False, False, 0)),
        (DEGRADED_EMPTY, ("degraded_empty", True, False, True, 0)),
        (TRUNCATED_EMPTY, ("truncated_empty", False, True, True, 0)),
        (BOTH_EMPTY, ("degraded_truncated_empty", True, True, True, 0)),
        ([CLEAN_ROW], ("clean_rows", False, False, False, 1)),
        ([DEGRADED_ROW], ("degraded_rows", True, False, False, 1)),
        ([TRUNCATED_ROW], ("truncated_rows", False, True, False, 1)),
        ([DEGRADED_ROW | {"truncated": True}], ("degraded_truncated_rows", True, True, False, 1)),
    ],
)
def test_every_response_class_keeps_its_shape(payload, expected):
    label, degraded, truncated, enveloped, count = expected
    outcome = RecallOutcome.from_result(copy.deepcopy(payload))
    assert outcome.envelope_class == label
    assert (outcome.degraded, outcome.truncated, outcome.enveloped) == (
        degraded,
        truncated,
        enveloped,
    )
    assert len(outcome.hits) == count
    if degraded:
        assert outcome.degraded_reason == "ann index unavailable for model test-model"
    else:
        assert outcome.degraded_reason is None


def test_degraded_row_and_clean_row_decode_to_different_values():
    clean = RecallHit.model_validate(CLEAN_ROW)
    degraded = RecallHit.model_validate(DEGRADED_ROW)
    assert clean.id == degraded.id == HIT_ID
    assert clean.score == degraded.score == 0.81
    assert not clean.is_degraded and degraded.is_degraded
    assert clean != degraded
    assert clean.model_dump().get("degraded") is None
    assert degraded.degraded == "ann_unavailable"


def test_session_recall_forwards_arguments_and_returns_typed_outcome():
    session, transport = _session([DEGRADED_ROW])
    outcome = session.recall(
        "a remembered line",
        limit=3,
        tags=["lesson"],
        exclude_tags=["superseded"],
        memory_type="semantic",
        namespace="caller-pinned",
        timeout=2.5,
    )
    assert isinstance(outcome, RecallOutcome)
    assert outcome.envelope_class == "degraded_rows"
    assert outcome.hits[0].id == HIT_ID and outcome.hits[0].is_degraded
    frame, timeout = transport.dispatches[0]
    assert timeout == 2.5
    assert json.loads(frame["ops"]) == [
        {
            "tool": "memory.recall",
            "args": {
                "query": "a remembered line",
                "limit": 3,
                "memory_type": "semantic",
                "tags": ["lesson"],
                "exclude_tags": ["superseded"],
                "namespace": "caller-pinned",
            },
        }
    ]
    assert not transport.responses


def test_session_recall_raises_operation_error_with_the_server_error_object():
    transport = ScriptedTransport(
        [
            {
                "ok": False,
                "tool": "memory.recall",
                "error": {"kind": "invalid_params", "message": "query is required"},
            }
        ]
    )
    session = Session(transport, namespace="frame-only", actor_id="test-client")
    with pytest.raises(OperationError) as raised:
        session.recall("")
    assert raised.value.tool == "memory.recall"
    assert raised.value.error == {"kind": "invalid_params", "message": "query is required"}


@pytest.mark.parametrize(
    "payload",
    [
        {"rows": []},
        "not a recall result",
        {"results": [], "degraded": "yes"},
        {"results": [], "truncated": 1},
        [{"score": 0.5}],
    ],
)
def test_malformed_recall_results_are_transport_errors(payload):
    session, _ = _session(payload)
    with pytest.raises(TransportError):
        session.recall("anything")


def test_rows_only_conversion_is_the_red_mutation():
    # A conversion that keeps only the rows cannot tell a degraded empty recall
    # from a clean one. The typed outcome must; if it collapsed to rows, these
    # two values would compare equal and this test would fail.
    degraded = RecallOutcome.from_result(copy.deepcopy(DEGRADED_EMPTY))
    clean = RecallOutcome.from_result(copy.deepcopy(CLEAN_EMPTY))
    assert degraded.hits == clean.hits == []
    assert degraded != clean
    assert (degraded.degraded, clean.degraded) == (True, False)


def test_live_recall_returns_a_typed_outcome(scratch_daemon):
    from khive import SocketTransport

    session = Session(
        SocketTransport(scratch_daemon["socket"]), namespace="local", actor_id="test-client"
    )
    remembered = session.remember("typed recall outcome live probe", memory_type="semantic")
    assert remembered["ok"], remembered
    outcome = session.recall("typed recall outcome live probe", limit=5)
    assert isinstance(outcome, RecallOutcome)
    assert isinstance(outcome.degraded, bool) and isinstance(outcome.truncated, bool)
    if outcome.hits:
        assert all(isinstance(hit, RecallHit) for hit in outcome.hits)
        assert any(hit.id == remembered["result"]["id"] for hit in outcome.hits)
    else:
        # An empty live answer must say why, or be a clean empty.
        assert outcome.envelope_class in {"clean_empty", "degraded_empty"}
