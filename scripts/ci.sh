#!/bin/sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/../crates"

phase_lockfile() {
    echo "=== Lockfile Freshness ==="
    # crates/Cargo.lock is committed (#920): dependency pins land as reviewable
    # diffs and CI resolves exactly what was reviewed. --locked fails instead of
    # silently re-resolving if Cargo.lock drifts from what the manifests allow.
    cargo check --workspace --locked
}

phase_forward_deployed() {
    echo "=== Forward-Deployed Crates Check ==="
    # Excluded workspace crates (forward-deployed infrastructure) must still compile,
    # pass clippy under -D warnings across all targets, and pass their test suite.
    # khive-merge declares its own [workspace] table, so it resolves a separate
    # dependency graph from crates/Cargo.lock and needs its own committed lock and
    # its own --locked for the phase_lockfile guarantee above to cover it too.
    RUSTFLAGS="-D warnings" cargo check --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --all-targets --locked
    cargo clippy --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --all-targets --locked -- -D warnings
    cargo test --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --locked
}

phase_lint() {
    echo "=== Format Check ==="
    cargo fmt --all -- --check

    echo "=== SQL Lint ==="
    sh "$SCRIPT_DIR/lint-sql.sh"

    echo "=== ADR Reference Lint ==="
    sh "$SCRIPT_DIR/lint-adr-refs.sh"

    echo "=== ADR Reference Lint Self-Test ==="
    sh "$SCRIPT_DIR/lint-adr-refs.sh" --self-test
}

phase_no_stubs_scan() {
    echo "=== No-Stub Guard (placeholder-string panic!/unreachable! scan) ==="
    # SECURITY ORDERING (#560 follow-up): this placeholder-string scan and its
    # self-test run as the FIRST ci phase, before phase_lockfile or any other
    # cargo invocation. The cargo phases compile PR-controlled code (build
    # scripts, proc-macros); if this guard ran after them, a build step could
    # replace a committed stub source, the committed allowlist, or this scanner
    # script itself with benign content and slip a stub past the guard. Running
    # before any cargo compilation removes that opportunity: at this point the
    # working tree is the pristine checkout, so the scanner reads exactly the
    # committed sources. The scanner self-test asserts this ordering so a future
    # reorder that moves the scan after a cargo phase fails loud.
    #
    # `todo!()`/`unimplemented!()` are denied unconditionally by the clippy pass
    # in phase_no_stubs, but `panic!`/`unreachable!` are legitimate everywhere
    # (assertion failures, invariant violations) -- clippy has no lint for "the
    # message looks like a stub", and denying the macros outright would fail
    # hundreds of correct call sites. This scans the string literal argument of
    # every panic!/unreachable! call for placeholder language across every .rs
    # file under crates/ (source, tests, benches, examples) -- a broader scope
    # than the --lib --bins clippy pass: a placeholder message reads as a stub
    # whether or not the code compiling it is test-gated (#560).
    sh "$SCRIPT_DIR/lint-stub-markers.sh"

    echo "=== No-Stub Guard (placeholder-string scanner self-test) ==="
    # Locks in the scanner's own fixture coverage so a future parser change
    # cannot silently regress it without the fixtures ever running in CI.
    sh "$SCRIPT_DIR/lint-stub-markers.sh" --self-test
}

phase_no_stubs() {
    echo "=== No-Stub Guard (clippy restriction lints) ==="
    # AST-aware "No stubs. Ever." enforcement. clippy parses the macros, so it is
    # immune to the grep failure modes (spacing like `todo !()`, brace forms like
    # `unimplemented!{}`, macro names inside comments or string literals). Scoped to
    # --lib --bins = shipping source only (excludes tests/benches/examples), matching
    # the prior policy. khive-merge is excluded from the workspace (forward-deployed),
    # so it gets its own pass to preserve coverage. The placeholder-string scan that
    # used to run here now runs first, in phase_no_stubs_scan, before any cargo
    # compilation (see that phase's security-ordering note).
    NOSTUB_LINTS="-Dclippy::todo -Dclippy::unimplemented -Dclippy::dbg_macro"
    # shellcheck disable=SC2086
    cargo clippy --workspace --lib --bins -- $NOSTUB_LINTS
    # shellcheck disable=SC2086
    cargo clippy --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --lib --bins --locked -- $NOSTUB_LINTS
}

phase_clippy() {
    echo "=== Clippy ==="
    cargo clippy --workspace --all-targets --all-features -- -D warnings
}

phase_docs() {
    echo "=== Doc Build (-D warnings) ==="
    # Mirrors the "Doc build" CI job (.github/workflows/ci.yml): intra-doc link
    # breakage and other rustdoc lints are a distinct gate that check/clippy/test
    # do not cover.
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
}

# #1204 tripwire (shared by phase_tests and phase_tests_doc):
# RuntimeConfig::default() resolves db_path to $HOME/.khive/khive.db, and
# migrations apply on open (forward-only, ADR-015). Run each guarded phase in
# an ephemeral HOME, while preserving the operator's Cargo and rustup homes,
# then fingerprint that isolated store before and after. A test that inherits
# the default path still fails loudly, but an unrelated live daemon can mutate
# the operator's real store without creating a false attribution (#1627).
# Covered paths are khive.db, khive.db-wal, khive.db-shm,
# khive.db.walpin/**, and khive.db.ann/**.
# A WAL-mode open can leave the main file unchanged until checkpoint, while
# WAL-pin attribution and ANN persistence write the adjacent directories.
# Size and mtime catch common changes cheaply; the content hash also catches
# same-size writes within the filesystem's timestamp granularity.
sentinel_file_fingerprint() {
    f=$1
    if [ -f "$f" ]; then
        m=$(stat -f%m "$f" 2>/dev/null || stat -c%Y "$f")
        s=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f")
        h=$({ shasum -a 256 "$f" 2>/dev/null || sha256sum "$f"; } | awk '{print $1}')
        printf '%s mtime=%s size=%s sha256=%s\n' "$f" "$m" "$s" "$h"
    else
        printf '%s absent\n' "$f"
    fi
}

sentinel_fingerprint() {
    for f in "$HOME/.khive/khive.db" "$HOME/.khive/khive.db-wal" "$HOME/.khive/khive.db-shm"; do
        sentinel_file_fingerprint "$f"
    done

    for d in "$HOME/.khive/khive.db.walpin" "$HOME/.khive/khive.db.ann"; do
        if [ -d "$d" ]; then
            printf '%s directory\n' "$d"
            find "$d" -type f -print | LC_ALL=C sort | while IFS= read -r f; do
                sentinel_file_fingerprint "$f"
            done
        else
            printf '%s absent\n' "$d"
        fi
    done
}

# Run the given test command inside the #1204 sentinel guard. Every phase that
# executes workspace tests of any kind (unit/integration or doctests) must go
# through this wrapper so no test-execution path escapes the default-store
# isolation invariant.
run_with_store_sentinel() (
    operator_home=${HOME:-}
    isolated_home=$(mktemp -d "${TMPDIR:-/tmp}/khive-ci-home.XXXXXX")
    trap 'rm -rf -- "$isolated_home"' 0
    trap 'exit 130' HUP INT TERM

    # `cargo` and `rustup` normally keep their toolchains below HOME. Preserve
    # those locations before swapping HOME so isolation does not trigger a
    # toolchain download or make `cargo` disappear on developer machines.
    if [ -z "${CARGO_HOME+x}" ] && [ -n "$operator_home" ]; then
        CARGO_HOME="$operator_home/.cargo"
        export CARGO_HOME
    fi
    if [ -z "${RUSTUP_HOME+x}" ] && [ -n "$operator_home" ]; then
        RUSTUP_HOME="$operator_home/.rustup"
        export RUSTUP_HOME
    fi
    HOME=$isolated_home
    export HOME

    sentinel_before=$(sentinel_fingerprint)

    if "$@"; then
        command_status=0
    else
        command_status=$?
    fi

    sentinel_after=$(sentinel_fingerprint)
    if [ "$sentinel_after" != "$sentinel_before" ]; then
        echo "FAIL: test suite touched the default store in its isolated sentinel HOME — a test opened/migrated it instead of using an explicit temporary db_path (#1204)" >&2
        echo "before:" >&2
        printf '%s\n' "$sentinel_before" >&2
        echo "after:" >&2
        printf '%s\n' "$sentinel_after" >&2
        exit 1
    fi

    exit "$command_status"
)

phase_tests() {
    echo "=== Tests ==="
    # NEXTEST_PARTITION (e.g. "1/2") routes this phase through cargo-nextest's
    # count-based partitioning instead of a full `cargo test --workspace` run,
    # so CI can shard the workspace test suite across parallel jobs. Unset
    # (the default, including every local/non-sharded invocation) keeps the
    # plain `cargo test --workspace` path unchanged.
    if [ -n "${NEXTEST_PARTITION:-}" ]; then
        run_with_store_sentinel cargo nextest run --workspace --partition "count:${NEXTEST_PARTITION}"
    else
        run_with_store_sentinel cargo test --workspace
    fi
}

phase_tests_doc() {
    echo "=== Doctests ==="
    # cargo-nextest does not execute doctests (upstream limitation); this phase
    # covers the doctest slice that phase_tests's nextest path skips. The plain
    # `cargo test --workspace` path in phase_tests already runs doctests itself,
    # so this phase only needs to run alongside the sharded nextest path.
    # Doctests execute arbitrary workspace code, so they carry the same #1204
    # sentinel guard as every other test-execution path.
    run_with_store_sentinel cargo test --doc --workspace
}

phase_channel_email() {
    echo "=== Channel-Email Feature Tests (channel-email feature) ==="
    # `--workspace` alone never runs any of the several `#[cfg(feature =
    # "channel-email")]` test modules in khive-mcp (ADR-094 channel lifecycle
    # sequencing, issue #449 cursor_commit gating, bootstrap-floor regressions,
    # etc.) -- the all-features clippy pass above only type-checks them. A prior
    # name filter here (`channel_lifecycle`) ran only one of those modules and
    # silently skipped the rest, including the daemon's durable-cursor
    # regression tests. Run the whole crate under the feature, unfiltered, so
    # every one of those modules fails CI on a regression.
    cargo test -p khive-mcp --features channel-email
}

run_daemon_recovery_repeats() {
    # #539/#544: both tests mutate the process-global daemon rendezvous and
    # intentionally create maximal recovery contention. Run them serially
    # inside each process, but repeat the paired scenario enough times on every
    # supported CI OS to expose scheduler-sensitive ownership leaks.
    #
    # Fail-closed count gate: a libtest name filter that matches zero tests
    # still exits 0, so renaming or moving either test would silently turn
    # this gate into a no-op. Enumerate the exact expected test names, pass
    # --exact because libtest filters are substring matches by default, capture
    # the harness summary on every iteration, and require exactly that many
    # passes; any mismatch (including zero) exits 1 naming the filter.
    expected_test_names="\
        daemon::tests::parallel_no_socket_recovery_converges_to_one_usable_daemon \
        daemon::tests::parallel_parse_failure_is_terminal_and_never_recovers"
    expected_tests=0
    for name in $expected_test_names; do
        expected_tests=$((expected_tests + 1))
    done
    repeat=1
    while [ "$repeat" -le 25 ]; do
        echo "daemon recovery repeat ${repeat}/25"
        output_file=$(mktemp)
        # --test-threads=1: the pair mutates process-global rendezvous state.
        # The explicit failure branch keeps this gate fail-fast regardless of
        # the caller's shell mode (the script runs `set -e`, but a pipeline or
        # a future context change must not be able to disarm it).
        # shellcheck disable=SC2086
        cargo test -p khive-mcp --lib -- --test-threads=1 --exact $expected_test_names \
            > "$output_file" 2>&1 || {
                cat "$output_file" >&2
                rm -f "$output_file"
                echo "FAIL: daemon recovery repeat ${repeat}/25: cargo test exited non-zero (filter: ${expected_test_names})" >&2
                exit 1
            }
        cat "$output_file"
        ran_count=$(sed -n 's/^test result: ok\. \{0,\}\([0-9][0-9]*\) passed;.*/\1/p' "$output_file" | head -n 1)
        rm -f "$output_file"
        if [ -z "$ran_count" ] || [ "$ran_count" -ne "$expected_tests" ]; then
            echo "FAIL: daemon recovery repeat ${repeat}/25 ran ${ran_count:-0} tests, expected ${expected_tests} (filter: ${expected_test_names}) — the gate must run exactly the enumerated tests" >&2
            exit 1
        fi
        repeat=$((repeat + 1))
    done
}

phase_daemon_recovery_flake() {
    echo "=== Daemon Recovery Flake Gate (25 repeats) ==="
    # One guard around the complete repeat batch detects any default-store
    # mutation without re-hashing a potentially large operator store 50 times.
    run_with_store_sentinel run_daemon_recovery_repeats
}

phase_no_default_features() {
    echo "=== No-Default-Features Check ==="
    cargo check --workspace --no-default-features
}

phase_release() {
    echo "=== Build (release) ==="
    cargo build --workspace --release
}

phase_contract_tests() {
    echo "=== Contract Tests ==="
    python3 "$SCRIPT_DIR/../tests/contract_test.py"
}

phase_deno_tests() {
    echo "=== Deno Tests ==="
    (cd "$SCRIPT_DIR/../cli" && deno test --allow-all .)
}

phase_smoke_tests() {
    echo "=== Smoke Test ==="
    python3 "$SCRIPT_DIR/../tests/test_documented_verb_counts.py"
    python3 "$SCRIPT_DIR/../tests/smoke_test.py"
    python3 "$SCRIPT_DIR/../tests/smoke_brain.py"
    python3 "$SCRIPT_DIR/../tests/smoke_comm.py"
    python3 "$SCRIPT_DIR/../tests/smoke_knowledge.py"
    python3 "$SCRIPT_DIR/../tests/smoke_schedule.py"
}

phase_vector_smoke() {
    echo "=== Vector Smoke (embed/recall path gate) ==="
    # smoke_vector.py self-guards empirically: it spawns kkernel, attempts one
    # memory.remember, and prints "SKIP: ..." + exits 0 when the embedder is not
    # usable (model weights absent or no engine resolves). GitHub Actions runners
    # that lack the model weights are unaffected. Set KHIVE_NO_EMBED=1 to bypass.
    python3 "$SCRIPT_DIR/../tests/smoke_vector.py"
}

phase_contract_suite() {
    echo "=== Contract Suite (khive-contract) ==="
    (cd "$SCRIPT_DIR/../tests/khive-contract" && uv run pytest -q)
}

phase_macos_pr_check() {
    echo "=== macOS PR Compile Check ==="
    # PRs keep cross-platform compile coverage without paying for the full lint,
    # release, and end-to-end suite twice. The excluded khive-merge crate needs an
    # explicit check because it is not a workspace member.
    cargo check --workspace --all-targets --all-features
    RUSTFLAGS="-D warnings" cargo check --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --all-targets --locked
}

phase_macos_pr_tests() {
    echo "=== macOS PR Platform Tests ==="
    # These crates own the SQLite/filesystem, daemon/process, and native CLI
    # boundaries where macOS behavior has historically differed from Linux.
    cargo test -p khive-db -p khive-runtime -p khive-mcp -p khive-pack-git -p kkernel --features khive-mcp/channel-email
}

run_phase() {
    case "$1" in
        no-stubs-scan) phase_no_stubs_scan ;;
        lockfile) phase_lockfile ;;
        forward-deployed) phase_forward_deployed ;;
        lint) phase_lint ;;
        no-stubs) phase_no_stubs ;;
        clippy) phase_clippy ;;
        docs) phase_docs ;;
        tests) phase_tests ;;
        tests-doc) phase_tests_doc ;;
        channel-email) phase_channel_email ;;
        daemon-recovery-flake) phase_daemon_recovery_flake ;;
        no-default-features) phase_no_default_features ;;
        release) phase_release ;;
        contract-tests) phase_contract_tests ;;
        deno-tests) phase_deno_tests ;;
        smoke-tests) phase_smoke_tests ;;
        vector-smoke) phase_vector_smoke ;;
        contract-suite) phase_contract_suite ;;
        macos-pr-check) phase_macos_pr_check ;;
        macos-pr-tests) phase_macos_pr_tests ;;
        *)
            echo "Unknown CI phase: $1" >&2
            echo "Valid phases: no-stubs-scan lockfile forward-deployed lint no-stubs clippy docs tests tests-doc channel-email daemon-recovery-flake no-default-features release contract-tests deno-tests smoke-tests vector-smoke contract-suite macos-pr-check macos-pr-tests" >&2
            exit 2
            ;;
    esac
}

run_all() {
    for phase in \
        no-stubs-scan \
        lockfile \
        forward-deployed \
        lint \
        no-stubs \
        clippy \
        docs \
        tests \
        channel-email \
        daemon-recovery-flake \
        no-default-features \
        release \
        contract-tests \
        deno-tests \
        smoke-tests \
        vector-smoke \
        contract-suite
    do
        run_phase "$phase"
    done
    echo "=== CI Passed ==="
}

case "$#" in
    0) run_all ;;
    1)
        if [ "$1" = "all" ]; then
            run_all
        else
            run_phase "$1"
        fi
        ;;
    *)
        echo "Usage: $0 [phase|all]" >&2
        exit 2
        ;;
esac
