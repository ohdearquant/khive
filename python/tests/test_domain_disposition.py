"""Disposition preservation across actual daemon socket frames."""

from __future__ import annotations

import json
import socket
import struct
import tempfile
import threading
from contextlib import contextmanager
from pathlib import Path

import pytest

from khive import Khive, op
from khive.envelope import _validate_envelope_results, _validate_op_errors
from khive.errors import ProtocolMismatch, RequestRejected, TransportError
from khive.models import OpError
from khive.transport import PROTOCOL_VERSION, Session, SocketTransport


def obligation_error():
    return {
        "kind": "obligation",
        "code": "store_failure",
        "message": "audit failed",
        "domain_disposition": "committed",
        "domain_result": {"id": "persisted-row", "nested": [None, {"value": 1}]},
    }


@contextmanager
def framed_daemon(response, *, config_id="test-config", raw_response=None):
    requests = []
    failures = []
    with tempfile.TemporaryDirectory(prefix="khive-a3-", dir="/tmp") as directory:
        path = Path(directory) / "daemon.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        listener.listen()
        listener.settimeout(2)

        def serve():
            try:
                for index, reply in enumerate([
                    {"ok": True, "served_config_id": config_id, "daemon_protocol_version": PROTOCOL_VERSION},
                    response,
                ]):
                    with listener.accept()[0] as connection:
                        connection.settimeout(2)
                        length = struct.unpack(">I", SocketTransport._read_exact(connection, 4))[0]
                        request = json.loads(SocketTransport._read_exact(connection, length))
                        requests.append(request)
                        if reply is None:
                            continue
                        payload = (
                            raw_response
                            if index == 1 and raw_response is not None
                            else json.dumps(reply).encode()
                        )
                        connection.sendall(struct.pack(">I", len(payload)) + payload)
            except Exception as error:
                failures.append(error)
            finally:
                listener.close()

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            yield SocketTransport(path), requests
        finally:
            worker.join(timeout=3)
            listener.close()
            assert not worker.is_alive(), "fixture did not receive the expected frames"
            assert not failures, failures


@pytest.mark.parametrize("disposition", ["committed", "not_committed", "unknown"])
def test_daemon_error_detail_is_exposed_without_replaying(disposition):
    detail = obligation_error()
    detail["domain_disposition"] = disposition
    if disposition != "committed":
        detail.pop("domain_result")
    response = {"ok": False, "error": "audit failed", "error_detail": detail}
    with framed_daemon(response) as (transport, requests):
        with pytest.raises(RequestRejected) as raised:
            Session(transport).request('create(kind="note", content="once")')
        assert str(raised.value) == "audit failed"
        assert raised.value.error_detail.model_dump(exclude_unset=True) == detail
        assert len(requests) == 2
        assert requests[0]["metrics_only"] is True
        assert requests[1]["ops"] == 'create(kind="note", content="once")'
        assert requests[1]["protocol_version"] == PROTOCOL_VERSION


def test_legacy_text_error_is_accepted_without_inventing_a_disposition_or_replay():
    with framed_daemon({"ok": False, "error": "legacy failure"}) as (transport, requests):
        with pytest.raises(RequestRejected) as raised:
            Session(transport).request("create()")
        assert str(raised.value) == "legacy failure"
        assert raised.value.error_detail is None
        assert len(requests) == 2


@pytest.mark.parametrize("detail", [None, obligation_error()])
def test_version_mismatch_remains_unknown_without_replaying(detail):
    response = {
        "ok": False,
        "version_mismatch": True,
        "daemon_protocol_version": 3,
        "error": "version mismatch",
    }
    if detail is not None:
        response["error_detail"] = detail
    with framed_daemon(response) as (transport, requests):
        with pytest.raises(ProtocolMismatch) as raised:
            Session(transport).request("create()")
        error = raised.value.error_detail
        assert error.domain_disposition == "unknown"
        assert "domain_result" not in error.model_fields_set
        assert (raised.value.client_version, raised.value.daemon_version) == (PROTOCOL_VERSION, 3)
        assert len(requests) == 2


def test_native_client_preserves_per_op_detail_and_aborted_entry_without_replay():
    detail = obligation_error()
    entries = [
        {"ok": False, "tool": "create", "error": detail},
        {"ok": False, "aborted": True, "domain_disposition": "not_committed"},
    ]
    response = {"ok": True, "result": json.dumps({"results": entries})}
    with framed_daemon(response) as (transport, requests):
        results = Khive(transport=transport).raw([op("create"), op("get")])
        assert results[0].error.model_dump(exclude_unset=True) == detail
        assert results[1].domain_disposition == "not_committed"
        assert results[1].aborted is True
        assert results[1].tool == ""
        assert len(requests) == 2


@pytest.mark.parametrize("value", [None, "retryable", True, 1])
def test_invalid_disposition_is_refused_in_error_and_aborted_entry(value):
    error = {"message": "failure", "domain_disposition": value}
    with pytest.raises(TransportError, match="malformed"):
        _validate_op_errors({"results": [{"ok": False, "tool": "create", "error": error}]}, "test")
    with pytest.raises(TransportError, match="malformed"):
        _validate_envelope_results(
            {"results": [{"ok": False, "aborted": True, "domain_disposition": value}]}, "test"
        )


def test_success_and_legacy_models_gain_no_supplied_disposition():
    envelope = {"results": [{"ok": True, "tool": "stats", "result": {"count": 1}}]}
    assert _validate_envelope_results(envelope, "test") == envelope
    legacy = OpError.model_validate({"message": "legacy failure"})
    assert legacy.model_dump(exclude_unset=True) == {"message": "legacy failure"}
    assert legacy.domain_disposition is None


def test_malformed_daemon_detail_does_not_trigger_replay():
    response = {"ok": False, "error": "failure", "error_detail": {"message": 1}}
    with framed_daemon(response) as (transport, requests):
        with pytest.raises(TransportError, match="malformed error_detail"):
            Session(transport).request("create()")
        assert len(requests) == 2


def test_lost_response_after_complete_request_does_not_trigger_replay():
    with framed_daemon(None) as (transport, requests):
        with pytest.raises(TransportError, match="connection closed"):
            Session(transport).request("create()")
        assert len(requests) == 2
