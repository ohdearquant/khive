#!/usr/bin/env python3
"""Validate khive verb examples in marketplace SKILL.md files.

Usage:
    uv run python marketplace/_validators/check_examples.py
    python marketplace/_validators/check_examples.py

Exit 0 if all examples valid, 1 if any are invalid.
"""

import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Known public verb surfaces
# ---------------------------------------------------------------------------

KG_VERBS = frozenset({
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "propose", "review", "withdraw",
})

GTD_VERBS = frozenset({
    "gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition",
})

MEMORY_VERBS = frozenset({
    "memory.remember", "memory.recall",
})

BRAIN_VERBS = frozenset({
    "brain.profiles", "brain.profile", "brain.resolve",
    "brain.activate", "brain.deactivate", "brain.archive",
    "brain.feedback", "brain.auto_feedback", "brain.reset",
    "brain.bind", "brain.unbind", "brain.bindings", "brain.create_profile",
    # internal/subhandler — callable by operators
    "brain.state", "brain.config", "brain.events", "brain.emit",
})

COMM_VERBS = frozenset({
    "comm.send", "comm.inbox", "comm.read", "comm.reply", "comm.thread",
})

SCHEDULE_VERBS = frozenset({
    "schedule.remind", "schedule.schedule", "schedule.agenda", "schedule.cancel",
})

KNOWLEDGE_VERBS = frozenset({
    "knowledge.learn", "knowledge.cite", "knowledge.topic",
    "knowledge.search", "knowledge.suggest", "knowledge.compose",
})

ALL_VERBS = KG_VERBS | GTD_VERBS | MEMORY_VERBS | BRAIN_VERBS | COMM_VERBS | SCHEDULE_VERBS | KNOWLEDGE_VERBS

# Regex that matches the start of a verb call line (supports dotted names like brain.profiles)
_VERB_RE = re.compile(
    r"^(?:request|\[)?(" + "|".join(sorted(ALL_VERBS, key=len, reverse=True)) + r")\s*\("
)
_REQUEST_RE = re.compile(r"^request\s*\(")

# Detect placeholder-only calls: verb(...) or [..., ...]
_PLACEHOLDER_RE = re.compile(r"\(\s*\.\.\.\s*\)")
_TRAILING_ELLIPSIS_RE = re.compile(r"\.\.\.\s*\]?\s*$")


# ---------------------------------------------------------------------------
# Extraction helpers
# ---------------------------------------------------------------------------

def extract_code_blocks(text: str) -> list[tuple[str, str, int]]:
    """Return [(lang, block_content, start_line_1indexed), ...]."""
    blocks = []
    pattern = re.compile(r"^```(\w*)\n(.*?)^```", re.MULTILINE | re.DOTALL)
    for m in pattern.finditer(text):
        lang = m.group(1).lower()
        content = m.group(2)
        start_line = text[: m.start()].count("\n") + 1
        blocks.append((lang, content, start_line))
    return blocks


def should_skip_block(lang: str, content: str) -> bool:
    """True if the block should be excluded from validation entirely."""
    if lang in ("json", "bash", "sh"):
        return True
    # Skip blocks that are purely table-formatted verb signatures
    stripped = content.strip()
    if stripped.startswith("|") or "| Verb" in stripped:
        return True
    return False


def _join_continuation_lines(text: str) -> str:
    """Join lines that are clearly continuing a previous call (indented or mid-list)."""
    lines = text.split("\n")
    joined: list[str] = []
    buf = ""
    for line in lines:
        stripped = line.strip()
        if not stripped:
            if buf:
                joined.append(buf)
                buf = ""
            continue
        if buf:
            # continuation if current line doesn't start a new verb/request call
            if _REQUEST_RE.match(stripped) or _VERB_RE.match(stripped):
                joined.append(buf)
                buf = stripped
            else:
                buf += " " + stripped
        else:
            buf = stripped
    if buf:
        joined.append(buf)
    return "\n".join(joined)


def extract_verb_calls(block_content: str) -> list[tuple[int, str]]:
    """Return [(offset_line, call_text), ...] for verb-like lines in a block."""
    content = _join_continuation_lines(block_content)
    calls = []
    for i, line in enumerate(content.split("\n")):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if _REQUEST_RE.match(stripped) or _VERB_RE.match(stripped):
            calls.append((i, stripped))
    return calls


# ---------------------------------------------------------------------------
# Validation
# ---------------------------------------------------------------------------

def validate_call(call_text: str) -> tuple[bool, str | None]:
    """Validate one call line. Returns (ok, error_msg)."""
    # Skip placeholder calls
    if _PLACEHOLDER_RE.search(call_text):
        return True, None  # placeholder; skip silently
    if _TRAILING_ELLIPSIS_RE.search(call_text):
        return True, None  # incomplete example; skip silently

    # --- request(ops="...") wrapper ---
    if _REQUEST_RE.match(call_text):
        # Extract ops value (handle single or double wrapped ops)
        ops_m = re.search(r'ops\s*=\s*"(.*)"', call_text)
        if not ops_m:
            # Could be multiline collapsed — just check the call structure exists
            if "ops=" not in call_text and 'ops =' not in call_text:
                return False, 'request() missing ops= argument'
            return True, None
        inner = ops_m.group(1)
        # Unescape inner content
        inner_unescaped = inner.replace('\\"', '"')
        # Validate inner verb(s)
        # Handle batch: starts with [
        if inner_unescaped.strip().startswith("["):
            return _validate_batch(inner_unescaped, call_text)
        return _validate_single_inner(inner_unescaped, call_text)

    # --- direct verb call (inner example) ---
    m = _VERB_RE.match(call_text)
    if m:
        verb = m.group(1)
        # Extract args portion
        paren_pos = call_text.index("(")
        args_text = call_text[paren_pos + 1 :]
        return _validate_verb_args(verb, args_text, call_text)

    # Shouldn't reach here given our extraction filter
    return True, None


def _validate_batch(inner: str, original: str) -> tuple[bool, str | None]:
    """Validate a batch call string like [verb1(...), verb2(...)]."""
    # Extract individual verb calls from within the brackets
    stripped = inner.strip().lstrip("[").rstrip("]").strip()
    # Split on top-level commas (simple approach: find verb( patterns)
    # Sort by length descending so dotted names (brain.profiles) match before prefixes (brain)
    verb_starts = [m.start() for m in re.finditer(
        r"(?<![a-z_.])(" + "|".join(sorted(ALL_VERBS, key=len, reverse=True)) + r")\s*\(", stripped
    )]
    if not verb_starts:
        return True, None  # No recognisable verbs in batch; skip
    for i, start in enumerate(verb_starts):
        end = verb_starts[i + 1] if i + 1 < len(verb_starts) else len(stripped)
        segment = stripped[start:end].strip().rstrip(", ")
        # Support dotted names (brain.profiles, brain.feedback, etc.)
        m = re.match(r"([a-z][a-z_.]*)\s*\(", segment)
        if not m:
            continue
        verb = m.group(1)
        if verb not in ALL_VERBS:
            return False, f"Unknown verb in batch: {verb!r} in: {original[:80]}"
        paren_pos = segment.index("(")
        args = segment[paren_pos + 1 :]
        ok, err = _validate_verb_args(verb, args, segment)
        if not ok:
            return False, err
    return True, None


def _validate_single_inner(inner: str, original: str) -> tuple[bool, str | None]:
    """Validate a single inner verb call string (unescaped)."""
    inner = inner.strip()
    # Support dotted names (brain.profiles, brain.feedback, etc.)
    m = re.match(r"([a-z][a-z_.]*)\s*\(", inner)
    if not m:
        return True, None  # Can't parse — skip
    verb = m.group(1)
    if verb not in ALL_VERBS:
        return False, f"Unknown verb: {verb!r} in: {original[:80]}"
    paren_pos = inner.index("(")
    args = inner[paren_pos + 1 :]
    return _validate_verb_args(verb, args, inner)


def _validate_verb_args(verb: str, args_text: str, original: str) -> tuple[bool, str | None]:
    """Check that the first argument looks like a keyword arg, not positional."""
    args_stripped = args_text.strip().lstrip("(").rstrip(")").strip()
    if not args_stripped:
        return True, None  # No args — fine for verbs like gtd.next()
    # Skip if placeholder content
    if _PLACEHOLDER_RE.search(args_stripped):
        return True, None

    # First arg token check: positional if starts with " or ' or digit
    # but NOT if the outer call is already unescaped (has real quotes)
    first_char = args_stripped[0]
    if first_char in ('"', "'") or first_char.isdigit():
        # Exception: request(ops="...") — already handled upstream
        return False, (
            f"Positional arg in {verb}(): "
            f"first arg starts with {args_stripped[:30]!r} — "
            f"use keyword args: {verb}(arg_name=value, ...)"
        )
    return True, None


# ---------------------------------------------------------------------------
# File scanning
# ---------------------------------------------------------------------------

def scan_file(path: Path, marketplace_root: Path) -> tuple[int, int, int, list[str]]:
    """Scan one file. Returns (checked, valid, skipped, [error_lines])."""
    text = path.read_text(encoding="utf-8")
    rel = path.relative_to(marketplace_root)
    errors = []
    checked = 0
    valid = 0
    skipped = 0

    for lang, content, block_start in extract_code_blocks(text):
        if should_skip_block(lang, content):
            skipped += 1
            continue
        calls = extract_verb_calls(content)
        for offset, call_text in calls:
            # Skip pure placeholder calls before counting
            if _PLACEHOLDER_RE.search(call_text) or _TRAILING_ELLIPSIS_RE.search(call_text):
                skipped += 1
                continue
            line_num = block_start + offset + 1
            checked += 1
            ok, err = validate_call(call_text)
            if ok:
                valid += 1
            else:
                errors.append(f"  {rel}:{line_num}: {err}")
                errors.append(f"    text: {call_text[:120]}")

    return checked, valid, skipped, errors


def main() -> int:
    here = Path(__file__).parent
    marketplace_root = here.parent

    skill_files = sorted(marketplace_root.glob("*/skills/**/SKILL.md"))
    agent_files = sorted(marketplace_root.glob("*/agents/*.md"))
    all_files = skill_files + agent_files

    if not all_files:
        print(f"No SKILL.md files found under {marketplace_root}")
        return 1

    total_checked = 0
    total_valid = 0
    total_skipped = 0
    all_errors: list[str] = []

    for path in all_files:
        checked, valid_, skipped, errors = scan_file(path, marketplace_root)
        total_checked += checked
        total_valid += valid_
        total_skipped += skipped
        if errors:
            all_errors.append(f"\n[FAIL] {path.relative_to(marketplace_root)}")
            all_errors.extend(errors)
        elif checked > 0:
            print(f"  ok   {path.relative_to(marketplace_root)}  ({checked} examples)")
        else:
            print(f"  --   {path.relative_to(marketplace_root)}  (no extractable examples)")

    print()
    print(f"checked={total_checked} valid={total_valid} invalid={total_checked - total_valid} skipped={total_skipped}")

    if all_errors:
        print("\nFAILURES:")
        print("\n".join(all_errors))
        return 1

    print("\nAll examples valid.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
