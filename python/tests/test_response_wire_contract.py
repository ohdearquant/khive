"""Check source-line citation presence, not response behavior or semantics."""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
DOC_PATH = Path(__file__).resolve().parents[1] / "docs" / "RESPONSE_WIRE_CONTRACT.md"
DOC_TEXT = DOC_PATH.read_text()
_CITE_RE = re.compile(r'([\w./-]+) -- ("(?:\\.|[^"\\])*")')


def _citation_rows() -> dict[str, list[tuple[str, str]]]:
    rows = {}
    for line in DOC_TEXT.splitlines():
        if not line.startswith("| R"):
            continue
        rule, separator, source_cell = line[2:].partition(" | ")
        rule = rule.strip()
        if not separator or not re.fullmatch(r"R\d+", rule):
            continue
        assert rule not in rows, f"duplicate citation rule: {rule}"
        rows[rule] = [
            (match.group(1), json.loads(match.group(2))) for match in _CITE_RE.finditer(source_cell)
        ]
    return rows


def test_response_contract_citation_inventory():
    rows = _citation_rows()
    assert set(rows) == {f"R{index}" for index in range(1, 18)}
    assert all(rows.values()), "every rule must cite at least one source line"
    citation_lines = [line for line in DOC_TEXT.splitlines() if line.startswith("| R")]
    # Detect an unparsed quote instead of silently reducing the audit surface.
    assert sum(line.count(" -- ") for line in citation_lines) == sum(map(len, rows.values()))


@pytest.mark.skipif(
    not (REPO_ROOT / "crates" / "khive-mcp").exists(),
    reason="server source is not present in this checkout",
)
def test_every_cited_response_line_exists_in_the_named_source():
    """Presence alone cannot prove branch identity or the associated explanation.

    Quotes may recur. Behavior is checked by separate client fixtures; this
    check only detects removal or rewriting of the cited source fragments.
    """
    failures = []
    file_cache: dict[str, list[str] | None] = {}
    for rule, citations in _citation_rows().items():
        for path, quoted in citations:
            if path not in file_cache:
                try:
                    file_cache[path] = (REPO_ROOT / path).read_text().splitlines()
                except FileNotFoundError:
                    file_cache[path] = None
            lines = file_cache[path]
            if lines is None:
                failures.append(f"{rule}: file not found: {path}")
            elif not any(quoted in line for line in lines):
                failures.append(f"{rule}: {path} has no line containing {quoted!r}")
    assert not failures, "\n".join(failures)
