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
#
# Filesystem-indirection (symlink) policy -- every path this script opens or
# walks, and the symlink handling applied to each:
#
#   script self-location     | `cd $(dirname $0) && pwd`       | trusted; resolved once at startup
#   crates/ regular .rs      | `find -type f -name '*.rs'`     | only real files scanned; symlinks excluded by -type f
#   crates/ any symlink      | `find -type l`                  | HARD-FAIL, named via sanitize_for_ci, before scanning
#   discovered .rs reads     | `open(path)` in the scan loop   | guaranteed real by the -type f / -type l split; CI checkout is static (no find->read swap)
#   allowlist file           | `os.open(O_RDONLY|O_NOFOLLOW)`  | symlink refused atomically; in-repo default must also resolve under the scan root; env override is containment-exempt but still symlink-refused
#   temp transport files     | `mktemp`                        | mktemp regular files (O_EXCL); not attacker-controlled
#   self-test fixture trees  | self-authored under `mktemp -d` | test-authored real files
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    # case-fail and case-pass assert detection and false-positive avoidance,
    # not suppression; they scan against an empty allowlist so the committed
    # file's real-repo paths are never validated against these temp trees (the
    # path-existence guard would otherwise fail loud on them).
    empty_allowlist="$tmp/empty-allowlist.txt"
    : > "$empty_allowlist"

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

pub fn after_format_split_arg_stub(flag: bool) -> u32 {
    if !flag {
        panic!("not {}", "implemented for the format-split case");
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

pub fn split_marker_by_continuation_stub(flag: bool) -> u32 {
    if !flag {
        panic!("to\
                do: a marker split across a backslash-newline continuation must still be caught");
    }
    1
}

pub fn mixed_positional_named_stub(flag: bool) -> u32 {
    if !flag {
        // Adversarial: a placeholder split across a positional and a named
        // argument. Slot substitution fills the named slot from x and the auto
        // slot from the positional regardless of source order, so the
        // reconstructed message reads as a stub even though neither literal does
        // alone.
        panic!("{x} {}", "implemented for the mixed positional and named case", x = "not");
    }
    1
}

pub fn c1_csi_forgery_stub(flag: bool) -> u32 {
    if !flag {
        // Adversarial: a single-byte C1 CSI control decoded from a unicode
        // escape, which no ESC-prefixed pattern would catch. The sanitizer must
        // strip every C0 and C1 control byte from CI output.
        panic!("todo: c1 single-byte CSI \u{9b}31m::error::forged sanitization probe");
    }
    1
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

    # Exactly 19 markers are seeded in case-fail above; asserting the count
    # (not just substring presence) catches a parser-overmatch regression
    # that would otherwise slip through as an unnoticed extra finding.
    expected_marker_count=19

    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-fail" > "$tmp/fail.log" 2>&1; then
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
            "implemented for the format-split case" \
            "concat-assembled stub message" \
            "only catches via unicode escape decoding" \
            "only catches via hex escape decoding" \
            "still not implemented after a backslash-newline string continuation" \
            "a marker split across a backslash-newline continuation must still be caught" \
            "a placeholder inside a #[cfg(test)] item must now be caught" \
            "mid-message newline injection" \
            "carries a newline and a forged CI workflow command sequence" \
            "a placeholder inside a tests/ directory file must now be caught" \
            "implemented for the mixed positional and named case" \
            "sanitization probe"
        do
            if ! grep -qF "$marker" "$tmp/fail.log"; then
                echo "self-test FAILED: expected finding missing: $marker"
                cat "$tmp/fail.log"
                status=1
            fi
        done

        # the ci_log_forgery_stub and c1_csi_forgery_stub messages carry raw
        # control bytes (an ANSI ESC, a single-byte C1 CSI decoded from \u{9b}, an
        # embedded newline) plus a literal "::error::forged" workflow-command
        # shape. sanitize_for_ci must leave NO C0 or C1 control byte in CI output;
        # assert the whole log is free of them. A real newline separates finding
        # lines, so 0x0a alone is excluded from the check.
        if ! python3 - "$tmp/fail.log" <<'PYCTL'
import sys
data = open(sys.argv[1], "rb").read()
leaked = sorted({b for b in data if b < 0x0a or 0x0a < b < 0x20 or b == 0x7f or 0x80 <= b <= 0x9f})
if leaked:
    sys.stderr.write("leaked control bytes: " + " ".join("0x%02x" % b for b in leaked) + "\n")
    sys.exit(1)
PYCTL
        then
            echo "self-test FAILED: a raw C0/C1 control byte (e.g. the single-byte CSI 0x9b) leaked into CI output"
            cat -v "$tmp/fail.log"
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

    if ! STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: legitimate panic!/unreachable! messages (raw strings, lookalike text inside strings, a #[cfg(test)] helper named StubService) false-positived:"
        cat "$tmp/pass.log"
        status=1
    fi

    # The committed allowlist is validated by running the scanner over the real
    # tree with it: the unused-entry guard fails loud on any entry that suppresses
    # no current finding (stale, moved, or pre-planted), and the malformed-line
    # guard fails loud on a no-TAB entry. That is the scanner's own contract, so a
    # comment-only file (its state today) passes and a legitimate committed
    # suppression -- one that matches a current finding -- passes too, where a
    # blunt "no active line" assertion would wrongly reject it. Any offending
    # entry is named through the scanner's own sanitize_for_ci output, never
    # echoed raw from the file. The committed file carries NO self-test fixture
    # anchor: a committed entry is a real, PR-recreatable suppression, so a fixture
    # there would let a PR add that exact path+message and suppress its own stub.
    # Suppression is exercised via a temp copy below.
    #
    # This repeats the production scan's full-tree pass; removing that duplication
    # is the self-test/production structure work tracked in #1108.
    committed_allowlist="$SCRIPT_DIR/stub-marker-allowlist.txt"
    if ! STUB_MARKER_ALLOWLIST="$committed_allowlist" scan "$ROOT" > "$tmp/committed.log" 2>&1; then
        echo "self-test FAILED: the committed allowlist did not pass the scanner over the real tree -- a malformed, stale, or pre-planted allowlist entry, or an un-suppressed placeholder stub in the tree (named below):"
        cat "$tmp/committed.log"
        status=1
    fi

    # Suppression is exercised against a TEMP allowlist: a copy of the committed
    # file (so a committed parse/format break still surfaces) with a fixture
    # anchor appended. The anchor entry suppresses the matching finding, so it is
    # used and the unused-entry guard stays satisfied; a sibling with a different
    # message at the same path still surfaces, checking exact-match both ways.
    mkdir -p "$tmp/case-allowlist/crates/fixture-crate/src"
    cat > "$tmp/case-allowlist/crates/fixture-crate/src/lib.rs" <<'ALWFIXTURE'
pub fn allowlisted_stub() -> u32 {
    panic!("todo: this one is allowlisted and must be suppressed")
}

pub fn not_allowlisted_stub() -> u32 {
    panic!("todo: this one is NOT allowlisted and must still be caught")
}
ALWFIXTURE
    temp_allowlist="$tmp/case-allowlist-allowlist.txt"
    cp "$committed_allowlist" "$temp_allowlist"
    printf 'crates/fixture-crate/src/lib.rs\ttodo: this one is allowlisted and must be suppressed\n' \
        >> "$temp_allowlist"

    if STUB_MARKER_ALLOWLIST="$temp_allowlist" scan "$tmp/case-allowlist" > "$tmp/allowlist.log" 2>&1; then
        echo "self-test FAILED: the non-allowlisted sibling finding should have failed the scan"
        cat "$tmp/allowlist.log"
        status=1
    else
        if grep -qF 'this one is allowlisted and must be suppressed' "$tmp/allowlist.log"; then
            echo "self-test FAILED: an allowlisted finding (exact temp-allowlist match) was not suppressed"
            cat "$tmp/allowlist.log"
            status=1
        fi
        if ! grep -qF 'this one is NOT allowlisted' "$tmp/allowlist.log"; then
            echo "self-test FAILED: a sibling non-allowlisted finding was incorrectly suppressed"
            cat "$tmp/allowlist.log"
            status=1
        fi
    fi

    # The scan must FAIL LOUD on an allowlist entry that suppresses no finding in
    # the scanned tree -- stale or pre-planted, the shape that could otherwise sit
    # ready to suppress a future finding. A path absent from the tree is the
    # simplest such entry (it can match nothing); the offending entry is named.
    mkdir -p "$tmp/case-stale-allowlist/crates/real-crate/src"
    cat > "$tmp/case-stale-allowlist/crates/real-crate/src/lib.rs" <<'STALEFIXTURE'
pub fn ok() -> u32 {
    1
}
STALEFIXTURE
    stale_allowlist="$tmp/stale-allowlist.txt"
    printf 'crates/does-not-exist/src/lib.rs\ttodo: entry for a path not in the tree\n' \
        > "$stale_allowlist"
    if STUB_MARKER_ALLOWLIST="$stale_allowlist" scan "$tmp/case-stale-allowlist" > "$tmp/stale.log" 2>&1; then
        echo "self-test FAILED: a scan with an allowlist entry for a nonexistent path should have failed loud"
        cat "$tmp/stale.log"
        status=1
    else
        if ! grep -qF 'crates/does-not-exist/src/lib.rs' "$tmp/stale.log"; then
            echo "self-test FAILED: the nonexistent-allowlist-path error did not name the offending entry"
            cat "$tmp/stale.log"
            status=1
        fi
    fi

    # An allowlist entry whose path DOES exist but whose message matches no
    # finding is equally stale: the unused-entry guard covers it, not just the
    # path-absent case above.
    mkdir -p "$tmp/case-unused-allowlist/crates/real-crate/src"
    cat > "$tmp/case-unused-allowlist/crates/real-crate/src/lib.rs" <<'UNUSEDFIXTURE'
pub fn ok() -> u32 {
    1
}
UNUSEDFIXTURE
    unused_allowlist_file="$tmp/unused-allowlist.txt"
    printf 'crates/real-crate/src/lib.rs\ttodo: no finding in this file matches this message\n' \
        > "$unused_allowlist_file"
    if STUB_MARKER_ALLOWLIST="$unused_allowlist_file" scan "$tmp/case-unused-allowlist" > "$tmp/unused.log" 2>&1; then
        echo "self-test FAILED: an allowlist entry with a valid path but no matching finding should have failed loud"
        cat "$tmp/unused.log"
        status=1
    else
        if ! grep -qF 'crates/real-crate/src/lib.rs' "$tmp/unused.log"; then
            echo "self-test FAILED: the unused-allowlist-entry error did not name the offending entry"
            cat "$tmp/unused.log"
            status=1
        fi
    fi

    # A non-comment allowlist line with no TAB is malformed and must fail loud,
    # naming the line -- never silently dropped (a mis-typed suppression that
    # silently does nothing is worse than a loud rejection).
    mkdir -p "$tmp/case-malformed-allowlist/crates/real-crate/src"
    cat > "$tmp/case-malformed-allowlist/crates/real-crate/src/lib.rs" <<'MALFIXTURE'
pub fn ok() -> u32 {
    1
}
MALFIXTURE
    malformed_allowlist_file="$tmp/malformed-allowlist.txt"
    printf 'crates/real-crate/src/lib.rs no tab between path and message\n' \
        > "$malformed_allowlist_file"
    if STUB_MARKER_ALLOWLIST="$malformed_allowlist_file" scan "$tmp/case-malformed-allowlist" > "$tmp/malformed.log" 2>&1; then
        echo "self-test FAILED: a malformed (no-TAB) allowlist line should have failed the scan loud"
        cat "$tmp/malformed.log"
        status=1
    else
        if ! grep -qF 'malformed' "$tmp/malformed.log"; then
            echo "self-test FAILED: the malformed-allowlist-line error was not reported as malformed"
            cat "$tmp/malformed.log"
            status=1
        fi
    fi

    # Filesystem-indirection: a symlinked allowlist must be refused (never
    # followed), and the target's content must NOT be echoed into output. The
    # target holds a sentinel that would only appear if the scanner followed the
    # link and read it.
    mkdir -p "$tmp/case-symlink-allowlist/crates/real-crate/src"
    cat > "$tmp/case-symlink-allowlist/crates/real-crate/src/lib.rs" <<'SLFIXTURE'
pub fn ok() -> u32 {
    1
}
SLFIXTURE
    printf 'SENTINEL_ALLOWLIST_SECRET_MUST_NOT_LEAK\tx\n' > "$tmp/symlink-allowlist-target.txt"
    symlink_allowlist="$tmp/symlink-allowlist.txt"
    ln -s "$tmp/symlink-allowlist-target.txt" "$symlink_allowlist"
    if STUB_MARKER_ALLOWLIST="$symlink_allowlist" scan "$tmp/case-symlink-allowlist" > "$tmp/symlink-allow.log" 2>&1; then
        echo "self-test FAILED: a symlinked allowlist file should have failed the scan loud"
        cat "$tmp/symlink-allow.log"
        status=1
    else
        if ! grep -qF 'symlink' "$tmp/symlink-allow.log"; then
            echo "self-test FAILED: the symlinked-allowlist error did not report a symlink"
            cat "$tmp/symlink-allow.log"
            status=1
        fi
        if grep -qF 'SENTINEL_ALLOWLIST_SECRET_MUST_NOT_LEAK' "$tmp/symlink-allow.log"; then
            echo "self-test FAILED: the symlinked allowlist's TARGET content leaked into CI output"
            cat "$tmp/symlink-allow.log"
            status=1
        fi
    fi

    # Filesystem-indirection: a symlinked .rs under crates/ must hard-fail the
    # scan (Cargo compiles it; -type f discovery would skip it). A real sibling
    # .rs keeps the discovery list non-empty so the failure is the symlink, not
    # an empty tree.
    mkdir -p "$tmp/case-symlink-rs/crates/real-crate/src"
    cat > "$tmp/case-symlink-rs/crates/real-crate/src/real.rs" <<'RSREAL'
pub fn ok() -> u32 {
    1
}
RSREAL
    cat > "$tmp/case-symlink-rs/elsewhere.rs" <<'RSHIDDEN'
pub fn hidden_stub() -> u32 {
    panic!("todo: this stub rode in behind a symlinked .rs")
}
RSHIDDEN
    ln -s "$tmp/case-symlink-rs/elsewhere.rs" "$tmp/case-symlink-rs/crates/real-crate/src/linked.rs"
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-symlink-rs" > "$tmp/symlink-rs.log" 2>&1; then
        echo "self-test FAILED: a symlinked .rs under crates/ should have hard-failed the scan"
        cat "$tmp/symlink-rs.log"
        status=1
    else
        if ! grep -qF 'linked.rs' "$tmp/symlink-rs.log"; then
            echo "self-test FAILED: the symlinked-.rs error did not name the offending link"
            cat "$tmp/symlink-rs.log"
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

    # Filesystem-indirection guard: collect EVERY symlink under crates/ (any
    # name, file or directory), excluding the same pruned build-artifact dirs.
    # A symlinked .rs is skipped by the -type f pass above but compiled by
    # Cargo; a symlinked directory is not descended by find (no -L) yet Cargo
    # builds through it. The python below hard-fails on any of them.
    symlink_list="$(mktemp)"
    find "$root/crates" \
        \( -name 'target' -o -name '.cargo-target' \) -type d -prune \
        -o -type l -print0 \
        > "$symlink_list"

    if [ ! -s "$file_list" ] && [ ! -s "$symlink_list" ]; then
        rm -f "$file_list" "$symlink_list"
        echo "no .rs files matched under $root/crates (excluding target/.cargo-target build-artifact dirs) -- the scanner would silently be a no-op; fix the file-layout selection" >&2
        return 1
    fi

    allowlist="${STUB_MARKER_ALLOWLIST:-$SCRIPT_DIR/stub-marker-allowlist.txt}"
    # Whether the allowlist path came from the env override (a deliberate
    # operator choice, containment-exempt) or the in-repo default (must resolve
    # under the scan root). Symlink refusal applies to BOTH; see load_allowlist.
    if [ -n "${STUB_MARKER_ALLOWLIST:-}" ]; then
        allowlist_is_override=1
    else
        allowlist_is_override=0
    fi

    # `|| rc=$?` keeps set -e from exiting the script the moment python3 returns
    # non-zero (findings present, or the allowlist-path guard failing): the temp
    # file_list below must still be removed on that path, and the caller needs
    # the real exit code, not a set-e-induced abort.
    rc=0
    python3 - "$file_list" "$allowlist" "$root" "$symlink_list" "$allowlist_is_override" <<'PY' || rc=$?
import errno
import os
import re
import stat
import sys

with open(sys.argv[1], "rb") as fh:
    files = sorted(os.fsdecode(f) for f in fh.read().split(b"\0") if f)

ALLOWLIST_PATH = sys.argv[2]
SCAN_ROOT = sys.argv[3]
SYMLINK_LIST_PATH = sys.argv[4]
ALLOWLIST_IS_OVERRIDE = sys.argv[5] == "1"

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
CONTROL_CHAR_RE = re.compile(r"[\x00-\x1f\x7f-\x9f]")
CLOSERS = {"(": ")", "{": "}", "[": "]"}
ESCAPE_RE = re.compile(r"\\(n|r|t|\\|\"|'|0|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\})")
# Rust string-continuation escape: a `\` immediately followed by a newline
# removes the `\`, the newline, and the following whitespace (Rust reference,
# "String continuation escapes"). A placeholder marker split across such a
# continuation (`"to\<newline>    do"` == "todo") would otherwise evade
# PLACEHOLDER_RE, so it is stripped before any other escape is decoded.
STRING_CONTINUATION_RE = re.compile(r"\\\r?\n\s*")
SIMPLE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0"}
# A placeholder phrase can be split across a format template and its arguments:
# `panic!("not {}", "implemented")` renders "not implemented" at runtime, and a
# naive scan of the template alone ("not {}") misses it. The message is instead
# reconstructed by substituting each literal argument into its own format slot
# (resolve_format_message), so out-of-source-order named args and mixed
# positional/named splits cannot hide a placeholder. FORMAT_SLOT_RE matches one
# `{...}` slot (or an escaped `{{`/`}}`); IDENT_ARG_RE matches a `name =` named
# argument prefix (a single `=`, never `==`).
FORMAT_SLOT_RE = re.compile(r"\{\{|\}\}|\{([^{}]*)\}")
IDENT_ARG_RE = re.compile(r"\s*([A-Za-z_][A-Za-z0-9_]*)\s*=(?!=)")


def sanitize_for_ci(raw):
    """The single sanitizer every output path routes through. Both the
    panic!/unreachable! message and the repo path this scanner echoes into CI
    stdout/stderr come straight from PR-controlled text (a string literal, a
    filename). GitHub Actions parses a `::name ...::value`-shaped line as a
    workflow command (`::error::`, `::add-mask::`, `::set-output::`, ...), so
    an embedded newline could let attacker text start a fresh line and forge
    one; an ANSI/CSI escape can rewrite terminal and log-viewer state; and a
    single-byte C1 CSI (U+009B) introduces a control sequence no ESC-prefixed
    pattern would catch. Collapse every newline to a literal `\\n` (never a
    real break), escape every remaining C0 control, DEL, and C1 control byte
    -- ESC, the single-byte CSI, and the rest -- to a visible inert `\\xNN`
    token, and break every `::` so no substring parses as workflow-command
    syntax, all while staying readable for a human operator."""
    s = raw.replace("\r\n", "\\n").replace("\n", "\\n").replace("\r", "\\n")
    s = CONTROL_CHAR_RE.sub(lambda m: "\\x%02x" % ord(m.group()), s)
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
    still caught. A backslash-newline string continuation is collapsed first
    (STRING_CONTINUATION_RE) so a placeholder split across a line
    continuation is rejoined before matching. Raw literals (`r"..."`,
    `r#"..."#`) have no escapes -- returned unchanged. Any other backslash
    sequence is left as-is."""
    if is_raw:
        return body

    body = STRING_CONTINUATION_RE.sub("", body)

    def repl(m):
        tok = m.group(1)
        if tok in SIMPLE_ESCAPES:
            return SIMPLE_ESCAPES[tok]
        if tok[0] == "x":
            return chr(int(tok[1:], 16))
        return chr(int(tok[2:-1], 16))  # u{...}

    return ESCAPE_RE.sub(repl, body)


def split_macro_args(code_only, clean, open_pos):
    """Scan a macro's balanced argument list in a single pass. code_only[open_pos]
    is the opening delimiter (`(`/`{`/`[`); return (arg_spans, close_pos), where
    arg_spans are (start, end) offsets into `clean` for each top-level,
    comma-separated argument (empty spans dropped) and close_pos is the offset of
    the matching closing delimiter (len(code_only) if unterminated). Folding
    bracket matching into the split avoids a separate find_matching_close pass
    over the same region. Commas nested inside (), {}, [] never split; string and
    comment content is already blanked in code_only, so a comma or bracket living
    inside a literal never desyncs the depth count."""
    n = len(code_only)
    depth = 1
    i = open_pos + 1
    arg_start = i
    spans = []
    while i < n and depth > 0:
        c = code_only[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                break
        elif c == "," and depth == 1:
            spans.append((arg_start, i))
            arg_start = i + 1
        i += 1
    spans.append((arg_start, i))
    close_pos = i if i < n else n
    return [(s, e) for (s, e) in spans if clean[s:e].strip()], close_pos


def arg_literal_value(clean, code_only, start, end):
    """The decoded string value of the argument clean[start:end] when it is a
    string literal (plain, raw, or byte-raw) or a concat!(...) of string
    literals; otherwise None. A non-literal argument -- a variable, a function
    call, a format! result assembled elsewhere -- is invisible to a static text
    scan and returns None. Leading whitespace is skipped; a concat! call's own
    literal arguments are joined."""
    i = start
    while i < end and clean[i].isspace():
        i += 1
    if i >= end:
        return None
    m = re.match(r"concat\s*!\s*[([{]", code_only[i:end])
    if m is not None:
        open_pos = i + m.end() - 1
        close_pos = min(find_matching_close(code_only, open_pos), end)
        parts = collect_string_literals(clean, open_pos + 1, close_pos)
        if not parts:
            return None
        return "".join(decode_rust_escapes(content, is_raw) for content, is_raw in parts)
    lit = match_string_literal(clean, i)
    if lit is None:
        return None
    return decode_rust_escapes(lit[1], lit[2])


def resolve_format_message(template, positionals, named):
    """Approximate the runtime message by substituting the macro's literal
    arguments into the format template's slots. `{{`/`}}` are literal braces; an
    auto `{}` takes the next positional in order, `{n}` takes positional n, and
    `{name}` takes a named argument. A slot backed by a literal argument is
    filled with that literal's decoded text; a slot backed by a non-literal or
    unresolved argument -- including a Rust 2021 inline-captured variable, which
    is not among the macro's explicit args -- is vacated. That is the
    conservative posture: it never fabricates text a static scan cannot prove,
    while denying a literal argument any way to hide a placeholder by sitting out
    of source order or behind a positional/named split."""
    auto = [0]

    def repl(m):
        tok = m.group(0)
        if tok == "{{":
            return "{"
        if tok == "}}":
            return "}"
        ref = m.group(1).split(":", 1)[0].strip()
        if ref == "":
            idx = auto[0]
            auto[0] += 1
            value = positionals[idx] if idx < len(positionals) else None
        elif ref.isdigit():
            idx = int(ref)
            value = positionals[idx] if idx < len(positionals) else None
        else:
            value = named.get(ref)
        return value if value is not None else ""

    return FORMAT_SLOT_RE.sub(repl, template)


def load_allowlist(path, scan_root, is_override):
    """Parse `<repo-relative-path>\\t<exact-decoded-message>` entries from the
    allowlist file at `path`, returning (entries, malformed). Each entry
    suppresses a finding only when both fields match exactly (see
    allowlist_match_index) -- the repo-relative path (as rendered under
    `crates/...`) and the fully decoded panic!/unreachable! message. Blank lines
    and lines starting with `#` (after stripping leading whitespace) are
    ignored. A non-comment line with no TAB is malformed -- collected in
    `malformed` with its 1-based line number so the caller can fail loud rather
    than silently drop a mis-typed entry that would suppress nothing. A missing
    file is an empty allowlist, not an error.

    Filesystem-indirection policy: the allowlist is opened with O_NOFOLLOW, so a
    symlinked allowlist file is refused atomically (no check-then-open gap) --
    a symlink could point at a sensitive out-of-tree file whose content the
    malformed-line reporter would otherwise echo into CI. For the in-repo
    DEFAULT allowlist (no env override) the resolved path must additionally
    stay under the scan root; an env override is a deliberate operator choice
    and is containment-exempt, but is still symlink-refused."""
    entries = []
    malformed = []
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError as exc:
        if exc.errno == errno.ENOENT:
            return entries, malformed
        if exc.errno in (errno.ELOOP, errno.EMLINK):
            sys.stderr.write(
                "stub-marker allowlist path is a symlink; refusing to follow it "
                "(a symlinked allowlist could disclose an out-of-tree file's "
                f"content into CI): {sanitize_for_ci(path)}\n"
            )
            sys.exit(1)
        raise
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        return entries, malformed
    if not is_override:
        real = os.path.realpath(path)
        root_real = os.path.realpath(scan_root)
        if real != root_real and not real.startswith(root_real + os.sep):
            os.close(fd)
            sys.stderr.write(
                "stub-marker default allowlist resolves outside the scan root; "
                f"refusing: {sanitize_for_ci(path)}\n"
            )
            sys.exit(1)
    with os.fdopen(fd, "r", encoding="utf-8") as fh:
        for lineno, raw_line in enumerate(fh, 1):
            line = raw_line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            if "\t" not in line:
                malformed.append((lineno, line))
                continue
            entry_path, entry_message = line.split("\t", 1)
            entries.append((entry_path, entry_message))
    return entries, malformed


def allowlist_match_index(rel_path, message, entries):
    """The index of the allowlist entry that matches this finding EXACTLY --
    the entry's repo-relative path equals the finding's path AND the entry's
    message equals the finding's fully decoded panic!/unreachable! message -- or
    None. Exact (not substring) so an entry can never suppress an unrelated NEW
    marker that merely shares a path prefix or a message fragment. Returning the
    index (not a bool) lets the caller record which entries actually suppressed
    a finding, so an entry that matches nothing this run can be rejected as
    stale."""
    for idx, (entry_path, entry_message) in enumerate(entries):
        if rel_path == entry_path and message == entry_message:
            return idx
    return None


# Filesystem-indirection class: the scan() find already collected EVERY symlink
# under crates/ (any name, file or directory), excluding pruned build-artifact
# dirs, into SYMLINK_LIST_PATH. A symlinked .rs rides past the -type f discovery
# while Cargo compiles it; a symlinked directory is not descended by find (no -L)
# yet Cargo builds through it. Either way a placeholder stub could ride past this
# guard, so refuse to scan through any of them -- named via sanitize_for_ci --
# before doing anything else.
with open(SYMLINK_LIST_PATH, "rb") as fh:
    symlinked = sorted(os.fsdecode(f) for f in fh.read().split(b"\0") if f)
if symlinked:
    sys.stderr.write(
        "stub-marker scan found symlink(s) under crates/ -- a symlinked source "
        "file is compiled by Cargo but skipped by -type f discovery, and a "
        "symlinked directory hides its source from the scan while Cargo still "
        "builds it; either way a placeholder stub could ride past this guard. "
        "Refusing to scan through them; replace each with a real file or dir:\n"
    )
    for entry in symlinked:
        rel = os.path.relpath(entry, SCAN_ROOT).replace(os.sep, "/")
        sys.stderr.write(f"  {sanitize_for_ci(rel)}\n")
    sys.exit(1)

allowlist, malformed_allowlist = load_allowlist(
    ALLOWLIST_PATH, SCAN_ROOT, ALLOWLIST_IS_OVERRIDE
)

# A non-comment allowlist line with no TAB is a mis-typed entry: it parses as
# neither a path nor a message and would silently suppress nothing. Fail loud
# and name it (sanitized) rather than dropping it.
if malformed_allowlist:
    sys.stderr.write(
        "stub-marker allowlist has malformed line(s) -- a non-comment entry needs "
        "a TAB between the repo-relative path and the message:\n"
    )
    for lineno, line in malformed_allowlist:
        sys.stderr.write(f"  line {lineno}: {sanitize_for_ci(line)}\n")
    sys.exit(1)

allowlist_used = [False] * len(allowlist)

findings = []
for path in files:
    rel_path = os.path.relpath(path, SCAN_ROOT).replace(os.sep, "/")
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
    clean = strip_comments_and_char_lits(text)
    code_only = blank_strings(clean)

    for m in MACRO_CALL_RE.finditer(code_only):
        call_start = m.end() - 1  # the opening delimiter char (`(`/`{`/`[`)
        # The first argument is the format template; every later argument is a
        # positional value or a `name = value` named value. Each literal
        # argument is substituted into its own slot (resolve_format_message), so
        # a placeholder split across the template and its args -- in any order --
        # is reconstructed rather than missed. Literal content is read from
        # `clean` (strings intact), never `code_only` (which blanked it).
        arg_spans, close_pos = split_macro_args(code_only, clean, call_start)
        if not arg_spans:
            continue
        template = arg_literal_value(clean, code_only, arg_spans[0][0], arg_spans[0][1])
        if template is None:
            # No static format template (the first argument is itself a variable
            # or a runtime expression). Fall back to the bag of any string
            # literals anywhere in the argument list so a lone literal message
            # still surfaces.
            parts = collect_string_literals(clean, call_start + 1, close_pos)
            if not parts:
                continue
            message = "".join(decode_rust_escapes(content, is_raw) for content, is_raw in parts)
        else:
            positionals = []
            named = {}
            for span_start, span_end in arg_spans[1:]:
                nm = IDENT_ARG_RE.match(code_only, span_start, span_end)
                if nm is not None:
                    named[nm.group(1)] = arg_literal_value(clean, code_only, nm.end(), span_end)
                else:
                    positionals.append(arg_literal_value(clean, code_only, span_start, span_end))
            message = resolve_format_message(template, positionals, named)
        if not PLACEHOLDER_RE.search(message):
            continue
        idx = allowlist_match_index(rel_path, message, allowlist)
        if idx is not None:
            allowlist_used[idx] = True
            continue
        line_no = clean.count("\n", 0, m.start()) + 1
        macro_name = m.group(1)
        findings.append(
            f'{sanitize_for_ci(rel_path)}:{line_no}: {macro_name}!("{sanitize_for_ci(message)}") '
            "reads as a placeholder stub, not a real error path"
        )

failed = False

# Every allowlist entry must suppress a CURRENT finding. An entry that matched
# nothing this run -- its path is gone, its file no longer holds that marker, or
# it was pre-planted ahead of the finding it would suppress -- is stale and must
# not sit here silently disabling a future finding. This subsumes a bare
# path-existence check: an entry whose path is absent trivially matches nothing.
unused_allowlist = [allowlist[i] for i, used in enumerate(allowlist_used) if not used]
if unused_allowlist:
    sys.stderr.write(
        "stub-marker allowlist has entr(y/ies) that suppress no current finding "
        "-- stale or pre-planted; remove or correct:\n"
    )
    for entry_path, entry_message in unused_allowlist:
        sys.stderr.write(f"  {sanitize_for_ci(entry_path)}\t{sanitize_for_ci(entry_message)}\n")
    failed = True

if findings:
    for f in findings:
        print(f)
    print(f"\nstub-marker lint: {len(findings)} issue(s)")
    failed = True

if failed:
    sys.exit(1)

print(f"stub-marker lint: {len(files)} file(s) OK")
PY
    rm -f "$file_list" "$symlink_list"
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
