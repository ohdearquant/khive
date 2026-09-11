"""The generic Python client must preserve required JSON and refused fence presence."""
import json

import pytest

from khive.ops import encode, op


@pytest.mark.parametrize("record", [None, False, 3, "scalar", [1, {"n": 2}], {"n": None}])
def test_stream_record_round_trip(record):
    assert json.loads(encode([op("stream.append", stream="s", record=record)]))[0]["args"]["record"] == record


def test_stream_explicit_null_fence_reaches_server_for_refusal():
    args = op("stream.append", stream="s", record=None, fence=None, expected_seq=None)["args"]
    assert "fence" in args and args["fence"] is None
    assert "expected_seq" not in args


def test_non_stream_optional_none_still_omitted():
    assert op("create", kind="observation", name=None)["args"] == {"kind": "observation"}


def test_stream_batch_omits_none_fence_but_keeps_member_nulls():
    # The batch defaults its mode by fence presence, so a Python None fence is
    # absent; inside a member, a null record or expected_seq is part of the shape.
    args = op(
        "stream.batch",
        ops=[{"op": "append", "stream": "s", "record": None, "expected_seq": None}],
        fence=None,
        observed=None,
        atomic=None,
    )["args"]
    assert set(args) == {"ops"}
    assert args["ops"][0]["record"] is None
    assert args["ops"][0]["expected_seq"] is None
