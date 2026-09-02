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
# A backtick-delimited inline code span. CommonMark opens and closes a span on
# a run of the SAME length, so a shorter or longer backtick run inside does
# not close it early — the backreference enforces that.
INLINE_CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)*?\1")
# The destination portion of a Markdown link, `](...)`. A status-shaped
# substring inside a URL (a path segment, a query string) is not a label.
LINK_DESTINATION = re.compile(r"\]\([^)]*\)")
# `## Status`, any heading level, nothing else on the line
HEADING_STATUS = re.compile(r"^#{1,6}\s+status\s*$", re.IGNORECASE)
# Any other `## ` heading closes the document header.
SECTION_HEADING = re.compile(r"^#{2,6}\s+\S")
# Fenced code blocks and HTML comments are display text, not declarations. A
# line-based parser with no notion of either counts a status-shaped example
# inside them as real, which lets an ADR with no visible header status pass.
# Fence lines follow CommonMark: at most three spaces of indentation (a
# tab or a fourth space makes the line indented code, not a fence), and a
# backtick fence's info string may not contain a backtick. A fence closes
# only on the opener's own character, at least as long as the opener, with
# only spaces or tabs after. Any looser recognition lets a lookalike line —
# an opposite-character fence, an info-text or shorter run, or a 4-space
# indented one — end a block early, turning fenced display text back into
# visible text, which reads a fenced-only status as a real declaration.
FENCE_OPEN = re.compile(r"^ {0,3}(?P<chars>`{3,}|~{3,})(?P<info>.*)$")
# A line indented 4+ spaces or led by a tab is CommonMark indented code, the
# same display-text treatment as a fence. The simplified rule implemented
# here: such a line counts as code only when the line immediately before it
# is blank, or it is the first line of the file. CommonMark's real rule is
# narrower — indented code cannot interrupt an actively continuing paragraph
# even across a non-blank predecessor that is itself a heading or a fence —
# but this checker does not distinguish predecessor kinds, so it always
# requires a blank line. That is the conservative direction: a missed
# indented-code line still reads as an ordinary line and gets matched
# normally, it is never mistaken for a hidden declaration.
INDENTED_CODE = re.compile(r"^(?: {4,}|\t)\S")
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
    visible, hidden = visible_view(lines)
    found: list[Declaration] = []
    for i, line in enumerate(visible):
        if hidden[i] or line is None:
            continue
        if SECTION_HEADING.match(line) and not HEADING_STATUS.match(line):
            break
        if HEADING_STATUS.match(line):
            for j in range(i + 1, len(lines)):
                if hidden[j]:
                    break  # a hidden (fenced/indented) value is display text
                v = visible[j]
                if v is not None and v.strip():
                    found.append(Declaration(j + 1, v.strip(), "heading"))
                    break
            continue
        if INLINE_STATUS.match(line):
            # Every label on the line is a declaration. Taking only the first
            # would let `**Status**: accepted **Status**: proposed` pass as a
            # single valid declaration whose rider swallows the second one.
            # Inline code spans and link destinations are stripped first: a
            # status-shaped substring inside either is display text or a URL
            # fragment, not a rider declaration.
            scan_line = strip_inline_noise(line)
            labels = list(INLINE_STATUS_ANYWHERE.finditer(scan_line))
            for k, m in enumerate(labels):
                end = labels[k + 1].start() if k + 1 < len(labels) else len(scan_line)
                value = scan_line[m.end() : end].strip()
                found.append(Declaration(i + 1, value, "inline"))
    return found


def strip_comments(line: str, in_comment: bool) -> tuple[str, bool]:
    """The text of one line with HTML-comment spans removed, plus the state.

    Character-position based, not `startswith`: a comment that opens after
    other text on the line (`Some text <!-- hidden`) must still open, or every
    status-shaped line until the matching `-->` is parsed as a declaration.
    A declaration BEFORE the comment opener on the same line survives.
    """
    out: list[str] = []
    i = 0
    while True:
        if in_comment:
            j = line.find("-->", i)
            if j == -1:
                return "".join(out), True
            i = j + 3
            in_comment = False
        else:
            j = line.find("<!--", i)
            if j == -1:
                out.append(line[i:])
                return "".join(out), False
            out.append(line[i:j])
            i = j + 4
            in_comment = True


def strip_inline_noise(line: str) -> str:
    """`line` with inline code spans and link destinations removed.

    Used only for counting "Status:" labels on an already-matched header
    line: a label inside a backtick-delimited code span or a Markdown link
    destination (`](...)`) is display text or a URL fragment, not a second
    declaration.
    """
    line = INLINE_CODE_SPAN.sub("", line)
    return LINK_DESTINATION.sub("", line)


def visible_view(lines: list[str]) -> tuple[list[str | None], list[bool]]:
    """Per-line declaration-relevant text: fences, indented code, and comments removed.

    `visible[i]` is the line with comment spans stripped, or None inside a
    fenced or indented-code block; `hidden[i]` marks every line that carries
    no declaration-relevant text at all — fence delimiters and indented-code
    lines alike. A fenced block closes only on its opener's own character, at
    least as many of them, and nothing but whitespace after (CommonMark);
    other fence-shaped lines inside the block are content. Fence openers
    inside a comment do not open a fence, and comment openers inside a fence
    do not open a comment — each construct is display text within the other.
    An indented-code line (see `INDENTED_CODE`) is recognised the same way,
    only outside a fence and outside a comment; `prev_blank` tracks whether
    the immediately preceding line was blank, which is what the simplified
    indented-code rule keys on.
    """
    visible: list[str | None] = []
    hidden: list[bool] = []
    fence_close: re.Pattern[str] | None = None
    in_comment = False
    prev_blank = True
    for line in lines:
        if fence_close is not None:
            if fence_close.match(line):
                fence_close = None
                visible.append(None)
                hidden.append(True)
            else:
                visible.append(None)
                hidden.append(False)
            prev_blank = line.strip() == ""
            continue
        if not in_comment:
            opener = FENCE_OPEN.match(line)
            if opener and not (
                opener.group("chars")[0] == "`" and "`" in opener.group("info")
            ):
                chars = opener.group("chars")
                fence_close = re.compile(
                    rf"^ {{0,3}}{re.escape(chars[0])}{{{len(chars)},}}[ \t]*$"
                )
                visible.append(None)
                hidden.append(True)
                prev_blank = False
                continue
            if prev_blank and INDENTED_CODE.match(line):
                visible.append(None)
                hidden.append(True)
                prev_blank = line.strip() == ""
                continue
        text, in_comment = strip_comments(line, in_comment)
        visible.append(text)
        hidden.append(False)
        prev_blank = line.strip() == ""
    return visible, hidden


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


def symlinked_base_component(base_dir: Path) -> Path | None:
    """`base_dir` or its parent, whichever is a symlink — None if neither is.

    `read_adr_contained`'s containment check compares `os.path.realpath` of
    both the candidate file's parent and `base_dir`; if `base_dir` itself, or
    its parent (the repository's `docs`), is a symlink, both operands resolve
    through the same link and the comparison agrees even though the tree
    `check_tree` enumerates lives elsewhere. Only these two components are
    checked — walking further up the path would trip on symlinks the
    surrounding filesystem carries for unrelated reasons (a machine's
    temp-directory root, for one).
    """
    for candidate in (base_dir, base_dir.parent):
        if candidate.is_symlink():
            return candidate
    return None


def read_adr_contained(path: Path, base_dir: Path) -> str | None:
    """Read an ADR without following links or trusting the file's shape.

    This runs in CI on pull-request content, so the file under a trusted name
    may be an attacker's: a path outside the ADR tree, a symlink pointing out
    of the tree (whose contents would then be echoed into public CI logs by
    the messages below), or a special file that blocks the read.

    Containment first: the path itself must live directly under `base_dir` —
    O_NOFOLLOW and fstat validate the FILE, not the PATH, and neither stops a
    caller handed `../../secrets.md`. Then `O_NOFOLLOW` refuses a symlink at
    open time rather than racing a separate check, and `O_NONBLOCK` makes the
    open RETURN on a FIFO instead of blocking until a writer appears — without
    it, CI hangs at `os.open` and the `S_ISREG` rejection below is never
    reached (O_NOFOLLOW covers links only, it does nothing for FIFOs).
    `O_NONBLOCK` is a no-op for regular-file reads, so the capped read below
    is unaffected. `None` means the file was refused — the caller reports
    that as a failure, never as a clean skip.
    """
    import os
    import stat as stat_mod

    resolved_parent = Path(os.path.realpath(path.parent))
    if resolved_parent != Path(os.path.realpath(base_dir)):
        return None
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        return None
    try:
        st = os.fstat(fd)
        if not stat_mod.S_ISREG(st.st_mode) or st.st_size > MAX_ADR_BYTES:
            return None
        return os.read(fd, MAX_ADR_BYTES).decode("utf-8", errors="replace")
    finally:
        os.close(fd)


def check_file(path: Path, base_dir: Path | None = None) -> list[str]:
    """Rule violations in one ADR, as printable messages. Empty means it passes."""
    text = read_adr_contained(path, base_dir if base_dir is not None else ADR_DIR)
    if text is None:
        return [
            f"{path.name}: refused — not a regular readable file under the size cap "
            f"inside the ADR directory (out-of-tree paths, symlinks, special files, "
            f"and oversized files are not lintable ADRs)."
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
    symlinked = symlinked_base_component(adr_dir)
    if symlinked is not None:
        print(
            f"lint-adr-status: refusing to lint {adr_dir} — {symlinked} is a "
            f"symlink. A symlinked ADR directory can make the containment "
            f"check match a tree outside the intended one.",
            file=sys.stderr,
        )
        return 1

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
        failures.extend(check_file(path, base_dir=adr_dir))

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
    # Fence-breaking lines INSIDE the block: an opposite-character fence line,
    # a same-character line with info text, a SHORTER same-character run, and
    # a 4-space-indented same-character run. None of them closes the block
    # under CommonMark, so the only status text stays fenced display text and
    # the record declares nothing — a parser that toggles on any
    # fence-prefixed line, or accepts arbitrary closer indentation, closes
    # the block early, reads the status as visible, and wrongly accepts this
    # file.
    "ADR-907-fence-not-closed-by-lookalikes.md": (
        "# ADR-907: Something\n"
        "\n"
        "````markdown\n"
        "~~~not-a-closing-fence\n"
        "```` info-text-means-not-a-closer\n"
        "```\n"
        "    ````\n"
        "Status: accepted\n"
        "````\n"
        "\n"
        "## Context\n"
    ),
    # A comment that OPENS MID-LINE. Open-detection keyed on startswith never
    # flips the state here, so the status on the next line — inside the
    # comment — is read as a real declaration. The record declares nothing
    # visible and must fail.
    "ADR-908-midline-comment.md": (
        "# ADR-908: Something\n"
        "\n"
        "Some text <!-- opening mid-line\n"
        "**Status**: proposed\n"
        "-->\n"
        "\n"
        "## Context\n"
    ),
    # The only status-shaped text is 4-space-indented, which CommonMark
    # renders as an indented code block — display text, not a declaration,
    # exactly like the fenced case above. A checker blind to indented code
    # accepts this as declaring "accepted" when the header in fact declares
    # nothing.
    "ADR-930-indented-status-only.md": (
        "# ADR-930: Something\n"
        "\n"
        "    Status: accepted\n"
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
    # A declaration BEFORE a comment that opens later on the same line must
    # survive comment stripping — the mid-line-open fix must not eat the text
    # ahead of the opener, and the trailing annotation must not read as a
    # second declaration.
    "ADR-916-status-then-trailing-comment.md": (
        "# ADR-916: Something\n"
        "\n"
        "**Status**: Accepted <!-- reviewed 2026-09-01, Status: proposed was rejected -->\n"
        "\n"
        "## Context\n"
    ),
    # Fence lookalikes in the HEADER region must not close the block early: a
    # parser that toggles on any fence-prefixed line sees the fenced example
    # status as a second visible declaration and raises a false duplicate.
    "ADR-917-header-fence-with-lookalikes.md": (
        "# ADR-917: Something\n"
        "\n"
        "**Status**: Accepted\n"
        "\n"
        "```text\n"
        "~~~\n"
        "Status: proposed\n"
        "```\n"
        "\n"
        "## Context\n"
    ),
    # A 4-space-indented fence lookalike is indented code, not an opener
    # (CommonMark). A parser that opens on it swallows the real declaration
    # below and rejects this record as declaring nothing.
    "ADR-918-indented-fence-lookalike-not-an-opener.md": (
        "# ADR-918: Something\n"
        "\n"
        "    ```text\n"
        "\n"
        "**Status**: Accepted\n"
        "\n"
        "## Context\n"
    ),
    # A backtick fence's info string may not contain a backtick (CommonMark);
    # such a line is inline code, not an opener. A parser that opens on it
    # swallows the real declaration below.
    "ADR-919-backtick-info-not-an-opener.md": (
        "# ADR-919: Something\n"
        "\n"
        "```a`b`\n"
        "\n"
        "**Status**: Accepted\n"
        "\n"
        "## Context\n"
    ),
    # A real header declaration beside an indented-code example of the same
    # label. The indented line must not count as a second declaration, and
    # must not displace the real one — the indented-code counterpart of
    # ADR-915's fenced-example case.
    "ADR-931-status-plus-indented-example.md": (
        "# ADR-931: Something\n"
        "\n"
        "**Status**: Accepted\n"
        "\n"
        "    Status: accepted   <- indented example, not a declaration\n"
        "\n"
        "## Context\n"
    ),
    # A second "Status:" label inside a backtick-delimited inline code span
    # is display text, not a rider declaration.
    "ADR-932-inline-code-span-status.md": (
        "# ADR-932: Something\n"
        "\n"
        "**Status**: Accepted (see `Status: Proposed`)\n"
        "\n"
        "## Context\n"
    ),
    # A "status:" substring inside a Markdown link's destination is a URL
    # fragment, not a rider declaration.
    "ADR-933-link-destination-status.md": (
        "# ADR-933: Something\n"
        "\n"
        "**Status**: Accepted (see [details](https://example.com/status:notes))\n"
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
            if not check_file(path, base_dir=adr_dir):
                print(f"SELF-TEST: {name} was accepted but must fail.", file=sys.stderr)
                problems += 1
            path.unlink()

        for name, body in MUST_PASS.items():
            path = adr_dir / name
            path.write_text(body, encoding="utf-8")
            failures = check_file(path, base_dir=adr_dir)
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
        if not check_file(link, base_dir=adr_dir):
            print("SELF-TEST: a symlinked ADR was linted instead of refused.", file=sys.stderr)
            problems += 1
        link.unlink()

        # Symlinked base-directory arm: `docs/adr` itself being a symlink must
        # be refused before anything is read, because `check_tree`'s glob
        # would otherwise enumerate the link's target and the containment
        # check in `read_adr_contained` would agree with it (both operands
        # resolve through the same `realpath`). Built as a fully separate
        # scratch tree so it does not disturb `adr_dir`, which every other
        # arm above and below still uses.
        sym_root = Path(tmp) / "symlink-base-arm"
        real_target = sym_root / "real-adr-storage"
        real_target.mkdir(parents=True)
        (real_target / "ADR-940-valid.md").write_text(
            "# ADR-940\n\n**Status**: accepted\n\n## Context\n", encoding="utf-8"
        )
        sym_docs = sym_root / "docs"
        sym_docs.mkdir()
        symlinked_adr_dir = sym_docs / "adr"
        symlinked_adr_dir.symlink_to(real_target)
        print("lint-adr-status self-test: the next line is the expected "
              "symlinked-base-directory refusal.", file=sys.stderr)
        if check_tree(symlinked_adr_dir) == 0:
            print(
                "SELF-TEST: a symlinked ADR base directory was linted instead of refused.",
                file=sys.stderr,
            )
            problems += 1

        # FIFO arm: os.open on a FIFO with no writer BLOCKS unless the open is
        # non-blocking, and a blocked open never reaches the S_ISREG rejection
        # — CI hangs instead of failing. The alarm turns a regression back
        # into a loud failure rather than a silent hang.
        import os as os_mod
        import signal

        fifo = adr_dir / "ADR-909-fifo.md"
        os_mod.mkfifo(fifo)
        signal.alarm(10)
        try:
            if not check_file(fifo, base_dir=adr_dir):
                print("SELF-TEST: a FIFO under an ADR name was linted instead of refused.",
                      file=sys.stderr)
                problems += 1
        finally:
            signal.alarm(0)
            fifo.unlink()

        # Containment arm: a path OUTSIDE the ADR directory must be refused
        # even when the file itself is a perfectly regular, valid ADR —
        # O_NOFOLLOW and fstat validate the file, only the path check
        # validates the path.
        outside = Path(tmp) / "ADR-910-outside.md"
        outside.write_text("# X\n\n**Status**: accepted\n\n## Context\n", encoding="utf-8")
        if not check_file(outside, base_dir=adr_dir):
            print("SELF-TEST: an out-of-tree path was linted instead of refused.",
                  file=sys.stderr)
            problems += 1

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

    total = len(MUST_FAIL) + len(MUST_PASS) + 6
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
