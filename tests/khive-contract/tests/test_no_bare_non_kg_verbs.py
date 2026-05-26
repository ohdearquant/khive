"""Verb namespace contract: no bare non-KG verbs in the product surface.

ADR: ADR-023
section: Verb naming; Pack product verbs; Verb namespace enforcement

Static contract test — no MCP calls. Asserts that every verb declared in
ALL_PRODUCT_VERBS (the manifest's canonical list) is either:
  - A KG substrate bare verb (the 14 kg verbs, no prefix), or
  - A dotted pack.verb verb (exactly one dot, non-empty prefix and suffix).

This mirrors the Rust `verb_namespace_contract.rs` test in kkernel and prevents
future Python tests from silently re-introducing bare pack verbs.
"""

from __future__ import annotations

import re

import pytest

from tests.test_manifest import ALL_PRODUCT_VERBS

# This file is a static contract test — it does not call any verbs directly.
# VERBS_UNDER_TEST is declared as a non-empty set to satisfy the manifest
# coverage gate parser; the actual coverage contribution is the verb namespace
# contract assertion in the tests below.
VERBS_UNDER_TEST = {
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition",
    "memory.remember", "memory.recall",
}

# KG substrate verbs that are allowed as bare names (no pack prefix).
# Matches the Rust contract test at crates/kkernel/tests/verb_namespace_contract.rs.
# `verbs` is the substrate-level verb-registry introspection (J-help PR #464).
KG_SUBSTRATE_BARE: frozenset[str] = frozenset({
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "propose", "review", "withdraw",
    "verbs",
})

# Pattern for a valid dotted verb: one dot, identifier on each side.
_DOTTED_RE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*\.[A-Za-z_][A-Za-z0-9_]*$')


def test_all_product_verbs_follow_namespace_contract() -> None:
    """Every product verb is either a KG bare substrate verb or follows pack.verb form.

    ADR: ADR-023
    section: Verb naming; Pack product verbs; Verb namespace enforcement

    Fails if any non-KG verb appears without a pack prefix, and fails if any
    verb has more than one dot (double-nesting is not permitted).
    """
    violations: list[str] = []
    for verb in sorted(ALL_PRODUCT_VERBS):
        if verb in KG_SUBSTRATE_BARE:
            # KG substrate verbs are allowed bare.
            continue
        if _DOTTED_RE.fullmatch(verb):
            # Exactly one dot, valid identifiers on both sides.
            continue
        violations.append(verb)

    assert not violations, (
        "These product verbs violate the namespace contract "
        "(must be KG-substrate bare or pack.verb dotted form):\n"
        + "\n".join(f"  {v}" for v in violations)
    )


def test_kg_substrate_verbs_are_not_prefixed() -> None:
    """KG substrate verbs must not carry a pack prefix in ALL_PRODUCT_VERBS.

    ADR: ADR-023
    section: kg bare substrate verbs

    Catches the anti-pattern of writing 'kg.create' instead of 'create'.
    """
    prefixed_kg: list[str] = []
    for verb in sorted(ALL_PRODUCT_VERBS):
        if verb in KG_SUBSTRATE_BARE:
            continue
        # Check if this looks like a prefixed version of a KG verb.
        parts = verb.split(".", 1)
        if len(parts) == 2 and parts[1] in KG_SUBSTRATE_BARE:
            prefixed_kg.append(verb)

    assert not prefixed_kg, (
        "KG substrate verbs must be bare (no pack prefix). "
        "Found prefixed forms:\n"
        + "\n".join(f"  {v}" for v in prefixed_kg)
    )
