"""Comm kwargs survive both request encodings and the native Session boundary."""

from __future__ import annotations

import copy
import json

import pytest
from _dsl_fake import parse_dsl

from khive import Session, Transport, encode, op
from khive.dsl import render_dsl

THREAD_ID = "12345678-90AB-4CDE-8F01-234567890ABC"
TAGS = ["mail_id:abc", 'team:"Research"\\café', " Exact_Case%_ ", "mail_id:abc"]
RESULTS = [
    {
        "ok": True,
        "tool": "comm.inbox",
        "result": {"messages": [], "total": 9, "next_offset": 8},
    }
]


class RecordingTransport(Transport):
    def __init__(self):
        self.frames = []

    def round_trip(self, frame, timeout):
        self.frames.append(copy.deepcopy(frame))
        response = {"ok": True, "served_config_id": "test-config"}
        if not frame.get("metrics_only"):
            response["result"] = json.dumps({"results": RESULTS})
        return response


@pytest.fixture(params=["json", "dsl"])
def wire_format(request):
    if request.param == "json":
        return encode, lambda payload: [
            (item["tool"], item["args"]) for item in json.loads(payload)
        ]
    return render_dsl, parse_dsl


@pytest.mark.parametrize("box", ["inbox", "sent"])
@pytest.mark.parametrize(
    "filters",
    [
        {"tags": TAGS},
        {"kind": "question"},
        {"thread_id": THREAD_ID},
        {"tags": TAGS, "kind": "question", "thread_id": THREAD_ID},
    ],
    ids=["tags", "kind", "thread", "combined"],
)
def test_comm_inbox_filters_reach_transport_verbatim(wire_format, box, filters):
    encode_ops, decode_ops = wire_format
    transport = RecordingTransport()
    session = Session(transport, actor_id="test-client")
    direction_args = {"from_actor": "test-peer"} if box == "inbox" else {"to_actor": "test-peer"}
    args = {
        "box": box,
        "since": "2026-01-02T03:04:05Z",
        "subject_contains": 'Re: "café"',
        "limit": 1,
        "offset": 7,
        **direction_args,
        **copy.deepcopy(filters),
    }
    payload = encode_ops([op("comm.inbox", **args)])

    assert session.request(payload) == RESULTS

    assert len(transport.frames) == 2
    handshake, request_frame = transport.frames
    assert handshake["metrics_only"] is True
    assert request_frame["config_id"] == "test-config"
    assert request_frame["ops"] == payload
    assert decode_ops(request_frame["ops"]) == [("comm.inbox", args)]
    assert not {"tags", "kind", "thread_id"} & request_frame.keys()


@pytest.mark.parametrize("box", ["inbox", "sent"])
def test_comm_inbox_empty_tags_remain_explicit(wire_format, box):
    encode_ops, decode_ops = wire_format
    transport = RecordingTransport()
    session = Session(transport, actor_id="test-client")

    session.request(encode_ops([op("comm.inbox", box=box, tags=[], limit=1)]))

    assert decode_ops(transport.frames[-1]["ops"]) == [
        ("comm.inbox", {"box": box, "tags": [], "limit": 1})
    ]


def test_comm_inbox_ordinary_request_after_filtered_request_stays_unchanged(wire_format):
    encode_ops, decode_ops = wire_format
    transport = RecordingTransport()
    session = Session(transport, actor_id="test-client")
    session.request(
        encode_ops([op("comm.inbox", tags=TAGS, kind="question", thread_id=THREAD_ID)])
    )
    ordinary = encode_ops(
        [op("comm.inbox", status="unread", limit=7, tags=None, kind=None, thread_id=None)]
    )

    assert session.request(ordinary) == RESULTS

    assert len(transport.frames) == 3
    assert transport.frames[-1]["ops"] == ordinary
    assert decode_ops(transport.frames[-1]["ops"]) == [
        ("comm.inbox", {"status": "unread", "limit": 7})
    ]
