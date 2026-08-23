#!/usr/bin/env python3
"""Generate and check the derived passages of ADR-137 Amendment 1.

Two sentences in the amendment state quantities and set memberships that a reader
cannot check without redoing the audit: how many decisions displace the crate
documentation, which ones they are, and which decisions carry the CHANGED label
that the implementation fence turns on. Typing those is how they go wrong. This
script derives all of them from the table (see TABLE below), and cross-checks the
label of every decision against the ADR's own heading text so the table cannot
drift from the document it describes without the check failing.

  uv run scripts/lint-adr-137-displacements.py           # print the generated passages
  uv run scripts/lint-adr-137-displacements.py --check   # verify the ADR contains them

--check is the mode the pre-commit hook runs, so an edit to either the ADR or
the table that leaves the two disagreeing fails before it can be committed.

Three things are checked, because each was found unguarded by review:
  * every quoted passage still exists in the file the row cites (citations),
  * the table describes exactly the ADR's decision set, with no duplicate or
    missing decision number (labels),
  * the ADR contains the generated passages verbatim (--check).

Exit status: 2 if the table and the ADR disagree about decisions or citations,
1 if --check finds a generated passage absent from the ADR, 0 otherwise.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import textwrap
from pathlib import Path

WIDTH = 100  # the ADR hard-wraps its prose; generated passages must match or the diff lies

ROOT = Path(__file__).resolve().parents[1]
TABLE = ROOT / "scripts" / "data" / "adr-137-amendment-1-displacements.json"
ADR = ROOT / "docs" / "adr" / "ADR-137-tailnet-wire-transport.md"

WORDS = {
    0: "None", 1: "One", 2: "Two", 3: "Three", 4: "Four", 5: "Five",
    6: "Six", 7: "Seven", 8: "Eight", 9: "Nine", 10: "Ten",
}
# The example decision spelled out in the generated precedence closing. It is
# named here rather than inline because the sentence that names it is only true
# while that decision is actually bounded, and a generated passage must not be
# able to publish a false sentence about its own membership.
SPELLED_OUT_EXAMPLE = "10"
DECISION_HEADING = re.compile(r"^(\d+)\. \*\*(.+?) — (RATIFIED|CHANGED)", re.M)

# A displacement is "bounded" when the parent rule survives and only its scope
# moves. Membership is a CLOSED vocabulary, not the truthiness of a prose field:
# the generated ADR publishes the count and the membership list, so deriving
# them from whether someone wrote a free-form note means a later edit to that
# note can silently move a published claim. `scope_note` stays, as the human
# explanation; `scope_kind` is what the count is computed from.
BOUNDED_KINDS = {
    "version_floor": "the rule holds, but not below the protocol version floor",
    "idless_error": "the rule holds except for the case carrying no operation id",
    "endpoint_role": "the rule holds; its scoping to one endpoint role is what moves",
    "outermost_shape": "only the outermost type is displaced, not the per-field shape",
}

# The other half of the classification, and it must be WRITTEN DOWN rather than
# inferred from the absence of a bounded kind. A row that declares nothing reads
# to a human as unremarkable and lands in the wholesale count with nothing
# anywhere disagreeing, which is the same fail-open the closed vocabulary was
# introduced to close — moved one field to the left. Every displacing row now
# classifies itself, so "the author had not decided yet" is a state the table
# can represent and the check can reject.
WHOLESALE = "wholesale"
SCOPE_KINDS = {**BOUNDED_KINDS, WHOLESALE: "the displaced passage does not survive in any form"}


def wrap(text: str, bullet: bool = False) -> str:
    return textwrap.fill(
        text,
        width=WIDTH,
        initial_indent="- " if bullet else "",
        subsequent_indent="  " if bullet else "",
        break_long_words=False,
        break_on_hyphens=False,
    )


def english_list(items: list[str]) -> str:
    """Join as prose: '', 'a', 'a and b', 'a, b, and c'.

    The empty case returns the empty string rather than raising, but no caller
    should reach it with a sentence that assumes a name: an empty list has no
    prose form, only a sentence that does not mention one. Callers branch on
    emptiness themselves; this guard exists so that a caller which forgets to
    produces nonsense rather than an IndexError three frames down.
    """
    if not items:
        return ""
    if len(items) == 1:
        return items[0]
    if len(items) == 2:
        return f"{items[0]} and {items[1]}"
    return ", ".join(items[:-1]) + f", and {items[-1]}"


def labels_from_adr(text: str) -> list[tuple[int, str]]:
    """Every decision heading, IN ORDER and WITH REPEATS.

    Returning a dict here was itself a defect: it keyed on the decision number,
    so an ADR carrying two `6.` headings kept whichever came last and discarded
    the other, silently. The duplicate side of the comparison was guarded on the
    table and unguarded on the document, and the two headings need not even agree
    on their label for the collapse to go unnoticed. The caller is what decides
    duplicates are an error, so this function must not decide it by data type.
    """
    return [(int(n), label) for n, _subject, label in DECISION_HEADING.findall(text)]


def check_labels(decisions: list[dict], adr_text: str) -> list[str]:
    """The table must describe exactly the ADR's decisions. Returns complaints."""
    adr_headings = labels_from_adr(adr_text)
    adr_numbers = [n for n, _ in adr_headings]
    from_adr = dict(adr_headings)
    numbers = [d["n"] for d in decisions]
    complaints = []

    # Compare the number SETS, not their sizes, and check BOTH sides for
    # repeats. A table that carries decision 4 twice and omits decision 7 has the
    # same length as a correct one, and the old size comparison passed it while
    # every generated count and set membership silently went wrong. The ADR can
    # carry the same duplicate for the same reason — headings are hand-written —
    # and a second `6.` is exactly what an editor adds when splitting a decision.
    for n in sorted({n for n in numbers if numbers.count(n) > 1}):
        complaints.append(f"decision {n} appears {numbers.count(n)} times in the table")
    for n in sorted({n for n in adr_numbers if adr_numbers.count(n) > 1}):
        labels = [lab for num, lab in adr_headings if num == n]
        complaints.append(
            f"decision {n} appears {adr_numbers.count(n)} times in the ADR "
            f"(labels {', '.join(labels)}); numbers must be unique"
        )
    for n in sorted(set(from_adr) - set(numbers)):
        complaints.append(f"decision {n} is in the ADR but not in the table")
    for n in sorted(set(numbers) - set(from_adr)):
        complaints.append(f"decision {n} is in the table but not in the ADR")

    for d in decisions:
        got = from_adr.get(d["n"])
        if got is not None and got != d["label"]:
            complaints.append(
                f"decision {d['n']}: table says {d['label']}, the ADR says {got}"
            )
    return complaints


def check_scope_kinds(decisions: list[dict]) -> list[str]:
    """Every displacing row classifies itself, and the classification is closed.

    The generated ADR publishes both a COUNT and a MEMBERSHIP LIST of bounded
    displacements. Deriving those from a free-form field's truthiness is how a
    published claim moves silently, which already happened once here: decision 1
    was bounded in fact, carried no note, and was dropped from a count that read
    as authoritative.

    The classification is EXHAUSTIVE, which is the part an earlier version of
    this function got wrong. It rejected a row declaring a `scope_note` without a
    `scope_kind`, and a row declaring a `scope_kind` without a note, and left the
    row that declares NEITHER passing in silence. That row is the original defect
    wearing different clothes: it is counted as wholesale by omission, so a
    bounded displacement whose author simply had not filled the field in yet
    publishes as wholesale, and the only thing that would ever contradict it is
    someone re-reading the decision. `wholesale` is now a value a row must state,
    so an unfilled row is a rejected row rather than a wholesale one.
    """
    complaints = []
    for d in decisions:
        kind, note = d.get("scope_kind"), d.get("scope_note")
        if kind is not None and kind not in SCOPE_KINDS:
            complaints.append(
                f"decision {d['n']}: scope_kind {kind!r} is not one of "
                f"{sorted(SCOPE_KINDS)}"
            )
        if d["crate_displaced"] and kind is None:
            complaints.append(
                f"decision {d['n']}: displaces a crate passage but declares no "
                f"scope_kind; state {WHOLESALE!r} or one of "
                f"{sorted(BOUNDED_KINDS)} — an unclassified row publishes as "
                f"wholesale by omission"
            )
        if kind in BOUNDED_KINDS and not note:
            complaints.append(
                f"decision {d['n']}: has scope_kind {kind!r} but no scope_note "
                f"explaining the bound to a reader"
            )
        if kind == WHOLESALE and note:
            complaints.append(
                f"decision {d['n']}: is {WHOLESALE} but carries a scope_note; a "
                f"note reads as a bound and this row publishes without one"
            )
        # Only when the row displaces nothing. A displacing row with a note and
        # no kind is already named by the check above, and the two complaints
        # describe one defect with one fix; emitting both makes the diagnostic
        # read as two problems and buries whichever the reader acts on second.
        if note and kind is None and not d["crate_displaced"]:
            complaints.append(
                f"decision {d['n']}: has a scope_note but no scope_kind, so it "
                f"reads as bounded but is excluded from the generated count"
            )
        if kind and not d["crate_displaced"]:
            complaints.append(
                f"decision {d['n']}: has scope_kind {kind!r} but displaces no "
                f"crate passage, so it can never appear in the generated count"
            )
    return complaints


def normalize(text: str) -> str:
    """Strip doc-comment markers and collapse whitespace.

    Canonicalization of a known syntax, not fuzzy matching. The cited crate
    passages are `//!` doc comments that hard-wrap mid-sentence, so no quoted
    phrase is a contiguous substring of the raw file; without this, a citation
    check could only ever be tolerant, and a tolerant one is worse than none.
    A lowercased quote in decision 2 matched an unrelated passage 93 lines from
    the intended one, which is the failure this exactness exists to prevent.
    """
    return normalize_with_lines(text)[0]


def normalize_with_lines(text: str) -> tuple[str, list[tuple[int, int]]]:
    """Normalize, and return a map from offset in the result to source line.

    The map is what lets a quote report the line it currently occupies. Deriving
    the line at check time is the whole point: a stored one is invalidated by the
    next edit to the cited file, which is exactly what happened here.
    """
    parts: list[str] = []
    index: list[tuple[int, int]] = []
    pos = 0
    for lineno, raw in enumerate(text.splitlines(), 1):
        collapsed = re.sub(r"\s+", " ", re.sub(r"^[ \t]*//[/!] ?", "", raw)).strip()
        if not collapsed:
            continue
        index.append((pos, lineno))
        parts.append(collapsed)
        pos += len(collapsed) + 1
    return " ".join(parts), index


def line_at(index: list[tuple[int, int]], offset: int) -> int:
    """The source line containing the given offset in the normalized text."""
    found = index[0][1] if index else 0
    for start, lineno in index:
        if start > offset:
            break
        found = lineno
    return found


def check_citations(decisions: list[dict], root: Path) -> tuple[list[str], list[str]]:
    """Every quoted passage must still exist in the file its row cites.

    Returns (complaints, resolved) where `resolved` reports the line each quote
    currently occupies. Line numbers are derived here and never stored: the
    amendment inserts into the very ADR the table cites, so a stored line is
    invalidated by the edit that makes the citation worth checking.
    """
    complaints, resolved = [], []
    bodies: dict[str, tuple[str, list[tuple[int, int]]]] = {}
    for d in decisions:
        for field in ("parent_displaced", "crate_displaced"):
            for site in d.get(field, []):
                path = site["cite"]
                if ":" in path:
                    complaints.append(
                        f"decision {d['n']}: cite {path!r} carries a line number; "
                        f"cite the file and let the quote locate the passage"
                    )
                    path = path.split(":", 1)[0]
                if path not in bodies:
                    target = root / path
                    if target.exists():
                        bodies[path] = normalize_with_lines(target.read_text())
                    else:
                        complaints.append(f"decision {d['n']}: cited file {path} does not exist")
                        bodies[path] = ("", [])
                body, index = bodies[path]
                for quote in site["quotes"]:
                    needle = normalize(quote)
                    hits = body.count(needle) if needle else 0
                    if not needle:
                        complaints.append(f"decision {d['n']}: empty quote for {path}")
                    elif hits == 1:
                        line = line_at(index, body.index(needle))
                        resolved.append(f"  decision {d['n']:<2} {path}:{line}")
                    elif hits == 0:
                        complaints.append(
                            f"decision {d['n']}: {path} no longer contains {quote[:70]!r}"
                        )
                    else:
                        # Ambiguity is fail-open here, not a cosmetic defect. The
                        # amendment quotes the passages it displaces, in the file it
                        # cites, so a short quote matches the amendment's own
                        # quotation as readily as the parent sentence -- and would
                        # keep matching after the parent sentence was deleted, which
                        # is the one change this check exists to catch.
                        lines = []
                        start = 0
                        for _ in range(hits):
                            at = body.index(needle, start)
                            lines.append(str(line_at(index, at)))
                            start = at + 1
                        complaints.append(
                            f"decision {d['n']}: {path} contains {quote[:50]!r} "
                            f"{hits} times (lines {', '.join(lines)}); lengthen the quote "
                            f"until it names one passage"
                        )
    return complaints, resolved


def precedence_passage(decisions: list[dict]) -> str:
    displacing = [d for d in decisions if d["crate_displaced"]]
    numbers = english_list([str(d["n"]) for d in displacing])

    lead = (
        f"The parent states that the crate's own documentation \"is the normative wire "
        f"specification\", which makes that documentation a second authority and not merely a "
        f"description. {WORDS[len(displacing)]} of the {WORDS[len(decisions)].lower()} decisions "
        f"above contradict it as it stands today — decisions {numbers} — so a second implementer "
        f"reading the crate docs, exactly as the parent instructs, would build behaviour this "
        f"amendment removes. **Where this amendment and the current crate documentation disagree, "
        f"this amendment governs, and the documentation is superseded in the following places:**"
    )

    bullets = []
    for d in displacing:
        sites = []
        for site in d["crate_displaced"]:
            # The table carries a file path and quoted passages, never a line
            # number: the decisions marked CHANGED require editing these very
            # files, so a line published here is stale by the time anyone acts on
            # it. check_citations rejects a cite carrying one and derives the
            # current line from the quote at run time, for diagnostics only.
            path = site["cite"]
            gloss = site.get("gloss") or f"the documentation in `{path}`"
            sites.append(f"{gloss} (`{path}`)")
        body = english_list(sites)
        tail = f" — superseded by decision {d['n']}"
        # Keyed on `scope_kind`, the closed field, for the same reason the count
        # below is: "the displacement is bounded" is a normative sentence, and a
        # normative sentence must not appear or vanish because someone added or
        # removed a prose explanation.
        if d.get("scope_kind") in BOUNDED_KINDS:
            tail += f". The displacement is bounded: {d['scope_note']}"
        bullets.append(wrap(f"{body[0].upper() + body[1:]}{tail}.", bullet=True))

    # The count of bounded entries is derived, not asserted. It is computed from
    # `scope_kind`, a closed vocabulary checked by check_scope_kinds, and NOT
    # from whether a prose `scope_note` happens to be present — a published
    # count must not move because someone reworded an explanation.
    bounded = [d for d in displacing if d.get("scope_kind") in BOUNDED_KINDS]
    if not bounded:
        # Every displacing entry is wholesale. The membership list and the
        # spelled-out example below both presuppose a member, so this case gets
        # its own sentence rather than a degenerate rendering of that one.
        closing = wrap(
            "None of those entries are bounded: every one of them displaces its crate "
            "passage wholesale, so there is no surviving parent rule whose scope moved."
        )
        return wrap(lead) + "\n\n" + "\n".join(bullets) + "\n\n" + closing

    # The sentence below names one decision as the illustrative case. That is a
    # claim about membership, so it is checked rather than trusted: if the named
    # decision stops being bounded, the passage must fail to generate instead of
    # publishing a sentence that contradicts the list beside it.
    if SPELLED_OUT_EXAMPLE not in {str(d["n"]) for d in bounded}:
        raise SystemExit(
            f"generator: the precedence closing spells out decision "
            f"{SPELLED_OUT_EXAMPLE}, which is no longer bounded. Pick a different "
            f"example and rewrite the sentence to match what it displaces; do not "
            f"just change the number."
        )
    bounded_names = "decisions " + english_list([str(d["n"]) for d in bounded])
    closing = wrap(
        f"{WORDS[len(bounded)]} of those entries are bounded rather than wholesale — "
        f"{bounded_names} — and in each case the "
        f"bound is the substance. Decision {SPELLED_OUT_EXAMPLE}'s is the one worth spelling out, "
        f"because it "
        f"displaces a disclaimer rather than a statement: the crate documentation never says a "
        f"non-object payload is acceptable, it says the shape of `event.payload` is not the "
        f"crate's business. That is not a weaker form of the same thing. A document that disclaims "
        f"ownership of a rule licenses the absence of that rule just as effectively as one that "
        f"states the permissive version, and the reader who follows the disclaimer arrives at the "
        f"same wrong implementation."
    )

    return wrap(lead) + "\n\n" + "\n".join(bullets) + "\n\n" + closing


def fence_passage(decisions: list[dict]) -> str:
    changed = [d for d in decisions if d["label"] == "CHANGED"]
    numbers = english_list([str(d["n"]) for d in changed])
    return wrap(
        f"**No first consumer of `khive-wire-protocol` may merge while any decision marked "
        f"CHANGED above still implements the superseded behaviour.** Those are decisions "
        f"{numbers} — {WORDS[len(changed)].lower()} of the {WORDS[len(decisions)].lower()}.",
        bullet=True,
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    table = json.loads(TABLE.read_text())
    decisions = table["decisions"]
    adr_text = ADR.read_text()

    complaints = check_labels(decisions, adr_text)
    if complaints:
        print("LABEL MISMATCH between the table and the ADR:", file=sys.stderr)
        for c in complaints:
            print(f"  - {c}", file=sys.stderr)
        return 2

    scope_complaints = check_scope_kinds(decisions)
    if scope_complaints:
        print("SCOPE-KIND MISMATCH in the table:", file=sys.stderr)
        for c in scope_complaints:
            print(f"  - {c}", file=sys.stderr)
        return 2

    cite_complaints, resolved = check_citations(decisions, ROOT)
    if cite_complaints:
        print("CITATION MISMATCH between the table and the cited sources:", file=sys.stderr)
        for c in cite_complaints:
            print(f"  - {c}", file=sys.stderr)
        return 2
    if not resolved:
        print("no citations were checked; the table carries no quotes", file=sys.stderr)
        return 2
    print(f"citations: {len(resolved)} quoted passages resolve in their cited files")
    for r in resolved:
        print(r)

    passages = {
        "precedence": precedence_passage(decisions),
        "fence": fence_passage(decisions),
    }

    if not args.check:
        for name, text in passages.items():
            print(f"\n===== {name} =====\n")
            print(text)
        return 0

    missing = [n for n, t in passages.items() if t not in adr_text]
    for name, text in passages.items():
        state = "MISSING" if name in missing else "present verbatim"
        print(f"{name}: {state}")
    if missing:
        print(
            "\nThe ADR does not contain the generated text for: "
            + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    print("\nAll derived passages match the table.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
