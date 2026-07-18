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
# examples alike. Production discovery is git-tracked-tree based (`git
# ls-tree -r -z HEAD -- crates/`): the COMMITTED TREE, not the mutable
# index. `scripts/ci.sh` runs clippy/build (executing PR-controlled
# build.rs / proc-macros) before this guard runs; a build step can `git rm
# --cached` a stub-bearing path to mutate the index while leaving the
# working-tree file in place, and index-based discovery (`git ls-files`)
# would then omit it. The HEAD tree object cannot be mutated by anything
# running after checkout, so this scan always reflects what was actually
# committed and compiled. Untracked build-artifact output (target/,
# .cargo-target/, anything gitignored) is never listed by git, so no
# name-based directory pruning is needed and, unlike a name-pruning `find`,
# a TRACKED Rust module nested under a dir literally named `target` (e.g.
# reachable via #[path]) is not invisible to the scan. There is no
# reachability/cfg analysis: a placeholder message is a placeholder message
# whether or not the code compiling it is test-gated. The small number of
# legitimate matches (a test mock whose type/method name happens to contain
# a placeholder word) are suppressed via the explicit, in-diff reviewed
# allowlist at stub-marker-allowlist.txt -- see that file's header for the
# format.
#
# SCOPE (documented non-goal): this is a placeholder-LANGUAGE net over the
# literal spellings `panic!`/`unreachable!` only. It does not resolve macro
# aliases, re-exports, or wrapper macros (e.g. a local `crash!` expanding to
# panic!) -- that would require macro-expansion-aware parsing this scanner
# does not attempt. Alias coverage is left to clippy and human review; this
# is a known non-goal, not a silent bypass of a stated promise.
#
# Filesystem-indirection (symlink) policy -- every path this script opens or
# walks, and the symlink handling applied to each:
#
#   script self-location     | `cd $(dirname $0) && pwd`         | trusted; resolved once at startup
#   crates/ regular .rs      | `git ls-tree -r HEAD` mode 100644/100755, self-test fallback `find -type f` | only tracked-regular/self-test-real files scanned; committed-tree, not index, state
#   crates/ any symlink      | `git ls-tree -r HEAD` mode 120000, self-test fallback `find -type l` | HARD-FAIL, named via sanitize_for_ci, before scanning
#   discovered .rs reads     | per-component `os.open(O_RDONLY|O_NOFOLLOW, dir_fd=...)` walk from a root fd, in the scan loop | proves every path component from the scan root down to the file -- not just the final one -- is not a symlink at read time; a post-discovery swap of the file OR any parent directory (a build step/proc-macro racing the guard, or a working-tree entry diverging from its clean git tree entry) raises ELOOP and hard-fails that path by name, rather than silently following or reading a replacement
#   allowlist file            | `os.open(O_RDONLY|O_NOFOLLOW)`    | symlink refused atomically; in-repo default must also resolve under the scan root; env override is containment-exempt but still symlink-refused
#   temp transport files      | `mktemp`                          | mktemp regular files (O_EXCL); not attacker-controlled
#   self-test fixture trees   | self-authored under `mktemp -d`   | test-authored real files (find-mode discovery; not git-tracked)
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

pub fn after_nested_format_args_stub(flag: bool) -> u32 {
    if !flag {
        // Adversarial: a placeholder assembled through a nested format_args!
        // call whose own arguments are all literals -- fully statically
        // derivable, so it must be reconstructed and caught, not treated as
        // an opaque non-literal argument.
        panic!("{}", format_args!("not {}", "implemented via nested format_args"));
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

pub fn named_variable_slot_dispatch(stage: &str) -> u32 {
    // Adversarial: a non-literal (runtime variable) argument filling a
    // format slot must never be silently vacated to an empty string -- that
    // would let "not {feature} implemented" collapse into a false-positive
    // "not  implemented". The slot resolves to UNRESOLVED_SLOT_SENTINEL
    // instead, which breaks the marker regex's `\s*` span.
    panic!("not {feature} implemented", feature = stage)
}

pub fn alias_macro_not_matched(flag: bool) -> u32 {
    if !flag {
        // Adversarial: macro-alias resolution is a documented non-goal (see
        // MACRO_CALL_RE's scope comment) -- only literal panic!/unreachable!
        // spellings are matched, so an alias must not be flagged here.
        crash!("todo: alias bypass is a documented non-goal, not a silent guard failure");
    }
    1
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

    # Exactly 20 markers are seeded in case-fail above; asserting the count
    # (not just substring presence) catches a parser-overmatch regression
    # that would otherwise slip through as an unnoticed extra finding.
    expected_marker_count=20

    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-fail" find > "$tmp/fail.log" 2>&1; then
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
            "sanitization probe" \
            "implemented via nested format_args"
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

    if ! STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-pass" find > "$tmp/pass.log" 2>&1; then
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
    if ! STUB_MARKER_ALLOWLIST="$committed_allowlist" scan "$ROOT" git > "$tmp/committed.log" 2>&1; then
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

    if STUB_MARKER_ALLOWLIST="$temp_allowlist" scan "$tmp/case-allowlist" find > "$tmp/allowlist.log" 2>&1; then
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
    if STUB_MARKER_ALLOWLIST="$stale_allowlist" scan "$tmp/case-stale-allowlist" find > "$tmp/stale.log" 2>&1; then
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
    if STUB_MARKER_ALLOWLIST="$unused_allowlist_file" scan "$tmp/case-unused-allowlist" find > "$tmp/unused.log" 2>&1; then
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
    if STUB_MARKER_ALLOWLIST="$malformed_allowlist_file" scan "$tmp/case-malformed-allowlist" find > "$tmp/malformed.log" 2>&1; then
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
    if STUB_MARKER_ALLOWLIST="$symlink_allowlist" scan "$tmp/case-symlink-allowlist" find > "$tmp/symlink-allow.log" 2>&1; then
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
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-symlink-rs" find > "$tmp/symlink-rs.log" 2>&1; then
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

    # git-mode TOCTOU: a git index entry's mode reflects what was STAGED, not
    # necessarily the current on-disk file. A build step that swaps a tracked
    # regular file for a symlink AFTER checkout (without re-staging) has git
    # ls-files -s still report 100644 -- so it lands in file_list, not
    # symlink_list, and the O_NOFOLLOW read guard is the ONLY thing that
    # catches the swap.
    mkdir -p "$tmp/case-git-toctou/crates/fixture-crate/src"
    (
        cd "$tmp/case-git-toctou" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && printf 'pub fn ok() -> u32 {\n    1\n}\n' > crates/fixture-crate/src/lib.rs \
            && git add -A && git commit -q -m seed
    )
    rm "$tmp/case-git-toctou/crates/fixture-crate/src/lib.rs"
    ln -s "/nonexistent-toctou-target" "$tmp/case-git-toctou/crates/fixture-crate/src/lib.rs"
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-git-toctou" git > "$tmp/git-toctou.log" 2>&1; then
        echo "self-test FAILED: a git-tracked file swapped for a symlink after commit (index still 100644) should have hard-failed the scan"
        cat "$tmp/git-toctou.log"
        status=1
    else
        if ! grep -qF 'crates/fixture-crate/src/lib.rs' "$tmp/git-toctou.log"; then
            echo "self-test FAILED: the post-commit symlink-swap error did not name the offending path"
            cat "$tmp/git-toctou.log"
            status=1
        fi
    fi

    # git-mode discovery: a tracked Rust module nested under a directory
    # literally named `target` (reachable via #[path], compilable) must now
    # be scanned and flagged -- name-based `find -prune` previously made it
    # invisible; git-tracked discovery has no such name-based exclusion.
    mkdir -p "$tmp/case-git-target/crates/fixture-crate/target"
    cat > "$tmp/case-git-target/crates/fixture-crate/target/tracked_via_path_attr.rs" <<'GITTARGETFIXTURE'
pub fn tracked_under_target_dir_stub() -> u32 {
    panic!("todo: a tracked module nested under a target/-named dir must now be caught")
}
GITTARGETFIXTURE
    (
        cd "$tmp/case-git-target" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && git add -A && git commit -q -m seed
    )
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-git-target" git > "$tmp/git-target.log" 2>&1; then
        echo "self-test FAILED: a tracked Rust module under a target/-named directory should have been scanned and flagged"
        cat "$tmp/git-target.log"
        status=1
    else
        if ! grep -qF 'tracked module nested under a target' "$tmp/git-target.log"; then
            echo "self-test FAILED: the tracked-under-target/ finding was not reported"
            cat "$tmp/git-target.log"
            status=1
        fi
    fi

    # Parent-directory symlink swap: git-tree discovery only reads the
    # COMMITTED tree, so a file committed under a real `src/` directory is
    # still discovered as a plain 100644 entry even after the working-tree
    # `src` directory is swapped to a symlink post-commit -- the same
    # decoupling the git-TOCTOU fixture above exercises for the file itself.
    # The per-component openat walk must refuse the symlinked PARENT
    # directory (ELOOP) before ever reaching the file, naming the full
    # discovered path.
    mkdir -p "$tmp/case-parent-symlink/crates/fixture-crate/src"
    cat > "$tmp/case-parent-symlink/crates/fixture-crate/src/f.rs" <<'PARENTFIXTURE'
pub fn parent_symlink_stub() -> u32 {
    panic!("todo: this file rode in behind a parent directory swapped to a symlink after discovery")
}
PARENTFIXTURE
    (
        cd "$tmp/case-parent-symlink" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && git add -A && git commit -q -m seed
    )
    rm -rf "$tmp/case-parent-symlink/crates/fixture-crate/src"
    mkdir -p "$tmp/parent-symlink-target"
    ln -s "$tmp/parent-symlink-target" "$tmp/case-parent-symlink/crates/fixture-crate/src"
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-parent-symlink" git > "$tmp/parent-symlink.log" 2>&1; then
        echo "self-test FAILED: a parent directory swapped to a symlink after discovery should have hard-failed the scan"
        cat "$tmp/parent-symlink.log"
        status=1
    else
        if ! grep -qF 'crates/fixture-crate/src/f.rs' "$tmp/parent-symlink.log"; then
            echo "self-test FAILED: the parent-directory-symlink error did not name the offending path"
            cat "$tmp/parent-symlink.log"
            status=1
        fi
    fi

    # Index-mutation immunity: `git rm --cached` unstages a committed file
    # (removing it from the index) while leaving the working-tree copy in
    # place and HEAD unchanged. `git ls-tree HEAD` reads the commit's tree
    # object, not the index, so it still lists the file and the stub is
    # still scanned and flagged -- the exact gap index-based `git ls-files`
    # discovery had.
    mkdir -p "$tmp/case-index-mutation/crates/fixture-crate/src"
    cat > "$tmp/case-index-mutation/crates/fixture-crate/src/lib.rs" <<'IDXFIXTURE'
pub fn index_mutation_stub() -> u32 {
    panic!("todo: this stub must still be caught after git rm --cached unstages it")
}
IDXFIXTURE
    (
        cd "$tmp/case-index-mutation" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && git add -A && git commit -q -m seed \
            && git rm --cached -q crates/fixture-crate/src/lib.rs
    )
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-index-mutation" git > "$tmp/index-mutation.log" 2>&1; then
        echo "self-test FAILED: a stub still present in the committed HEAD tree (after git rm --cached unstaged it from the index) should have been caught, not skipped"
        cat "$tmp/index-mutation.log"
        status=1
    else
        if ! grep -qF 'this stub must still be caught after git rm --cached unstages it' "$tmp/index-mutation.log"; then
            echo "self-test FAILED: the index-mutation-immunity finding was not reported"
            cat "$tmp/index-mutation.log"
            status=1
        fi
    fi

    # Producer-failure fail-closed: corrupt the committed tree object itself
    # (delete its loose object file) so `git ls-tree -r HEAD` fails even
    # though `git rev-parse --verify HEAD` succeeds (the commit object is
    # still intact). The scan must fail loud and non-zero, never silently
    # scan an empty or partial listing as if it were a clean pass.
    mkdir -p "$tmp/case-ls-tree-fail/crates/fixture-crate/src"
    cat > "$tmp/case-ls-tree-fail/crates/fixture-crate/src/lib.rs" <<'LSTREEFAILFIXTURE'
pub fn ok() -> u32 {
    1
}
LSTREEFAILFIXTURE
    (
        cd "$tmp/case-ls-tree-fail" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && git add -A && git commit -q -m seed
    )
    tree_sha="$(cd "$tmp/case-ls-tree-fail" && git rev-parse 'HEAD^{tree}')"
    obj_dir="${tree_sha%"${tree_sha#??}"}"
    obj_file="${tree_sha#??}"
    rm -f "$tmp/case-ls-tree-fail/.git/objects/$obj_dir/$obj_file"
    if STUB_MARKER_ALLOWLIST="$empty_allowlist" scan "$tmp/case-ls-tree-fail" git > "$tmp/ls-tree-fail.log" 2>&1; then
        echo "self-test FAILED: a git ls-tree HEAD failure (corrupted tree object) should have hard-failed the scan, not passed"
        cat "$tmp/ls-tree-fail.log"
        status=1
    else
        if ! grep -qF 'ls-tree' "$tmp/ls-tree-fail.log"; then
            echo "self-test FAILED: the git-producer-failure error did not name the failing command"
            cat "$tmp/ls-tree-fail.log"
            status=1
        fi
    fi

    # Security-ordering regression (#560 follow-up): the placeholder scan phase
    # must run before any cargo phase in ci.sh, so no PR-controlled build step or
    # proc-macro runs before the guard reads the tree. A guard running after cargo
    # could read a committed stub source, the committed allowlist, or this script
    # itself after a build step replaced it with benign content. Extract run_all's
    # phase order and assert no-stubs-scan is first; a reorder that moves it after
    # a cargo phase reopens that mutable-checkout bypass and fails here.
    ci_sh="$SCRIPT_DIR/ci.sh"
    if [ -f "$ci_sh" ]; then
        first_phase="$(awk '/^run_all\(\)/{f=1} f&&/for phase in/{g=1;next} g&&/do$/{exit} g{gsub(/[^a-z-]/,"");if($0!="")print}' "$ci_sh" | head -1)"
        if [ "$first_phase" != "no-stubs-scan" ]; then
            echo "self-test FAILED: ci.sh run_all must run 'no-stubs-scan' first (before any cargo phase); found '$first_phase'. Moving the placeholder scan after a cargo phase reopens the mutable-checkout bypass (#560)."
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
    mode="${2:-find}"
    file_list="$(mktemp)"
    symlink_list="$(mktemp)"

    if [ "$mode" = "git" ]; then
        # Tracked-tree discovery (production): `git ls-tree -r HEAD` reads
        # the HEAD COMMIT's tree object, which is immutable once committed --
        # unlike `git ls-files -s` (index state), nothing running after
        # checkout (a build step or proc-macro executed earlier in
        # scripts/ci.sh, which runs clippy/build before this guard) can
        # mutate it via `git rm --cached` or any other index surgery.
        # Untracked build-artifact output (target/, .cargo-target/, anything
        # gitignored) is still never listed -- no name-based pruning needed
        # -- and a TRACKED module nested under a dir literally named `target`
        # is still not invisible the way `find -prune` made it. `-r -z`
        # recurses fully (only blob/commit entries are emitted, never
        # intermediate tree entries) and NUL-terminates each entry
        # ("<mode> <type> <object>\t<path>"); mode 120000 is a tracked
        # symlink, the same filesystem-indirection hazard the find-mode
        # -type l pass catches below. This is the COMMITTED tree, not a live
        # stat of the working tree: a working-tree entry that diverges from
        # its clean HEAD entry (a prior build step/proc-macro swapping the
        # file, or a parent directory, to a symlink after checkout, without
        # re-staging) is what the per-component O_NOFOLLOW read guard in the
        # python scan loop catches; this check and that one are
        # complementary.
        if ! git -C "$root" rev-parse --is-inside-work-tree > /dev/null 2>&1; then
            rm -f "$file_list" "$symlink_list"
            echo "stub-marker scan: $root is not inside a git working tree -- tracked-tree discovery requires git; refusing to fall back to a raw filesystem walk" >&2
            return 1
        fi
        if ! git -C "$root" rev-parse --verify HEAD > /dev/null 2>&1; then
            rm -f "$file_list" "$symlink_list"
            echo "stub-marker scan: $root has no valid HEAD commit -- tracked-tree discovery requires a committed HEAD; refusing to fall back to the mutable index or a raw filesystem walk" >&2
            return 1
        fi

        # Capture git's own exit code explicitly (`|| git_rc=$?` keeps set -e
        # from aborting before the check runs) and write its output to a temp
        # file rather than piping straight into python3: a pipe lets python's
        # exit status mask git's, so a partial listing followed by git's own
        # non-zero exit would otherwise still read as an overall success and
        # scan only a subset of files. Fail closed on any non-zero rc.
        tree_list="$(mktemp)"
        git_rc=0
        git -C "$root" ls-tree -r -z HEAD -- crates/ > "$tree_list" || git_rc=$?
        if [ "$git_rc" -ne 0 ]; then
            rm -f "$file_list" "$symlink_list" "$tree_list"
            echo "stub-marker scan: git ls-tree -r HEAD -- crates/ failed (exit $git_rc) -- refusing to scan a partial or absent tree listing" >&2
            return 1
        fi
        python3 -c '
import os
import sys

root, out_files, out_symlinks, tree_list_path = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
with open(tree_list_path, "rb") as fh:
    raw = fh.read()
files = []
symlinks = []
for entry in raw.split(b"\0"):
    if not entry:
        continue
    meta, sep, relpath = entry.partition(b"\t")
    if not sep:
        continue
    # ls-tree metadata is "<mode> <type> <object>" (no index stage field,
    # unlike ls-files -s).
    file_mode = meta.split(b" ", 1)[0]
    relpath_s = os.fsdecode(relpath)
    abspath = os.path.join(root, relpath_s)
    if file_mode == b"120000":
        symlinks.append(abspath)
    elif relpath_s.endswith(".rs"):
        files.append(abspath)
with open(out_files, "wb") as fh:
    fh.write(b"\0".join(os.fsencode(p) for p in files))
    if files:
        fh.write(b"\0")
with open(out_symlinks, "wb") as fh:
    fh.write(b"\0".join(os.fsencode(p) for p in symlinks))
    if symlinks:
        fh.write(b"\0")
' "$root" "$file_list" "$symlink_list" "$tree_list"
        rm -f "$tree_list"
    else
        # find-mode discovery: self-test fixture trees only (mktemp -d
        # trees, never git repos) -- production always uses git mode above.
        # NUL-delimited discovery and transport end-to-end: a filename may
        # legally contain a newline (or any byte but NUL and `/`), and this
        # scanner's own findings later echo filenames straight into CI
        # stdout. `-print0` + a NUL-delimited transport file sidesteps
        # splitting entirely; `sanitize_for_ci` (applied to every rendered
        # path below, the same function already used on messages) handles
        # the render side. Only `target`/`.cargo-target` (build-artifact
        # dirs, per .gitignore) are pruned here -- fixture trees have no git
        # index to consult.
        find "$root/crates" \
            \( -name 'target' -o -name '.cargo-target' \) -type d -prune \
            -o -name '*.rs' -type f -print0 \
            > "$file_list"

        # Filesystem-indirection guard: collect EVERY symlink under crates/
        # (any name, file or directory), excluding the same pruned
        # build-artifact dirs. A symlinked .rs is skipped by the -type f pass
        # above but compiled by Cargo; a symlinked directory is not descended
        # by find (no -L) yet Cargo builds through it. The python below
        # hard-fails on any of them.
        find "$root/crates" \
            \( -name 'target' -o -name '.cargo-target' \) -type d -prune \
            -o -type l -print0 \
            > "$symlink_list"
    fi

    if [ ! -s "$file_list" ] && [ ! -s "$symlink_list" ]; then
        rm -f "$file_list" "$symlink_list"
        echo "no .rs files matched under $root/crates -- the scanner would silently be a no-op; fix the file-layout selection" >&2
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
#
# SCOPE (documented non-goal, not a silent bypass): this matches the LITERAL
# spellings `panic!`/`unreachable!` only. A macro alias, re-export, or
# wrapper (e.g. a local `crash!` that expands to panic!) is not resolved --
# that requires macro-expansion-aware parsing this scanner does not attempt,
# and expanding this regex to guess at alias names would be unreliable and
# unmaintainable. Alias coverage is intentionally left to clippy and human
# review; see the scanner's file-header SCOPE note.
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
# A slot backed by a non-literal argument (a variable, a function call, a
# Rust-2021 inline-captured identifier) must never be silently vacated to an
# empty string: `panic!("not {feature} implemented")` with `feature` a
# runtime value would collapse to "not  implemented" and false-positive
# through PLACEHOLDER_RE's `\s*` alternatives. Substituting a byte no
# PLACEHOLDER_RE alternative's `\s*`/word content can match through prevents
# the slot from bridging two unrelated neighboring words into a marker,
# without itself ever completing one (`\b` never matches NUL, and no
# alternative contains it). It only ever reaches CI output through
# sanitize_for_ci (whose CONTROL_CHAR_RE already covers 0x00), so it never
# leaks raw.
UNRESOLVED_SLOT_SENTINEL = "\x00"


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
    string literal (plain, raw, or byte-raw), a concat!(...) of string
    literals, or a format_args!(...) call whose own arguments are ALL
    themselves literal-derivable (recursively, via resolve_format_message_strict)
    -- statically reconstructible end to end; otherwise None. A non-literal
    argument -- a variable, a function call, a format_args!(...) with even one
    non-literal slot -- is invisible to a static text scan and returns None
    rather than a partial/fabricated reconstruction. Leading whitespace is
    skipped; a concat! call's own literal arguments are joined."""
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
    m = re.match(r"format_args\s*!\s*[([{]", code_only[i:end])
    if m is not None:
        open_pos = i + m.end() - 1
        inner_spans, _inner_close = split_macro_args(code_only, clean, open_pos)
        if not inner_spans:
            return None
        inner_template = arg_literal_value(clean, code_only, inner_spans[0][0], inner_spans[0][1])
        if inner_template is None:
            return None
        inner_positionals = []
        inner_named = {}
        for span_start, span_end in inner_spans[1:]:
            nm = IDENT_ARG_RE.match(code_only, span_start, span_end)
            if nm is not None:
                inner_named[nm.group(1)] = arg_literal_value(clean, code_only, nm.end(), span_end)
            else:
                inner_positionals.append(arg_literal_value(clean, code_only, span_start, span_end))
        return resolve_format_message_strict(inner_template, inner_positionals, inner_named)
    lit = match_string_literal(clean, i)
    if lit is None:
        return None
    return decode_rust_escapes(lit[1], lit[2])


def resolve_format_message_strict(template, positionals, named):
    """Like resolve_format_message, but for fully reconstructing a NESTED
    format_args!(...) call (see arg_literal_value): returns the substituted
    text only if EVERY slot the template references resolves to a literal
    argument, else None. None means "not statically derivable end to end" --
    the caller (arg_literal_value) then reports the whole format_args! call as
    non-literal, so its slot falls back to UNRESOLVED_SLOT_SENTINEL at the
    outer level instead of silently reconstructing a partial message."""
    auto = [0]
    unresolved = [False]

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
        if value is None:
            unresolved[0] = True
            return ""
        return value

    result = FORMAT_SLOT_RE.sub(repl, template)
    return None if unresolved[0] else result


def resolve_format_message(template, positionals, named):
    """Approximate the runtime message by substituting the macro's literal
    arguments into the format template's slots. `{{`/`}}` are literal braces; an
    auto `{}` takes the next positional in order, `{n}` takes positional n, and
    `{name}` takes a named argument. A slot backed by a literal argument is
    filled with that literal's decoded text; a slot backed by a non-literal or
    unresolved argument -- including a Rust 2021 inline-captured variable, which
    is not among the macro's explicit args -- is filled with
    UNRESOLVED_SLOT_SENTINEL, never an empty string (an empty-string vacate
    would let "not {var} implemented" collapse into a false-positive "not
    implemented" through PLACEHOLDER_RE's `\\s*` alternatives). That is the
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
        return value if value is not None else UNRESOLVED_SLOT_SENTINEL

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


def open_no_indirection(scan_root, rel_path):
    """Open `rel_path` (forward-slash separated, relative to `scan_root`) for
    reading, refusing a symlink at EVERY path component -- not just the
    final one. `os.open(path, O_NOFOLLOW)` on a full path string only binds
    the LAST component: a PARENT directory swapped to a symlink after
    discovery (e.g. a build step racing this guard) redirects the read to
    wherever that symlink points, and a final-component-only O_NOFOLLOW
    never sees it. Walk component-by-component with `dir_fd=` (openat(2)
    semantics) starting from a root directory fd opened once with
    O_DIRECTORY|O_NOFOLLOW, refusing a symlink -- ELOOP -- at each step, so
    every component from the scan root down to the file is proven a
    non-symlink at read time. Raises OSError (ELOOP/EMLINK on a symlinked
    component, ENOENT/ENOTDIR on a vanished or non-directory component) --
    the caller handles these the same way it handled a final-component
    O_NOFOLLOW failure."""
    parts = rel_path.split("/")
    if not parts or any(p in ("", ".", "..") for p in parts):
        raise OSError(errno.ELOOP, "path has an empty, '.', or '..' component: " + rel_path)
    dir_fd = os.open(scan_root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=dir_fd)
            os.close(dir_fd)
            dir_fd = next_fd
        file_fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW, dir_fd=dir_fd)
    finally:
        os.close(dir_fd)
    return file_fd


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
    # Bind the read to the exact path discovery found, atomically, at EVERY
    # path component (not just the final one): a path that was a real
    # regular file under a real directory tree at discovery time (a
    # `find -type f` hit, or a git-tree 100644/100755 entry) but has since
    # had itself OR any parent directory swapped to a symlink -- by a
    # PR-controlled build script or proc-macro racing this guard, or simply
    # because a git-tracked working-tree entry diverged from its clean
    # HEAD-tree entry -- raises ELOOP here instead of silently following the
    # replacement or reading different content than what was discovered.
    try:
        fd = open_no_indirection(SCAN_ROOT, rel_path)
    except OSError as exc:
        # ENOTDIR joins ELOOP/EMLINK here: opening a symlinked directory
        # component with O_DIRECTORY|O_NOFOLLOW raises ELOOP on Linux but
        # ENOTDIR on macOS/BSD (O_NOFOLLOW refuses to dereference it, so the
        # kernel reports the un-dereferenced node -- a symlink -- as not a
        # directory rather than as a symlink loop). Both mean the same thing
        # here: a parent component that should be a directory is not one at
        # read time.
        if exc.errno in (errno.ELOOP, errno.EMLINK, errno.ENOTDIR):
            sys.stderr.write(
                "stub-marker scan: a path component (the discovered file "
                "itself, or a PARENT directory) is a symlink at read time "
                "(swapped after discovery) -- refusing to follow a "
                f"post-discovery filesystem indirection: {sanitize_for_ci(rel_path)}\n"
            )
            sys.exit(1)
        raise
    with os.fdopen(fd, "r", encoding="utf-8") as fh:
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
        scan "$ROOT" git
        ;;
    *)
        echo "usage: $0 [--self-test]" >&2
        exit 2
        ;;
esac
