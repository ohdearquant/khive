#!/bin/sh
# Detect placeholder-string `panic!`/`unreachable!` calls in shipping source
# (#560, closes the gap in the clippy-based No-Stub Guard).
#
# `-Dclippy::todo`/`-Dclippy::unimplemented` (scripts/ci.sh phase_no_stubs)
# unconditionally deny `todo!()`/`unimplemented!()` regardless of message, but
# `panic!`/`unreachable!` are legitimate everywhere (assertion failures,
# invariant violations) -- denying them outright would fail hundreds of
# existing, correct call sites. The actual stub signal is the MESSAGE: a
# `panic!("not implemented yet")` or `unreachable!("stub")` is a placeholder
# wearing a real macro. This scans the string literal argument of every
# panic!/unreachable! call for that language, not the macro itself.
#
# Scope mirrors phase_no_stubs (`--lib --bins`): only crates/*/src (workspace +
# the forward-deployed khive-merge crate), never crates/*/tests, benches, or
# examples. Within src, a `#[cfg(test)]`-gated item is source clippy never
# type-checks under `--lib --bins` (no `--cfg test`), so this scanner skips
# those blocks the same way.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    mkdir -p "$tmp/case-fail/crates/fixture-crate/src"
    mkdir -p "$tmp/case-pass/crates/fixture-crate/src"

    cat > "$tmp/case-fail/crates/fixture-crate/src/lib.rs" <<'FIXTURE'
pub fn dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        _ => unreachable!("stub: not implemented for other kinds yet"),
    }
}
FIXTURE

    cat > "$tmp/case-pass/crates/fixture-crate/src/lib.rs" <<'FIXTURE'
pub fn dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        "b" => 2,
        _ => unreachable!("dispatch: unknown kind {kind:?} (validated by caller)"),
    }
}

pub fn divide(a: i32, b: i32) -> i32 {
    if b == 0 {
        panic!("divide: b must be non-zero, got {b}");
    }
    a / b
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubService;

    impl StubService {
        fn call(&self) {
            panic!("StubService::call must not be invoked in this test")
        }
    }

    #[test]
    fn dispatch_a() {
        assert_eq!(dispatch("a"), 1);
    }
}
FIXTURE

    status=0

    if scan "$tmp/case-fail" > "$tmp/fail.log" 2>&1; then
        echo "self-test FAILED: placeholder unreachable!(\"stub: ...\") was not caught"
        cat "$tmp/fail.log"
        status=1
    elif ! grep -q "stub" "$tmp/fail.log"; then
        echo "self-test FAILED: scan failed, but not for the expected reason:"
        cat "$tmp/fail.log"
        status=1
    fi

    if ! scan "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: legitimate panic!/unreachable! messages (incl. a #[cfg(test)] helper named StubService) false-positived:"
        cat "$tmp/pass.log"
        status=1
    fi

    if [ "$status" -eq 0 ]; then
        echo "lint-stub-markers self-test: OK"
    fi
    return "$status"
}

scan() {
    root="$1"
    files=$(find "$root/crates" \
        \( -name 'target' -o -name 'target-wt' -o -name 'tests' -o -name 'benches' -o -name 'examples' \) -type d -prune \
        -o -path '*/src/*.rs' -type f -print \
        | sort)

    if [ -z "$files" ]; then
        echo "no source files found under $root/crates"
        return 0
    fi

    python3 - "$files" <<'PY'
import re
import sys

files = [f for f in sys.argv[1].split("\n") if f.strip()]

PLACEHOLDER_RE = re.compile(
    r"\b(stub|todo|fixme|placeholder|unimplemented|not\s*yet\s*implemented|"
    r"not\s*implemented|coming\s*soon)\b",
    re.IGNORECASE,
)
MACRO_CALL_RE = re.compile(r"\b(panic|unreachable)!\s*\(")
STRING_LIT_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
CFG_TEST_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def strip_comments_and_char_lits(text):
    """Blank out //, /* */ comments and char literals so brace-counting and
    macro-matching never trip on braces/quotes living inside them. String
    literals are left untouched -- their content is exactly what this script
    inspects."""
    out = []
    i = 0
    n = len(text)
    in_line_comment = False
    in_block_comment = False
    in_string = False
    while i < n:
        c = text[i]
        if in_line_comment:
            out.append(" " if c != "\n" else "\n")
            if c == "\n":
                in_line_comment = False
            i += 1
            continue
        if in_block_comment:
            if c == "*" and i + 1 < n and text[i + 1] == "/":
                out.append("  ")
                i += 2
                in_block_comment = False
                continue
            out.append(" " if c != "\n" else "\n")
            i += 1
            continue
        if in_string:
            out.append(c)
            if c == "\\" and i + 1 < n:
                out.append(text[i + 1])
                i += 2
                continue
            if c == '"':
                in_string = False
            i += 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            in_line_comment = True
            out.append("  ")
            i += 2
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            in_block_comment = True
            out.append("  ")
            i += 2
            continue
        if c == '"':
            in_string = True
            out.append(c)
            i += 1
            continue
        if c == "'" and i + 1 < n:
            # char literal e.g. 'a', '\'', or a lifetime 'a -- only blank the
            # `'x'`/`'\x'` form so lifetimes (no closing quote) pass through.
            j = i + 1
            if text[j] == "\\" and j + 1 < n:
                j += 2
            else:
                j += 1
            if j < n and text[j] == "'":
                out.append(" " * (j - i + 1))
                i = j + 1
                continue
        out.append(c)
        i += 1
    return "".join(out)


def test_gated_spans(clean_text):
    """Byte-offset [start, end) spans of every #[cfg(test)]-attributed item's
    body -- clippy --lib --bins never compiles these (no --cfg test), so the
    scanner must not flag placeholder messages living only in test code."""
    spans = []
    for m in CFG_TEST_RE.finditer(clean_text):
        brace_open = clean_text.find("{", m.end())
        if brace_open == -1:
            continue
        depth = 1
        i = brace_open + 1
        n = len(clean_text)
        while i < n and depth > 0:
            if clean_text[i] == "{":
                depth += 1
            elif clean_text[i] == "}":
                depth -= 1
            i += 1
        spans.append((m.start(), i))
    return spans


def in_span(offset, spans):
    return any(start <= offset < end for start, end in spans)


findings = []
for path in files:
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
    clean = strip_comments_and_char_lits(text)
    test_spans = test_gated_spans(clean)

    for m in MACRO_CALL_RE.finditer(clean):
        if in_span(m.start(), test_spans):
            continue
        call_start = m.end() - 1  # the '(' of the macro call
        # Only consider a string literal immediately after the '(' (allowing
        # whitespace) -- this is the macro's message argument, not some
        # unrelated string buried deeper in a multi-arg call.
        lookahead = clean[call_start + 1 : call_start + 1 + 40]
        stripped = lookahead.lstrip()
        if not stripped.startswith('"'):
            continue
        msg_start = call_start + 1 + (len(lookahead) - len(stripped))
        msg_m = STRING_LIT_RE.match(clean, msg_start)
        if msg_m is None:
            continue
        message = msg_m.group(1)
        if PLACEHOLDER_RE.search(message):
            line_no = clean.count("\n", 0, m.start()) + 1
            macro_name = m.group(1)
            findings.append(f"{path}:{line_no}: {macro_name}!(\"{message}\") reads as a placeholder stub, not a real error path")

if findings:
    for f in findings:
        print(f)
    print(f"\nstub-marker lint: {len(findings)} issue(s)")
    sys.exit(1)

print(f"stub-marker lint: {len(files)} file(s) OK")
PY
}

case "${1:-}" in
    --self-test)
        self_test
        ;;
    "")
        scan "$ROOT"
        ;;
    *)
        echo "usage: $0 [--self-test]" >&2
        exit 2
        ;;
esac
