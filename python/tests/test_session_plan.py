"""Plan frames and decoded results through a daemon stub; no database or socket."""

from __future__ import annotations

from copy import deepcopy
import json

import pytest

from khive import envelope
from khive.errors import ConfigMismatch, ProtocolMismatch, RequestRejected, TransportError
from khive.transport import PROTOCOL_VERSION, Session, Transport


LIMITS = {"max_ops": 100, "max_depth": 64, "max_input_len": 1048576}
OPS = 'create(kind="note", content="sample") | get(id=$prev.id) | get(id=$prev.id)'
PLAN = {
    "parsed": True,
    "mode": "chain",
    "stage_count": 3,
    "stages": [
        {
            "index": 0,
            "verb": "create",
            "pack": "kg",
            "known": True,
            "args": {"kind": "note", "content": "sample"},
            "prev_refs": [],
        },
        {
            "index": 1,
            "verb": "get",
            "pack": "kg",
            "known": True,
            "args": {"id": "$prev.id"},
            "prev_refs": ["id"],
        },
        {
            "index": 2,
            "verb": "get",
            "pack": "kg",
            "known": True,
            "args": {"id": "$prev.id"},
            "prev_refs": ["id"],
        },
    ],
    "limits": LIMITS,
}
PARSE_ERROR = {"parsed": False, "error": "unexpected end of input", "limits": LIMITS}


class DaemonStub(Transport):
    def __init__(self, payload=PLAN, *, encoded=True, version=5, mismatches=0, rejection=None):
        self.payload = payload
        self.encoded = encoded
        self.version = version
        self.mismatches = mismatches
        self.rejection = rejection
        self.frames = []
        self.timeouts = []
        self.dispatched_ops = []

    def round_trip(self, frame, timeout):
        self.frames.append(deepcopy(frame))
        self.timeouts.append(timeout)
        response = {"ok": True, "served_config_id": "catalog-config"}
        if frame["protocol_version"] != self.version:
            return {
                "ok": False,
                "version_mismatch": True,
                "daemon_protocol_version": self.version,
                "error": "protocol version mismatch",
            }
        if frame.get("metrics_only"):
            return response
        if self.mismatches:
            self.mismatches -= 1
            return {"ok": False, "config_mismatch": True, "error": "config changed"}
        if self.rejection is not None:
            return {"ok": False, "error": self.rejection}
        if frame.get("plan"):
            response["result"] = json.dumps(self.payload) if self.encoded else self.payload
        else:
            self.dispatched_ops.append(frame["ops"])
            response["result"] = json.dumps({"results": [{"ok": True, "tool": "stats"}]})
        return response


def assert_no_identity_or_rendering(frames):
    excluded = {
        "actor_id", "process_ref", "visible_namespaces", "request_id",
        "presentation", "presentation_per_op", "format", "format_per_op", "save_to",
    }
    for frame in frames:
        assert excluded.isdisjoint(frame)
        assert frame["namespace"] == ""


def test_plan_protocol_version_refuses_daemons_that_cannot_plan():
    assert PROTOCOL_VERSION == 6


@pytest.mark.parametrize("encoded", [True, False])
def test_plan_sends_isolated_frame_and_preserves_the_complete_result(encoded):
    transport = DaemonStub(encoded=encoded)
    session = Session(
        transport, actor_id="test-client", namespace="project", visible_namespaces=["shared"]
    )
    assert session.plan(OPS) == PLAN
    assert transport.frames == [
        {
            "ops": "", "namespace": "", "config_id": "", "protocol_version": 6,
            "metrics_only": True,
        },
        {
            "ops": OPS, "namespace": "", "config_id": "catalog-config",
            "protocol_version": 6, "plan": True,
        },
    ]
    assert_no_identity_or_rendering(transport.frames)
    assert transport.dispatched_ops == []


def test_plan_parse_failure_is_a_successful_decoded_result():
    transport = DaemonStub(PARSE_ERROR)
    result = Session(transport).plan("create(")
    assert result == PARSE_ERROR
    assert "stages" not in result
    assert transport.dispatched_ops == []


def test_plan_unknown_verb_and_null_pack_are_preserved():
    payload = deepcopy(PLAN)
    payload.update(mode="single", stage_count=1)
    payload["stages"] = [{
        "index": 0, "verb": "unknown.call", "pack": None,
        "known": False, "args": {}, "prev_refs": [],
    }]
    assert Session(DaemonStub(payload)).plan("unknown.call()") == payload


def test_plan_preserves_new_server_metadata_without_normalization():
    payload = deepcopy(PLAN)
    payload["future_metadata"] = {"catalog_revision": "example"}
    payload["stages"][0]["future_metadata"] = [1, None, False]
    assert Session(DaemonStub(payload, encoded=False)).plan(OPS) is payload


def test_plan_timeout_override_applies_to_the_plan_exchange():
    transport = DaemonStub()
    session = Session(transport, timeout=12.0)
    session.plan(OPS, timeout=3.0)
    session.plan(OPS)
    assert transport.timeouts == [12.0, 3.0, 12.0]
    assert len([frame for frame in transport.frames if frame.get("metrics_only")]) == 1


def test_plan_rehandshakes_once_on_config_change_without_identity():
    transport = DaemonStub(mismatches=1)
    session = Session(transport, actor_id="test-client")
    session._config_id = "stale-config"
    assert session.plan(OPS) == PLAN
    assert [frame.get("metrics_only", False) for frame in transport.frames] == [False, True, False]
    assert transport.frames[-1]["config_id"] == "catalog-config"
    assert_no_identity_or_rendering(transport.frames)
    assert transport.dispatched_ops == []


def test_plan_persistent_config_mismatch_is_not_retried_again():
    transport = DaemonStub(mismatches=2)
    session = Session(transport)
    session._config_id = "stale-config"
    with pytest.raises(ConfigMismatch, match="config changed"):
        session.plan(OPS)
    assert len(transport.frames) == 3
    assert_no_identity_or_rendering(transport.frames)
    assert transport.dispatched_ops == []


@pytest.mark.parametrize("handshaken", [True, False])
def test_previous_protocol_rejects_plan_without_dispatch(handshaken):
    transport = DaemonStub(version=4)
    session = Session(transport)
    if handshaken:
        session._config_id = "catalog-config"
    with pytest.raises(ProtocolMismatch) as raised:
        session.plan(OPS)
    assert raised.value.client_version == 5
    assert raised.value.daemon_version == 4
    assert len(transport.frames) == 1
    assert transport.frames[0].get("plan", False) is handshaken
    assert transport.dispatched_ops == []


def test_plan_outer_rejection_remains_request_rejected():
    transport = DaemonStub(rejection="invalid_params: presentation cannot be combined with plan")
    with pytest.raises(RequestRejected, match="invalid_params: presentation"):
        Session(transport).plan(OPS)


def test_plan_malformed_json_is_a_transport_error():
    with pytest.raises(TransportError, match="malformed JSON body from daemon"):
        Session(DaemonStub("{not json", encoded=False)).plan(OPS)


@pytest.mark.parametrize(
    "field",
    ["presentation", "presentation_per_op", "format", "format_per_op", "save_to", "request_id"],
)
def test_plan_api_cannot_add_dispatch_envelope_fields(field):
    transport = DaemonStub()
    with pytest.raises(TypeError, match=field):
        Session(transport).plan(OPS, **{field: "value"})
    assert transport.frames == []


def test_request_retains_dispatch_frame_and_per_operation_result_list():
    transport = DaemonStub(version=PROTOCOL_VERSION)
    session = Session(
        transport, actor_id="test-client", namespace="project", visible_namespaces=["shared"]
    )
    assert session.request("stats()") == [{"ok": True, "tool": "stats"}]
    assert transport.frames[-1] == {
        "ops": "stats()", "presentation": "verbose", "format": "json",
        "namespace": "project", "actor_id": "test-client", "visible_namespaces": ["shared"],
        "config_id": "catalog-config", "protocol_version": PROTOCOL_VERSION, "from_wire": False,
    }
    assert transport.dispatched_ops == ["stats()"]


@pytest.mark.parametrize("payload", [PLAN, PARSE_ERROR])
def test_plan_envelope_validator_preserves_decoded_objects(payload):
    assert envelope._plan_from_payload(payload, "test-daemon") is payload


def malformed_plans():
    yield None
    yield []
    yield {"results": []}
    yield {"parsed": "false", "error": "bad syntax", "limits": LIMITS}
    yield {"parsed": False, "limits": LIMITS}
    yield {"parsed": False, "error": 42, "limits": LIMITS}
    yield PARSE_ERROR | {"stages": []}
    yield PLAN | {"parsed": 1}
    yield PLAN | {"mode": "unknown"}
    yield PLAN | {"stage_count": True}
    yield PLAN | {"stage_count": 4}
    yield PLAN | {"stages": None}
    yield {key: value for key, value in PLAN.items() if key != "limits"}
    yield PLAN | {"limits": {"max_ops": 100, "max_depth": 64}}
    yield PLAN | {"limits": LIMITS | {"max_depth": "64"}}
    yield PLAN | {"limits": LIMITS | {"max_input_len": False}}
    yield PLAN | {"limits": LIMITS | {"max_ops": -1}}
    for field, value in [
        ("index", True), ("index", 2), ("verb", 1), ("pack", 1),
        ("known", "true"), ("args", []), ("prev_refs", "id"), ("prev_refs", [1]),
    ]:
        payload = deepcopy(PLAN)
        payload["stages"][0][field] = value
        yield payload
    payload = deepcopy(PLAN)
    del payload["stages"][0]["pack"]
    yield payload
    payload = deepcopy(PLAN)
    del payload["stages"][0]["verb"]
    yield payload


@pytest.mark.parametrize("payload", list(malformed_plans()))
def test_malformed_plan_shapes_are_transport_errors(payload):
    with pytest.raises(TransportError, match="daemon.*plan"):
        Session(DaemonStub(payload)).plan(OPS)


def test_plan_envelope_rejection_names_the_origin():
    with pytest.raises(TransportError, match="test-daemon.*plan"):
        envelope._plan_from_payload({"results": []}, "test-daemon")


@pytest.mark.parametrize("config_mismatch", [False, True])
def test_plan_preserves_frame_error_detail_without_dispatch(config_mismatch):
    detail = {"kind": "protocol", "message": "plan refused", "domain_disposition": "not_committed"}

    class Refusal(DaemonStub):
        def round_trip(self, frame, timeout):
            response = super().round_trip(frame, timeout)
            if frame.get("plan"):
                return {
                    "ok": False, "error": "plan refused", "error_detail": detail,
                    "config_mismatch": config_mismatch,
                }
            return response

    transport = Refusal()
    error_class = ConfigMismatch if config_mismatch else RequestRejected
    with pytest.raises(error_class) as raised:
        Session(transport).plan(OPS)
    assert raised.value.error_detail.model_dump(exclude_unset=True) == detail
    assert transport.dispatched_ops == []
    assert len(transport.frames) == (4 if config_mismatch else 2)
