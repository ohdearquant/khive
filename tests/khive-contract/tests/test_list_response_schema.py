"""Focused contract tests for stable KG list response envelopes.

ADR: ADR-023
section: Stable list response envelopes
"""

from __future__ import annotations

from typing import Any

import pytest

from khive_contract.schema import assert_list_response


VERBS_UNDER_TEST = {"list"}


LIMIT_METADATA = {
    "requested_limit": 20,
    "effective_limit": 20,
    "limit_clamped": False,
}


@pytest.mark.parametrize(
    "page",
    [
        {"items": [], **LIMIT_METADATA},
        {"entities": [], "next_after": None, **LIMIT_METADATA},
        {"notes": [], "next_after": None, "scan_incomplete": True, **LIMIT_METADATA},
        {"edges": [], "next_after": None, **LIMIT_METADATA},
    ],
    ids=["offset", "entity-cursor", "note-cursor", "edge-cursor"],
)
def test_list_schema_accepts_each_stable_envelope(page: dict[str, Any]) -> None:
    assert_list_response(page)


@pytest.mark.parametrize("missing", ["requested_limit", "effective_limit", "limit_clamped"])
def test_list_schema_rejects_missing_limit_metadata(missing: str) -> None:
    page = {"items": [], **LIMIT_METADATA}
    del page[missing]

    with pytest.raises(AssertionError, match="Schema validation failed"):
        assert_list_response(page)


@pytest.mark.parametrize(
    "page",
    [
        {"next_after": None, **LIMIT_METADATA},
        {"entities": [], **LIMIT_METADATA},
        {"items": [], "next_after": None, **LIMIT_METADATA},
        {"items": [], "entities": [], "next_after": None, **LIMIT_METADATA},
        {"entities": [], "notes": [], "next_after": None, **LIMIT_METADATA},
        {"entities": [], "next_after": 7, **LIMIT_METADATA},
        {"items": {}, **LIMIT_METADATA},
    ],
    ids=[
        "cursor-without-substrate",
        "cursor-without-next-after",
        "offset-with-cursor-marker",
        "offset-and-cursor",
        "mixed-cursor-substrates",
        "malformed-next-after",
        "malformed-items",
    ],
)
def test_list_schema_rejects_missing_or_malformed_variants(page: dict[str, Any]) -> None:
    with pytest.raises(AssertionError, match="Schema validation failed"):
        assert_list_response(page)
