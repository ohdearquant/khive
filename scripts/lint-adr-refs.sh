#!/bin/sh
# Validate titled ADR references against the authoritative ADR H1 headings.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# `--self-test`: exercise the parenthetical-prose-citation extraction (the
# `ADR-NNN: <title>` form embedded in plain prose, e.g. inside a crate's
# docs/design.md "ADR Compliance" section) against synthetic fixtures rather
# than the live repo, since the real corpus only carries a handful of these.
# Regression case 1 reproduces the bm25 design.md drift this was added for
# (PR #886 review r1): a parenthetical citation that echoes a truncated ADR
# title must fail. Regression case 2 asserts a bare "(ADR-030)" reference
# with no restated title never false-positives.
self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    mkdir -p "$tmp/case-fail/docs/adr" "$tmp/case-fail/crates/fixture-crate/docs"
    mkdir -p "$tmp/case-pass/docs/adr" "$tmp/case-pass/crates/fixture-crate/docs"

    for case in case-fail case-pass; do
        cat > "$tmp/$case/docs/adr/ADR-030-retrieval-stack-port.md" <<'FIXTURE'
# ADR-030: Retrieval Stack Port — khive-retrieval

**Status**: accepted
FIXTURE
        cat > "$tmp/$case/docs/adr/README.md" <<'FIXTURE'
# ADR Index

| ADR | Title |
| --- | --- |
FIXTURE
    done

    cat > "$tmp/case-fail/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- ported as part of the retrieval stack (ADR-030: Retrieval Stack Port).
FIXTURE

    cat > "$tmp/case-pass/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- ported as part of the retrieval stack (ADR-030).
FIXTURE

    status=0

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-fail" > "$tmp/fail.log" 2>&1; then
        echo "self-test FAILED: drifted parenthetical prose citation (ADR-030: Retrieval Stack Port, missing the '-- khive-retrieval' suffix) was not caught"
        cat "$tmp/fail.log"
        status=1
    elif ! grep -q "ADR-030 title mismatch" "$tmp/fail.log"; then
        echo "self-test FAILED: lint failed, but not for the expected reason:"
        cat "$tmp/fail.log"
        status=1
    else
        echo "self-test OK: drifted parenthetical prose citation caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: bare ADR-030 reference (no restated title) should not trip the lint"
        cat "$tmp/pass.log"
        status=1
    else
        echo "self-test OK: bare ADR reference does not false-positive"
    fi

    return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit $?
fi

ROOT="${1:-$SCRIPT_DIR/..}"

python3 - "$ROOT" <<'PY'
from __future__ import annotations

import re
import sys
import unicodedata
from pathlib import Path


root = Path(sys.argv[1]).resolve()
adr_dir = root / "docs" / "adr"
adr_file_re = re.compile(r"^ADR-(\d{3})-.*\.md$", re.IGNORECASE)
h1_re = re.compile(
    r"^#\s+ADR-(?P<number>\d{3})(?:\s+Rev\s+\d+)?\s*:\s*(?P<title>.+?)\s*#*\s*$",
    re.IGNORECASE,
)
colon_ref_re = re.compile(r"\bADR-(?P<number>\d{3})\s*:\s*", re.IGNORECASE)
paren_ref_re = re.compile(r"\bADR-(?P<number>\d{3})\s+\(", re.IGNORECASE)
dash_ref_re = re.compile(r"\bADR-(?P<number>\d{3})\s+(?:--?|–|—)\s+", re.IGNORECASE)
heading_re = re.compile(r"^\s{0,3}#{1,6}\s+(?P<body>.+?)\s*#*\s*$")
link_re = re.compile(r"\[(?P<label>[^]\n]+)\]\((?P<target>[^\n)]+)\)")
adr_led_re = re.compile(r"^(?:\[)?ADR-\d{3}\b", re.IGNORECASE)
index_row_re = re.compile(
    r"^\|\s*\[ADR-(?P<number>\d{3})\]\((?P<target>[^)]+)\)\s*"
    r"\|\s*(?P<title>.*?)\s*\|\s*$",
    re.IGNORECASE,
)
edge_punctuation = " \t\r\n`*_~\\\"'“”‘’[]{}<>:;,.!?()#-–—"


def normalize(title: str) -> str:
    title = unicodedata.normalize("NFKC", title)
    previous = None
    while title != previous:
        previous = title
        title = re.sub(r"\s+", " ", title).strip(edge_punctuation)
    return title.casefold()


def first_h1(path: Path) -> tuple[int, str] | None:
    with path.open(encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, 1):
            if line.startswith("# "):
                return line_number, line.rstrip("\n")
    return None


def closing_delimiter(line: str, start: int, opening: str, closing: str) -> int | None:
    depth = 0
    for index in range(start, len(line)):
        if line[index] == opening:
            depth += 1
        elif line[index] == closing:
            depth -= 1
            if depth == 0:
                return index
    return None


def parenthesized_title(line: str, match: re.Match[str]) -> str | None:
    opening = match.end() - 1
    closing = closing_delimiter(line, opening, "(", ")")
    if closing is None:
        return None
    return line[opening + 1 : closing]


def top_level_parens(line: str) -> list[tuple[int, int]]:
    spans: list[tuple[int, int]] = []
    index = 0
    while index < len(line):
        if line[index] == "(":
            closing = closing_delimiter(line, index, "(", ")")
            if closing is None:
                index += 1
                continue
            spans.append((index, closing))
            index = closing + 1
        else:
            index += 1
    return spans


def prose_parenthetical_references(line: str) -> list[tuple[str, str]]:
    # Titled ADR references embedded in plain prose, e.g. "(ADR-030: Retrieval
    # Stack Port)" -- distinct from headings and Markdown link labels, which
    # are handled separately. Bounded to matching parens so trailing sentence
    # content never leaks into the captured title (unlike a naive end-of-line
    # capture). Only fires on the colon-titled form -- a bare "(ADR-030)" or a
    # descriptive gloss like "(ADR-030, hybrid retrieval)" never matches
    # colon_ref_re, so it is left alone.
    references: list[tuple[str, str]] = []
    for open_idx, close_idx in top_level_parens(line):
        inner = line[open_idx + 1 : close_idx]
        inner_matches = list(colon_ref_re.finditer(inner))
        for match_index, inner_match in enumerate(inner_matches):
            end = (
                inner_matches[match_index + 1].start()
                if match_index + 1 < len(inner_matches)
                else len(inner)
            )
            title = inner[inner_match.end() : end].split(";", 1)[0].strip()
            if not title or title[0].isdigit():
                # Empty capture, or a section/line locator like "ADR-017:451-480"
                # rather than a titled reference.
                continue
            references.append((inner_match.group("number"), title))
    return references


def titled_references(label: str) -> list[tuple[str, str]]:
    references: list[tuple[str, str]] = []
    for match in colon_ref_re.finditer(label):
        references.append((match.group("number"), label[match.end() :]))
    for match in paren_ref_re.finditer(label):
        title = parenthesized_title(label, match)
        if title is not None:
            references.append((match.group("number"), title))
    for match in dash_ref_re.finditer(label):
        references.append((match.group("number"), label[match.end() :]))
    return references


def is_local_adr_link(source: Path, target: str) -> bool:
    target = target.split("#", 1)[0]
    if re.match(r"^[a-z]+://", target, re.IGNORECASE):
        return bool(
            re.search(
                r"github\.com/ohdearquant/khive/(?:blob|tree)/[^/]+/docs/adr/ADR-",
                target,
                re.IGNORECASE,
            )
        )
    resolved = (source.parent / target).resolve()
    return resolved.parent == adr_dir.resolve() and resolved.name.upper().startswith("ADR-")


errors: list[str] = []
titles: dict[str, tuple[str, Path]] = {}

for path in sorted(adr_dir.glob("ADR-*.md")):
    file_match = adr_file_re.match(path.name)
    if file_match is None or re.match(r"^ADR-\d{3}-amendment-", path.name, re.IGNORECASE):
        continue
    h1 = first_h1(path)
    relative = path.relative_to(root)
    if h1 is None:
        errors.append(f"{relative}: missing ADR H1 heading")
        continue
    line_number, heading = h1
    heading_match = h1_re.match(heading)
    if heading_match is None:
        errors.append(f"{relative}:{line_number}: malformed ADR H1: {heading!r}")
        continue
    file_number = file_match.group(1)
    heading_number = heading_match.group("number")
    if file_number != heading_number:
        errors.append(
            f"{relative}:{line_number}: filename ADR-{file_number} does not match H1 ADR-{heading_number}"
        )
        continue
    if file_number in titles:
        other = titles[file_number][1].relative_to(root)
        errors.append(f"{relative}:{line_number}: duplicate ADR-{file_number}; also defined by {other}")
        continue
    titles[file_number] = (heading_match.group("title"), path)

index_path = adr_dir / "README.md"
with index_path.open(encoding="utf-8") as handle:
    for line_number, line in enumerate(handle, 1):
        match = index_row_re.match(line.rstrip("\n"))
        if match is None:
            continue
        number = match.group("number")
        relative = index_path.relative_to(root)
        canonical = titles.get(number)
        if canonical is None:
            errors.append(f"{relative}:{line_number}: ADR-{number} index entry has no authoritative file")
            continue
        expected, expected_path = canonical
        target_path = (adr_dir / match.group("target")).resolve()
        if target_path != expected_path.resolve():
            errors.append(
                f"{relative}:{line_number}: ADR-{number} index target mismatch; "
                f'expected "{expected_path.name}", found "{match.group("target")}"'
            )
        found = match.group("title")
        if normalize(found) != normalize(expected):
            errors.append(
                f'{relative}:{line_number}: ADR-{number} index title mismatch; '
                f'expected "{expected}", found "{found}"'
            )

scan_paths = set((root / "docs").glob("**/*.md"))
scan_paths.update((root / "crates").glob("**/docs/**/*.md"))
scan_paths.update((root / "crates").glob("**/design*.md"))

adr_dir_resolved = adr_dir.resolve()

reference_count = 0
for path in sorted(scan_paths):
    relative = path.relative_to(root)
    # docs/adr/**/*.md itself is excluded from prose-citation scanning: ADR
    # bodies routinely cross-reference sibling ADRs with a deliberately
    # abbreviated gloss ("(ADR-002: Edge Ontology governs the endpoint
    # contract)", "(ADR-001: Artifact entities)") rather than a literal title
    # restatement -- an established, reviewed convention, not drift. Headings
    # and links are still checked everywhere, including docs/adr/.
    prose_eligible = adr_dir_resolved not in path.resolve().parents
    in_fence = False
    with path.open(encoding="utf-8") as handle:
        for line_number, raw_line in enumerate(handle, 1):
            line = raw_line.rstrip("\n")
            if re.match(r"^\s*(```|~~~)", line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue

            labels: list[str] = []
            heading_match = heading_re.match(line)
            if heading_match is not None:
                body = heading_match.group("body")
                if not body.startswith("[") and adr_led_re.match(body):
                    labels.append(body)
            labels.extend(
                match.group("label")
                for match in link_re.finditer(line)
                if adr_led_re.match(match.group("label"))
                and is_local_adr_link(path, match.group("target"))
            )

            references: list[tuple[str, str]] = []
            for label in labels:
                for reference in titled_references(label):
                    if reference not in references:
                        references.append(reference)

            if heading_match is None and prose_eligible:
                for reference in prose_parenthetical_references(line):
                    if reference not in references:
                        references.append(reference)

            for number, found in references:
                reference_count += 1
                canonical = titles.get(number)
                if canonical is None:
                    errors.append(
                        f'{relative}:{line_number}: ADR-{number} has no authoritative file; found title "{found.strip()}"'
                    )
                    continue
                expected = canonical[0]
                if normalize(found) != normalize(expected):
                    errors.append(
                        f'{relative}:{line_number}: ADR-{number} title mismatch; expected "{expected}", found "{found.strip()}"'
                    )

if errors:
    for error in errors:
        print(error)
    print(f"\nADR reference lint: {len(errors)} issue(s)")
    raise SystemExit(1)

print(
    f"ADR reference lint: {len(scan_paths)} file(s), "
    f"{reference_count} titled reference(s) OK"
)
PY
