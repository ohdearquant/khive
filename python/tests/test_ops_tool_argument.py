"""Verb arguments must not collide with the operation builder's own parameters."""

import json

import pytest

from khive.ops import encode, op


@pytest.mark.parametrize(
    "verb",
    [
        "exec.run",
        "exec.runs",
        "tool.check",
        "tool.describe",
        "tool.policy",
        "tool.request",
        "tool.requests",
    ],
)
def test_tool_argument_survives_operation_encoding(verb):
    operation = op(verb, tool="my.tool", actor="test:caller")

    assert json.loads(encode([operation])) == [
        {"tool": verb, "args": {"tool": "my.tool", "actor": "test:caller"}}
    ]


def test_optional_tool_filter_is_still_omitted_when_none():
    assert op("exec.runs", tool=None)["args"] == {}
