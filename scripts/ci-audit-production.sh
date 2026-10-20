#!/bin/sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
case "${TEST_SCOPE:-full}" in
    full) ;;
    *)
        case " $TEST_SCOPE " in
            *" khive-runtime "*|*" khive-runtime-production-tests "*) ;;
            *) echo "scope: ordinary audit-batch tests skipped"; exit 0 ;;
        esac
        ;;
esac

cd "$SCRIPT_DIR/../crates"
python3 "$SCRIPT_DIR/tests/test_audit_production_artifacts.py"
log=$(mktemp "${TMPDIR:-/tmp}/audit-production.XXXXXX")
trap 'rm -f "$log"' 0
# This must be a separate Cargo invocation: workspace tests enable the
# runtime's self dev-dependency and cannot establish its production behavior.
if cargo test --locked -p khive-runtime-production-tests --test audit_batch \
    --message-format=json > "$log"; then
    cat "$log"
else
    cat "$log"
    exit 1
fi
python3 "$SCRIPT_DIR/check-audit-production-artifacts.py" "$log"
