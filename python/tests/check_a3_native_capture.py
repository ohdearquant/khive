"""Check one fresh native A3 capture through the Python socket client.

Usage: python python/tests/check_a3_native_capture.py --capture /path/to/capture.json

The capture argument is required; missing evidence never becomes a skipped test.
Produce it by setting A3_NATIVE_CAPTURE_PATH for the Rust test
a3_same_committed_failure_crosses_mcp_request_and_native_frame_once. Run this
checker with this checkout's python directory on PYTHONPATH and the Python
development dependencies installed. The peer replays the captured native frame;
it performs no new Rust dispatch or domain write. Native fault and storage proof
belong to the producing Rust test, whose raw frame and storage rows are checked
here alongside the MCP payload and the Python result.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from uuid import UUID

from khive.models import OpResult
from khive.transport import Session

from test_domain_disposition import framed_daemon


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", type=Path, required=True)
    args = parser.parse_args()
    raw = args.capture.read_bytes()
    capture = json.loads(raw)
    assert capture["schema"] == "khive-a3-native-capture-v1"
    assert capture["native_dispatches"] == capture["audit_rejections"] == 1

    frame = json.loads(capture["daemon_response_json"])
    assert frame["ok"] is True
    assert frame.get("error") is None
    assert frame.get("error_detail") is None
    assert frame["daemon_protocol_version"] == 4
    assert frame["result"] == capture["dispatch_payload"] == capture["mcp_payload"]
    envelope = json.loads(capture["mcp_payload"])
    assert len(envelope["results"]) == 1
    entry = envelope["results"][0]
    assert entry["ok"] is False and entry["tool"] == "comm.send"
    expected = entry["error"]
    assert expected["kind"] == "obligation"
    assert expected["code"] == "store_failure"
    assert expected["domain_disposition"] == "committed"
    full_id = expected["domain_result"]["full_id"]
    assert str(UUID(full_id)) == full_id
    assert expected["domain_result"]["thread_id"] == full_id

    notes = capture["physical_notes"]
    ids = capture["physical_note_ids"]
    assert len(notes) == len(ids) == len(set(ids)) == 2
    assert {note["id"] for note in notes} == set(ids)
    assert ids[0] == full_id
    outbound = next(note for note in notes if note["id"] == full_id)
    inbound = next(note for note in notes if note["id"] != full_id)
    assert outbound["properties"]["direction"] == "outbound"
    assert "outbound_ref" not in outbound["properties"]
    assert inbound["properties"]["direction"] == "inbound"
    assert inbound["properties"]["outbound_ref"] == full_id
    for note in notes:
        assert note["kind"] == "message" and note["namespace"] == "local"
        assert note["content"] == "a3-identical-replay"
        props = note["properties"]
        assert props["thread_id"] == full_id
        assert props["from"] == props["to"] == "local"
        assert props["from_actor"] == props["to_actor"] == "local"
        assert props["read"] is False

    request_ops = capture["request_ops"]
    assert capture["daemon_request"]["ops"] == request_ops
    assert capture["daemon_request"]["from_wire"] is True
    assert frame["request_id"] == capture["daemon_request"]["request_id"]
    # The handshake is synthetic; the operation reply is the original native
    # JSON bytes, length-framed without reserializing its captured error object.
    with framed_daemon(
        frame,
        config_id=frame["served_config_id"],
        raw_response=capture["daemon_response_json"].encode("utf-8"),
    ) as (transport, requests):
        results = Session(transport).request(request_ops)
        assert len(results) == 1
        actual = OpResult.model_validate(results[0])
        assert actual.ok is False and actual.tool == "comm.send"
        assert actual.error is not None
        assert actual.error.model_dump(exclude_unset=True) == expected
        assert len(requests) == 2, "one handshake and one operation, with no retry"
        assert requests[0]["metrics_only"] is True
        assert requests[1]["ops"] == request_ops
        assert requests[1]["config_id"] == frame["served_config_id"]
        assert requests[1]["protocol_version"] == 4

    canonical = json.dumps(expected, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    print(json.dumps({
        "ok": True,
        "mode": "captured native frame replay through Python SocketTransport and Session",
        "capture_sha256": hashlib.sha256(raw).hexdigest(),
        "error_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
        "full_id": full_id,
        "native_dispatches_in_capture": 1,
        "physical_notes_in_capture": 2,
        "python_handshakes": 1,
        "python_operation_requests": 1,
        "new_native_dispatches": 0,
    }))


if __name__ == "__main__":
    main()
