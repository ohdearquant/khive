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
    mkdir -p "$tmp/case-uncataloged/docs/adr"

    for case in case-fail case-pass case-uncataloged; do
        cat > "$tmp/$case/docs/adr/ADR-030-retrieval-stack-port.md" <<'FIXTURE'
# ADR-030: Retrieval Stack Port — khive-retrieval

**Status**: accepted
FIXTURE
        cat > "$tmp/$case/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-030](ADR-030-retrieval-stack-port.md) | Retrieval Stack Port — khive-retrieval |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    done

    # Regression case 3 (must-FAIL control for the catalog-coverage arm): the
    # same tree with the ADR-030 row deleted from the index must go red. The
    # arm was added because a merged ADR landed with no catalog row and the
    # lint passed; an assertion that has never been observed failing is not
    # yet a check, so this case IS that observation, kept permanent.
    grep -v '^| \[ADR-030\]' "$tmp/case-uncataloged/docs/adr/README.md" > "$tmp/case-uncataloged/README.tmp"
    mv "$tmp/case-uncataloged/README.tmp" "$tmp/case-uncataloged/docs/adr/README.md"

    # Regression case 4 (must-FAIL control, letter-suffixed ids): a letter-suffixed ADR id
    # (the real "ADR-117a" shape) is invisible to the old \d{3}-only file
    # pattern on both sides of the join -- neither added to the authoritative
    # set nor checked against the index -- so it silently passed uncatalogued.
    # A header-only index with no row for it must now go red.
    mkdir -p "$tmp/case-letter-uncataloged/docs/adr"
    cat > "$tmp/case-letter-uncataloged/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-uncataloged/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 5 (must-PASS control, letter-suffixed ids): the same letter-suffixed ADR
    # correctly catalogued must not be flagged -- proves the fix admits the
    # id rather than merely rejecting it everywhere.
    mkdir -p "$tmp/case-letter-cataloged/docs/adr"
    cat > "$tmp/case-letter-cataloged/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-cataloged/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 6 (must-FAIL control, inert catalog rows): the only index row for the
    # ADR sits inside an HTML comment spanning multiple lines -- a stale
    # example kept for reference, not a live catalog entry. Must go red.
    mkdir -p "$tmp/case-comment-row/docs/adr"
    cat > "$tmp/case-comment-row/docs/adr/ADR-201-fixture-comment-row.md" <<'FIXTURE'
# ADR-201: Fixture Comment Row

**Status**: accepted
FIXTURE
    cat > "$tmp/case-comment-row/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
<!--
| [ADR-201](ADR-201-fixture-comment-row.md) | Fixture Comment Row |
-->

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 7 (must-FAIL control, inert catalog rows): the only index row for the
    # ADR sits inside a fenced code block. Must go red.
    mkdir -p "$tmp/case-fenced-row/docs/adr"
    cat > "$tmp/case-fenced-row/docs/adr/ADR-202-fixture-fenced-row.md" <<'FIXTURE'
# ADR-202: Fixture Fenced Row

**Status**: accepted
FIXTURE
    cat > "$tmp/case-fenced-row/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
```
| [ADR-202](ADR-202-fixture-fenced-row.md) | Fixture Fenced Row |
```

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 8 (must-FAIL control, catalog boundary): the catalog
    # opens but never closes, and a valid-looking row sits in the trailing
    # prose past the intended boundary. Without an end-of-marker assertion the
    # scan runs to EOF and adopts that row as live coverage, so the file reads
    # as fully catalogued.
    mkdir -p "$tmp/case-unclosed-catalog/docs/adr"
    cat > "$tmp/case-unclosed-catalog/docs/adr/ADR-203-fixture-unclosed.md" <<'FIXTURE'
# ADR-203: Fixture Unclosed Catalog

**Status**: accepted
FIXTURE
    cat > "$tmp/case-unclosed-catalog/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |

## Superseded entries kept for reference

| [ADR-203](ADR-203-fixture-unclosed.md) | Fixture Unclosed Catalog |
FIXTURE

    # Regression case 9 (must-FAIL control, titled reference): a titled prose
    # citation of a letter-suffixed ADR carrying the wrong title. The coverage
    # grammar admitted such ids before the reference recognizers did, so an
    # ADR-117a citation could restate any title at all and never be compared
    # against the authoritative H1.
    mkdir -p "$tmp/case-letter-ref/docs/adr" "$tmp/case-letter-ref/crates/fixture-crate/docs"
    cat > "$tmp/case-letter-ref/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-ref/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    cat > "$tmp/case-letter-ref/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- follows the suffix convention (ADR-117a: Deliberately Wrong Title).
FIXTURE

    # Regression case 10 (must-PASS control, titled reference): the same
    # citation restating the real title must stay green -- proves case 9 fails
    # on the title comparison rather than on the widened id shape itself.
    mkdir -p "$tmp/case-letter-ref-ok/docs/adr" "$tmp/case-letter-ref-ok/crates/fixture-crate/docs"
    cat > "$tmp/case-letter-ref-ok/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-ref-ok/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    cat > "$tmp/case-letter-ref-ok/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- follows the suffix convention (ADR-117a: Fixture Letter Suffix).
FIXTURE

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

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-uncataloged" > "$tmp/uncataloged.log" 2>&1; then
        echo "self-test FAILED: ADR file with no index catalog row was not caught"
        cat "$tmp/uncataloged.log"
        status=1
    elif ! grep -q "ADR-030 (ADR-030-retrieval-stack-port.md) has no index catalog row" "$tmp/uncataloged.log"; then
        echo "self-test FAILED: uncataloged lint failed, but not for the expected reason:"
        cat "$tmp/uncataloged.log"
        status=1
    else
        echo "self-test OK: uncataloged ADR caught"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-uncataloged" > "$tmp/letter-uncataloged.log" 2>&1; then
        echo "self-test FAILED: uncatalogued letter-suffixed ADR-117a was not caught"
        cat "$tmp/letter-uncataloged.log"
        status=1
    elif ! grep -q "ADR-117a (ADR-117a-fixture-letter-suffix.md) has no index catalog row" "$tmp/letter-uncataloged.log"; then
        echo "self-test FAILED: letter-suffixed lint failed, but not for the expected reason:"
        cat "$tmp/letter-uncataloged.log"
        status=1
    else
        echo "self-test OK: uncatalogued letter-suffixed ADR caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-cataloged" > "$tmp/letter-cataloged.log" 2>&1; then
        echo "self-test FAILED: correctly catalogued letter-suffixed ADR-117a should not trip the lint"
        cat "$tmp/letter-cataloged.log"
        status=1
    else
        echo "self-test OK: correctly catalogued letter-suffixed ADR does not false-positive"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-comment-row" > "$tmp/comment-row.log" 2>&1; then
        echo "self-test FAILED: index row hidden inside an HTML comment was counted as catalog coverage"
        cat "$tmp/comment-row.log"
        status=1
    elif ! grep -q "ADR-201 (ADR-201-fixture-comment-row.md) has no index catalog row" "$tmp/comment-row.log"; then
        echo "self-test FAILED: comment-row lint failed, but not for the expected reason:"
        cat "$tmp/comment-row.log"
        status=1
    else
        echo "self-test OK: index row hidden inside an HTML comment does not count as coverage"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-fenced-row" > "$tmp/fenced-row.log" 2>&1; then
        echo "self-test FAILED: index row hidden inside a fenced code block was counted as catalog coverage"
        cat "$tmp/fenced-row.log"
        status=1
    elif ! grep -q "ADR-202 (ADR-202-fixture-fenced-row.md) has no index catalog row" "$tmp/fenced-row.log"; then
        echo "self-test FAILED: fenced-row lint failed, but not for the expected reason:"
        cat "$tmp/fenced-row.log"
        status=1
    else
        echo "self-test OK: index row hidden inside a fenced code block does not count as coverage"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-unclosed-catalog" > "$tmp/unclosed.log" 2>&1; then
        echo "self-test FAILED: catalog opened but never closed was accepted, and a row past the intended boundary counted as coverage"
        cat "$tmp/unclosed.log"
        status=1
    elif ! grep -q 'missing "<!-- END GENERATED ADR CATALOG -->" marker' "$tmp/unclosed.log"; then
        echo "self-test FAILED: unclosed-catalog lint failed, but not for the expected reason:"
        cat "$tmp/unclosed.log"
        status=1
    else
        echo "self-test OK: catalog with no end marker caught"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-ref" > "$tmp/letter-ref.log" 2>&1; then
        echo "self-test FAILED: titled citation of letter-suffixed ADR-117a restating a wrong title was not caught"
        cat "$tmp/letter-ref.log"
        status=1
    elif ! grep -q "ADR-117a title mismatch" "$tmp/letter-ref.log"; then
        echo "self-test FAILED: letter-suffixed reference lint failed, but not for the expected reason:"
        cat "$tmp/letter-ref.log"
        status=1
    else
        echo "self-test OK: wrong-titled citation of a letter-suffixed ADR caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-ref-ok" > "$tmp/letter-ref-ok.log" 2>&1; then
        echo "self-test FAILED: correctly titled citation of letter-suffixed ADR-117a should not trip the lint"
        cat "$tmp/letter-ref-ok.log"
        status=1
    else
        echo "self-test OK: correctly titled citation of a letter-suffixed ADR does not false-positive"
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
# Shared by the authoritative-file, H1-header, and index-row patterns so the
# three agree on what an ADR id looks like -- e.g. the letter-suffixed
# "ADR-117a" naming a follow-on ADR that shares its parent's number. Captured
# numbers are lowercased at every use site so "117A" and "117a" join.
ADR_NUMBER = r"\d{3}[a-z]?"
adr_file_re = re.compile(rf"^ADR-({ADR_NUMBER})-.*\.md$", re.IGNORECASE)
h1_re = re.compile(
    rf"^#\s+ADR-(?P<number>{ADR_NUMBER})(?:\s+Rev\s+\d+)?\s*:\s*(?P<title>.+?)\s*#*\s*$",
    re.IGNORECASE,
)
colon_ref_re = re.compile(rf"\bADR-(?P<number>{ADR_NUMBER})\s*:\s*", re.IGNORECASE)
paren_ref_re = re.compile(rf"\bADR-(?P<number>{ADR_NUMBER})\s+\(", re.IGNORECASE)
dash_ref_re = re.compile(
    rf"\bADR-(?P<number>{ADR_NUMBER})\s+(?:--?|–|—)\s+", re.IGNORECASE
)
heading_re = re.compile(r"^\s{0,3}#{1,6}\s+(?P<body>.+?)\s*#*\s*$")
link_re = re.compile(r"\[(?P<label>[^]\n]+)\]\((?P<target>[^\n)]+)\)")
adr_led_re = re.compile(rf"^(?:\[)?ADR-{ADR_NUMBER}\b", re.IGNORECASE)
index_row_re = re.compile(
    rf"^\|\s*\[ADR-(?P<number>{ADR_NUMBER})\]\((?P<target>[^)]+)\)\s*"
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
            references.append((inner_match.group("number").lower(), title))
    return references


def titled_references(label: str) -> list[tuple[str, str]]:
    references: list[tuple[str, str]] = []
    for match in colon_ref_re.finditer(label):
        references.append((match.group("number").lower(), label[match.end() :]))
    for match in paren_ref_re.finditer(label):
        title = parenthesized_title(label, match)
        if title is not None:
            references.append((match.group("number").lower(), title))
    for match in dash_ref_re.finditer(label):
        references.append((match.group("number").lower(), label[match.end() :]))
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
    file_number = file_match.group(1).lower()
    heading_number = heading_match.group("number").lower()
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
relative = index_path.relative_to(root)
# Scoped to the generated catalog table's own markers rather than the whole
# file: a prose table elsewhere in README.md, or a row sitting inside an HTML
# comment or fenced block *within* the marked region (e.g. a stale example
# kept for reference), must never count as catalog coverage.
catalog_begin = "<!-- BEGIN GENERATED ADR CATALOG -->"
catalog_end = "<!-- END GENERATED ADR CATALOG -->"
cataloged_numbers: set[str] = set()
in_catalog = False
saw_catalog_end = False
in_fence = False
in_comment = False
with index_path.open(encoding="utf-8") as handle:
    for line_number, raw_line in enumerate(handle, 1):
        line = raw_line.rstrip("\n")
        if not in_catalog:
            if line.strip() == catalog_begin:
                in_catalog = True
            continue
        if line.strip() == catalog_end:
            saw_catalog_end = True
            break
        if re.match(r"^\s*(```|~~~)", line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if in_comment:
            if "-->" in line:
                in_comment = False
            continue
        if "<!--" in line:
            if "-->" not in line:
                in_comment = True
            continue
        match = index_row_re.match(line)
        if match is None:
            continue
        number = match.group("number").lower()
        cataloged_numbers.add(number)
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
if not in_catalog:
    errors.append(
        f'{relative}: missing "{catalog_begin}" marker; catalog-coverage scan did not run'
    )
elif not saw_catalog_end:
    # Reaching EOF still inside the catalog means the scan silently consumed
    # every line after the intended boundary. Any later Markdown table row --
    # a stale example, a prose table -- then counts as live coverage, so an
    # unclosed catalog can read as fully covered. The boundary is only a
    # boundary if both ends are asserted.
    errors.append(
        f'{relative}: missing "{catalog_end}" marker; catalog-coverage scan read to end of file'
    )

# Catalog coverage: every authoritative ADR file must have an index row.
# The index and the tree can otherwise drift silently -- a merged ADR with no
# catalog row passes every per-reference check above because no reference to
# it exists to check.
index_relative = index_path.relative_to(root)
for number in sorted(titles):
    if number not in cataloged_numbers:
        missing_name = titles[number][1].name
        errors.append(
            f"{index_relative}: ADR-{number} ({missing_name}) has no index catalog row"
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
