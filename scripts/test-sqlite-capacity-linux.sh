#!/bin/sh
set -eu

# Pass Cargo's exact compiled capacity_floor_real_fs test executable. Building
# stays outside the mount namespace and cannot consume the constrained device.
if [ "$(uname -s)" != Linux ]; then
    echo "SKIP ADR154_CAPACITY: requires Linux user and mount namespaces" >&2
    exit 77
fi
if [ "$#" -ne 1 ] || [ ! -x "$1" ]; then
    echo "usage: $0 /absolute/path/to/capacity_floor_real_fs-test-binary" >&2
    exit 2
fi
case "$1" in
    /*) capacity_binary=$1 ;;
    *) echo "test executable must be an absolute path" >&2; exit 2 ;;
esac
for capacity_command in unshare mount umount mktemp stat; do
    if ! command -v "$capacity_command" >/dev/null 2>&1; then
        echo "SKIP ADR154_CAPACITY: missing $capacity_command" >&2
        exit 77
    fi
done

capacity_receipts=$(mktemp -d /tmp/khive-capacity-run.XXXXXXXX)
cleanup_receipts() {
    rm -f "$capacity_receipts/entered" "$capacity_receipts/output"
    rmdir "$capacity_receipts"
}
trap cleanup_receipts EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

set +e
unshare --user --map-root-user --mount --propagation private /bin/sh -eu -c '
    capacity_receipts=$1
    capacity_binary=$2
    : > "$capacity_receipts/entered"
    capacity_mount=$capacity_receipts/mount
    mkdir "$capacity_mount"
    capacity_mounted=false
    cleanup_mount() {
        capacity_status=$?
        trap - EXIT
        if [ "$capacity_mounted" = true ]; then
            umount "$capacity_mount" || capacity_status=1
        fi
        rmdir "$capacity_mount" || capacity_status=1
        exit "$capacity_status"
    }
    trap cleanup_mount EXIT
    trap "exit 130" INT
    trap "exit 143" TERM
    if ! mount -t tmpfs -o size=256m,nosuid,nodev tmpfs "$capacity_mount"; then
        echo "SKIP ADR154_CAPACITY: cannot mount isolated bounded tmpfs" >&2
        exit 77
    fi
    capacity_mounted=true
    capacity_device=$(stat -c %d "$capacity_mount")
    for capacity_protected in / "$PWD" "${HOME:?HOME required for isolation proof}"; do
        if [ "$capacity_device" = "$(stat -c %d "$capacity_protected")" ]; then
            echo "SKIP ADR154_CAPACITY: constrained device is a protected filesystem" >&2
            exit 77
        fi
    done
    export KHIVE_TEST_CONSTRAINED_MOUNT="$capacity_mount"
    "$capacity_binary" --exact refuses_before_sqlite_full_with_old_reader_and_recoverable_reserve \
        --ignored --nocapture --test-threads=1
' khive-capacity "$capacity_receipts" "$capacity_binary" >"$capacity_receipts/output" 2>&1
capacity_status=$?
set -e
cat "$capacity_receipts/output"
if [ ! -f "$capacity_receipts/entered" ]; then
    echo "SKIP ADR154_CAPACITY: isolated namespace unavailable; no mount or test ran" >&2
    exit 77
fi
if [ "$capacity_status" -ne 0 ]; then
    exit "$capacity_status"
fi
if ! grep -qx ADR154_CAPACITY_PASS "$capacity_receipts/output"; then
    echo "SKIP ADR154_CAPACITY: isolated-device acceptance did not complete" >&2
    exit 77
fi
