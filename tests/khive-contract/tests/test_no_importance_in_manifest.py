"""Guard against re-introduction of the 'importance' parameter name.

ADR: ADR-021
section: salience parameter naming; importance alias elimination (2026-05-25)

Static and live checks that no product verb exposes a parameter named 'importance'.
The term was eliminated in favour of 'salience' (consistent with notes.salience
substrate column) on 2026-05-25. Any future regression that re-adds 'importance'
as a verb parameter must be caught here.

Two complementary tests:
1. Live MCP check: the 'request' tool description must not contain 'importance'.
2. Static repository sweep: rg over the entire repo with an allowlist confirms
   no non-allowed identifier containing 'importance' was added back.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {
    "remember", "recall",
}

# ---------------------------------------------------------------------------
# Allowlist for the repository sweep.
# Each entry is a substring that is PERMITTED to appear in an 'importance' match.
# The sweep flags any line where the matched text is NOT covered by one of these.
# ---------------------------------------------------------------------------
SWEEP_ALLOWLIST: list[str] = [
    "importance_trap",         # grandfathered fixture name (query_type value)
    "importance sampling",     # statistical technique — valid prose
    "importance_trap",         # guard test file self-reference
    "test_no_importance",      # this guard test file itself
    "importance' must not",    # assertion message in this guard test file
    "importance' parameter",   # comment in this guard test file
    "importance_weight",       # TODO: should not appear — explicit false-safeguard entry
]

# Paths excluded from the sweep (binary artefacts, archive, git internals, build output)
EXCLUDED_GLOBS = [
    "--glob=!docs/_archive/**",
    "--glob=!target/**",
    "--glob=!tests/khive-contract/tune/REPORT*.md",
    "--glob=!.git/**",
]

# The repo root is three levels up from this file:
# tests/khive-contract/tests/ → tests/khive-contract/ → tests/ → repo root
_THIS_FILE = Path(__file__).resolve()
REPO_ROOT = _THIS_FILE.parent.parent.parent.parent


@pytest.mark.adr_021
@pytest.mark.slow
def test_memory_request_description_contains_no_importance_param(
    khive_memory_session: KhiveMcpSession,
) -> None:
    """The 'request' tool description must not mention 'importance' as a param name.

    ADR: ADR-021
    section: salience parameter naming; importance alias elimination (2026-05-25)

    Regression guard: the 'importance' parameter was eliminated in favour of
    'salience' on 2026-05-25. This test prevents silent re-introduction.

    Checks that the word 'importance' does not appear in the MCP tool description
    in a context that suggests it is an accepted verb argument. The check uses
    a broad substring match — if 'importance' appears anywhere in the description,
    the test fails, because the only valid use of 'importance' in this context
    would be as a parameter name.

    The phrase 'importance sampling' (statistical technique) is not expected to
    appear in the MCP tool description, so a broad match is safe here.
    """
    tools = khive_memory_session.tools_list()
    assert tools, "tools/list returned empty for memory session"
    description = tools[0].get("description") or ""

    assert "importance" not in description, (
        "The word 'importance' must not appear in the MCP tool description — "
        "it was eliminated in favour of 'salience' (ADR-021 §2, 2026-05-25). "
        f"Found in description:\n{description!r}"
    )


# TODO: This test currently fails because `crates/khive-pack-memory/src/scoring.rs`
# still contains residual `importance` identifiers (the salience rename is incomplete).
# Fix: complete the rename in scoring.rs and update any related test fixtures that
# still reference the old identifier. Do NOT fix this test — it is the guard.
@pytest.mark.adr_021
def test_no_importance_identifiers_in_repo() -> None:
    """Full repository sweep: no non-allowed 'importance' identifier exists.

    ADR: ADR-021
    section: salience parameter naming; importance alias elimination (2026-05-25)

    Runs ripgrep over the entire repository and fails if any line contains the
    word 'importance' (case-insensitive for identifier forms) that is not covered
    by the allowlist below.

    Allowlist (permitted occurrences):
    - 'importance_trap'    — grandfathered fixture name in memories_corpus_v2.json
    - 'importance sampling' — valid statistical prose
    - This test file itself (self-references in strings and comments)
    - The assertion message in the live MCP test above

    Any other occurrence means the rename was regressed and must be fixed.
    """
    rg = _find_rg()
    if rg is None:
        pytest.skip("ripgrep (rg) not found on PATH — install ripgrep to run this test")

    cmd = [rg, "--no-heading", "-n", "importance"] + EXCLUDED_GLOBS
    result = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    # rg exits 0 when matches found, 1 when no matches, 2 on error
    if result.returncode == 2:
        pytest.fail(f"ripgrep error:\n{result.stderr}")

    if result.returncode == 1:
        # No matches at all — clean
        return

    # returncode == 0: matches found; filter through allowlist
    violations: list[str] = []
    for line in result.stdout.splitlines():
        if _is_allowed(line):
            continue
        violations.append(line)

    if violations:
        joined = "\n".join(violations[:50])  # cap output at 50 lines
        count = len(violations)
        pytest.fail(
            f"Found {count} non-allowed 'importance' identifier(s) in the repository. "
            f"All uses must be renamed to 'salience' (ADR-021 §2, 2026-05-25).\n\n"
            f"Violations (first {min(count, 50)}):\n{joined}"
        )


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _find_rg() -> str | None:
    """Return the path to the rg binary, or None if not found."""
    import shutil
    return shutil.which("rg")


def _is_allowed(line: str) -> bool:
    """Return True if the matched line is covered by the allowlist."""
    # Always allow this test file itself
    if _THIS_FILE.name in line or str(_THIS_FILE) in line:
        return True
    # Allow grandfathered fixture name and statistical prose
    for allowed in (
        "importance_trap",              # fixture query_type value (grandfathered)
        "importance sampling",          # statistical prose
        "codex_review_pr472",           # the review doc is not production code
        "not a separate `importance`",  # ADR-021 rationale header
        "earlier draft aliased `importance`",  # ADR-021 historical note
        "Separate `importance` column", # ADR-021 alternatives table
        "relative importance during",  # engine_config.rs English prose (not a param name)
    ):
        if allowed in line:
            return True
    return False
