#!/bin/sh
set -eu

if [ "$#" -eq 0 ]; then
    echo "usage: ci-contract-harness.sh <command> [args...]" >&2
    exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONTRACT_PROJECT="$SCRIPT_DIR/../tests/khive-contract"
cd "$CONTRACT_PROJECT"

# Keep dependency downloads separate from test execution. uv's own retries
# cover each request; these three setup attempts also cover a transient outage
# that outlasts one request's retry window. A stale lock or other setup error
# still refuses after the same bounded number of attempts.
echo "=== Contract Harness Dependency Setup ==="
attempt=1
while :; do
    if uv sync --project "$CONTRACT_PROJECT" --locked; then
        break
    else
        setup_status=$?
    fi
    if [ "$attempt" -eq 3 ]; then
        echo "Contract harness dependency download/environment setup failed after 3 attempts; no harness test was run." >&2
        exit "$setup_status"
    fi
    delay=$((attempt * 2))
    echo "Contract harness dependency setup failed (attempt $attempt/3); retrying in ${delay}s." >&2
    sleep "$delay"
    attempt=$((attempt + 1))
done

# The successful sync installed the locked environment. Tests must neither
# download again nor retry: a genuine test failure keeps its original status.
echo "=== Contract Harness Execution ==="
exec uv run --project "$CONTRACT_PROJECT" --offline --no-sync "$@"
