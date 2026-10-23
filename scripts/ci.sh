#!/bin/sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/../crates"

# TEST_SCOPE selects what a pull-request run builds and tests (ADR-205 D5). Unset or `full` keeps every
# phase in its workspace form. Otherwise it is the space-separated crate list that
# scripts/ci-test-scope.py printed: the changed crates and every crate that depends on them.
scoped() {
    [ -n "${TEST_SCOPE:-}" ] && [ "$TEST_SCOPE" != "full" ]
}

# True when the run is not scoped, or the named crate is in the scope.
in_scope() {
    scoped || return 0
    case " $TEST_SCOPE " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

# The `-p` arguments for the scoped crates that are workspace members, read from cargo's own member
# list. A crate outside the workspace (khive-merge) is tested by the forward-deployed phase instead.
# Fails, rather than printing nothing, when the member list cannot be read, so a broken read never
# turns into an unfiltered workspace command.
scope_package_args() {
    members=$(cargo metadata --no-deps --format-version 1 --locked | python3 -P -c '
import json, sys
meta = json.load(sys.stdin)
ids = set(meta["workspace_members"])
print("\n".join(sorted(p["name"] for p in meta["packages"] if p["id"] in ids)))
') || { echo "scope: cannot read the workspace member list" >&2; return 1; }
    [ -n "$members" ] || { echo "scope: the workspace member list is empty" >&2; return 1; }
    args=""
    for crate in $TEST_SCOPE; do
        if printf '%s\n' "$members" | grep -qxF -e "$crate"; then
            args="$args -p $crate"
        fi
    done
    printf '%s\n' "$args"
}

# Prints the skip line and returns 0 when a scoped run has nothing of this phase to run.
scope_skips() {
    if scoped && ! in_scope "$1"; then
        echo "scope: $1 is not under test in this run (TEST_SCOPE=$TEST_SCOPE); phase skipped"
        return 0
    fi
    return 1
}

phase_lockfile() {
    echo "=== Lockfile Freshness ==="
    # crates/Cargo.lock is committed (#920): dependency pins land as reviewable
    # diffs and CI resolves exactly what was reviewed. --locked fails instead of
    # silently re-resolving if Cargo.lock drifts from what the manifests allow.
    if scoped; then
        # A -p selection unifies features over the selected packages only, so a target that compiles only
        # under whole-workspace unification is invisible to the scoped test and clippy phases. Checking
        # every target of every crate closes that class without code generation for the test targets.
        cargo check --workspace --all-targets --locked
    else
        cargo check --workspace --locked
    fi
}

phase_forward_deployed() {
    echo "=== Forward-Deployed Crates Check ==="
    scope_skips khive-merge && return 0
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
    python3 "$SCRIPT_DIR/tests/test_lint_sql.py"

    phase_adr_lint

    echo "=== Local Build Artifact Verification Tests ==="
    python3 "$SCRIPT_DIR/tests/test_verify_local_artifact.py"

    echo "=== Writer Census Contract Tests ==="
    python3 "$SCRIPT_DIR/tests/test_writer_census.py"

    echo "=== CI Workflow Contract Tests ==="
    python3 "$SCRIPT_DIR/tests/test_ci_workflows.py"
    python3 "$SCRIPT_DIR/tests/test_ci_clippy.py"
    python3 "$SCRIPT_DIR/tests/test_ci_infra_retry.py"

    echo "=== CI Empty-HOME Sentinel Tests ==="
    python3 "$SCRIPT_DIR/tests/test_ci_home_sentinel.py"
    python3 "$SCRIPT_DIR/tests/test_kkernel_test_isolation.py"

    echo "=== Binary Resolution Harness Tests ==="
    python3 "$SCRIPT_DIR/tests/test_binary_resolution.py"

    echo "=== Contract Harness Tests ==="
    python3 "$SCRIPT_DIR/tests/test_ci_contract_harness.py"
    sh "$SCRIPT_DIR/ci-contract-harness.sh" python "$SCRIPT_DIR/tests/test_contract_harness.py"
}

# The hosted docs job runs this phase too, so a change that touches only Markdown, which skips the Rust
# jobs, still has its ADR references and status lines checked.
phase_adr_lint() {
    echo "=== ADR Reference Lint ==="
    sh "$SCRIPT_DIR/lint-adr-refs.sh"

    echo "=== ADR Reference Lint Self-Test ==="
    sh "$SCRIPT_DIR/lint-adr-refs.sh" --self-test

    echo "=== ADR Status Lint ==="
    python3 "$SCRIPT_DIR/lint-adr-status.py"

    echo "=== ADR Status Lint Self-Test ==="
    python3 "$SCRIPT_DIR/lint-adr-status.py" --self-test
}

phase_docs_fmt() {
    echo "=== Docs Format Check ==="
    # The hosted docs job runs `deno fmt --check` from the repo root over the
    # set named in ./deno.json, and until this phase existed nothing in this
    # script did, so a markdown table padded by hand passed every local phase
    # and failed on the hosted side. The job now runs this phase, so the two
    # sides execute one command rather than two copies of it.
    #
    # A missing deno is a refusal, not a skip: a gate whose discovery can be
    # emptied silently exits 0 and checks nothing. The deno-tests phase needs
    # the same binary, so a checkout that cannot run this cannot run that either.
    if ! command -v deno > /dev/null 2>&1; then
        echo "deno not found on PATH: install deno v2.x (https://deno.com); the deno-tests phase needs it too" >&2
        exit 1
    fi
    (cd "$SCRIPT_DIR/.." && deno fmt --check)
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

    echo "=== Source File Size Cap (line cap with a shrink-only allow-list) ==="
    # Runs in this phase so every lane that runs the stub scan, both suites and
    # every shard, runs the cap too, and before any cargo phase like the scan.
    sh "$SCRIPT_DIR/lint-source-file-size.sh"

    echo "=== Source File Size Cap self-test ==="
    sh "$SCRIPT_DIR/lint-source-file-size.sh" --self-test
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
    if scoped; then
        packages=$(scope_package_args) || return 1
        if [ -z "$packages" ]; then
            echo "scope: no workspace member is under test; clippy skipped"
            return 0
        fi
    else
        packages="--workspace"
    fi
    # Optional features can hide warnings in the default configuration. Keep
    # both checks on the same affected-package closure, with default first.
    echo "=== Clippy (default features) ==="
    # shellcheck disable=SC2086
    cargo clippy $packages --all-targets -- -D warnings || return $?
    echo "=== Clippy (all features) ==="
    # shellcheck disable=SC2086
    cargo clippy $packages --all-targets --all-features -- -D warnings
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
# isolation invariant. The initially empty HOME must remain completely empty.
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

    # A clean default database is not enough: timeout logs, recovery locks,
    # dotenv state and empty directories also couple tests to the caller HOME.
    # The directory was empty before the command; no allowlist hides new paths.
    sentinel_home_entries=$(find "$isolated_home" -mindepth 1 -print)
    if [ -n "$sentinel_home_entries" ]; then
        echo "FAIL: test suite left files or directories in its isolated sentinel HOME (#3138)" >&2
        printf '%s\n' "$sentinel_home_entries" >&2
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
    if scoped; then
        packages=$(scope_package_args) || return 1
        if [ -z "$packages" ]; then
            echo "scope: no workspace member is under test; tests skipped"
            return 0
        fi
        echo "scope: testing$packages"
        if [ -n "${NEXTEST_PARTITION:-}" ]; then
            # shellcheck disable=SC2086
            run_with_store_sentinel cargo nextest run $packages --no-fail-fast --partition "count:${NEXTEST_PARTITION}"
        else
            # shellcheck disable=SC2086
            run_with_store_sentinel cargo test $packages
        fi
        return
    fi
    if [ -n "${NEXTEST_PARTITION:-}" ]; then
        # --no-fail-fast: one red test must not cancel the rest of the shard, so a
        # head reports every failure it has.
        run_with_store_sentinel cargo nextest run --workspace --no-fail-fast --partition "count:${NEXTEST_PARTITION}"
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
    if scoped; then
        packages=$(scope_package_args) || return 1
        if [ -z "$packages" ]; then
            echo "scope: no workspace member is under test; doctests skipped"
            return 0
        fi
        # shellcheck disable=SC2086
        run_with_store_sentinel cargo test --doc $packages
        return
    fi
    run_with_store_sentinel cargo test --doc --workspace
}

phase_channel_email() {
    echo "=== Feature Tests (email + Telegram channels, retrieval executor) ==="
    # `--workspace` alone never runs any of the several `#[cfg(feature =
    # "channel-email")]` test modules in khive-mcp (ADR-094 channel lifecycle
    # sequencing, issue #449 cursor_commit gating, bootstrap-floor regressions,
    # etc.) -- the all-features clippy pass above only type-checks them. A prior
    # name filter here (`channel_lifecycle`) ran only one of those modules and
    # silently skipped the rest, including the daemon's durable-cursor
    # regression tests. Run the whole crate under the feature, unfiltered, so
    # every one of those modules fails CI on a regression. Keep Telegram in
    # the same feature job: its poll/outbox admission is equally capable of
    # external side effects and must execute tests, not merely type-check in
    # the all-features clippy pass.
    if in_scope khive-mcp; then
        cargo test -p khive-mcp --features channel-email,channel-telegram
    else
        echo "scope: khive-mcp is not under test; channel feature tests skipped"
    fi
    # The shared retrieval executor is opt-in and has no caller in the
    # workspace yet, so no build enables its feature and `--workspace` compiles
    # neither the executor nor its tests. Run them under the feature here.
    if in_scope khive-retrieval; then
        cargo test -p khive-retrieval --features executor
    else
        echo "scope: khive-retrieval is not under test; executor feature tests skipped"
    fi
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
    scope_skips khive-mcp && return 0
    # One guard around the complete repeat batch detects any default-store
    # mutation without re-hashing a potentially large operator store 50 times.
    run_with_store_sentinel run_daemon_recovery_repeats
}

phase_no_default_features() {
    echo "=== No-Default-Features Clippy ==="
    # Lint the package alone: workspace feature unification enables fold's
    # default serde feature through its runtime/pack dependents (#3341).
    if in_scope khive-fold; then
        cargo clippy --locked -p khive-fold --no-default-features --lib -- -D warnings
        cargo test --locked -p khive-fold --no-default-features --test no_default_checkpoint
    else
        echo "scope: khive-fold is not under test; its no-default-features checks skipped"
    fi
    # Workspace dependents also enable khive-types features, hiding no-default test warnings.
    if in_scope khive-types; then
        cargo clippy --locked -p khive-types --no-default-features --all-targets -- -D warnings
    else
        echo "scope: khive-types is not under test; its no-default-features checks skipped"
    fi
    cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
}

# Single source of truth for where the built kkernel binary lives: KKERNEL_BINARY
# if the caller set it explicitly, else CARGO_TARGET_DIR (absolute, or relative
# to crates/) if set, else crates/target. phase_release exports the result, but
# that export lives only in phase_release's own process — the GitHub workflow
# runs release, contract-tests, smoke-tests, vector-smoke, and contract-suite as
# separate `ci.sh <phase>` processes, so anything that needs the binary later
# calls this function fresh rather than trusting an inherited export.
kkernel_binary_path() {
    if [ -n "${KKERNEL_BINARY:-}" ]; then
        printf '%s\n' "$KKERNEL_BINARY"
        return
    fi
    cargo_target_dir=${CARGO_TARGET_DIR:-"$SCRIPT_DIR/../crates/target"}
    case "$cargo_target_dir" in
        /*) ;;
        *) cargo_target_dir="$SCRIPT_DIR/../crates/$cargo_target_dir" ;;
    esac
    printf '%s\n' "$cargo_target_dir/release/kkernel"
}

phase_release() {
    echo "=== Build (release) ==="
    scope_skips kkernel && return 0
    KKERNEL_BINARY="$(kkernel_binary_path)"
    export KKERNEL_BINARY
    # pack-formal is an optional dependency, so a default build does not link it.
    # smoke_test.py's formal section spawns the binary with `--pack formal` to
    # cover the pack's additive EntityOfType endpoint rules, and an unlinked pack
    # is UnknownPack at startup, not a skip. Link it here so that coverage
    # survives. pack-moodboard is deliberately NOT linked: it registers verbs,
    # and test_documented_verb_counts.py asserts the verb surface in this same
    # phase.
    cargo build --workspace --release --features kkernel/pack-formal
}

phase_contract_tests() {
    echo "=== Contract Tests ==="
    scope_skips kkernel && return 0
    python3 "$SCRIPT_DIR/../tests/contract_test.py"
}

phase_deno_tests() {
    echo "=== Deno Tests ==="
    scope_skips kkernel && return 0
    (cd "$SCRIPT_DIR/../cli" && deno test --allow-all .)
}

phase_smoke_tests() {
    echo "=== Smoke Test ==="
    scope_skips kkernel && return 0
    python3 "$SCRIPT_DIR/../tests/test_documented_verb_counts.py"
    python3 "$SCRIPT_DIR/../tests/smoke_test.py"
    python3 "$SCRIPT_DIR/../tests/smoke_brain.py"
    python3 "$SCRIPT_DIR/../tests/smoke_comm.py"
    python3 "$SCRIPT_DIR/../tests/smoke_knowledge.py"
    python3 "$SCRIPT_DIR/../tests/smoke_schedule.py"
}

phase_vector_smoke() {
    echo "=== Vector Smoke (embed/recall path gate) ==="
    scope_skips kkernel && return 0
    # smoke_vector.py self-guards empirically: it spawns kkernel, attempts one
    # memory.remember, and prints "SKIP: ..." + exits 0 when the embedder is not
    # usable (model weights absent or no engine resolves). GitHub Actions runners
    # that lack the model weights are unaffected. Set KHIVE_NO_EMBED=1 to bypass.
    python3 "$SCRIPT_DIR/../tests/smoke_vector.py"
}

phase_contract_suite() {
    echo "=== Contract Suite (khive-contract) ==="
    scope_skips kkernel && return 0
    sh "$SCRIPT_DIR/ci-contract-harness.sh" pytest -q
}

# The documentation lane skips the contract suite, but one of its tests reads every file in the repository,
# Markdown included, so the hosted docs job runs that test here. It needs ripgrep: under CI it fails rather
# than skips when rg is absent, and a renamed test fails collection rather than selecting nothing.
phase_content_sweeps() {
    echo "=== Repository Identifier Ban (khive-contract) ==="
    sh "$SCRIPT_DIR/ci-contract-harness.sh" pytest -q \
        tests/test_no_importance_in_manifest.py::test_no_importance_identifiers_in_repo
    echo "=== Shipping Pack-Set Copy Check ==="
    # Reads Markdown under docs/, crates/ and scripts/ by content, so the documentation lane must run it.
    # Named by test id: a renamed test is an error, not an empty selection.
    python3 "$SCRIPT_DIR/tests/test_ci_workflows.py" \
        HarnessEnvironmentTests.test_no_new_hand_typed_copy_of_the_shipping_pack_set
    echo "=== Developer Guide Table Checks ==="
    # The directory map is compared with crates/*/Cargo.toml here, where every change runs it; the
    # verb tables need a verbs() result and are compared inside smoke_test.py.
    python3 "$SCRIPT_DIR/../tests/test_documented_guide_tables.py"
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
    # The selection is derived from where the tree actually diverges by target
    # OS, not from which crates sound platform-shaped. `cfg(unix)` is true on
    # Linux as well, so it selects nothing here; the discriminator is a
    # `target_os` arm, and every one of them in the workspace lives in
    # khive-fs (the thread errno accessor), khive-db (walpin, blob store),
    # khive-runtime (daemon, engine config, resource), khive-pack-exec (sandbox
    # and rlimits), khive-pack-moodboard (resolving an open file descriptor back
    # to a path) or khive-pack-git (local remote handling).
    #
    # Everything dropped from this list still runs, on the Linux shards, which
    # partition `cargo nextest run --workspace`. What is given up is a second
    # execution on macOS of code that compiles identically on both.
    #
    # A `target_os` arm is a COMPILE-time divergence, and it is not the only
    # kind. khive-mcp has no such arm and is still here, because its daemon
    # rendezvous pair diverges at RUN time: the tests exercise socket recovery
    # under deliberate contention, which is scheduler behaviour, and the flake
    # gate that repeats them 25 times says in its own comment that it does so
    # on every supported CI OS for exactly that reason. That gate runs only on
    # the full lanes, so dropping khive-mcp here would leave a pull request
    # with no macOS execution of the pair at all. `--lib` keeps the unit tests
    # that hold it without pulling in the integration targets.
    #
    # khive-pack-exec carries test functions that only exist under
    # `cfg(target_os = "macos")`, so a Linux shard cannot run them at all.
    # Before this list included it they ran on pushes to main and never on the
    # pull request that introduced them.
    #
    # khive-pack-moodboard is here for the opposite reason: it has no macOS-only
    # test, it has two different IMPLEMENTATIONS of one function, reading an
    # open descriptor's path from /proc on Linux and from fcntl on macOS. Both
    # compile everywhere, the result guards a symlink escape out of the model
    # directory, and only running the test decides whether the macOS one is
    # right.
    cargo test -p khive-fs -p khive-db -p khive-runtime -p khive-pack-exec -p khive-pack-moodboard
    # khive-pack-git's platform arms are in its unit tests; its integration
    # targets are git plumbing that behaves the same on both systems. khive-mcp
    # is `--lib` for the run-time reason given above.
    cargo test -p khive-pack-git -p khive-mcp --lib
}

run_phase() {
    # Test-only hook: naming a phase in CI_SH_TEST_FAIL_PHASE forces that phase
    # to fail without running its real body, so the run_all failure/skip
    # reporting can be exercised without paying for a build. Unset in every
    # real invocation; never read anywhere else.
    if [ -n "${CI_SH_TEST_FAIL_PHASE:-}" ] && [ "$1" = "$CI_SH_TEST_FAIL_PHASE" ]; then
        echo "CI_SH_TEST_FAIL_PHASE: forcing phase '$1' to fail (test mode)" >&2
        return 1
    fi
    case "$1" in
        no-stubs-scan) phase_no_stubs_scan ;;
        docs-fmt) phase_docs_fmt ;;
        lockfile) phase_lockfile ;;
        forward-deployed) phase_forward_deployed ;;
        lint) phase_lint ;;
        adr-lint) phase_adr_lint ;;
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
        content-sweeps) phase_content_sweeps ;;
        macos-pr-check) phase_macos_pr_check ;;
        macos-pr-tests) phase_macos_pr_tests ;;
        *)
            echo "Unknown CI phase: $1" >&2
            echo "Valid phases: no-stubs-scan docs-fmt lockfile forward-deployed lint adr-lint no-stubs clippy docs tests tests-doc channel-email daemon-recovery-flake no-default-features release contract-tests deno-tests smoke-tests vector-smoke contract-suite content-sweeps macos-pr-check macos-pr-tests" >&2
            exit 2
            ;;
    esac
}

set_run_all_phases() {
    run_all_phases="\
        no-stubs-scan \
        docs-fmt \
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
        contract-suite"
}

print_phases() {
    set_run_all_phases
    for phase in $run_all_phases; do
        echo "$phase"
    done
}

run_all() {
    set_run_all_phases
    current_phase=
    run_all_complete=0

    report_incomplete_ci() {
        status=$?
        trap - 0
        if [ "$run_all_complete" -eq 0 ] && [ "$status" -ne 0 ]; then
            skipped_phases=
            after_failed=0
            for candidate in $run_all_phases; do
                if [ "$after_failed" -eq 1 ]; then
                    skipped_phases="${skipped_phases}${skipped_phases:+ }${candidate}"
                elif [ "$candidate" = "$current_phase" ]; then
                    after_failed=1
                fi
            done
            echo "=== CI Failed ===" >&2
            echo "Failed phase: $current_phase (exit $status)" >&2
            if [ -n "$skipped_phases" ]; then
                echo "Skipped phases: $skipped_phases" >&2
            fi
        fi
        exit "$status"
    }

    trap report_incomplete_ci 0
    for phase in $run_all_phases; do
        current_phase=$phase
        run_phase "$phase"
    done
    run_all_complete=1
    trap - 0
    echo "=== CI Passed ==="
}

case "$#" in
    0) run_all ;;
    1)
        case "$1" in
            all) run_all ;;
            --print-phases) print_phases ;;
            --print-binary-path) kkernel_binary_path ;;
            *) run_phase "$1" ;;
        esac
        ;;
    *)
        echo "Usage: $0 [phase|all|--print-phases|--print-binary-path]" >&2
        exit 2
        ;;
esac
