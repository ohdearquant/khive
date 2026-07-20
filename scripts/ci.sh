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

phase_tests() {
    echo "=== Tests ==="
    cargo test --workspace
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
    python3 "$SCRIPT_DIR/../tests/smoke_test.py"
    python3 "$SCRIPT_DIR/../tests/smoke_brain.py"
    python3 "$SCRIPT_DIR/../tests/smoke_comm.py"
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
    cargo test -p khive-db -p khive-runtime -p khive-mcp -p khive-pack-git -p kkernel
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
            echo "Valid phases: no-stubs-scan lockfile forward-deployed lint no-stubs clippy docs tests no-default-features release contract-tests deno-tests smoke-tests vector-smoke contract-suite macos-pr-check macos-pr-tests" >&2
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
