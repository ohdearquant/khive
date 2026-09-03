"""`render_dsl`: every value class in the request DSL grammar, plus a
round-trip property test against the fake parser the offline test servers
use to enforce the real wire contract."""

from __future__ import annotations

import json

import pytest
from _dsl_fake import DslParseError, PrevRef, parse_dsl

from khive.dsl import render_dsl
from khive.errors import TransportError
from khive.ops import op


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
    assert render_dsl([op("stats"), op("whoami")], chained=True) == "stats() | whoami()"


def test_single_op_chained_renders_bare():
    assert render_dsl([op("stats")], chained=True) == "stats()"


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


def test_dsl_text_passes_through_untouched():
    assert render_dsl("whoami()") == "whoami()"
    chain = "whoami() | stats()"
    assert render_dsl(chain) == chain
    assert render_dsl(" [whoami(),  stats()] ") == " [whoami(),  stats()] "


def test_dsl_string_elements_render_verbatim_beside_dicts():
    assert render_dsl(["whoami()", op("stats")]) == "[whoami(), stats()]"
    assert render_dsl(['search(query="x")']) == 'search(query="x")'
    assert parse_dsl(render_dsl(["whoami()", op("search", query="x")])) == [
        ("whoami", {}),
        ("search", {"query": "x"}),
    ]


def test_empty_string_element_in_list_raises_transport_error():
    with pytest.raises(TransportError, match="empty"):
        render_dsl([""])
    with pytest.raises(TransportError, match="empty"):
        render_dsl(["", op("stats")])


def test_whitespace_only_string_element_in_list_raises_transport_error():
    with pytest.raises(TransportError, match="empty"):
        render_dsl(["  \t\n  "])
    with pytest.raises(TransportError, match="empty"):
        render_dsl(["  \t\n  ", "whoami()"])


def test_mixed_dsl_string_and_op_dict_pack_verb_accepted():
    rendered = render_dsl(["whoami()", op("blob.put", bytes="x")])
    assert rendered == '[whoami(), blob.put(bytes="x")]'
    assert parse_dsl(rendered) == [("whoami", {}), ("blob.put", {"bytes": "x"})]


def test_mixed_dsl_string_and_op_dict_double_nested_verb_rejected():
    rendered = render_dsl(["whoami()", 'a.b.c(x="1")'])
    assert rendered == '[whoami(), a.b.c(x="1")]'
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


def test_entry_without_tool_name_raises_transport_error():
    with pytest.raises(TransportError, match="no 'tool' name"):
        render_dsl([{"args": {}}])
    with pytest.raises(TransportError, match="no 'tool' name"):
        render_dsl([{"tool": "", "args": {}}])


def test_non_op_entry_raises_transport_error():
    with pytest.raises(TransportError, match="cannot render int"):
        render_dsl([3])


def test_missing_args_key_renders_empty_call():
    assert render_dsl([{"tool": "stats"}]) == "stats()"


@pytest.mark.parametrize("bad_args", [[], "", 0, False, None])
def test_non_dict_args_value_raises_transport_error(bad_args):
    with pytest.raises(TransportError, match="args must be an object"):
        render_dsl([{"tool": "stats", "args": bad_args}])


@pytest.mark.parametrize("bad", [float("nan"), float("inf"), float("-inf")])
def test_non_finite_float_scalar_raises_transport_error(bad):
    with pytest.raises(TransportError):
        render_dsl([op("update", weight=bad)])


@pytest.mark.parametrize("bad", [float("nan"), float("inf"), float("-inf")])
def test_non_finite_float_in_array_raises_transport_error(bad):
    with pytest.raises(TransportError):
        render_dsl([op("update", weights=[1.0, bad])])


@pytest.mark.parametrize("bad", [float("nan"), float("inf"), float("-inf")])
def test_non_finite_float_in_object_raises_transport_error(bad):
    with pytest.raises(TransportError):
        render_dsl([op("update", properties={"w": bad})])


def test_finite_float_round_trip():
    args = {"weight": -2.5e-3}
    rendered = render_dsl([op("verb", **args)])
    assert rendered == "verb(weight=-0.0025)"
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == args


@pytest.mark.parametrize(
    "args",
    [
        {"kind": None},
        {"hard": False},
        {"tags": []},
        {"properties": {}},
        {"weight": -0.5},
        {"weight": 1.5e10},
        {"weight": -2.5e-3},
        {"note": "literal \\q unknown escape"},
        {"query": "$prev"},
        {"query": "$prev.id"},
    ],
)
def test_round_trip_additional_value_shapes(args):
    # Built as a raw op dict, not `op(**args)`: `op()` prunes `None` values
    # before `render_dsl` ever sees them, so `{"kind": None}` could not
    # reach the renderer's `null` arm through the normal call path.
    rendered = render_dsl([{"tool": "verb", "args": args}])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == args


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev.a.b.c",
        "$prev[0]",
        "$prev[0].id",
        "$prev.not_actually_a_reference_just_starts_this_way",
    ],
)
def test_prev_shaped_literal_round_trips_top_level(prev_like):
    """A caller's own string that merely starts like a `$prev` reference
    renders as an escaped literal and decodes back to the exact original
    text — never as a chain reference — at the argument's own top level."""
    rendered = render_dsl([op("verb", query=prev_like)])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == {"query": prev_like}


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_prev_shaped_literal_round_trips_nested(prev_like):
    """The same escape applies at any depth of an array or object argument."""
    rendered = render_dsl([op("verb", tags=[prev_like], properties={"note": prev_like})])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == {"tags": [prev_like], "properties": {"note": prev_like}}


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_single_backslash_prev_literal_raises_top_level(prev_like):
    """A caller value with exactly one leading backslash ahead of `$prev`-shaped
    text has no wire representation (see `khive.dsl` module docstring) — it
    must raise rather than silently decode back to something else."""
    with pytest.raises(TransportError, match="no representation"):
        render_dsl([op("verb", query="\\" + prev_like)])


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_single_backslash_prev_literal_raises_nested_in_list(prev_like):
    with pytest.raises(TransportError, match="no representation"):
        render_dsl([op("verb", tags=["\\" + prev_like])])


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_single_backslash_prev_literal_raises_nested_in_object(prev_like):
    with pytest.raises(TransportError, match="no representation"):
        render_dsl([op("verb", properties={"note": "\\" + prev_like})])


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_double_backslash_prev_literal_round_trips_top_level(prev_like):
    value = "\\\\" + prev_like
    rendered = render_dsl([op("verb", query=value)])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == {"query": value}


@pytest.mark.parametrize(
    "prev_like",
    [
        "$prev",
        "$prev.id",
        "$prev[0]",
    ],
)
def test_double_backslash_prev_literal_round_trips_nested(prev_like):
    value = "\\\\" + prev_like
    rendered = render_dsl([op("verb", tags=[value], properties={"note": value})])
    [(verb, parsed_args)] = parse_dsl(rendered)
    assert verb == "verb"
    assert parsed_args == {"tags": [value], "properties": {"note": value}}


def test_intentional_prev_reference_in_a_chain_still_parses_as_a_reference():
    """Control for the two tests above: a caller who deliberately writes a
    `$prev` reference as raw DSL text inside a chain must still get a
    reference back, not a literal — the escape only applies to values
    `render_dsl` itself renders from Python values, never to DSL text a
    caller already holds."""
    [(_get_verb, _get_args), (verb, args)] = parse_dsl('get(id="x") | update(id=$prev.id)')
    assert verb == "update"
    assert args == {"id": PrevRef("id")}


def test_prev_reference_outside_a_chain_is_rejected():
    with pytest.raises(DslParseError):
        parse_dsl("update(id=$prev.id)")
    with pytest.raises(DslParseError):
        parse_dsl("[update(id=$prev.id), other()]")


# -- wire-invalid renders ----------------------------------------------------
#
# `render_dsl` must reject a call it cannot legally hand to the request
# parser, rather than returning wire-invalid text for a transport to send.
# Each case below is rejected identically by the Rust grammar
# (`parser_impl.rs`) and by the offline fake (`_dsl_fake.py`), pinning the
# renderer to the same decision both make.


def test_invalid_argument_name_raises_transport_error():
    with pytest.raises(TransportError, match="bad-name"):
        render_dsl([{"tool": "verb", "args": {"bad-name": 1}}])


def test_non_string_argument_name_raises_transport_error():
    with pytest.raises(TransportError, match="must be a string"):
        render_dsl([{"tool": "verb", "args": {1: 2}}])


def test_empty_operation_list_raises_transport_error():
    with pytest.raises(TransportError, match="empty"):
        render_dsl([])


# `dispatch.rs::RESERVED_ENVELOPE_ARGS` (`khive_types::pack`), mirrored in
# `khive.dsl._RESERVED_ENVELOPE_ARGS` — kept as one list here so a change to
# either drifts this test instead of silently under-covering the set.
_RESERVED_ENVELOPE_ARG_NAMES = ("presentation", "presentation_per_op")


@pytest.mark.parametrize("name", _RESERVED_ENVELOPE_ARG_NAMES)
def test_reserved_envelope_argument_name_raises_transport_error(name):
    with pytest.raises(TransportError, match=name):
        render_dsl([{"tool": "stats", "args": {name: "table"}}])


def test_triple_segment_tool_name_raises_transport_error():
    with pytest.raises(TransportError, match="a.b.c"):
        render_dsl([{"tool": "a.b.c", "args": {}}])


def test_tool_name_starting_with_digit_raises_transport_error():
    with pytest.raises(TransportError, match="1x"):
        render_dsl([{"tool": "1x", "args": {}}])


def test_render_stops_at_the_byte_cap_before_rendering_later_entries():
    # Two 600 KiB entries already exceed the 1 MiB request cap. The third entry
    # is unrenderable, so reaching it would raise a different error: the cap
    # must fire first, proving later entries are never rendered or joined.
    big = "x" * (600 * 1024)
    ops = [op("blob.put", bytes=big), op("blob.put", bytes=big), {"tool": 42}]
    with pytest.raises(TransportError, match="exceeds 1048576 bytes after 2 of 3"):
        render_dsl(ops)


def test_render_under_the_byte_cap_still_joins_every_entry():
    small = "x" * 1024
    rendered = render_dsl([op("blob.put", bytes=small), op("blob.put", bytes=small)])
    assert rendered.startswith("[blob.put(") and rendered.endswith(")]")
    assert rendered.count("blob.put(") == 2
