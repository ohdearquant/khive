#!/bin/sh
# Detect placeholder-string `panic!`/`unreachable!` calls in crate source
# (#560, closes the gap in the clippy-based No-Stub Guard).
#
# `-Dclippy::todo`/`-Dclippy::unimplemented` (scripts/ci.sh phase_no_stubs)
# unconditionally deny `todo!()`/`unimplemented!()` regardless of message, but
# `panic!`/`unreachable!` are legitimate everywhere (assertion failures,
# invariant violations) -- denying them outright would fail hundreds of
# existing, correct call sites. The actual stub signal is the MESSAGE: a
# `panic!("not implemented yet")` or `unreachable!("stub")` is a placeholder
# wearing a real macro. This scans the string literal argument(s) of every
# panic!/unreachable! call for that language, not the macro itself.
#
# Scans every `.rs` file under crates/ -- source, tests, benches, and
# examples alike (build-artifact `target`/`.cargo-target` dirs excluded,
# same as .gitignore). There is no reachability/cfg analysis: a placeholder
# message is a placeholder message whether or not the code compiling it is
# test-gated. The small number of legitimate matches (a test mock whose
# type/method name happens to contain a placeholder word) are suppressed via
# the explicit, in-diff reviewed allowlist at stub-marker-allowlist.txt --
# see that file's header for the format.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    mkdir -p "$tmp/case-fail/crates/fixture-crate/src"
    mkdir -p "$tmp/case-fail/crates/fixture-crate/tests"
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

pub fn after_format_arg_stub(flag: bool) -> u32 {
    if !flag {
        panic!("{}", "todo: not implemented for the format-arg case");
    }
    1
}

pub fn after_concat_stub(flag: bool) -> u32 {
    if !flag {
        panic!(concat!("todo: ", "concat-assembled stub message"));
    }
    1
}

pub fn unicode_escape_stub(flag: bool) -> u32 {
    if !flag {
        panic!("t\u{6f}do: only catches via unicode escape decoding");
    }
    1
}

pub fn hex_escape_stub(flag: bool) -> u32 {
    if !flag {
        panic!("s\x74ub: only catches via hex escape decoding");
    }
    1
}

pub fn sql_with_line_continuation() -> &'static str {
    // Rust's backslash-newline string continuation (used throughout this
    // codebase for long multi-line SQL literals) strips the newline and
    // following whitespace from the string value -- it must not desync the
    // lexer for whatever code follows it.
    "SELECT * FROM widgets \
     WHERE active = 1"
}

pub fn after_line_continuation_string(flag: bool) -> u32 {
    if !flag {
        panic!("todo: still not implemented after a backslash-newline string continuation");
    }
    1
}

#[cfg(test)]
fn const_generic_default_guard<const N: usize = { 4 }>() -> usize {
    if N == 0 {
        panic!("todo: a placeholder inside a #[cfg(test)] item must now be caught");
    }
    N
}
FIXTURE

    # Appended via printf (not the quoted heredoc above) so the fixture can
    # carry a real ESC byte and a real embedded newline inside the panic
    # message -- the exact CI-log-forgery shape this scanner sanitizes.
    printf '\npub fn ci_log_forgery_stub(flag: bool) -> u32 {\n    if !flag {\n        panic!("todo: stub \033[31m::error::forged\nmid-message newline injection");\n    }\n    1\n}\n' \
        >> "$tmp/case-fail/crates/fixture-crate/src/lib.rs"

    # A placeholder panic living under tests/ (previously pruned from
    # discovery entirely) must now be caught -- nothing is excluded by
    # location anymore, only by the explicit allowlist.
    cat > "$tmp/case-fail/crates/fixture-crate/tests/integration.rs" <<'TESTFIXTURE'
#[test]
fn placeholder_inside_tests_dir() {
    panic!("todo: a placeholder inside a tests/ directory file must now be caught");
}
TESTFIXTURE

    # Filename-injection fixture: the filename itself -- not the panic
    # message -- carries a real newline and an unbroken `::error::forged`
    # sequence. If a rendered finding line ever echoes this path
    # unsanitized, it forges a GitHub Actions workflow command the same way
    # an unsanitized message would.
    newline_filename="exploit$(printf '\n')::error::forged.rs"
    cat > "$tmp/case-fail/crates/fixture-crate/src/$newline_filename" <<'NLFIXTURE'
pub fn newline_filename_stub() -> u32 {
    panic!("todo: this file's name itself carries a newline and a forged CI workflow command sequence")
}
NLFIXTURE

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

pub fn format_arg_dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        _ => panic!("{}", "dispatch: unexpected kind encountered during normal operation"),
    }
}

pub fn concat_arg_dispatch(kind: &str) -> u32 {
    match kind {
        "a" => 1,
        _ => panic!(concat!("dispatch failure: ", "unexpected kind encountered")),
    }
}

// Mirrors the real crates/kkernel/src/reindex.rs mock pattern (a #[cfg(test)]
// EmbeddingService mock named StubService whose guard message names the
// mock's own type/method, not a real placeholder) -- the reason
// stub-marker-allowlist.txt exists.
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

    # Exactly 15 markers are seeded in case-fail above; asserting the count
    # (not just substring presence) catches a parser-overmatch regression
    # that would otherwise slip through as an unnoticed extra finding.
    expected_marker_count=15

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
            "todo: still not implemented after a long comment gap" \
            "todo: still not implemented after a byte-raw string literal" \
            "todo: not implemented for the format-arg case" \
            "concat-assembled stub message" \
            "only catches via unicode escape decoding" \
            "only catches via hex escape decoding" \
            "still not implemented after a backslash-newline string continuation" \
            "a placeholder inside a #[cfg(test)] item must now be caught" \
            "mid-message newline injection" \
            "carries a newline and a forged CI workflow command sequence" \
            "a placeholder inside a tests/ directory file must now be caught"
        do
            if ! grep -qF "$marker" "$tmp/fail.log"; then
                echo "self-test FAILED: expected finding missing: $marker"
                cat "$tmp/fail.log"
                status=1
            fi
        done

        # the ci_log_forgery_stub message above carries a raw ANSI escape, a
        # literal "::error::forged" workflow-command shape, and an embedded
        # newline -- none of the three may survive into CI stdout.
        esc="$(printf '\033')"
        if grep -qF "$esc" "$tmp/fail.log"; then
            echo "self-test FAILED: a raw ANSI escape byte leaked into CI output"
            cat "$tmp/fail.log"
            status=1
        fi
        if grep -qF '::error::forged' "$tmp/fail.log"; then
            echo "self-test FAILED: an unbroken '::error::forged' workflow command leaked into CI output (from a message or a filename)"
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

        # the fixture filename itself (not its content) carries a real
        # newline and an unbroken "::error::forged" sequence -- the rendered
        # path must be sanitized the same way a message is, and the whole
        # finding must survive as a single, unbroken CI line.
        if ! grep -Eq 'exploit.*forged\.rs.*reads as a placeholder stub' "$tmp/fail.log"; then
            echo "self-test FAILED: the embedded newline in the fixture filename split the CI finding line in two, or the sanitized path did not render as expected"
            cat "$tmp/fail.log"
            status=1
        fi
    fi

    if ! scan "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: legitimate panic!/unreachable! messages (raw strings, lookalike text inside strings, a #[cfg(test)] helper named StubService) false-positived:"
        cat "$tmp/pass.log"
        status=1
    fi

    # Allowlist suppression: one fixture finding matches an allowlist entry
    # (by path substring AND decoded-message substring) and must be
    # suppressed; a sibling finding that does not match must still surface.
    mkdir -p "$tmp/case-allowlist/crates/fixture-crate/src"
    cat > "$tmp/case-allowlist/crates/fixture-crate/src/lib.rs" <<'ALWFIXTURE'
pub fn allowlisted_stub() -> u32 {
    panic!("todo: this one is allowlisted and must be suppressed")
}

pub fn not_allowlisted_stub() -> u32 {
    panic!("todo: this one is NOT allowlisted and must still be caught")
}
ALWFIXTURE
    printf 'fixture-crate/src/lib.rs\tthis one is allowlisted\n' > "$tmp/self-test-allowlist.txt"

    if STUB_MARKER_ALLOWLIST="$tmp/self-test-allowlist.txt" scan "$tmp/case-allowlist" > "$tmp/allowlist.log" 2>&1; then
        echo "self-test FAILED: the non-allowlisted sibling finding should have failed the scan"
        cat "$tmp/allowlist.log"
        status=1
    else
        if grep -qF 'this one is allowlisted' "$tmp/allowlist.log"; then
            echo "self-test FAILED: an allowlisted finding was not suppressed"
            cat "$tmp/allowlist.log"
            status=1
        fi
        if ! grep -qF 'this one is NOT allowlisted' "$tmp/allowlist.log"; then
            echo "self-test FAILED: a sibling non-allowlisted finding was incorrectly suppressed"
            cat "$tmp/allowlist.log"
            status=1
        fi
    fi

    if [ "$status" -eq 0 ]; then
        echo "lint-stub-markers self-test: OK"
    fi
    return "$status"
}

scan() {
    root="$1"
    file_list="$(mktemp)"

    # NUL-delimited discovery and transport end-to-end: a filename may
    # legally contain a newline (or any byte but NUL and `/`), and this
    # scanner's own findings later echo filenames straight into CI stdout.
    # `-print0` + a NUL-delimited transport file sidesteps splitting
    # entirely; `sanitize_for_ci` (applied to every rendered path below, the
    # same function already used on messages) handles the render side.
    #
    # Only `target`/`.cargo-target` (build-artifact dirs, per .gitignore)
    # are pruned -- everything else under crates/, including tests/,
    # benches/, and examples/, is in scan scope.
    find "$root/crates" \
        \( -name 'target' -o -name '.cargo-target' \) -type d -prune \
        -o -name '*.rs' -type f -print0 \
        > "$file_list"

    if [ ! -s "$file_list" ]; then
        rm -f "$file_list"
        echo "no .rs files matched under $root/crates (excluding target/.cargo-target build-artifact dirs) -- the scanner would silently be a no-op; fix the file-layout selection" >&2
        return 1
    fi

    allowlist="${STUB_MARKER_ALLOWLIST:-$SCRIPT_DIR/stub-marker-allowlist.txt}"

    python3 - "$file_list" "$allowlist" <<'PY'
import os
import re
import sys

with open(sys.argv[1], "rb") as fh:
    files = sorted(os.fsdecode(f) for f in fh.read().split(b"\0") if f)

ALLOWLIST_PATH = sys.argv[2]

PLACEHOLDER_RE = re.compile(
    r"\b(stub|todo|fixme|placeholder|unimplemented|not\s*yet\s*implemented|"
    r"not\s*implemented|coming\s*soon)\b",
    re.IGNORECASE,
)
# Rust accepts whitespace before `!` and any of the three delimiter kinds
# (`panic!(...)`, `panic!{...}`, `panic![...]`) -- all three are legal macro
# call syntax, not just parens.
MACRO_CALL_RE = re.compile(r"\b(panic|unreachable)\s*!\s*([(\{\[])")
# re.DOTALL: Rust's backslash-newline string continuation (`"...\<newline>
# ..."`, used throughout this codebase for long multi-line SQL literals)
# puts a literal `\` immediately before a `\n` inside a plain string. Without
# DOTALL, `\.` in the escaped-char alternative never matches that `\n`, so
# the literal never finds its closing quote here -- and the next real `"`
# anywhere later in the file gets mistaken for a fresh opening quote,
# silently blanking arbitrary code (including real panic!/unreachable!
# calls) as if it were string content. `[^"\\]` already matches newlines
# regardless of DOTALL (character classes aren't affected by the flag); this
# only changes what `\.` matches.
STRING_LIT_RE = re.compile(r'"((?:[^"\\]|\\.)*)"', re.DOTALL)
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b.")
CLOSERS = {"(": ")", "{": "}", "[": "]"}
ESCAPE_RE = re.compile(r"\\(n|r|t|\\|\"|'|0|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\})")
SIMPLE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0"}


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
    variant). Returns (end_offset, inner_content, is_raw) or None."""
    if pos >= len(text):
        return None
    if text[pos] == '"':
        m = STRING_LIT_RE.match(text, pos)
        return None if m is None else (m.end(), m.group(1), False)
    rs_end = raw_string_end(text, pos)
    if rs_end is None:
        return None
    j = pos + (2 if text[pos] == "b" else 1)  # skip the `br`/`r` prefix
    hashes = 0
    while text[j] == "#":
        hashes += 1
        j += 1
    return rs_end, text[j + 1 : rs_end - 1 - hashes], True


def blank_strings(clean_text):
    """Given `clean` (comments/char-lits stripped, strings intact), replace
    every plain and raw string literal's content with spaces (newlines kept)
    so downstream macro-call matching never mistakes text living inside a
    string literal for real code."""
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


def find_matching_close(code_only, open_pos):
    """code_only[open_pos] is an opening `(`/`{`/`[`; return the offset of its
    same-type-nesting-depth matching close, or len(code_only) if
    unterminated. Runs on `code_only` (strings already blanked to spaces),
    so a bracket character living inside string content can never desync
    the depth count."""
    open_ch = code_only[open_pos]
    close_ch = CLOSERS[open_ch]
    n = len(code_only)
    depth = 1
    i = open_pos + 1
    while i < n and depth > 0:
        if code_only[i] == open_ch:
            depth += 1
        elif code_only[i] == close_ch:
            depth -= 1
        i += 1
    return i - 1 if depth == 0 else n


def collect_string_literals(clean, start, end):
    """All (content, is_raw) string-literal pairs found anywhere in
    clean[start:end], in source order. Used to scan a panic!/unreachable!
    macro's full balanced argument list -- not just the token immediately
    after the opening delimiter -- so a placeholder message hiding behind a
    leading format-string argument (`panic!("{}", "todo: ...")`) or a
    nested `concat!("not ", "implemented")` call is still found. Does not
    evaluate non-literal arguments (variables, function calls, `format!`
    results assembled elsewhere) -- those are invisible to a static text
    scan and are out of scope."""
    parts = []
    i = start
    while i < end:
        lit = match_string_literal(clean, i)
        if lit is not None and lit[0] <= end:
            parts.append((lit[1], lit[2]))
            i = lit[0]
            continue
        i += 1
    return parts


def decode_rust_escapes(body, is_raw):
    """Decode the Rust string-literal escapes a PLACEHOLDER_RE match needs
    to see through (\\n \\r \\t \\\\ \\" \\' \\0, \\xNN, \\u{...}) so an
    escape-obfuscated placeholder (e.g. `panic!("t\\u{6f}do: ...")`) is
    still caught. Raw literals (`r"..."`, `r#"..."#`) have no escapes --
    returned unchanged. Any other backslash sequence is left as-is."""
    if is_raw:
        return body

    def repl(m):
        tok = m.group(1)
        if tok in SIMPLE_ESCAPES:
            return SIMPLE_ESCAPES[tok]
        if tok[0] == "x":
            return chr(int(tok[1:], 16))
        return chr(int(tok[2:-1], 16))  # u{...}

    return ESCAPE_RE.sub(repl, body)


def load_allowlist(path):
    """Parse `<path-substring>\\t<message-substring>` entries from the
    allowlist file at `path`. Blank lines and lines starting with `#` (after
    stripping leading whitespace) are ignored. A missing file (e.g. a caller
    override pointing at a path that does not exist) is an empty allowlist,
    not an error."""
    entries = []
    if not os.path.isfile(path):
        return entries
    with open(path, "r", encoding="utf-8") as fh:
        for raw_line in fh:
            line = raw_line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            if "\t" not in line:
                continue
            path_sub, message_sub = line.split("\t", 1)
            entries.append((path_sub, message_sub))
    return entries


def is_allowlisted(path, message, entries):
    """A finding is suppressed iff some entry's path-substring is contained
    in the file path AND its message-substring is contained in the decoded
    panic!/unreachable! message. An entry that matches nothing in a given
    run is not an error."""
    return any(path_sub in path and message_sub in message for path_sub, message_sub in entries)


allowlist = load_allowlist(ALLOWLIST_PATH)

findings = []
for path in files:
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
    clean = strip_comments_and_char_lits(text)
    code_only = blank_strings(clean)

    for m in MACRO_CALL_RE.finditer(code_only):
        call_start = m.end() - 1  # the opening delimiter char (`(`/`{`/`[`)
        # Scan every string literal within the macro's own balanced argument
        # list (in source order), not just the token immediately after the
        # opening delimiter -- see collect_string_literals. Read from
        # `clean` (strings intact), never `code_only` (which has blanked
        # exactly the string content being looked for here).
        close_pos = find_matching_close(code_only, call_start)
        parts = collect_string_literals(clean, call_start + 1, close_pos)
        if not parts:
            continue
        message = "".join(decode_rust_escapes(content, is_raw) for content, is_raw in parts)
        if not PLACEHOLDER_RE.search(message):
            continue
        if is_allowlisted(path, message, allowlist):
            continue
        line_no = clean.count("\n", 0, m.start()) + 1
        macro_name = m.group(1)
        safe_path = sanitize_for_ci(path)
        safe_message = sanitize_for_ci(message)
        findings.append(f"{safe_path}:{line_no}: {macro_name}!(\"{safe_message}\") reads as a placeholder stub, not a real error path")

if findings:
    for f in findings:
        print(f)
    print(f"\nstub-marker lint: {len(findings)} issue(s)")
    sys.exit(1)

print(f"stub-marker lint: {len(files)} file(s) OK")
PY
    rc=$?
    rm -f "$file_list"
    return "$rc"
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
