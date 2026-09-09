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
