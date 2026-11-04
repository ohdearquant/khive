#!/bin/sh
# Fail when a Rust source file grows past a line cap (#3931).
#
# Nothing else stops a source file from growing. This check freezes the files
# that are already over the cap at their current sizes, fails any other file
# that crosses it, and makes every split lower a number on the allow-list.
#
# Rules (the cap is the CAP constant below):
#   1. A counted file over the cap that is not on the allow-list fails.
#   2. A listed file over its ceiling fails. A listed file at or under the cap,
#      or no longer a counted source file, fails as a stale row: delete it.
#   3. In a pull request a row may be lowered or deleted but not raised or
#      added, judged against the base branch copy of the allow-list. Outside a
#      pull request (GITHUB_EVENT_NAME is not pull_request) this arm is skipped.
#   4. scripts/source-file-size-allowlist.txt lists every file over the cap,
#      each with its line count as the ceiling.
#
# Population: `.rs` files at crates/<crate>/src/**, excluding any path with a
# `tests` directory and the file names tests.rs, *_tests.rs, *_test.rs and
# test_support.rs. Tests, benches, examples and non-Rust files are not counted.
#
# Discovery follows scripts/lint-stub-markers.sh: the files and their line
# counts come from the COMMITTED tree (`git grep -c` over HEAD), never from the
# index or the working tree, so a build step that ran earlier in scripts/ci.sh
# cannot change what is counted. Commit a split before running this locally.
# The allow-list is the one working-tree input: it is the reviewed file whose
# diff this check judges. A symlink under crates/ is refused by the stub-marker
# scan that runs in the same ci.sh phase, so this check counts regular blobs
# only by construction of the population rule.
#
# Pull request arm: actions/checkout builds the merge commit of the pull
# request, whose first parent is the base branch and whose second parent is the
# pull request head. A depth-1 checkout hides both parents, so in a shallow
# repository the arm fetches them from origin; a full clone is never fetched
# into, because `git fetch --depth` would make it shallow. When the parents
# still cannot be had the arm fails instead of skipping, because a gate that can
# be emptied silently checks nothing. A base branch that has no allow-list yet
# (the change that introduces it) leaves nothing to compare against, and says
# so.
#
# Paths reach the log from the committed tree and from the allow-list, both
# pull-request-controlled, and a log line that starts with `::` is a workflow
# command. Every rendered path goes through safe(), which replaces each byte
# outside a plain path alphabet.
#
# Exit status: 0 pass, 1 a violation or a read that failed (fail closed),
# 2 usage.
#
# Usage: lint-source-file-size.sh [--self-test]
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

CAP=3000
ALLOWLIST_REL="scripts/source-file-size-allowlist.txt"

AWK_SAFE='function safe(s) { gsub(/[^A-Za-z0-9_.\/ -]/, "?", s); return s }'

# Reads the `git grep -c` listing ("HEAD:<path>:<lines>") in $1 and prints
# "<path><TAB><lines>" for every counted source file. A path git had to quote
# (control bytes, a quote, a backslash, non-ASCII) cannot be matched reliably,
# so it fails the read instead of dropping out of the population.
count_lines() {
    awk "$AWK_SAFE"'
    {
        line = $0
        if (substr(line, 1, 5) != "HEAD:" || !match(line, /:[0-9]+$/)) {
            printf "source-file-size lint: unexpected git grep output: %s\n", safe(line) > "/dev/stderr"
            bad = 1
            next
        }
        path = substr(line, 6, RSTART - 6)
        lines = substr(line, RSTART + 1)
        if (substr(path, 1, 1) == "\"") {
            printf "source-file-size lint: a path git quotes cannot be counted, rename it: %s\n", safe(path) > "/dev/stderr"
            bad = 1
            next
        }
        if (path !~ /^crates\/[^\/]+\/src\/.+\.rs$/) next
        n = split(path, part, "/")
        for (i = 1; i < n; i++) if (part[i] == "tests") next
        file = part[n]
        if (file == "tests.rs" || file == "test_support.rs") next
        if (file ~ /_tests\.rs$/ || file ~ /_test\.rs$/) next
        print path "\t" lines
    }
    END { exit bad }
    ' "$1"
}

# Validates the allow-list in $1 (messages carry the label $2) and prints one
# normalized "<path> <ceiling>" row per entry.
read_allowlist() {
    awk -v label="$2" "$AWK_SAFE"'
    /^[ \t]*#/ || /^[ \t]*$/ { next }
    {
        if (NF != 2 || $2 !~ /^[0-9]+$/ || $2 + 0 == 0) {
            printf "%s: malformed row at line %d (want \"<path> <ceiling>\"): %s\n", label, NR, safe($0) > "/dev/stderr"
            bad = 1
            next
        }
        if ($1 in seen) {
            printf "%s: duplicate row for %s at line %d\n", label, safe($1), NR > "/dev/stderr"
            bad = 1
            next
        }
        seen[$1] = 1
        print $1 " " $2
    }
    END { exit bad }
    ' "$1"
}

# Applies rules 1 to 3. $1 = counted files, $2 = allow-list rows, $3 = base
# branch rows (empty when the growth arm does not run).
compare_rows() {
    awk -v cap="$CAP" -v allowrel="$ALLOWLIST_REL" -v counts="$1" -v rows="$2" -v basefile="$3" "$AWK_SAFE"'
    function issue(msg) { print msg; issues++ }
    BEGIN {
        while ((getline line < counts) > 0) {
            split(line, f, "\t")
            npop++
            popath[npop] = f[1]
            count[f[1]] = f[2] + 0
        }
        close(counts)
        while ((getline line < rows) > 0) {
            split(line, f, " ")
            nrow++
            rowpath[nrow] = f[1]
            ceiling[f[1]] = f[2] + 0
        }
        close(rows)
        if (basefile != "") {
            while ((getline line < basefile) > 0) {
                split(line, f, " ")
                baseceiling[f[1]] = f[2] + 0
            }
            close(basefile)
        }
        for (i = 1; i <= npop; i++) {
            p = popath[i]
            n = count[p]
            if (n > cap && !(p in ceiling)) {
                issue(sprintf("%s: %d lines is over the %d-line cap and is not listed in %s; split the file (the allow-list takes no new rows)", safe(p), n, cap, allowrel))
            }
        }
        for (i = 1; i <= nrow; i++) {
            p = rowpath[i]
            if (!(p in count)) {
                issue(sprintf("%s: stale row, not a counted source file at HEAD; delete it from %s", safe(p), allowrel))
            } else if (count[p] <= cap) {
                issue(sprintf("%s: stale row, %d lines is at or under the %d-line cap; delete it from %s", safe(p), count[p], cap, allowrel))
            } else if (count[p] > ceiling[p]) {
                issue(sprintf("%s: %d lines is over its allow-list ceiling of %d; split the file (a ceiling cannot be raised)", safe(p), count[p], ceiling[p]))
            }
            if (basefile != "") {
                if (!(p in baseceiling)) {
                    issue(sprintf("%s: allow-list row added in this pull request; a row may only be lowered or deleted", safe(p)))
                } else if (ceiling[p] > baseceiling[p]) {
                    issue(sprintf("%s: allow-list ceiling raised from %d to %d in this pull request; a row may only be lowered or deleted", safe(p), baseceiling[p], ceiling[p]))
                }
            }
        }
        if (issues > 0) {
            printf "\nsource-file-size lint: %d issue(s)\n", issues
            exit 1
        }
        printf "source-file-size lint: %d file(s) counted, %d listed over the %d-line cap: OK\n", npop, nrow, cap
        exit 0
    }
    '
}

# Pull request arm. $1 = repo root, $2 = work directory. Leaves $2/base holding
# the base branch rows when the base branch has an allow-list.
pull_request_base() {
    pr_root="$1"
    pr_work="$2"
    # Only a shallow checkout can lack parents the merge commit really has.
    # `git fetch --depth` in a full clone would make it shallow, so a full clone
    # whose HEAD is not a merge commit goes straight to the refusal below.
    if ! git -C "$pr_root" rev-parse --verify -q 'HEAD^2^{commit}' > /dev/null 2>&1 \
        && [ "$(git -C "$pr_root" rev-parse --is-shallow-repository)" = true ]; then
        pr_head="$(git -C "$pr_root" rev-parse HEAD)" || return 1
        if ! git -C "$pr_root" fetch -q --no-tags --depth=2 origin "$pr_head"; then
            echo "source-file-size lint: could not fetch the parents of $pr_head from origin" >&2
        fi
    fi
    if ! git -C "$pr_root" rev-parse --verify -q 'HEAD^1^{commit}' > /dev/null 2>&1 \
        || ! git -C "$pr_root" rev-parse --verify -q 'HEAD^2^{commit}' > /dev/null 2>&1; then
        echo "source-file-size lint: pull request mode needs HEAD to be the pull request merge commit with both parents available; the base branch copy of $ALLOWLIST_REL cannot be read, and the growth arm is not skipped silently" >&2
        return 1
    fi
    if git -C "$pr_root" cat-file -e "HEAD^1:$ALLOWLIST_REL" 2> /dev/null; then
        if ! git -C "$pr_root" show "HEAD^1:$ALLOWLIST_REL" > "$pr_work/base.raw"; then
            echo "source-file-size lint: could not read $ALLOWLIST_REL from the base branch" >&2
            return 1
        fi
        if ! read_allowlist "$pr_work/base.raw" "base branch allow-list" > "$pr_work/base"; then
            return 1
        fi
    else
        echo "source-file-size lint: the base branch has no $ALLOWLIST_REL, so there is nothing to compare rows against (this change introduces it)"
    fi
    return 0
}

# Every step reports its own failure: the self-test calls this inside an `if`,
# where `set -e` does not apply.
check_tree_in() {
    root="$1"
    pr_mode="$2"
    work="$3"
    allowlist="$root/$ALLOWLIST_REL"

    if [ ! -f "$allowlist" ]; then
        echo "source-file-size lint: allow-list not found: $ALLOWLIST_REL" >&2
        return 1
    fi
    if ! git -C "$root" rev-parse --verify HEAD > /dev/null 2>&1; then
        echo "source-file-size lint: $root has no valid HEAD commit -- the committed tree is the population, refusing to guess" >&2
        return 1
    fi

    grep_rc=0
    git -C "$root" grep -c -a -e '' HEAD -- 'crates/*/src/*.rs' > "$work/grep.out" || grep_rc=$?
    if [ "$grep_rc" -eq 1 ]; then
        echo "source-file-size lint: no .rs files under crates/*/src at HEAD -- the check would silently be a no-op; fix the file-layout selection" >&2
        return 1
    elif [ "$grep_rc" -ne 0 ]; then
        echo "source-file-size lint: git grep over the HEAD tree failed (exit $grep_rc) -- refusing to count a partial listing" >&2
        return 1
    fi
    if ! count_lines "$work/grep.out" > "$work/counts"; then
        return 1
    fi
    if [ ! -s "$work/counts" ]; then
        echo "source-file-size lint: no counted source files -- the check would silently be a no-op; fix the population rule" >&2
        return 1
    fi
    if ! read_allowlist "$allowlist" "allow-list" > "$work/rows"; then
        return 1
    fi

    base_rows=""
    if [ "$pr_mode" = 1 ]; then
        if ! pull_request_base "$root" "$work"; then
            return 1
        fi
        if [ -f "$work/base" ]; then
            base_rows="$work/base"
        fi
    else
        echo "source-file-size lint: not a pull request, so the allow-list growth arm is skipped"
    fi

    compare_rows "$work/counts" "$work/rows" "$base_rows"
}

# $1 = repo root, $2 = 1 when the pull request arm applies.
check_tree() {
    work="$(mktemp -d)"
    tree_rc=0
    check_tree_in "$1" "$2" "$work" || tree_rc=$?
    rm -rf "$work"
    return "$tree_rc"
}

fixture_init() {
    mkdir -p "$1"
    (
        cd "$1" && git init -q . \
            && git config user.email "test@example.com" && git config user.name "test" \
            && git config commit.gpgsign false
    )
}

fixture_commit() {
    ( cd "$1" && git add -A && git commit -q --no-verify -m "$2" )
}

fixture_lines() {
    awk -v n="$1" 'BEGIN { for (i = 1; i <= n; i++) print "// line " i }'
}

# Rows arrive on stdin.
write_allowlist() {
    mkdir -p "$1/scripts"
    { printf '# fixture allow-list\n'; cat; } > "$1/$ALLOWLIST_REL"
}

# $1 = dir, $2 = "list" when the base commit already has an allow-list, $3 =
# line count of big.rs at the pull request head, $4 and $5 = head rows. The
# base has big.rs at CAP + 10 lines listed at CAP + 10. The head is merged with
# --no-ff, so HEAD^1 is the base and HEAD^2 is the pull request head.
pull_request_fixture() {
    pr_dir="$1"
    fixture_init "$pr_dir"
    mkdir -p "$pr_dir/crates/fixture/src" "$pr_dir/scripts"
    fixture_lines "$((CAP + 10))" > "$pr_dir/crates/fixture/src/big.rs"
    if [ "$2" = list ]; then
        printf '%s\n' "crates/fixture/src/big.rs $((CAP + 10))" > "$pr_dir/$ALLOWLIST_REL"
    fi
    fixture_commit "$pr_dir" base
    ( cd "$pr_dir" && git checkout -q -b pr-head )
    fixture_lines "$3" > "$pr_dir/crates/fixture/src/big.rs"
    if [ -n "$5" ]; then
        fixture_lines "$((CAP + 1))" > "$pr_dir/crates/fixture/src/new.rs"
    fi
    {
        printf '# fixture allow-list\n'
        if [ -n "$4" ]; then printf '%s\n' "$4"; fi
        if [ -n "$5" ]; then printf '%s\n' "$5"; fi
    } > "$pr_dir/$ALLOWLIST_REL"
    fixture_commit "$pr_dir" head
    ( cd "$pr_dir" && git checkout -q - && git merge -q --no-ff --no-verify -m merge pr-head )
}

# $1 = label, $2 = repo, $3 = pull request arm (0 or 1), $4 = pass or fail, $5
# = text the log must contain, $6 = extended regex the log must not match.
run_case() {
    case_label="$1"
    case_log="$tmp/$case_label.log"
    if check_tree "$2" "$3" > "$case_log" 2>&1; then
        case_got=pass
    else
        case_got=fail
    fi
    if [ "$case_got" != "$4" ]; then
        echo "self-test FAILED: $case_label should $4, got $case_got"
        cat "$case_log"
        status=1
        return 0
    fi
    if [ -n "$5" ] && ! grep -qF -e "$5" "$case_log"; then
        echo "self-test FAILED: $case_label output does not contain: $5"
        cat "$case_log"
        status=1
    fi
    if [ -n "$6" ] && grep -qE -e "$6" "$case_log"; then
        echo "self-test FAILED: $case_label output matches the forbidden pattern: $6"
        cat "$case_log"
        status=1
    fi
}

self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    status=0
    big=$((CAP + 10))

    # Population: one counted file over the cap, one exactly at the cap, and
    # every shape the population rule must leave out, each far over the cap.
    # The unlisted case is the known-positive control for that rule: its log
    # must name big.rs and none of the ignored files in the same run.
    repo="$tmp/population"
    fixture_init "$repo"
    mkdir -p "$repo/crates/fixture/src/tests" "$repo/crates/fixture/tests"
    fixture_lines "$big" > "$repo/crates/fixture/src/big.rs"
    fixture_lines "$CAP" > "$repo/crates/fixture/src/exact.rs"
    for ignored in src/tests.rs src/big_tests.rs src/big_test.rs src/test_support.rs src/tests/deep.rs tests/it.rs src/notes.txt; do
        fixture_lines "$((CAP + 50))" > "$repo/crates/fixture/$ignored"
    done
    fixture_commit "$repo" seed
    ignored_re='exact\.rs|tests\.rs|_test\.rs|test_support\.rs|src/tests/|fixture/tests/|notes\.txt'

    printf '' | write_allowlist "$repo"
    run_case unlisted-over-cap "$repo" 0 fail "crates/fixture/src/big.rs: $big lines is over the $CAP-line cap" "$ignored_re"

    printf '%s\n' "crates/fixture/src/big.rs $big" | write_allowlist "$repo"
    run_case listed-within-ceiling "$repo" 0 pass "1 listed over the $CAP-line cap: OK" ""

    printf '%s\n' "crates/fixture/src/big.rs $((CAP + 5))" | write_allowlist "$repo"
    run_case listed-over-ceiling "$repo" 0 fail "crates/fixture/src/big.rs: $big lines is over its allow-list ceiling of $((CAP + 5))" ""

    { printf '%s\n' "crates/fixture/src/big.rs $big"; printf '%s\n' "crates/fixture/src/exact.rs $big"; } | write_allowlist "$repo"
    run_case stale-row-at-cap "$repo" 0 fail "crates/fixture/src/exact.rs: stale row, $CAP lines is at or under" ""

    { printf '%s\n' "crates/fixture/src/big.rs $big"; printf '%s\n' "crates/fixture/src/gone.rs 4000"; } | write_allowlist "$repo"
    run_case stale-row-missing-file "$repo" 0 fail "crates/fixture/src/gone.rs: stale row, not a counted source file" ""

    printf '%s\n' "crates/fixture/src/big.rs lots" | write_allowlist "$repo"
    run_case malformed-row "$repo" 0 fail "malformed row at line 2" ""

    { printf '%s\n' "crates/fixture/src/big.rs $big"; printf '%s\n' "crates/fixture/src/big.rs $big"; } | write_allowlist "$repo"
    run_case duplicate-row "$repo" 0 fail "duplicate row for crates/fixture/src/big.rs" ""

    { printf '%s\n' "crates/fixture/src/big.rs $big"; printf '%s\n' "::error::forged 4000"; } | write_allowlist "$repo"
    run_case forged-annotation-row "$repo" 0 fail "stale row, not a counted source file" '^::'

    # Pull request arm: against the base branch copy, a row may be lowered or
    # deleted, never raised or added.
    pull_request_fixture "$tmp/pr-lowered" list "$((CAP + 5))" "crates/fixture/src/big.rs $((CAP + 5))" ""
    run_case pr-row-lowered "$tmp/pr-lowered" 1 pass "1 listed over the $CAP-line cap: OK" ""

    pull_request_fixture "$tmp/pr-deleted" list "$CAP" "" ""
    run_case pr-row-deleted "$tmp/pr-deleted" 1 pass "0 listed over the $CAP-line cap: OK" ""

    pull_request_fixture "$tmp/pr-raised" list "$((CAP + 20))" "crates/fixture/src/big.rs $((CAP + 20))" ""
    run_case pr-row-raised "$tmp/pr-raised" 1 fail "crates/fixture/src/big.rs: allow-list ceiling raised from $big to $((CAP + 20))" ""
    run_case local-row-raised "$tmp/pr-raised" 0 pass "growth arm is skipped" ""

    pull_request_fixture "$tmp/pr-added" list "$big" "crates/fixture/src/big.rs $big" "crates/fixture/src/new.rs $((CAP + 1))"
    run_case pr-row-added "$tmp/pr-added" 1 fail "crates/fixture/src/new.rs: allow-list row added" ""

    pull_request_fixture "$tmp/pr-introduced" none "$big" "crates/fixture/src/big.rs $big" ""
    run_case pr-allowlist-introduced "$tmp/pr-introduced" 1 pass "this change introduces it" ""

    # A single-commit tree has no parents to compare with, and no origin to
    # fetch them from: the pull request arm must fail, not skip.
    printf '%s\n' "crates/fixture/src/big.rs $big" | write_allowlist "$repo"
    run_case pr-without-parents "$repo" 1 fail "needs HEAD to be the pull request merge commit" ""

    # A full clone whose HEAD is not a merge commit must be refused without
    # fetching: `git fetch --depth` would turn that clone into a shallow one.
    fixture_init "$tmp/linear"
    mkdir -p "$tmp/linear/crates/fixture/src"
    fixture_lines "$big" > "$tmp/linear/crates/fixture/src/big.rs"
    printf '%s\n' "crates/fixture/src/big.rs $big" | write_allowlist "$tmp/linear"
    fixture_commit "$tmp/linear" first
    fixture_lines 3 > "$tmp/linear/crates/fixture/src/small.rs"
    fixture_commit "$tmp/linear" second
    git clone -q "file://$tmp/linear" "$tmp/linear-clone" > /dev/null 2>&1
    run_case pr-full-clone-without-merge "$tmp/linear-clone" 1 fail "needs HEAD to be the pull request merge commit" ""
    if [ "$(git -C "$tmp/linear-clone" rev-parse --is-shallow-repository)" != false ]; then
        echo "self-test FAILED: the pull request arm turned a full clone into a shallow one"
        status=1
    fi

    # A depth-1 clone of a merge commit hides both parents, as a pull request
    # checkout does. The fixture must really be shallow, or the case proves
    # nothing; the arm then fetches the parents from origin and passes.
    git clone -q --depth 1 "file://$tmp/pr-lowered" "$tmp/pr-shallow" > /dev/null 2>&1
    if git -C "$tmp/pr-shallow" rev-parse --verify -q 'HEAD^1^{commit}' > /dev/null 2>&1; then
        echo "self-test FAILED: the shallow fixture still has its parents, so it proves nothing"
        status=1
    else
        run_case pr-shallow-checkout "$tmp/pr-shallow" 1 pass "1 listed over the $CAP-line cap: OK" ""
    fi

    if [ "$status" -eq 0 ]; then
        echo "lint-source-file-size self-test: OK"
    fi
    return "$status"
}

case "${1:-}" in
    --self-test)
        self_test
        ;;
    "")
        if [ "${GITHUB_EVENT_NAME:-}" = "pull_request" ]; then
            check_tree "$ROOT" 1
        else
            check_tree "$ROOT" 0
        fi
        ;;
    *)
        echo "usage: $0 [--self-test]" >&2
        exit 2
        ;;
esac
