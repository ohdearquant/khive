#!/usr/bin/env bash
# Sandboxed test suite for the ADR-100 backup tooling. NO real databases, NO
# real launchctl, NO real ssh — everything runs against a temp-dir sandbox
# with tiny sqlite3-created fixture DBs and stub sqlite3_rsync/ssh/scp/
# kkernel/launchctl binaries prepended onto PATH. Every test prints
# PASS/FAIL; the suite exits non-zero on any failure.
#
# Usage: deploy/backup/test_backup.sh

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
KHIVE_BACKUP_SH="${SCRIPT_DIR}/khive-backup.sh"
RESTORE_DRILL_SH="${SCRIPT_DIR}/restore-drill.sh"
INSTALL_SH="${SCRIPT_DIR}/install-backup.sh"

PASS=0
FAIL=0

ok() {
  PASS=$((PASS + 1))
  printf '[test_backup] PASS: %s\n' "$1"
}
fail() {
  FAIL=$((FAIL + 1))
  printf '[test_backup] FAIL: %s\n' "$1" >&2
}
assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "${expected}" = "${actual}" ]; then ok "${desc}"; else fail "${desc} (expected '${expected}', got '${actual}')"; fi
}
assert_contains() {
  local desc="$1" haystack="$2" needle="$3"
  case "${haystack}" in *"${needle}"*) ok "${desc}" ;; *) fail "${desc} (expected to find '${needle}' in: ${haystack})" ;; esac
}
assert_not_contains() {
  local desc="$1" haystack="$2" needle="$3"
  case "${haystack}" in *"${needle}"*) fail "${desc} (did NOT expect to find '${needle}' in: ${haystack})" ;; *) ok "${desc}" ;; esac
}
assert_file_exists() {
  local desc="$1" path="$2"
  if [ -f "${path}" ]; then ok "${desc}"; else fail "${desc} (missing: ${path})"; fi
}

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/khive-backup-test.XXXXXX")"
trap 'rm -rf "${SANDBOX}"' EXIT

STUB_BIN="${SANDBOX}/stub-bin"
mkdir -p "${STUB_BIN}"
export PATH="${STUB_BIN}:${PATH}"
export HOME="${SANDBOX}/home"
mkdir -p "${HOME}"
export KHIVE_BACKUP_ROOT="${SANDBOX}/backups"
export KHIVE_BACKUP_LOCAL_VERSION="3.53.3"
export KHIVE_BACKUP_REMOTE_SQLITE3_RSYNC="sqlite3_rsync"

# --- fixture databases (real sqlite3, no real product DBs) -------------

FIXTURE_DIR="${SANDBOX}/fixtures"
mkdir -p "${FIXTURE_DIR}"
ORIGIN_DB="${FIXTURE_DIR}/origin.db"

make_fixture_db() {
  local db="$1"
  rm -f "${db}"
  sqlite3 "${db}" <<'SQL'
CREATE TABLE notes (id TEXT PRIMARY KEY, content TEXT);
INSERT INTO notes (id, content) VALUES ('marker-1', 'hello world');
INSERT INTO notes (id, content) VALUES ('n2', 'second row');
SQL
}
make_fixture_db "${ORIGIN_DB}"

CONF="${SANDBOX}/stores.conf"

write_conf() {
  local t1="$1" t2="$2" t3="$3"
  cat >"${CONF}" <<EOF
fixture|${ORIGIN_DB}|${t1}|${t2}|${t3}
EOF
}

T1_REPLICA="${SANDBOX}/replica/fixture.db"
T3_ARCHIVE="${SANDBOX}/archive/fixture"
mkdir -p "$(dirname "${T1_REPLICA}")" "${T3_ARCHIVE}"
write_conf "${T1_REPLICA}" "CHANGE_ME@backup-host:/backups/fixture.db" "${T3_ARCHIVE}"
export KHIVE_BACKUP_CONF="${CONF}"

# --- stub external tools -------------------------------------------------
# A real sqlite3_rsync/ssh/scp/kkernel/launchctl is never invoked by this
# suite. Stubs below let khive-backup.sh's control flow (locking, timeout,
# preflight, staged promote, retention, alerting) be exercised without a
# second host or the real binaries.

write_stub() {
  local name="$1" body="$2"
  cat >"${STUB_BIN}/${name}" <<EOF
#!/usr/bin/env bash
${body}
EOF
  chmod +x "${STUB_BIN}/${name}"
}

# Default: a well-behaved sqlite3_rsync stub that just copies origin over
# the replica argument (good enough to exercise promote/integrity-check
# control flow; the real binary's diff algorithm is out of scope here).
write_stub sqlite3_rsync '
if [ "$1" = "--version" ]; then
  echo "2026-06-26 20:14:12 deadbeef"
  exit 0
fi
origin="$1"
replica="$2"
case "$replica" in
  *:*)
    # user@host:path form is not exercised by the local-tier stub path.
    exit 0
    ;;
esac
cp "$origin" "$replica"
exit 0
'

KKERNEL_CALLS_LOG="${SANDBOX}/kkernel-calls.log"
: >"${KKERNEL_CALLS_LOG}"
write_stub kkernel "
echo \"\$*\" >> '${KKERNEL_CALLS_LOG}'
exit 0
"

write_stub ssh 'exit 0'
write_stub scp 'exit 1'

echo "=== test: version preflight refuses a floor violation ==="
(
  KHIVE_BACKUP_LOCAL_VERSION="3.49.0" "${KHIVE_BACKUP_SH}" t1 fixture
) >/tmp/khive-test-ver.out 2>&1 && fail "t1 sync should refuse a below-floor local version" \
  || ok "t1 sync refuses a below-floor local version"
assert_contains "failure log recorded version-preflight-failed" \
  "$(cat "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl" 2>/dev/null || true)" "version-preflight-failed"
rm -f "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl"

echo "=== test: tier-1 sync succeeds, promotes replica, logs success ==="
"${KHIVE_BACKUP_SH}" t1 fixture
assert_file_exists "t1 replica exists after a successful sync" "${T1_REPLICA}"
assert_contains "success row logged for t1" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" '"outcome":"success"'
assert_contains "success row records tier t1" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" '"tier":"t1"'

echo "=== test: lock exclusion — a second invocation for the same store fails fast ==="
mkdir -p "${KHIVE_BACKUP_ROOT}/lock"
LOCKDIR="${KHIVE_BACKUP_ROOT}/lock/fixture.lockdir"
mkdir -p "${LOCKDIR}"
echo "999999" >"${LOCKDIR}/pid" # a pid almost certainly not alive... but we need a LIVE holder to test true exclusion
echo "$$" >"${LOCKDIR}/pid" # this shell's own pid IS alive, simulating a genuinely running holder
if "${KHIVE_BACKUP_SH}" t1 fixture >/tmp/khive-test-lock.out 2>&1; then
  fail "second invocation should fail fast while the lock is held"
else
  ok "second invocation exits non-zero while the lock is held"
fi
assert_contains "lock-held message mentions no queueing" "$(cat /tmp/khive-test-lock.out)" "no queueing"
rm -rf "${LOCKDIR}"

echo "=== test: stale lock (dead pid) is reclaimed rather than blocking forever ==="
mkdir -p "${LOCKDIR}"
echo "999999" >"${LOCKDIR}/pid" # a pid that should not be alive on any normal box
"${KHIVE_BACKUP_SH}" t1 fixture >/tmp/khive-test-stale.out 2>&1 \
  && ok "sync proceeds past a stale (dead-pid) lock" \
  || fail "sync should reclaim a stale lock and proceed: $(cat /tmp/khive-test-stale.out)"

echo "=== test: timeout kill path records a failure JSONL row ==="
write_stub sqlite3_rsync '
if [ "$1" = "--version" ]; then
  echo "2026-06-26 20:14:12 deadbeef"
  exit 0
fi
sleep 5
'
if KHIVE_BACKUP_TIMEOUT_T1=1 "${KHIVE_BACKUP_SH}" t1 fixture >/tmp/khive-test-timeout.out 2>&1; then
  fail "sync exceeding the timeout should exit non-zero"
else
  ok "sync exceeding the timeout exits non-zero"
fi
assert_contains "timeout outcome logged" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" '"outcome":"timeout"'
# restore the well-behaved stub for subsequent tests
write_stub sqlite3_rsync '
if [ "$1" = "--version" ]; then
  echo "2026-06-26 20:14:12 deadbeef"
  exit 0
fi
origin="$1"
replica="$2"
case "$replica" in *:*) exit 0 ;; esac
cp "$origin" "$replica"
exit 0
'

echo "=== test: disk preflight refusal ==="
TINY_DIR="${SANDBOX}/tiny-dest"
mkdir -p "${TINY_DIR}"
write_conf "${TINY_DIR}/fixture.db" "CHANGE_ME@backup-host:/backups/fixture.db" "${T3_ARCHIVE}"
if KHIVE_BACKUP_MARGIN_BYTES=999999999999999 "${KHIVE_BACKUP_SH}" t1 fixture >/tmp/khive-test-disk.out 2>&1; then
  fail "disk preflight should refuse an impossible margin"
else
  ok "disk preflight refuses an impossible margin"
fi
assert_contains "disk-preflight-failed logged" "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "disk-preflight-failed"
write_conf "${T1_REPLICA}" "CHANGE_ME@backup-host:/backups/fixture.db" "${T3_ARCHIVE}"

echo "=== test: t2 refuses on the CHANGE_ME placeholder ==="
if "${KHIVE_BACKUP_SH}" t2 fixture >/tmp/khive-test-t2-placeholder.out 2>&1; then
  fail "t2 sync should refuse while t2_remote is CHANGE_ME"
else
  ok "t2 sync refuses while t2_remote is CHANGE_ME"
fi
assert_contains "not-configured outcome logged" "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "not-configured"

echo "=== test: t3 staged promote + retention prune ==="
# No sqlite3 stub here: STUB_BIN does not shadow it, so PATH resolves the
# real system sqlite3 for VACUUM INTO / PRAGMA quick_check, producing
# genuine SQLite archive files for the retention/prune logic to operate on.
for i in 1 2 3 4 5 6; do
  "${KHIVE_BACKUP_SH}" t3 fixture >/tmp/khive-test-t3-"$i".out 2>&1 || fail "t3 archive run ${i} failed: $(cat /tmp/khive-test-t3-"$i".out)"
  sleep 1.1
done
ARCHIVE_COUNT="$(find "${T3_ARCHIVE}" -maxdepth 1 -name 'fixture-*.db' | wc -l | tr -d ' ')"
assert_eq "retention keeps exactly KHIVE_BACKUP_RETENTION_LOCAL (default 4) archives" "4" "${ARCHIVE_COUNT}"
assert_contains "t3 success rows logged" "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" '"tier":"t3"'

echo "=== test: t3 off-host archive copy failure logs a distinct event + alerts (local archive still promoted) ==="
# A real (non-placeholder) t2_remote so run_tier3 attempts the off-host
# scp copy; the scp stub above always fails, exercising the degraded path.
write_conf "${T1_REPLICA}" "op@remotehost:/backups/fixture.db" "${T3_ARCHIVE}"
: >"${KKERNEL_CALLS_LOG}"
if "${KHIVE_BACKUP_SH}" t3 fixture >/tmp/khive-test-t3-offhost.out 2>&1; then
  T3_OFFHOST_RC=0
else
  T3_OFFHOST_RC=$?
fi
assert_eq "t3 still exits 0 when only the off-host copy fails (local tier succeeded)" "0" "${T3_OFFHOST_RC}"
LAST_TWO_EVENTS="$(tail -n2 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")"
assert_contains "offhost-copy-failed event logged" "${LAST_TWO_EVENTS}" '"outcome":"offhost-copy-failed"'
assert_contains "offhost-copy-failed event still tagged tier t3" "${LAST_TWO_EVENTS}" '"tier":"t3"'
assert_contains "local archive still promoted (success row also logged)" "${LAST_TWO_EVENTS}" '"outcome":"success"'
assert_contains "alert (kkernel comm.send) invoked for the off-host failure" "$(cat "${KKERNEL_CALLS_LOG}")" "offhost-copy-failed"
# restore the CHANGE_ME placeholder for the remaining tests in this suite
write_conf "${T1_REPLICA}" "CHANGE_ME@backup-host:/backups/fixture.db" "${T3_ARCHIVE}"

echo "=== test: restore drill — capture (before the designated sync) then validate exact-equality pass ==="
# ADR order: capture the manifest from the ORIGIN first, THEN run the
# designated sync, THEN validate the resulting backup against the RECORDED
# manifest file — never a manifest recaptured from the origin at validate
# time.
DRILL_MANIFEST="${SANDBOX}/drill-manifest-pass.txt"
if "${RESTORE_DRILL_SH}" capture fixture marker-1 "${DRILL_MANIFEST}" >/tmp/khive-test-drill-capture.out 2>&1; then
  ok "capture succeeds and verifies the marker in the origin"
else
  fail "capture should succeed: $(cat /tmp/khive-test-drill-capture.out)"
fi
assert_file_exists "capture writes the manifest file" "${DRILL_MANIFEST}"
assert_contains "capture prints MANIFEST_PATH" "$(cat /tmp/khive-test-drill-capture.out)" "MANIFEST_PATH=${DRILL_MANIFEST}"
assert_contains "manifest contains a COUNT row" "$(cat "${DRILL_MANIFEST}")" "COUNT|notes|"

# the designated sync, run AFTER capture (origin is unchanged since capture,
# so the resulting replica matches the recorded manifest exactly)
"${KHIVE_BACKUP_SH}" t1 fixture >/dev/null

DRILL_OUT="$(mktemp -d "${SANDBOX}/drill.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${DRILL_MANIFEST}" "${DRILL_OUT}" >/tmp/khive-test-drill-pass.out 2>&1; then
  ok "validate passes when the backup matches the recorded manifest"
else
  fail "validate should pass on an exact backup copy: $(cat /tmp/khive-test-drill-pass.out)"
fi
assert_contains "validate reports RTO_SECONDS" "$(cat /tmp/khive-test-drill-pass.out)" "RTO_SECONDS="

echo "=== test: restore drill — deliberate mismatch (mutated AFTER capture+sync) fails ==="
# Same recorded manifest as above (captured before the sync); the backup fed
# to validate is mutated AFTER both capture and the sync ran, simulating a
# tampered/corrupted backup file rather than a spurious "moving origin" diff.
DRILL_BAD_DB="${SANDBOX}/drill-backup-bad.db"
cp "${T1_REPLICA}" "${DRILL_BAD_DB}"
sqlite3 "${DRILL_BAD_DB}" "INSERT INTO notes (id, content) VALUES ('extra-row', 'this should not be here');"
DRILL_BAD_OUT="$(mktemp -d "${SANDBOX}/drill-bad.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${DRILL_BAD_DB}" marker-1 "${DRILL_MANIFEST}" "${DRILL_BAD_OUT}" >/tmp/khive-test-drill-fail.out 2>&1; then
  fail "validate should fail on a deliberately mismatched copy"
else
  ok "validate fails on a deliberately mismatched copy"
fi
assert_contains "mismatch failure names the manifest diff" "$(cat /tmp/khive-test-drill-fail.out)" "manifest"

echo "=== test: restore drill fails when the marker row is absent from the restored copy ==="
DRILL_NOMARKER_DB="${SANDBOX}/drill-backup-nomarker.db"
cp "${T1_REPLICA}" "${DRILL_NOMARKER_DB}"
sqlite3 "${DRILL_NOMARKER_DB}" "DELETE FROM notes WHERE id = 'marker-1';"
DRILL_NM_OUT="$(mktemp -d "${SANDBOX}/drill-nm.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${DRILL_NOMARKER_DB}" marker-1 "${DRILL_MANIFEST}" "${DRILL_NM_OUT}" >/tmp/khive-test-drill-nomarker.out 2>&1; then
  fail "validate should fail when the marker row is missing"
else
  ok "validate fails when the marker row is missing from the restored copy"
fi

echo "=== test: validate refuses a missing manifest file ==="
DRILL_MISSING_OUT="$(mktemp -d "${SANDBOX}/drill-missing.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${SANDBOX}/does-not-exist.manifest" "${DRILL_MISSING_OUT}" >/tmp/khive-test-drill-missing.out 2>&1; then
  fail "validate should refuse a missing manifest file"
else
  ok "validate refuses a missing manifest file"
fi
assert_contains "missing-manifest error names the file" "$(cat /tmp/khive-test-drill-missing.out)" "manifest file not found"

echo "=== test: validate refuses a malformed manifest file ==="
BAD_MANIFEST="${SANDBOX}/malformed.manifest"
printf 'this is not a manifest line\n' >"${BAD_MANIFEST}"
DRILL_MALFORMED_OUT="$(mktemp -d "${SANDBOX}/drill-malformed.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${BAD_MANIFEST}" "${DRILL_MALFORMED_OUT}" >/tmp/khive-test-drill-malformed.out 2>&1; then
  fail "validate should refuse a malformed manifest file"
else
  ok "validate refuses a malformed manifest file"
fi
assert_contains "malformed-manifest error names the problem" "$(cat /tmp/khive-test-drill-malformed.out)" "malformed"

echo "=== test: installer idempotent re-run preserves edited interval ==="
export KHIVE_BACKUP_INSTALL_TEST=1
export KHIVE_BACKUP_SCRIPT_PATH="${KHIVE_BACKUP_SH}"
"${INSTALL_SH}" install t1 fixture --interval 300 >/tmp/khive-test-install1.out 2>&1 \
  || fail "initial install should succeed: $(cat /tmp/khive-test-install1.out)"
PLIST_DEST="${HOME}/Library/LaunchAgents/com.khive.backup.t1.fixture.plist"
assert_file_exists "plist installed after install" "${PLIST_DEST}"
V1="$(/usr/libexec/PlistBuddy -c 'Print :StartInterval' "${PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "installed interval matches the --interval flag" "300" "${V1}"

"${INSTALL_SH}" install t1 fixture >/tmp/khive-test-install2.out 2>&1 \
  || fail "rerun with no --interval should succeed: $(cat /tmp/khive-test-install2.out)"
V2="$(/usr/libexec/PlistBuddy -c 'Print :StartInterval' "${PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "rerun with no override preserves the previously-set interval" "${V1}" "${V2}"

"${INSTALL_SH}" install t1 fixture --interval 120 >/tmp/khive-test-install3.out 2>&1 \
  || fail "rerun with a new --interval should succeed: $(cat /tmp/khive-test-install3.out)"
V3="$(/usr/libexec/PlistBuddy -c 'Print :StartInterval' "${PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "rerun with a new --interval updates it" "120" "${V3}"

echo "=== test: installer status output contains no secrets ==="
STATUS_OUT="$("${INSTALL_SH}" status t1 fixture 2>&1)"
assert_not_contains "status output contains no CHANGE_ME remote credential leakage" "${STATUS_OUT}" "backup-host-password"
assert_contains "status output reports the plist path" "${STATUS_OUT}" "com.khive.backup.t1.fixture.plist"

echo "=== test: uninstall removes the plist ==="
"${INSTALL_SH}" uninstall t1 fixture >/tmp/khive-test-uninstall.out 2>&1 \
  || fail "uninstall should succeed: $(cat /tmp/khive-test-uninstall.out)"
if [ -f "${PLIST_DEST}" ]; then
  fail "plist should be removed after uninstall"
else
  ok "plist removed after uninstall"
fi

echo
echo "[test_backup.sh] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]
