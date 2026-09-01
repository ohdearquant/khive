#!/usr/bin/env python3
"""Check that every ADR declares exactly one status, in its document header.

An ADR's status is a claim other tooling reads: which decisions are in force,
which are proposals, which have been withdrawn. Reading it looks trivial and is
not, because the word "Status" appears in ADRs in two unrelated positions:

  * the document header, where it states the status of the decision, and
  * inside a later section, where an amendment or a sub-proposal states its own.

A parser that takes the first status-shaped line in the file gets the second one
whenever the header uses a form the parser does not recognise. That is not
hypothetical: ADR-051 declares its header status under a `## Status` heading with
the value on the following line, and carries `Status: proposed.` inside
`## Amendment 1` two hundred lines later. A colon-anchored search skips the
heading, finds the amendment, and reports an accepted-and-implemented decision as
a proposal. Nothing about the output looks wrong.

So the rule this enforces is about SCOPE, not spelling. Within the document
header there must be exactly one status declaration, and its value must be a word
from the known vocabulary. Below the header, anything goes: an amendment stating
its own status is legitimate and is deliberately not touched.

Three declaration forms are accepted, because all three are in live use across
the corpus and each is internally consistent within the file that uses it:

    **Status**: Accepted            bold form, with or without a trailing `\\`
    - Status: Accepted              list form, inside a bulleted metadata block
    - **Status:** Accepted          list form, bold label
    ## Status                       heading form, value on the next non-empty line
    Accepted

The list form's siblings (`- Date:`, `- Supersedes:`) make it a block, and the
bold form's trailing backslashes are markdown hard breaks holding the header
lines together as one rendered paragraph. Rewriting either into the other is a
rendering change with no parsing benefit, which is why this checks scope and
leaves form alone.

Usage:

    uv run scripts/lint-adr-status.py              # check the repository
    uv run scripts/lint-adr-status.py --self-test  # check the checker

Exit status: 1 if any ADR fails the rule or if the corpus is unexpectedly empty,
0 otherwise.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ADR_DIR = ROOT / "docs" / "adr"

# Files named ADR-*.md are decision records. Everything else in the directory
# (the index, feasibility notes) carries no status and is not subject to this.
ADR_GLOB = "ADR-*.md"

# A status value must begin with one of these. The set is deliberately closed:
# an unrecognised value is far more likely to be a typo or a sentence that
# happens to start with "Status:" than a new lifecycle state, and adding a state
# should be a deliberate edit here.
KNOWN_STATUSES = (
    "accepted",
    "proposed",
    "draft",
    "superseded",
    "withdrawn",
    "rejected",
    "deprecated",
    "implemented",
)

# `**Status**:` / `- Status:` / `- **Status:**` / bare `Status:`
INLINE_STATUS = re.compile(r"^\s*(?:-\s*)?\*{0,2}\s*status\s*\*{0,2}\s*:", re.IGNORECASE)
# The same label ANYWHERE in a line. One line carrying two of these carries two
# declarations, and taking only the first would silently prefer one of them —
# the exact defect this lint exists to reject when the copies sit on two lines.
INLINE_STATUS_ANYWHERE = re.compile(r"\*{0,2}\bstatus\s*\*{0,2}\s*:", re.IGNORECASE)
# `## Status`, any heading level, nothing else on the line
HEADING_STATUS = re.compile(r"^#{1,6}\s+status\s*$", re.IGNORECASE)
# Any other `## ` heading closes the document header.
SECTION_HEADING = re.compile(r"^#{2,6}\s+\S")
# Fenced code blocks and HTML comments are display text, not declarations. A
# line-based parser with no notion of either counts a status-shaped example
# inside them as real, which lets an ADR with no visible header status pass.
FENCE = re.compile(r"^\s*(```|~~~)")
# The cap exists so a special file or an oversized substitute cannot make CI
# read without bound; a plausible ADR is orders of magnitude smaller.
MAX_ADR_BYTES = 1 << 20


class Declaration:
    """One status declaration found in a document header."""

    def __init__(self, line_no: int, value: str, form: str) -> None:
        self.line_no = line_no
        self.value = value
        self.form = form


def header_declarations(lines: list[str]) -> list[Declaration]:
    """Every status declaration inside the document header.

    The header runs from the top of the file to the first section heading that
    is not itself a Status heading. Returning a list rather than the first hit
    is what lets the caller reject a second declaration instead of silently
    preferring one of them.
    """
    found: list[Declaration] = []
    in_fence = False
    in_comment = False
    for i, line in enumerate(lines):
        # Fence and HTML-comment state first: nothing inside either is a
        # declaration, and a fence marker must not be read as a value either.
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if in_comment:
            if "-->" in line:
                in_comment = False
            continue
        stripped = line.strip()
        if stripped.startswith("<!--"):
            if "-->" not in stripped:
                in_comment = True
            continue
        if SECTION_HEADING.match(line) and not HEADING_STATUS.match(line):
            break
        if HEADING_STATUS.match(line):
            for j in range(i + 1, len(lines)):
                if FENCE.match(lines[j]):
                    break  # a fenced value is display text, not a declaration
                if lines[j].strip():
                    found.append(Declaration(j + 1, lines[j].strip(), "heading"))
                    break
            continue
        if INLINE_STATUS.match(line):
            # Every label on the line is a declaration. Taking only the first
            # would let `**Status**: accepted **Status**: proposed` pass as a
            # single valid declaration whose rider swallows the second one.
            labels = list(INLINE_STATUS_ANYWHERE.finditer(line))
            for k, m in enumerate(labels):
                end = labels[k + 1].start() if k + 1 < len(labels) else len(line)
                value = line[m.end() : end].strip()
                found.append(Declaration(i + 1, value, "inline"))
    return found


def status_word(value: str) -> str | None:
    """The lifecycle word a status value starts with, if it is a known one.

    Values carry riders — "Accepted/Ratified (2026-06-19)", "Superseded by
    ADR-102", "Proposed" followed by a hard-break backslash — so this reads the
    leading word rather than requiring an exact match.
    """
    cleaned = value.lstrip("*").strip().lower()
    for word in KNOWN_STATUSES:
        if not cleaned.startswith(word):
            continue
        # Token boundary: the character after the matched word must not extend
        # it. Without this, `acceptedish` passes as `accepted`. Riders remain
        # fine — `/`, ` `, `(`, `\` and end-of-value are all boundaries.
        rest = cleaned[len(word) :]
        if rest and (rest[0].isalnum() or rest[0] == "_"):
            continue
        return word
    return None


def read_adr_contained(path: Path) -> str | None:
    """Read an ADR without following links or trusting the file's shape.

    This runs in CI on pull-request content, so the file under a trusted name
    may be an attacker's: a symlink pointing out of the tree (whose contents
    would then be echoed into public CI logs by the messages below), or a
    special file that blocks the read forever. `O_NOFOLLOW` refuses the link at
    open time rather than racing a separate check; `fstat` on the open
    descriptor refuses non-regular files; the read is capped at MAX_ADR_BYTES.
    `None` means the file was refused — the caller reports that as a failure,
    never as a clean skip.
    """
    import os
    import stat as stat_mod

    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError:
        return None
    try:
        st = os.fstat(fd)
        if not stat_mod.S_ISREG(st.st_mode) or st.st_size > MAX_ADR_BYTES:
            return None
        return os.read(fd, MAX_ADR_BYTES).decode("utf-8", errors="replace")
    finally:
        os.close(fd)


def check_file(path: Path) -> list[str]:
    """Rule violations in one ADR, as printable messages. Empty means it passes."""
    text = read_adr_contained(path)
    if text is None:
        return [
            f"{path.name}: refused — not a regular readable file under the size cap "
            f"(symlinks, special files, and oversized files are not lintable ADRs)."
        ]
    lines = text.splitlines()
    decls = header_declarations(lines)
    name = path.name

    if not decls:
        return [
            f"{name}: no status declaration in the document header. "
            f"A status further down the file states an amendment's status, not the record's."
        ]
    if len(decls) > 1:
        where = ", ".join(f"line {d.line_no}" for d in decls)
        return [
            f"{name}: {len(decls)} status declarations in the document header ({where}). "
            f"Exactly one is readable; more than one leaves which is authoritative undecided."
        ]

    decl = decls[0]
    if status_word(decl.value) is None:
        shown = decl.value[:60]
        return [
            f"{name}:{decl.line_no}: status value {shown!r} does not begin with a known "
            f"status ({', '.join(KNOWN_STATUSES)})."
        ]
    return []


def check_tree(adr_dir: Path) -> int:
    paths = sorted(adr_dir.glob(ADR_GLOB))
    if not paths:
        print(
            f"lint-adr-status: no files matching {ADR_GLOB} under {adr_dir}. "
            f"An empty corpus is a broken invocation, not a clean result.",
            file=sys.stderr,
        )
        return 1

    failures: list[str] = []
    for path in paths:
        failures.extend(check_file(path))

    for message in failures:
        print(f"ERROR: {message}", file=sys.stderr)
    print(f"lint-adr-status: checked {len(paths)} records, {len(failures)} failing.")
    return 1 if failures else 0


# --------------------------------------------------------------------------
# Self-test
#
# A checker whose only evidence is that the live corpus passes cannot tell a
# working rule from a rule that never fires. Each case below is a whole synthetic
# ADR: the must-fail set reproduces the defects this exists to catch, and the
# must-pass set pins the three legitimate forms so a future tightening cannot
# quietly outlaw one of them.
# --------------------------------------------------------------------------

MUST_FAIL = {
    # The defect this lint was written for, in its pure form: the header carries
    # no status, and a section far below carries one. A first-match parser
    # reports "proposed"; the correct answer is that the record does not say.
    "ADR-901-amendment-status-only.md": (
        "# ADR-901: Something\n"
        "\n"
        "**Date**: 2026-01-01\n"
        "\n"
        "## Context\n"
        "\n"
        "Body.\n"
        "\n"
        "## Amendment 1: A later proposal\n"
        "\n"
        "Status: proposed.\n"
    ),
    # Two header declarations. Neither is wrong on its face, and that is the
    # point: which one is authoritative is undecided, so the record is unreadable.
    "ADR-902-two-header-statuses.md": (
        "# ADR-902: Something\n"
        "\n"
        "**Status**: Accepted\n"
        "- Status: Proposed\n"
        "\n"
        "## Context\n"
    ),
    # A value that is not a lifecycle word at all.
    "ADR-903-unknown-value.md": (
        "# ADR-903: Something\n"
        "\n"
        "**Status**: see the discussion thread\n"
        "\n"
        "## Context\n"
    ),
    # A malformed value that merely BEGINS with a lifecycle word. A prefix
    # match without a token boundary reads this as `accepted`.
    "ADR-904-token-boundary.md": (
        "# ADR-904: Something\n"
        "\n"
        "**Status**: acceptedish\n"
        "\n"
        "## Context\n"
    ),
    # Two declarations on ONE line. A first-match parser reads this as a single
    # valid declaration whose rider swallows the second — the same undecided
    # authority as ADR-902, hidden by line packing.
    "ADR-905-two-on-one-line.md": (
        "# ADR-905: Something\n"
        "\n"
        "**Status**: accepted **Status**: proposed\n"
        "\n"
        "## Context\n"
    ),
    # The only status-shaped text is display text: a fenced example and an HTML
    # comment. Neither is a declaration, so the record declares nothing and
    # must fail — a fence-blind parser passes it.
    "ADR-906-fenced-status-only.md": (
        "# ADR-906: Something\n"
        "\n"
        "```markdown\n"
        "Status: accepted\n"
        "```\n"
        "\n"
        "<!-- Status: accepted -->\n"
        "\n"
        "## Context\n"
    ),
}

MUST_PASS = {
    # Bold form with the trailing hard-break backslash the corpus uses to hold
    # the header block together.
    "ADR-911-bold-backslash.md": (
        "# ADR-911: Something\n"
        "\n"
        "**Status**: accepted\\\n"
        "**Date**: 2026-05-22\\\n"
        "**Authors**: maintainers\n"
        "\n"
        "## Context\n"
    ),
    # List form: the status is one bullet in a metadata block.
    "ADR-912-list-block.md": (
        "# ADR-912: Something\n"
        "\n"
        "- **Status:** Proposed\n"
        "- **Date:** 2026-07-29\n"
        "- **Relates to:** ADR-094\n"
        "\n"
        "## Context\n"
    ),
    # Heading form WITH a later amendment that states its own status. This is
    # ADR-051's exact shape and the case that motivated the whole check: the
    # header answer is "Accepted", and the amendment's "proposed" must not
    # displace it.
    "ADR-913-heading-plus-amendment.md": (
        "# ADR-913: Something\n"
        "\n"
        "## Status\n"
        "\n"
        "Accepted (2026-06-07). **Fully implemented** (2026-06-08).\n"
        "\n"
        "## Context\n"
        "\n"
        "Body.\n"
        "\n"
        "## Amendment 1: A later proposal\n"
        "\n"
        "Status: proposed.\n"
    ),
    # Riders on the value must not trip the vocabulary check.
    "ADR-914-value-with-rider.md": (
        "# ADR-914: Something\n"
        "\n"
        "**Status**: Accepted/Ratified (2026-06-19)\n"
        "\n"
        "## Context\n"
    ),
    # A fenced example in the header must not displace or duplicate the real
    # declaration beside it — the fence-tracking fix must ignore display text
    # without breaking a legitimate header that contains both.
    "ADR-915-real-status-plus-fenced-example.md": (
        "# ADR-915: Something\n"
        "\n"
        "**Status**: Accepted\n"
        "\n"
        "```text\n"
        "Status: proposed   <- example output, not a declaration\n"
        "```\n"
        "\n"
        "## Context\n"
    ),
}


def self_test() -> int:
    problems = 0
    with tempfile.TemporaryDirectory() as tmp:
        adr_dir = Path(tmp) / "docs" / "adr"
        adr_dir.mkdir(parents=True)

        for name, body in MUST_FAIL.items():
            path = adr_dir / name
            path.write_text(body, encoding="utf-8")
            if not check_file(path):
                print(f"SELF-TEST: {name} was accepted but must fail.", file=sys.stderr)
                problems += 1
            path.unlink()

        for name, body in MUST_PASS.items():
            path = adr_dir / name
            path.write_text(body, encoding="utf-8")
            failures = check_file(path)
            if failures:
                print(
                    f"SELF-TEST: {name} must pass but was rejected: {failures[0]}",
                    file=sys.stderr,
                )
                problems += 1
            path.unlink()

        # The empty-corpus arm: a glob that matches nothing must be reported as a
        # broken invocation, never as a clean pass. Without this the whole check
        # fails toward looking green when it is pointed at the wrong directory.
        # It prints its own refusal to stderr, which is the point of the arm and
        # not a failure of the run — hence the announcement, so a reader of the
        # CI log does not mistake the expected refusal for a broken build.
        # On stderr, not stdout: the refusal it announces is written to stderr,
        # and announcing it on the other stream leaves the ordering to whichever
        # buffer flushes first — which puts the announcement after the thing it
        # announces.
        print("lint-adr-status self-test: the next line is the expected "
              "empty-corpus refusal.", file=sys.stderr)
        if check_tree(adr_dir) == 0:
            print(
                "SELF-TEST: an empty ADR directory was reported clean; "
                "an empty corpus must fail.",
                file=sys.stderr,
            )
            problems += 1

        # Symlink arm: a link under a trusted ADR name must be REFUSED, never
        # read. The target is a real file with a valid header, so a parser that
        # follows the link would report a clean pass — refusal is the only
        # correct outcome, and it must arrive as a failure, not a skip.
        target = Path(tmp) / "outside-the-adr-dir.md"
        target.write_text("# X\n\n**Status**: accepted\n\n## Context\n", encoding="utf-8")
        link = adr_dir / "ADR-907-symlink.md"
        link.symlink_to(target)
        if not check_file(link):
            print("SELF-TEST: a symlinked ADR was linted instead of refused.", file=sys.stderr)
            problems += 1
        link.unlink()

        # Non-empty tree arm: the CLI path is check_tree, and a self-test that
        # only ever calls check_file cannot see a regression in discovery or
        # aggregation. One passing and one failing record: rc must be 1.
        (adr_dir / "ADR-920-pass.md").write_text(
            "# ADR-920\n\n**Status**: accepted\n\n## Context\n", encoding="utf-8"
        )
        (adr_dir / "ADR-921-fail.md").write_text(
            "# ADR-921\n\n**Date**: 2026-01-01\n\n## Context\n", encoding="utf-8"
        )
        print("lint-adr-status self-test: the next lines are the expected "
              "mixed-tree failure report.", file=sys.stderr)
        if check_tree(adr_dir) != 1:
            print(
                "SELF-TEST: a tree with one failing record did not exit 1 from check_tree.",
                file=sys.stderr,
            )
            problems += 1

    total = len(MUST_FAIL) + len(MUST_PASS) + 3
    print(f"lint-adr-status self-test: {total - problems}/{total} cases correct.")
    return 1 if problems else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the checker against synthetic records instead of the repository",
    )
    args = parser.parse_args()
    return self_test() if args.self_test else check_tree(ADR_DIR)


if __name__ == "__main__":
    sys.exit(main())
