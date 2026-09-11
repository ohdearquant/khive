#!/usr/bin/env bash
# Cargo succeeds on an empty selection. This gate checks selection, not test success.
set -uo pipefail
set +e

if [[ $# != 2 ]]; then
    echo "usage: assert-tests-ran.sh <captured-log> <positive-floor>" >&2
    exit 2
fi

log=$1
floor=$2
if [[ ! "$floor" =~ ^[1-9][0-9]*$ ]] || [[ ${#floor} -gt 10 ]] || [[ "$floor" -gt 2147483647 ]]; then
    printf 'test selection: counted=unavailable floor=%s log=%s; floor must be 1..2147483647\n' "$floor" "$log" >&2
    exit 2
fi
if [[ ! -f "$log" || ! -r "$log" ]]; then
    printf 'test selection: counted=unavailable floor=%s log=%s; log is missing or unreadable\n' "$floor" "$log" >&2
    exit 2
fi

count=$(LC_ALL=C awk '
    BEGIN { total = 0; invalid = 0 }
    {
        line = $0
        gsub(/\033\[[0-9;]*m/, "", line)
        sub(/\r$/, "", line)
        # A Rust test path can begin with result::; it is not a summary.
        if (line !~ /^[[:space:]]*test result:([^:]|$)/) next
        # The whole cargo summary grammar, so a truncated line or trailing text is malformed, not counted.
        if (line !~ /^[[:space:]]*test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed; [0-9]+ ignored; [0-9]+ measured; [0-9]+ filtered out(; finished in [0-9]+(\.[0-9]+)?s)?[[:space:]]*$/) {
            printf "malformed test summary at line %d\n", NR > "/dev/stderr"
            invalid = 1
            next
        }
        sub(/^[[:space:]]*test result: (ok|FAILED)\. /, "", line)
        split(line, fields, ";")
        passed = fields[1]
        failed = fields[2]
        sub(/ passed$/, "", passed)
        sub(/^ /, "", failed)
        sub(/ failed$/, "", failed)
        passed += 0
        failed += 0
        # Keep accumulation exact on every supported awk implementation.
        if (passed > 2147483647 || failed > 2147483647 || total + passed + failed > 2147483647) {
            printf "test count exceeds supported range at line %d\n", NR > "/dev/stderr"
            invalid = 1
            next
        }
        total += passed + failed
    }
    END {
        if (invalid) exit 2
        printf "%.0f\n", total
    }
' < "$log")
parse_status=$?
if [[ $parse_status != 0 ]]; then
    printf 'test selection: counted=unavailable floor=%s log=%s; parser exited %s\n' "$floor" "$log" "$parse_status" >&2
    exit 2
fi
if [[ ! "$count" =~ ^(0|[1-9][0-9]*)$ ]] || [[ ${#count} -gt 10 ]] || [[ "$count" -gt 2147483647 ]]; then
    printf 'test selection: counted=unavailable floor=%s log=%s; invalid parser output\n' "$floor" "$log" >&2
    exit 2
fi

printf 'test selection: counted=%s floor=%s log=%s\n' "$count" "$floor" "$log"
if [[ "$count" -lt "$floor" ]]; then
    echo "test selection is below the required floor" >&2
    exit 1
fi
