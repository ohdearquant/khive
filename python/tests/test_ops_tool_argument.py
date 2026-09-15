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


def test_keyword_tool_names_the_verb_when_no_positional_verb():
    assert op(tool="stats") == {"tool": "stats", "args": {}}
    assert op(tool="get", id="abc") == {"tool": "get", "args": {"id": "abc"}}


def test_positional_verb_keeps_tool_keyword_as_argument():
    assert op("tool.describe", tool="stats") == {
        "tool": "tool.describe",
        "args": {"tool": "stats"},
    }


def test_missing_verb_is_rejected():
    with pytest.raises(TypeError):
        op()
    with pytest.raises(TypeError):
        op(tool=None)
