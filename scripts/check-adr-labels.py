#!/usr/bin/env python3
"""Validate that ADR-NNN labels in crates/*/docs/design.md match the
canonical ADR title (the H1 on line 1 of docs/adr/ADR-NNN-*.md).

Two deterministic passes:
  1. Build a canonical title registry from the first line of every
     docs/adr/ADR-*.md file.
  2. Scan crates/*/docs/design.md for single-ID labels and check that each
     label's title starts with the canonical title for its ADR ID.

Only single-ID labels in one of these forms are checked:
    ### ADR-NNN: Title [optional local qualifier]
    [ADR-NNN: Title]

Prose citations, section citations (e.g. "ADR-024 section ..." or
'ADR-024 §"..."'), bare IDs, and multi-ID headings (e.g. "ADR-004 / ADR-005:
...") are not label forms and are ignored.

Exit status is nonzero for unknown ADR IDs (a label citing an ADR number with
no canonical H1), malformed/duplicate canonical H1s, and title-prefix
mismatches between a label and its canonical H1. A local qualifier after the
complete canonical title (for example "ADR-004: Substrate Observables - Note
lifecycle") is permitted and is not a mismatch. All findings are reported,
sorted, before exiting nonzero.
"""
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ADR_DIR = REPO_ROOT / "docs" / "adr"
CRATES_DIR = REPO_ROOT / "crates"

ADR_H1_RE = re.compile(r"^# ADR-(\d+)( [A-Za-z]+ \d+)?: (.+?)\s*$")
HEADING_LABEL_RE = re.compile(r"^### ADR-(\d+): (.+?)\s*$")
BRACKET_LABEL_RE = re.compile(r"^\[ADR-(\d+): (.+?)\]\s*$")

DASH_VARIANTS = re.compile("[\u2010\u2011\u2012\u2013\u2014\u2015]")


def normalize(text):
    text = DASH_VARIANTS.sub("-", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def build_registry():
    """First pass: canonical H1 registry, one record per ADR ID.

    Some ADR IDs have a qualified companion file (e.g. "ADR-088 Amendment 1:
    ...") alongside the primary unqualified file ("ADR-088: ..."). The
    unqualified H1 is the canonical title; a qualified companion is not a
    conflicting duplicate.
    """
    records = {}
    errors = []
    for path in sorted(ADR_DIR.glob("ADR-*.md")):
        with path.open(encoding="utf-8") as fh:
            first_line = fh.readline()
        match = ADR_H1_RE.match(first_line)
        if not match:
            errors.append(f"{path.relative_to(REPO_ROOT)}:1: malformed canonical H1: {first_line.strip()!r}")
            continue
        adr_id, qualifier, title = match.group(1), match.group(2), normalize(match.group(3))
        records.setdefault(adr_id, []).append((qualifier, path.relative_to(REPO_ROOT), title))

    registry = {}
    for adr_id, entries in records.items():
        unqualified = [e for e in entries if e[0] is None]
        if len(unqualified) > 1:
            paths = ", ".join(str(e[1]) for e in unqualified)
            errors.append(f"ADR-{adr_id}: duplicate unqualified canonical H1 across files: {paths}")
            continue
        primary = unqualified[0] if unqualified else entries[0]
        _, path, title = primary
        registry[adr_id] = (path, title)
    return registry, errors


def extract_labels(path):
    """Second pass: single-ID labels recognized by the label grammar."""
    labels = []
    with path.open(encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, start=1):
            for pattern in (HEADING_LABEL_RE, BRACKET_LABEL_RE):
                match = pattern.match(line)
                if match:
                    labels.append((lineno, match.group(1), normalize(match.group(2))))
                    break
    return labels


def title_matches(label_title, canonical_title):
    if label_title == canonical_title:
        return True
    if not label_title.startswith(canonical_title):
        return False
    rest = label_title[len(canonical_title):].lstrip(" ")
    return rest[:1] in ("-", "(")


def check_design_docs(registry):
    errors = []
    for path in sorted(CRATES_DIR.glob("*/docs/design.md")):
        rel = path.relative_to(REPO_ROOT)
        for lineno, adr_id, label_title in extract_labels(path):
            if adr_id not in registry:
                errors.append(f"{rel}:{lineno}: unknown ADR-{adr_id} (no canonical H1 registered)")
                continue
            canonical_path, canonical_title = registry[adr_id]
            if not title_matches(label_title, canonical_title):
                replacement = f"ADR-{adr_id}: {canonical_title} - {label_title}"
                errors.append(
                    f"{rel}:{lineno}: ADR-{adr_id} label title {label_title!r} does not start with "
                    f"canonical title {canonical_title!r} ({canonical_path}); replace with {replacement!r}"
                )
    return errors


def main():
    registry, registry_errors = build_registry()
    design_errors = check_design_docs(registry)
    errors = sorted(registry_errors) + sorted(design_errors)

    if errors:
        for line in errors:
            print(f"error: {line}", file=sys.stderr)
        print(f"\n{len(errors)} ADR label error(s).", file=sys.stderr)
        return 1
    print(f"ADR label check OK: {len(registry)} canonical titles, 0 errors.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
