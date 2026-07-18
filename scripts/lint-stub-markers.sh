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

pub fn raw_stub(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        _ => panic!(r#"not implemented for other kinds yet"#),
    }
}

pub fn braced_stub(flag: bool) -> u32 {
    if !flag {
        panic! { "todo: handle the false branch" }
    }
    1
}

const NOTE: &str = "warning: #[cfg(test)] { this is not a real attribute }";

pub fn after_lookalike_string(flag: bool) -> u32 {
    if !flag {
        panic!("not implemented for this flag");
    }
    1
}

#[cfg(test)]
const GREETING: &str = "x";

pub fn after_nonbraced_cfg_test_item(flag: bool) -> u32 {
    if !flag {
        panic!("not implemented after a non-braced cfg(test) item");
    }
    1
}

pub fn after_long_comment_gap(flag: bool) -> u32 {
    if !flag {
        panic!(
            /* this block comment is deliberately long so that the gap of
               whitespace and comment text between the macro's opening
               delimiter and its message argument exceeds forty characters,
               /* nested comments are legal Rust and must not end the outer
                  comment early */
               which used to blind a fixed-width lookahead window */
            "todo: still not implemented after a long comment gap"
        );
    }
    1
}

pub fn after_byte_raw_string_literal(flag: bool) -> u32 {
    let _marker: &[u8] = br#"raw byte data, not a panic argument"#;
    if !flag {
        panic!("todo: still not implemented after a byte-raw string literal");
    }
    1
}
FIXTURE

    # Appended via printf (not the quoted heredoc above) so the fixture can
    # carry a real ESC byte and a real embedded newline inside the panic
    # message -- the exact CI-log-forgery shape blocking fix 2 sanitizes.
    printf '\npub fn ci_log_forgery_stub(flag: bool) -> u32 {\n    if !flag {\n        panic!("todo: stub \033[31m::error::forged\nmid-message newline injection");\n    }\n    1\n}\n' \
        >> "$tmp/case-fail/crates/fixture-crate/src/lib.rs"

    cat > "$tmp/case-pass/crates/fixture-crate/src/lib.rs" <<'FIXTURE'
pub fn dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        "b" => 2,
        _ => unreachable!("dispatch: unknown kind {kind:?} (validated by caller)"),
    }
}

pub fn raw_dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        _ => panic!(r#"dispatch: unknown kind {kind:?}"#),
    }
}

pub fn divide(a: i32, b: i32) -> i32 {
    if b == 0 {
        panic!("divide: b must be non-zero, got {b}");
    }
    a / b
}

pub fn help_text() -> &'static str {
    r#"call panic!("stub") to simulate a crash in the demo harness"#
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

#[cfg(test)]
fn const_generic_default_guard<const N: usize = { 4 }>() -> usize {
    if N == 0 {
        panic!("todo: still a stub guarded by cfg(test), must never reach shipping scan");
    }
    N
}
FIXTURE

    status=0

    # Exactly 8 markers are seeded in case-fail above; asserting the count
    # (not just substring presence) catches a parser-overmatch regression
    # that would otherwise slip through as an unnoticed extra finding.
    expected_marker_count=8

    if scan "$tmp/case-fail" > "$tmp/fail.log" 2>&1; then
        echo "self-test FAILED: expected placeholder call sites were not caught"
        cat "$tmp/fail.log"
        status=1
    else
        found_marker_count=$(grep -c 'reads as a placeholder stub' "$tmp/fail.log")
        if [ "$found_marker_count" -ne "$expected_marker_count" ]; then
            echo "self-test FAILED: expected exactly $expected_marker_count findings, got $found_marker_count"
            cat "$tmp/fail.log"
            status=1
        fi
        for marker in \
            "stub: not implemented for other kinds yet" \
            "not implemented for other kinds yet" \
            "todo: handle the false branch" \
            "not implemented for this flag" \
            "not implemented after a non-braced cfg(test) item" \
            "todo: still not implemented after a long comment gap" \
            "todo: still not implemented after a byte-raw string literal" \
            "mid-message newline injection"
        do
            if ! grep -qF "$marker" "$tmp/fail.log"; then
                echo "self-test FAILED: expected finding missing: $marker"
                cat "$tmp/fail.log"
                status=1
            fi
        done

        # blocking fix 2: the ci_log_forgery_stub message above carries a raw
        # ANSI escape, a literal "::error::forged" workflow-command shape, and
        # an embedded newline -- none of the three may survive into CI stdout.
        esc="$(printf '\033')"
        if grep -qF "$esc" "$tmp/fail.log"; then
            echo "self-test FAILED: a raw ANSI escape byte leaked into CI output"
            cat "$tmp/fail.log"
            status=1
        fi
        if grep -qF '::error::forged' "$tmp/fail.log"; then
            echo "self-test FAILED: an unbroken '::error::forged' workflow command leaked into CI output"
            cat "$tmp/fail.log"
            status=1
        fi
        if grep -n '^::' "$tmp/fail.log" | grep -q .; then
            echo "self-test FAILED: a CI output line starts with '::' (workflow-command syntax)"
            cat "$tmp/fail.log"
            status=1
        fi
        if ! grep -q 'mid-message newline injection.*reads as a placeholder stub' "$tmp/fail.log"; then
            echo "self-test FAILED: the embedded newline in the panic message split the CI log line in two"
            cat "$tmp/fail.log"
            status=1
        fi
    fi

    if ! scan "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: legitimate panic!/unreachable! messages (raw strings, lookalike text inside strings, a #[cfg(test)] helper named StubService, a #[cfg(test)] item with a signature brace before its body) false-positived:"
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
        echo "no source files matched crates/*/src/**/*.rs under $root/crates (excluding target*/tests/benches/examples) -- the scanner would silently be a no-op; fix the file-layout selection" >&2
        return 1
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
# Rust accepts whitespace before `!` and any of the three delimiter kinds
# (`panic!(...)`, `panic!{...}`, `panic![...]`) -- all three are legal macro
# call syntax, not just parens.
MACRO_CALL_RE = re.compile(r"\b(panic|unreachable)\s*!\s*([(\{\[])")
STRING_LIT_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
CFG_TEST_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
WHITESPACE_RE = re.compile(r"\s*")
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b.")


def sanitize_for_ci(raw):
    """The panic!/unreachable! message text this scanner echoes into CI
    stdout comes straight from a PR-controlled string literal. GitHub Actions
    parses a `::name ...::value`-shaped line as a workflow command
    (`::error::`, `::add-mask::`, `::set-output::`, ...), so an embedded
    newline could let attacker text start a fresh line and forge one, and
    ANSI escapes can rewrite terminal/log-viewer state. Strip ANSI escapes,
    collapse newlines to a literal `\\n` (never a real line break), and break
    every `::` so no substring can be parsed as workflow-command syntax --
    all while keeping the text readable for a human operator."""
    s = ANSI_ESCAPE_RE.sub("", raw)
    s = s.replace("\x1b", "")
    s = s.replace("\r\n", "\\n").replace("\n", "\\n").replace("\r", "\\n")
    s = s.replace("::", ": :")
    return s


def raw_string_end(text, i):
    """If text[i] starts a raw or byte-raw string literal (`r#*"..."#*` or
    `br#*"..."#*`, word-boundary checked at the start of the prefix so an
    identifier ending in `r`/`br` -- e.g. `xr"..."` or `abr"..."` -- is never
    mistaken for one), return the offset just past its closing quote+hashes
    (len(text) if unterminated). Otherwise None."""
    n = len(text)
    if i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_"):
        return None
    if text[i] == "b" and i + 1 < n and text[i + 1] == "r":
        j = i + 2
    elif text[i] == "r":
        j = i + 1
    else:
        return None
    hashes = 0
    while j < n and text[j] == "#":
        hashes += 1
        j += 1
    if j >= n or text[j] != '"':
        return None
    closer = '"' + "#" * hashes
    end = text.find(closer, j + 1)
    return n if end == -1 else end + len(closer)


def strip_comments_and_char_lits(text):
    """Blank out //, /* */ comments (nesting -- Rust block comments nest, so
    a depth counter tracks inner `/* */` pairs rather than ending at the
    first `*/`) and char literals, so brace-counting and macro-matching
    never trip on braces/quotes living inside them. Plain and raw string
    literals are left untouched -- their content is exactly what this
    script inspects. Raw strings are hand-scanned to their matching
    "###-count closer so quotes inside them (e.g. `r#"say "hi""#`) do not
    prematurely end the literal."""
    out = []
    i = 0
    n = len(text)
    in_line_comment = False
    block_depth = 0
    in_string = False
    while i < n:
        c = text[i]
        if in_line_comment:
            out.append(" " if c != "\n" else "\n")
            if c == "\n":
                in_line_comment = False
            i += 1
            continue
        if block_depth > 0:
            if c == "/" and i + 1 < n and text[i + 1] == "*":
                block_depth += 1
                out.append("  ")
                i += 2
                continue
            if c == "*" and i + 1 < n and text[i + 1] == "/":
                block_depth -= 1
                out.append("  ")
                i += 2
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
            block_depth = 1
            out.append("  ")
            i += 2
            continue
        rs_end = raw_string_end(text, i)
        if rs_end is not None:
            out.append(text[i:rs_end])
            i = rs_end
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


def match_string_literal(text, pos):
    """Match a plain or raw string literal beginning at pos in `text` (which
    must have string content intact, i.e. `clean`, never the strings-blanked
    variant). Returns (end_offset, inner_content) or None."""
    if pos >= len(text):
        return None
    if text[pos] == '"':
        m = STRING_LIT_RE.match(text, pos)
        return None if m is None else (m.end(), m.group(1))
    rs_end = raw_string_end(text, pos)
    if rs_end is None:
        return None
    j = pos + (2 if text[pos] == "b" else 1)  # skip the `br`/`r` prefix
    hashes = 0
    while text[j] == "#":
        hashes += 1
        j += 1
    return rs_end, text[j + 1 : rs_end - 1 - hashes]


def blank_strings(clean_text):
    """Given `clean` (comments/char-lits stripped, strings intact), replace
    every plain and raw string literal's content with spaces (newlines kept)
    so downstream scans (cfg(test) attribute detection, macro-call matching)
    never mistake text living inside a string literal for real code."""
    out = list(clean_text)
    i = 0
    n = len(clean_text)
    while i < n:
        c = clean_text[i]
        end = None
        if c == '"':
            m = STRING_LIT_RE.match(clean_text, i)
            if m is not None:
                end = m.end()
        else:
            end = raw_string_end(clean_text, i)
        if end is not None:
            for k in range(i, end):
                if out[k] != "\n":
                    out[k] = " "
            i = end
            continue
        i += 1
    return "".join(out)


def find_item_head_end(code_only, start):
    """Scan an item's head (from just after its `#[cfg(test)]` attribute) for
    the item's own body-opening `{` or, for a non-braced item, its
    terminating `;` -- both counted only at signature level, i.e. outside any
    `()`, `[]`, or `<>` nesting. A brace living inside the signature (a
    generic const default's block, e.g. `<const N: usize = { 4 }>`, or an
    array-length const block in a where-clause bound, e.g.
    `where [(); { 1 }]: Sized`) is not the item body -- it is skipped as a
    balanced, opaque unit so it can never be mistaken for the body opener.
    Returns `("brace", offset)` / `("semi", offset)`, or None if the head
    runs off the end of the file unterminated."""
    n = len(code_only)
    i = start
    paren = bracket = angle = 0
    while i < n:
        c = code_only[i]
        if c == "(":
            paren += 1
        elif c == ")":
            paren = max(0, paren - 1)
        elif c == "[":
            bracket += 1
        elif c == "]":
            bracket = max(0, bracket - 1)
        elif c == "<":
            angle += 1
        elif c == ">":
            if angle > 0:
                angle -= 1
        elif c == "{":
            if paren == 0 and bracket == 0 and angle == 0:
                return ("brace", i)
            depth = 1
            i += 1
            while i < n and depth > 0:
                if code_only[i] == "{":
                    depth += 1
                elif code_only[i] == "}":
                    depth -= 1
                i += 1
            continue
        elif c == ";":
            if paren == 0 and bracket == 0 and angle == 0:
                return ("semi", i)
        i += 1
    return None


def test_gated_spans(code_only):
    """Byte-offset [start, end) spans of every #[cfg(test)]-attributed item --
    clippy --lib --bins never compiles these (no --cfg test), so the scanner
    must not flag placeholder messages living only in test code. Runs on the
    strings-blanked `code_only` text so a `#[cfg(test)]`-shaped substring
    living inside a string literal is never mistaken for a real attribute.
    For a non-braced item (e.g. `#[cfg(test)] const X: u32 = 1;`) the span
    ends at the terminating `;`; for a braced item it ends at the body's
    balanced close, per find_item_head_end above."""
    spans = []
    n = len(code_only)
    for m in CFG_TEST_RE.finditer(code_only):
        head = find_item_head_end(code_only, m.end())
        if head is None:
            continue
        kind, pos = head
        if kind == "semi":
            spans.append((m.start(), pos + 1))
            continue
        depth = 1
        i = pos + 1
        while i < n and depth > 0:
            if code_only[i] == "{":
                depth += 1
            elif code_only[i] == "}":
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
    code_only = blank_strings(clean)
    test_spans = test_gated_spans(code_only)

    for m in MACRO_CALL_RE.finditer(code_only):
        if in_span(m.start(), test_spans):
            continue
        call_start = m.end() - 1  # the opening delimiter char (`(`/`{`/`[`)
        # Only consider a string literal immediately after the delimiter --
        # this is the macro's message argument, not some unrelated string
        # buried deeper in a multi-arg call. Rust allows arbitrary whitespace
        # and comments between the delimiter and the argument; `clean` has
        # already blanked every comment to whitespace (nesting-aware), so
        # skipping whitespace with no length cap is a full trivia skip, not
        # just a wider fixed window -- a placeholder message is caught no
        # matter how much trivia precedes it. Read from `clean` (strings
        # intact), never `code_only` (which has blanked exactly the string
        # content being looked for here).
        after_delim = call_start + 1
        trivia = WHITESPACE_RE.match(clean, after_delim)
        msg_start = trivia.end()
        msg_m = match_string_literal(clean, msg_start)
        if msg_m is None:
            continue
        _, message = msg_m
        if PLACEHOLDER_RE.search(message):
            line_no = clean.count("\n", 0, m.start()) + 1
            macro_name = m.group(1)
            safe_message = sanitize_for_ci(message)
            findings.append(f"{path}:{line_no}: {macro_name}!(\"{safe_message}\") reads as a placeholder stub, not a real error path")

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
