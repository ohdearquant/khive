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
    1: "One", 2: "Two", 3: "Three", 4: "Four", 5: "Five",
    6: "Six", 7: "Seven", 8: "Eight", 9: "Nine", 10: "Ten",
}
DECISION_HEADING = re.compile(r"^(\d+)\. \*\*(.+?) — (RATIFIED|CHANGED)", re.M)


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
    """Join as prose: 'a', 'a and b', 'a, b, and c'."""
    if len(items) == 1:
        return items[0]
    if len(items) == 2:
        return f"{items[0]} and {items[1]}"
    return ", ".join(items[:-1]) + f", and {items[-1]}"


def labels_from_adr(text: str) -> dict[int, str]:
    return {int(n): label for n, _subject, label in DECISION_HEADING.findall(text)}


def check_labels(decisions: list[dict], adr_text: str) -> list[str]:
    """The table must describe exactly the ADR's decisions. Returns complaints."""
    from_adr = labels_from_adr(adr_text)
    numbers = [d["n"] for d in decisions]
    complaints = []

    # Compare the number SETS, not their sizes. `from_adr` is a dict and so
    # collapses duplicates; `decisions` is a list and does not. A table that
    # carries decision 4 twice and omits decision 7 therefore has the same
    # length as a correct one, and the old size comparison passed it while
    # every generated count and set membership silently went wrong.
    for n in sorted({n for n in numbers if numbers.count(n) > 1}):
        complaints.append(f"decision {n} appears {numbers.count(n)} times in the table")
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
            # The table carries line ranges; the ADR carries only the file. The
            # decisions marked CHANGED require editing these very files, so a line
            # range published here is stale by the time anyone acts on it. The
            # passage name is what makes the citation findable either way.
            path = site["cite"].split(":", 1)[0]
            gloss = site.get("gloss") or f"the documentation in `{path}`"
            sites.append(f"{gloss} (`{path}`)")
        body = english_list(sites)
        tail = f" — superseded by decision {d['n']}"
        if d.get("scope_note"):
            tail += f". The displacement is bounded: {d['scope_note']}"
        bullets.append(wrap(f"{body[0].upper() + body[1:]}{tail}.", bullet=True))

    # The count of bounded entries is derived, not asserted: it moves whenever a
    # scope note is added to or removed from the table.
    bounded = [d for d in displacing if d.get("scope_note")]
    bounded_names = "decisions " + english_list([str(d["n"]) for d in bounded])
    closing = wrap(
        f"{WORDS[len(bounded)]} of those entries are bounded rather than wholesale — "
        f"{bounded_names} — and in each case the "
        f"bound is the substance. Decision 10's is the one worth spelling out, because it "
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
