"""`render_dsl`: every value class in the request DSL grammar, plus a
round-trip property test against the fake parser the offline test servers
use to enforce the real wire contract."""

from __future__ import annotations

import json

import pytest

from khive.dsl import render_dsl
from khive.errors import TransportError
from khive.ops import op

from _dsl_fake import parse_dsl


def test_bare_string():
    assert render_dsl([op("whoami")]) == "whoami()"


def test_empty_args_op():
    assert render_dsl([op("stats")]) == "stats()"


def test_string_escapes_and_nonascii_exact_rendering():
    value = 'a "quoted" c\\d\ne\tf,g)h=i café'
    rendered = render_dsl([op("search", query=value)])
    expected_body = 'a \\"quoted\\" c\\\\d\\ne\\tf,g)h=i café'
    assert rendered == f'search(query="{expected_body}")'


def test_int_float_bool_none_dropped():
    # `kind=None` is pruned by `op()` before `render_dsl` ever sees it.
    rendered = render_dsl([op("list", limit=5, min_weight=0.5, hard=True, kind=None)])
    assert rendered == "list(limit=5, min_weight=0.5, hard=true)"


def test_nested_list():
    rendered = render_dsl([op("list", relations=["extends", "depends_on"])])
    assert rendered == 'list(relations=["extends", "depends_on"])'


def test_nested_object_uses_strict_json_encoding():
    obj = {"note": 'a "quoted" c\\d\ne\tf,g)h=i café'}
    rendered = render_dsl([op("create", properties=obj)])
    expected_json = json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
    assert rendered == f"create(properties={expected_json})"


def test_two_op_batch():
    assert render_dsl([op("stats"), op("whoami")]) == "[stats(), whoami()]"


def test_chained_pair():
    assert render_dsl([op("stats"), op("whoami")], chained=True) == "[stats() | whoami()]"


def test_control_character_raises_transport_error():
    with pytest.raises(TransportError):
        render_dsl([op("search", query="bad\x01char")])


@pytest.mark.parametrize(
    "args",
    [
        {},
        {"query": "hello"},
        {"query": 'a "quoted" word\nline2', "kind": "entity", "limit": 1},
        {"weight": 0.9, "hard": True},
        {"tags": ["x", "y", "z"]},
        {"properties": {"a": 1, "b": [1, 2, {"c": "d"}]}},
        {"note": "non-ascii café ☕"},
        {"note": "line1\ttab\rcr"},
        {"note": "a=b (c) [d] {e}"},
    ],
)
def test_round_trip_through_fake_parser(args):
    rendered = render_dsl([op("verb", **args)])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == args


def test_old_daemon_shaped_string_refused_by_fake(rest_server, api_key):
    httpx = pytest.importorskip("httpx")
    response = httpx.post(
        f"{rest_server.url}/v1/request",
        json={"ops": '[{"tool": "whoami", "args": {}}]'},
        headers={"Authorization": f"ApiKey {api_key}"},
    )
    assert response.status_code == 400
    assert response.json() == {"error": "unknown verb: Missing 'verb' field in JSON"}
