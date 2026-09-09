"""Thin-method forwarding and failure controls; these use a scripted transport."""

from __future__ import annotations

import copy
import json

import pytest

from khive import Session, Transport
from khive.errors import TransportError


class ScriptedTransport(Transport):
    def __init__(self, response):
        self.response = response
        self.calls = []

    def round_trip(self, frame, timeout):
        if frame.get("metrics_only"):
            return {"ok": True, "served_config_id": "test-config"}
        self.calls.append((copy.deepcopy(frame), timeout))
        assert len(self.calls) == 1, "operation must not be retried"
        if isinstance(self.response, Exception):
            raise self.response
        return {"ok": True, "result": {"results": self.response}}


@pytest.fixture(params=["send", "reply"])
def method(request):
    name = request.param
    required = {"to": "actor:recipient"} if name == "send" else {"id": "01234567"}
    return name, {**required, "content": 'line one\n"λ"'}


@pytest.mark.parametrize("key", [None, "", 'key:"λ"\n'])
def test_comm_methods_forward_exact_arguments_and_preserve_raw_outcome(method, key):
    name, args = method
    outcome = {"ok": True, "tool": f"comm.{name}", "result": {"replayed": True, "future": 1}}
    transport = ScriptedTransport([outcome])
    args |= {"idempotency_key": key, "tags": [], "namespace": "explicit-scope"}
    if name == "send":
        args |= {"subject": "", "thread_id": "canonical-thread", "self_send": False}
    result = getattr(Session(transport, namespace="frame-scope", actor_id="actor:sender"), name)(
        **args, timeout=2.5
    )
    assert result == outcome
    frame, timeout = transport.calls[0]
    assert timeout == 2.5
    assert frame["actor_id"] == "actor:sender" and frame["namespace"] == "frame-scope"
    assert json.loads(frame["ops"]) == [
        {"tool": f"comm.{name}", "args": {k: v for k, v in args.items() if v is not None}}
    ]


@pytest.mark.parametrize("disposition", ["not_committed", "unknown", "committed"])
def test_comm_methods_preserve_operation_errors_without_retry(method, disposition):
    name, args = method
    error = {
        "kind": "conflict",
        "message": "diagnostic",
        "domain_disposition": disposition,
        "details": {"reason": "key_conflict", "key": "stable", "existing_id": "holder"},
        "future_metadata": {"attempt": 1},
    }
    outcome = {"ok": False, "tool": f"comm.{name}", "error": error}
    transport = ScriptedTransport([outcome])
    assert getattr(Session(transport), name)(**args, idempotency_key="stable") == outcome
    assert len(transport.calls) == 1


def test_comm_methods_do_not_retry_transport_failure(method):
    name, args = method
    failure = TransportError("acknowledgement lost")
    transport = ScriptedTransport(failure)
    with pytest.raises(TransportError) as raised:
        getattr(Session(transport), name)(**args, idempotency_key="stable")
    assert raised.value is failure and len(transport.calls) == 1


@pytest.mark.parametrize("shape", ["empty", "multiple", "wrong-tool", "malformed"])
def test_comm_methods_reject_invalid_result_envelopes(method, shape):
    name, args = method
    row = {"ok": True, "tool": f"comm.{name}", "result": {}}
    rows = {
        "empty": [],
        "multiple": [row, row],
        "wrong-tool": [{"ok": True, "tool": "stats", "result": {}}],
        "malformed": [{"ok": False, "tool": f"comm.{name}", "error": {"message": 1}}],
    }[shape]
    transport = ScriptedTransport(rows)
    with pytest.raises(TransportError):
        getattr(Session(transport), name)(**args)
    assert len(transport.calls) == 1
