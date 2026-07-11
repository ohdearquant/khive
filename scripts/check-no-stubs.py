#!/usr/bin/env python3
"""Scan tracked crates/**/*.rs for stub markers.

Fails on production `todo!`, `unimplemented!`, and `dbg!` macro calls (any
delimiter, any whitespace before `!`), and on `panic!`/`unreachable!` calls
whose first string-literal argument contains a case-insensitive stub word:
"stub", "not implemented", "not yet", "placeholder", "fixme", "tbd". Only the
literal message text is scanned: comments inside the macro call, non-literal
arguments (bare identifiers or expressions), and later format arguments are
not message content and do not trigger a finding. Bare invariant calls and
ordinary diagnostic messages are allowed.

Complements the clippy restriction lints in scripts/ci.sh (AST-aware,
workspace-scoped for todo!/unimplemented!/dbg!); this scanner is a fast,
dependency-free second pass that also covers panic!/unreachable! stub
messages, which clippy does not gate.

Scoped to shipping source: `tests/`, `benches/`, `examples/` directories,
`*_test.rs`/`*_tests.rs`/`tests.rs` files, and inline `#[cfg(test)] mod`
blocks are excluded. Comments and string-literal content elsewhere in the file
are excluded from macro-name matching.
"""
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CRATES_DIR = REPO_ROOT / "crates"

STUB_MACROS = ("todo", "unimplemented", "dbg")
MESSAGE_MACROS = ("panic", "unreachable")
STUB_WORDS = ("stub", "not implemented", "not yet", "placeholder", "fixme", "tbd")
TEST_DIR_PARTS = ("tests", "benches", "examples")

CODE = "code"
COMMENT = "comment"
STRING = "string"

MACRO_RE = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*!\s*([(\[{])")
CLOSERS = {"(": ")", "[": "]", "{": "}"}


def is_test_path(path):
    rel_parts = path.relative_to(CRATES_DIR).parts
    if any(part in TEST_DIR_PARTS for part in rel_parts[:-1]):
        return True
    name = path.name
    return name == "tests.rs" or name.endswith("_test.rs") or name.endswith("_tests.rs")


def classify(source):
    """Per-character span classification: CODE, COMMENT, or STRING."""
    n = len(source)
    kinds = [CODE] * n
    i = 0
    while i < n:
        two = source[i:i + 2]
        if two == "//":
            start = i
            while i < n and source[i] != "\n":
                i += 1
            for j in range(start, i):
                kinds[j] = COMMENT
            continue
        if two == "/*":
            start = i
            depth = 1
            i += 2
            while i < n and depth > 0:
                if source[i:i + 2] == "/*":
                    depth += 1
                    i += 2
                elif source[i:i + 2] == "*/":
                    depth -= 1
                    i += 2
                else:
                    i += 1
            for j in range(start, i):
                kinds[j] = COMMENT
            continue
        raw_match = re.match(r'(b?r)(#*)"', source[i:i + 16])
        if raw_match:
            start = i
            closer = '"' + raw_match.group(2)
            i += raw_match.end()
            end_idx = source.find(closer, i)
            end_idx = n if end_idx == -1 else end_idx + len(closer)
            for j in range(start, end_idx):
                kinds[j] = STRING
            i = end_idx
            continue
        c = source[i]
        if c == '"' or two == 'b"':
            start = i
            i += 1 if c == '"' else 2
            while i < n:
                if source[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                i += 1
                if source[i - 1] == '"':
                    break
            for j in range(start, i):
                kinds[j] = STRING
            continue
        if c == "'":
            char_match = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^'\\\n])'", source[i:i + 12])
            if char_match:
                start = i
                i += char_match.end()
                for j in range(start, i):
                    kinds[j] = STRING
                continue
        i += 1
    return kinds


def find_matching_close(source, kinds, open_idx, open_ch):
    close_ch = CLOSERS[open_ch]
    depth = 1
    i = open_idx + 1
    n = len(source)
    while i < n:
        if kinds[i] == CODE:
            if source[i] == open_ch:
                depth += 1
            elif source[i] == close_ch:
                depth -= 1
                if depth == 0:
                    return i
        i += 1
    return n - 1


def line_of(source, idx):
    return source.count("\n", 0, idx) + 1


def first_string_literal(source, kinds, start, end):
    """Content of the first string-literal argument in source[start:end], or
    None if the first non-whitespace, non-comment token is not a string
    literal (a bare identifier/expression argument, e.g. `panic!(reason)`,
    carries no literal message to scan).
    """
    i = start
    while i < end:
        if kinds[i] == COMMENT or source[i].isspace():
            i += 1
            continue
        if kinds[i] == STRING:
            j = i
            while j < end and kinds[j] == STRING:
                j += 1
            return source[i:j]
        return None
    return None


def excluded_test_mod_spans(source, kinds):
    excluded = [False] * len(source)
    for attr in re.finditer(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]", source):
        if kinds[attr.start()] != CODE:
            continue
        brace_idx = source.find("{", attr.end())
        if brace_idx == -1:
            continue
        between = source[attr.end():brace_idx]
        if not re.search(r"\bmod\b", between):
            continue
        end_idx = find_matching_close(source, kinds, brace_idx, "{")
        for j in range(attr.start(), end_idx + 1):
            excluded[j] = True
    return excluded


def scan_file(path, rel):
    source = path.read_text(encoding="utf-8", errors="replace")
    kinds = classify(source)
    excluded = excluded_test_mod_spans(source, kinds)

    findings = []
    for match in MACRO_RE.finditer(source):
        idx = match.start()
        if kinds[idx] != CODE or excluded[idx]:
            continue
        name = match.group(1)
        open_ch = match.group(2)
        open_idx = match.end() - 1
        lineno = line_of(source, idx)
        if name in STUB_MACROS:
            findings.append((lineno, f"{rel}:{lineno}: production `{name}!` is not allowed"))
        elif name in MESSAGE_MACROS:
            close_idx = find_matching_close(source, kinds, open_idx, open_ch)
            literal = first_string_literal(source, kinds, open_idx + 1, close_idx)
            if literal is None:
                continue
            message = literal.lower()
            hit = next((w for w in STUB_WORDS if w in message), None)
            if hit:
                findings.append(
                    (lineno, f"{rel}:{lineno}: `{name}!` message contains stub marker {hit!r}")
                )
    return findings


def main():
    findings = []
    for path in sorted(CRATES_DIR.rglob("*.rs")):
        if is_test_path(path):
            continue
        rel = path.relative_to(REPO_ROOT)
        findings.extend(scan_file(path, rel))
    findings.sort()
    if findings:
        for _, msg in findings:
            print(msg, file=sys.stderr)
        print(f"\n{len(findings)} stub marker(s) found.", file=sys.stderr)
        return 1
    print("No-stub scan OK: no stub markers found in production source.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
