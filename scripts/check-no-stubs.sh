#!/usr/bin/env bash
# check-no-stubs.sh — block stub-intent placeholders from shipping source.
#
# Scans non-test Rust source under crates/ and fails on:
#   todo!(...)         — always blocked; unambiguous stub intent
#   unimplemented!()   — always blocked; unambiguous stub intent
#   unreachable!(msg)  — blocked only when the message argument contains a
#                        stub-intent word (stub, not implemented, not yet,
#                        placeholder, fixme, tbd); bare unreachable!() and
#                        legitimate "invalid state" messages are allowed
#   panic!(msg)        — same stub-word rule as unreachable!
#
# Exclusions:
#   - files under any tests/, benches/, examples/ directory
#   - files ending in _test.rs or tests.rs
#   - inline #[cfg(test)] mod blocks within source files (see implementation
#     note below on the Python block-stripper approach)
#
# Implementation note — inline cfg(test) exclusion:
#   Stripping #[cfg(test)] blocks requires brace-matching (the block ends at
#   the closing } for the mod). The Python scanner below tracks brace depth to
#   exclude these regions. It handles the common `#[cfg(test)]\nmod tests { ... }`
#   pattern robustly. Nested cfg(test) items inside a test mod are also excluded.
#   Edge cases (multi-line attribute + intervening blank lines) are handled by
#   scanning for the attribute on any line before the `mod {` opener.
#
# Wire: make ci → scripts/ci.sh (which calls lint-sql.sh inline; this script
#   mirrors that pattern). Also wired as a standalone ci.yml job so it runs
#   without the full Rust toolchain (fast, cheap gate).
#
# Bypass: KHIVE_ALLOW_STUBS=1 bash scripts/check-no-stubs.sh
#   Use only under explicit Ocean direction; never in automated merge pipelines.

set -uo pipefail

[ "${KHIVE_ALLOW_STUBS:-0}" = "1" ] && exit 0

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

# Stub-intent words for unreachable!/panic! message arguments (case-insensitive).
# A bare unreachable!() or a message that does not contain these words is allowed.
STUB_WORDS="stub|not implemented|not yet|placeholder|fixme|tbd"

# Collect non-test .rs files under crates/, skipping the cargo build artifact
# directory and standard exclusion dirs.
RS_FILES=$(find "$ROOT/crates" \
    \( -name 'target' -o -name 'target-wt' \) -type d -prune \
    -o \( \
        -name '*.rs' \
        -not -path '*/tests/*' \
        -not -path '*/benches/*' \
        -not -path '*/examples/*' \
        -not -name '*_test.rs' \
        -not -name 'tests.rs' \
    \) -type f -print \
    | sort)

if [ -z "$RS_FILES" ]; then
    echo "check-no-stubs: no Rust source files found under crates/"
    exit 0
fi

python3 - "$RS_FILES" "$STUB_WORDS" <<'PY'
"""
Scan non-test Rust source for stub-intent macros.

Inline #[cfg(test)] blocks are stripped by tracking brace depth:
  - When a line matches #[cfg(test)] (possibly with whitespace), the scanner
    enters "pending_cfg_test" state.
  - When the next `mod <name> {` (or bare `{` on the next line) is seen while
    in that state, brace depth tracking starts and the block is excluded.
  - All lines within that brace-balanced block are skipped.

This handles the common pattern:
    #[cfg(test)]
    mod tests {
        ...  // excluded
    }

It does NOT handle #[cfg(test)] applied to items other than `mod` blocks (e.g.
a single `#[cfg(test)] fn foo() {}` one-liner). Those are rare in this codebase
and would at worst cause a false positive only if they contain stub macros, which
is an acceptable limitation given the "test-only stub" scenario is already handled
by the file-path exclusions for dedicated test files.
"""

import re
import sys

files_arg = sys.argv[1] if len(sys.argv) > 1 else ""
stub_words_arg = sys.argv[2] if len(sys.argv) > 2 else ""

files = [f for f in files_arg.split("\n") if f.strip()]
stub_pattern = re.compile(stub_words_arg, re.IGNORECASE) if stub_words_arg else None

# Macros that are ALWAYS blocked in non-test source (no message arg inspection).
ALWAYS_BLOCKED = re.compile(
    r"""
    (?<!\w)          # not preceded by word char (avoids matching my_todo!)
    (?:todo|unimplemented)
    \s*              # optional whitespace before the delimiter (e.g. `todo !()`)
    [!]              # the ! suffix
    \s*              # optional whitespace between ! and delimiter
    [\(\[\{]         # opening delimiter ( [ {
    """,
    re.VERBOSE,
)

# Macros that are blocked only when the message argument contains stub words.
CONDITIONAL_BLOCKED = re.compile(
    r"""
    (?<!\w)
    (?:unreachable|panic)
    \s*[!]\s*[\(\[\{]
    (.*)             # capture everything after the opening delimiter (same line)
    """,
    re.VERBOSE,
)


def in_cfg_test_block(lines):
    """
    Return a set of 1-based line numbers that are inside #[cfg(test)] mod blocks.

    Algorithm:
      - Track a `pending` flag: set when we see a line matching #[cfg(test)].
      - When `pending` and we see a line opening a block (`{`), start tracking
        brace depth. Every `{` increments, every `}` decrements. When depth
        hits 0 the block ends.
      - All lines from the opening `{` through the closing `}` are excluded.
      - Handles nested braces correctly.
    """
    excluded = set()
    pending = False
    in_block = False
    depth = 0

    cfg_test_re = re.compile(r"^\s*#\s*\[cfg\(test\)\]")
    # Matches a line that opens a mod/impl/trait/fn block — or just a `{` on its own.
    # We use this to detect the { that starts the cfg(test) region.
    opens_block_re = re.compile(r"\{")

    for i, line in enumerate(lines, 1):
        if in_block:
            excluded.add(i)
            # Count brace balance on this line (ignoring strings/comments — good enough
            # for the common case; false negatives are acceptable per spec).
            depth += line.count("{") - line.count("}")
            if depth <= 0:
                in_block = False
                depth = 0
            continue

        if cfg_test_re.match(line):
            pending = True
            excluded.add(i)  # exclude the attribute line itself
            continue

        if pending:
            # The line after #[cfg(test)] — may be a blank line, an inner attribute,
            # or the mod opener. Accept any line that contains a `{`.
            if opens_block_re.search(line):
                in_block = True
                depth = line.count("{") - line.count("}")
                excluded.add(i)
                pending = False
                if depth <= 0:
                    # Single-line block (rare) — already closed
                    in_block = False
                    depth = 0
            elif line.strip() == "" or line.strip().startswith("#"):
                # Blank line or another attribute line between #[cfg(test)] and the mod.
                excluded.add(i)
            else:
                # Not a block opener and not a blank/attr — the cfg(test) applies to
                # a non-mod item (e.g. a single fn). We can't reliably track its end,
                # so we conservatively exclude only the attribute line (already done)
                # and reset pending. The item itself might contain stubs — that is the
                # documented limitation.
                pending = False
        # else: normal line, not excluded

    return excluded


failed = 0

for path in files:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            content = fh.read()
    except OSError as e:
        print(f"{path}: could not read: {e}")
        failed += 1
        continue

    lines = content.splitlines()
    excluded = in_cfg_test_block(lines)

    for i, line in enumerate(lines, 1):
        if i in excluded:
            continue

        # Check always-blocked macros (todo!, unimplemented!).
        if ALWAYS_BLOCKED.search(line):
            # Skip if the match is inside a line comment.
            stripped = line.lstrip()
            if stripped.startswith("//"):
                continue
            print(f"{path}:{i}: stub macro — {line.strip()}")
            failed += 1
            continue

        # Check conditional macros (unreachable!, panic!) — only if stub words present.
        m = CONDITIONAL_BLOCKED.search(line)
        if m:
            stripped = line.lstrip()
            if stripped.startswith("//"):
                continue
            tail = m.group(1)
            if stub_pattern and stub_pattern.search(tail):
                print(f"{path}:{i}: stub-intent message — {line.strip()}")
                failed += 1

if failed:
    print(f"\ncheck-no-stubs: {failed} violation(s) — stubs in non-test source are not allowed")
    sys.exit(1)

print(f"check-no-stubs: {len(files)} file(s) scanned, 0 violations")
PY
