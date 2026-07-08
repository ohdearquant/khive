"""Manifest and coverage gate — meta-tests for the test suite itself.

ADR: ADR-023
section: Verb naming; Coverage gates; ADR docstring conventions

These tests are static (no MCP calls). They introspect the test suite files
to enforce structural conventions:
  - Every test module declares VERBS_UNDER_TEST
  - Every test module's docstring references an ADR and section
  - The union of all VERBS_UNDER_TEST covers all 18 product verbs
  - No test file hardcodes namespace="local" in verb calls (defeats isolation)
"""

from __future__ import annotations

import ast
import pathlib
import re

import pytest

from khive_contract.fixtures import (
    PLAY_SPEC_MINIMUM_VERB_COUNT,
    PRODUCT_VERB_MANIFEST as ALL_PRODUCT_VERBS,
)

TESTS_DIR = pathlib.Path(__file__).parent
_THIS_FILE = pathlib.Path(__file__)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _test_files() -> list[pathlib.Path]:
    """All test_*.py files in this directory except this manifest."""
    return sorted(
        f for f in TESTS_DIR.glob("test_*.py")
        if f.resolve() != _THIS_FILE.resolve()
    )


def _module_docstring(path: pathlib.Path) -> str:
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(path))
    return ast.get_docstring(tree) or ""


def _verbs_under_test(path: pathlib.Path) -> set[str] | None:
    """Extract VERBS_UNDER_TEST set from a module via AST. Returns None if not found."""
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(path))
    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign):
            continue
        for target in node.targets:
            if not (isinstance(target, ast.Name) and target.id == "VERBS_UNDER_TEST"):
                continue
            val = node.value
            if isinstance(val, ast.Set):
                return {
                    elt.value
                    for elt in val.elts
                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
                }
            if isinstance(val, ast.Call) and isinstance(val.func, ast.Name):
                # frozenset({...}) or set({...})
                if val.args and isinstance(val.args[0], ast.Set):
                    return {
                        elt.value
                        for elt in val.args[0].elts
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
                    }
    return None


def _has_hardcoded_local_namespace(path: pathlib.Path) -> list[int]:
    """Return list of line numbers where namespace='local' appears in verb calls."""
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(path))
    bad_lines: list[int] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for kw in node.keywords:
            if (
                kw.arg == "namespace"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value == "local"
            ):
                bad_lines.append(kw.value.lineno)
    return bad_lines


# ---------------------------------------------------------------------------
# Static structure tests (no markers needed — these are fast local checks)
# ---------------------------------------------------------------------------


def test_all_test_modules_define_verbs_under_test() -> None:
    """Every test_*.py module (except this manifest) defines VERBS_UNDER_TEST.

    ADR: ADR-023
    section: Verb naming

    Allows the combined coverage gate to aggregate verb coverage across modules.
    """
    files = _test_files()
    assert files, f"No test files found in {TESTS_DIR}"
    missing: list[str] = []
    for path in files:
        verbs = _verbs_under_test(path)
        if verbs is None:
            missing.append(path.name)
    assert not missing, (
        f"These test modules do not define VERBS_UNDER_TEST: {missing}\n"
        f"Add 'VERBS_UNDER_TEST = {{\"verb\", ...}}' at module level."
    )


def test_all_test_modules_have_adr_docstring() -> None:
    """Every test_*.py module (except this manifest) has a docstring citing ADR: and section:.

    ADR: ADR-023
    section: ADR docstring conventions

    Enforces the convention that every contract test file is traceable to an ADR.
    """
    files = _test_files()
    assert files, f"No test files found in {TESTS_DIR}"
    missing: list[str] = []
    for path in files:
        doc = _module_docstring(path)
        if "ADR:" not in doc or "section:" not in doc:
            missing.append(f"{path.name} (docstring: {doc[:80]!r})")
    assert not missing, (
        f"These modules lack 'ADR:' or 'section:' in their module docstring:\n"
        + "\n".join(f"  {m}" for m in missing)
    )


def test_combined_verb_coverage_is_complete() -> None:
    """The union of all VERBS_UNDER_TEST across modules covers all 18 product verbs.

    ADR: ADR-023
    section: Coverage gates; Verb naming

    Fails if any product verb is missing from every test module's coverage claim.
    """
    files = _test_files()
    assert files, f"No test files found in {TESTS_DIR}"
    covered: set[str] = set()
    for path in files:
        verbs = _verbs_under_test(path)
        if verbs:
            covered.update(verbs)

    missing_verbs = ALL_PRODUCT_VERBS - covered
    assert not missing_verbs, (
        f"These product verbs are not claimed in any VERBS_UNDER_TEST: {sorted(missing_verbs)}\n"
        f"Covered: {sorted(covered)}"
    )

    assert len(covered & ALL_PRODUCT_VERBS) >= PLAY_SPEC_MINIMUM_VERB_COUNT, (
        f"Play spec requires >= {PLAY_SPEC_MINIMUM_VERB_COUNT} product verbs; "
        f"only {len(covered & ALL_PRODUCT_VERBS)} covered."
    )


def test_no_hardcoded_local_namespace() -> None:
    """No test module hardcodes namespace='local' in verb calls.

    ADR: ADR-003
    section: Namespace isolation

    Tests must use temp_namespace (the function-scoped fixture) to prevent
    cross-test contamination. Hardcoding 'local' bypasses isolation.
    """
    files = _test_files()
    violations: list[str] = []
    for path in files:
        bad_lines = _has_hardcoded_local_namespace(path)
        if bad_lines:
            violations.append(f"{path.name}: lines {bad_lines}")
    assert not violations, (
        f"These files use namespace='local' (defeats isolation):\n"
        + "\n".join(f"  {v}" for v in violations)
        + "\nUse 'namespace=temp_namespace' instead."
    )


def test_verb_coverage_count_reported() -> None:
    """Report the actual covered verb count vs 18-verb baseline (informational).

    ADR: ADR-023
    section: Coverage gates

    Always passes — records coverage count for CI visibility.
    """
    files = _test_files()
    covered: set[str] = set()
    for path in files:
        verbs = _verbs_under_test(path)
        if verbs:
            covered.update(verbs)
    product_covered = covered & ALL_PRODUCT_VERBS
    # Report in assert message (visible in pytest verbose output)
    assert len(product_covered) == len(ALL_PRODUCT_VERBS), (
        f"Partial coverage: {len(product_covered)}/{len(ALL_PRODUCT_VERBS)} product verbs covered.\n"
        f"Covered: {sorted(product_covered)}\n"
        f"Missing: {sorted(ALL_PRODUCT_VERBS - product_covered)}"
    )


@pytest.mark.xfail(
    reason="golden/ snapshots not yet seeded — run with --update-golden to populate",
    strict=False,
)
def test_golden_snapshot_directory_has_snapshots() -> None:
    """The golden/ directory must contain at least one snapshot file once seeded.

    ADR: ADR-023
    section: Coverage gates

    xfail until golden snapshots are generated (ignores .gitkeep placeholder).
    Run with --update-golden to seed.
    """
    golden_dir = TESTS_DIR.parent / "golden"
    assert golden_dir.exists(), (
        f"golden/ directory not found at {golden_dir}."
    )
    real_files = [f for f in golden_dir.iterdir() if f.name != ".gitkeep"]
    assert real_files, (
        f"golden/ directory has no snapshot files (only .gitkeep). "
        f"Run: uv run pytest --update-golden to seed."
    )


@pytest.mark.xfail(
    reason="baselines/latency.json not yet created",
    strict=False,
)
def test_latency_baseline_file_exists() -> None:
    """The baselines/latency.json file must exist for regression tracking.

    ADR: ADR-023
    section: Coverage gates

    xfail until baselines are recorded.
    """
    baseline_path = TESTS_DIR.parent / "baselines" / "latency.json"
    assert baseline_path.exists(), (
        f"Latency baseline not found at {baseline_path}."
    )
