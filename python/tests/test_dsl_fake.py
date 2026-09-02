"""Regression coverage for the offline DSL fake parser's grammar enforcement.

`_dsl_fake.parse_dsl` exists only so the offline test servers reject what the
real cloud grammar rejects instead of silently accepting whatever a
not-yet-fixed renderer sends; these tests pin the arms most likely to drift
back to permissive.
"""

from __future__ import annotations

import pytest
from _dsl_fake import DslParseError, parse_dsl


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
