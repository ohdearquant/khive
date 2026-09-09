"""Scripted transport controls for Session.remember; no daemon is started."""

from __future__ import annotations

import copy
import json

import pytest

from khive import Session, Transport
from khive.errors import TransportError

MEMORY_ID = "00000000-0000-4000-8000-000000000001"
SOURCE_ID = "00000000-0000-4000-8000-000000000002"
KEY = 'turn-1/"remember"/\u03bb'
NAMESPACE = "caller-pinned"
RECEIPT = {
    "id": MEMORY_ID,
    "kind": "memory",
    "salience": 0.7,
    "decay_factor": 0.95,
    "memory_type": "episodic",
    "created_at": "2026-09-08T12:00:00Z",
}
SUCCESS = {"ok": True, "tool": "memory.remember", "result": RECEIPT}
KEY_CONFLICT = {
    "kind": "conflict",
    "message": "memory key already held",
    "details": {"reason": "key_conflict", "key": KEY, "existing_id": MEMORY_ID},
    "domain_disposition": "not_committed",
}


class ScriptedTransport(Transport):
    def __init__(self, responses):
        self.responses = list(responses)
        self.handshakes = []
        self.dispatches = []

    def round_trip(self, frame, timeout):
        call = (copy.deepcopy(frame), timeout)
        if frame.get("metrics_only"):
            self.handshakes.append(call)
            return {"ok": True, "served_config_id": "test-config"}
        self.dispatches.append(call)
        ops = json.loads(frame["ops"])
        assert len(ops) == 1 and ops[0]["tool"] == "memory.remember"
        assert self.responses, "unexpected additional operation dispatch"
        response = self.responses.pop(0)
        if isinstance(response, Exception):
            raise response
        return {"ok": True, "result": json.dumps({"results": response})}


def test_remember_forwards_arguments_and_transport_timeout():
    transport = ScriptedTransport([[SUCCESS]])
    session = Session(transport, namespace="frame-only", actor_id="test-client", timeout=30.0)
    arguments = {
        "content": 'line one\nline "two"',
        "key": KEY,
        "memory_type": "episodic",
        "salience": 0.0,
        "decay_factor": 0.0,
        "source_id": SOURCE_ID,
        "tags": [],
        "embedding_model": "test-model",
        "namespace": NAMESPACE,
    }

    result = session.remember(**arguments, timeout=2.5)

    assert isinstance(result, dict) and result == SUCCESS
    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert transport.handshakes[0][1] == 30.0
    frame, timeout = transport.dispatches[0]
    assert timeout == 2.5
    assert json.loads(frame["ops"]) == [{"tool": "memory.remember", "args": arguments}]
    assert frame["namespace"] == "frame-only"
    assert frame["actor_id"] == "test-client"
    assert frame["presentation"] == "verbose" and frame["format"] == "json"
    assert not transport.responses


@pytest.mark.parametrize("memory_type", [None, "episodic", "semantic"])
@pytest.mark.parametrize("key", [None, ""])
def test_remember_omits_none_but_preserves_empty_key_without_inferred_namespace(memory_type, key):
    transport = ScriptedTransport([[SUCCESS]])
    session = Session(transport, namespace="frame-only", actor_id="test-client", timeout=9.0)

    assert session.remember("memory", key=key, memory_type=memory_type) == SUCCESS

    expected = {"content": "memory"}
    if key is not None:
        expected["key"] = key
    if memory_type is not None:
        expected["memory_type"] = memory_type
    frame, timeout = transport.dispatches[0]
    assert json.loads(frame["ops"]) == [{"tool": "memory.remember", "args": expected}]
    assert timeout == 9.0
    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert not transport.responses


@pytest.mark.parametrize(
    "error",
    [
        KEY_CONFLICT,
        "legacy error",
        {
            "kind": "storage",
            "message": "write outcome unknown",
            "retryable": True,
            "domain_disposition": "unknown",
            "future_metadata": {"attempt": 1},
        },
    ],
)
def test_remember_preserves_operation_errors_without_retry(error):
    entry = {"ok": False, "tool": "memory.remember", "error": error}
    transport = ScriptedTransport([[entry]])

    result = Session(transport).remember("memory", key=KEY, namespace=NAMESPACE)

    assert result == entry
    assert result["error"] == error
    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert not transport.responses


def test_remember_committed_error_then_conflict_requires_two_explicit_calls():
    committed = {
        "ok": False,
        "tool": "memory.remember",
        "error": {
            "kind": "obligation",
            "code": "store_failure",
            "message": "audit obligation failed after domain dispatch",
            "domain_disposition": "committed",
            "domain_result": RECEIPT,
        },
    }
    conflict = {"ok": False, "tool": "memory.remember", "error": KEY_CONFLICT}
    transport = ScriptedTransport([[committed], [conflict]])
    session = Session(transport, namespace="frame-only", actor_id="test-client")

    first = session.remember("memory", key=KEY, memory_type="episodic", namespace=NAMESPACE)
    assert first == committed
    assert first["ok"] is False
    assert first["error"]["domain_result"]["id"] == MEMORY_ID
    assert len(transport.dispatches) == 1
    assert len(transport.responses) == 1

    second = session.remember("memory", key=KEY, memory_type="episodic", namespace=NAMESPACE)
    assert second == conflict
    assert second["ok"] is False
    assert second["error"]["details"]["existing_id"] == MEMORY_ID
    assert second["error"]["domain_disposition"] == "not_committed"
    assert len(transport.dispatches) == 2
    assert len(transport.handshakes) == 1
    assert not transport.responses
    first_ops, second_ops = [json.loads(frame["ops"]) for frame, _ in transport.dispatches]
    assert (
        first_ops
        == second_ops
        == [
            {
                "tool": "memory.remember",
                "args": {
                    "content": "memory",
                    "key": KEY,
                    "memory_type": "episodic",
                    "namespace": NAMESPACE,
                },
            }
        ]
    )


def test_remember_does_not_retry_a_transport_failure():
    failure = TransportError("connection closed before the response arrived")
    transport = ScriptedTransport([failure])

    with pytest.raises(TransportError) as raised:
        Session(transport).remember("memory", key=KEY, namespace=NAMESPACE)

    assert raised.value is failure
    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert not transport.responses


@pytest.mark.parametrize(
    "entries",
    [[], [SUCCESS, SUCCESS], [{"ok": True, "tool": "stats", "result": {}}]],
)
def test_remember_refuses_a_response_that_is_not_its_singleton_result(entries):
    transport = ScriptedTransport([entries])

    with pytest.raises(TransportError, match="single memory.remember result"):
        Session(transport).remember("memory", key=KEY)

    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert not transport.responses


def test_remember_keeps_existing_envelope_validation():
    transport = ScriptedTransport(
        [[{"ok": False, "tool": "memory.remember", "error": {"message": 1}}]]
    )

    with pytest.raises(TransportError, match="malformed result"):
        Session(transport).remember("memory", key=KEY)

    assert len(transport.handshakes) == len(transport.dispatches) == 1
    assert not transport.responses
