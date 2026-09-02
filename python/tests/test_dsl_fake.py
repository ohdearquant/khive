"""Regression coverage for the offline DSL fake parser's grammar enforcement.

`_dsl_fake.parse_dsl` exists only so the offline test servers reject what the
real cloud grammar rejects instead of silently accepting whatever a
submitted renderer sends; these tests pin the arms most likely to drift back
to permissive.
"""

from __future__ import annotations

import pytest
from _dsl_fake import DslParseError, PrevRef, parse_dsl


def test_raw_control_character_in_string_rejected():
    with pytest.raises(DslParseError):
        parse_dsl('verb(note="bad\x01char")')


@pytest.mark.parametrize("word", ["nan", "-nan", "inf", "-inf", "infinity", "-infinity"])
def test_non_finite_scalar_word_rejected(word):
    with pytest.raises(DslParseError):
        parse_dsl(f"verb(weight={word})")


@pytest.mark.parametrize("constant", ["NaN", "Infinity", "-Infinity"])
def test_non_finite_constant_in_object_literal_rejected(constant):
    with pytest.raises(DslParseError):
        parse_dsl(f'verb(properties={{"w": {constant}}})')


def test_finite_numbers_still_accepted():
    assert parse_dsl("verb(weight=-2.5e-3)") == [("verb", {"weight": -2.5e-3})]
    assert parse_dsl("verb(count=42)") == [("verb", {"count": 42})]
    assert parse_dsl('verb(properties={"w": 1.5})') == [("verb", {"properties": {"w": 1.5}})]


@pytest.mark.parametrize("bad", ["01", "007", "+1", "1_0", "1_000.5"])
def test_invalid_number_spellings_rejected(bad):
    with pytest.raises(DslParseError):
        parse_dsl(f"verb(count={bad})")


def test_double_dot_verb_nesting_rejected():
    with pytest.raises(DslParseError):
        parse_dsl('a.b.c(x="1")')


def test_single_dot_pack_verb_accepted():
    assert parse_dsl('blob.put(bytes="x")') == [("blob.put", {"bytes": "x"})]


def test_whitespace_between_tokens_tolerated():
    assert parse_dsl('verb( count = 1 , note = "x" )') == [("verb", {"count": 1, "note": "x"})]
    assert parse_dsl("[stats() ,  whoami() ]") == [("stats", {}), ("whoami", {})]
    assert parse_dsl(" stats() | whoami() ") == [("stats", {}), ("whoami", {})]


def test_bare_top_level_chain_parses_as_chain():
    assert parse_dsl("stats() | whoami()") == [("stats", {}), ("whoami", {})]


def test_prev_reference_resolved_inside_a_chain():
    [_first, (verb, args)] = parse_dsl('get(id="x") | update(id=$prev.id)')
    assert verb == "update"
    assert args == {"id": PrevRef("id")}


def test_prev_reference_rejected_outside_a_chain():
    with pytest.raises(DslParseError):
        parse_dsl('update(id=$prev.id)')


def test_prev_literal_escape_decodes_back_to_the_original_text():
    # Two raw backslashes in the DSL source decode (standard JSON string
    # escaping) to one literal backslash ahead of the $prev-shaped text —
    # the escaped-literal form `string_as_prev_ref` strips back to plain text.
    assert parse_dsl(r'verb(query="\\$prev.id")') == [("verb", {"query": "$prev.id"})]
