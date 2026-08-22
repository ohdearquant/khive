#!/usr/bin/env python3
"""Generate and check the derived passages of ADR-137 Amendment 1.

Two sentences in the amendment state quantities and set memberships that a reader
cannot check without redoing the audit: how many decisions displace the crate
documentation, which ones they are, and which decisions carry the CHANGED label
that the implementation fence turns on. Typing those is how they go wrong. This
script derives all of them from `.khive/displacements.json`, and cross-checks the
label of every decision against the ADR's own heading text so the table cannot
drift from the document it describes without the check failing.

  uv run scripts/lint-adr-137-displacements.py           # print the generated passages
  uv run scripts/lint-adr-137-displacements.py --check   # verify the ADR contains them

--check is the mode the pre-commit hook runs, so an edit to either the ADR or
the table that leaves the two disagreeing fails before it can be committed.

Exit status is nonzero if a label disagrees, or, under --check, if a generated
passage is absent from the ADR.
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
    """The table's labels must equal the ADR's own. Returns a list of complaints."""
    from_adr = labels_from_adr(adr_text)
    complaints = []
    if len(from_adr) != len(decisions):
        complaints.append(
            f"the ADR carries {len(from_adr)} numbered decisions, the table has {len(decisions)}"
        )
    for d in decisions:
        got = from_adr.get(d["n"])
        if got is None:
            complaints.append(f"decision {d['n']} is in the table but not in the ADR")
        elif got != d["label"]:
            complaints.append(
                f"decision {d['n']}: table says {d['label']}, the ADR says {got}"
            )
    return complaints


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
