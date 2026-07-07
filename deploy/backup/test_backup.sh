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

# Every ssh call is logged (raw args) so a test can assert a given remote
# command was never sent — e.g. "the seed-staging call never ran because the
# preflight refused first". The stub also answers the two remote probes
# khive-backup.sh/lib.sh send over ssh: the sqlite3_rsync --version relay
# (matched on the literal "VERSION=" token the real command embeds) and the
# `df -Pk` free-space probe — both tunable per-test via env vars so a single
# stub covers the matching-version/plenty-of-space default AND the
# mismatch/insufficient-space test cases. Every other remote command (rm,
# mkdir, mv, prune) falls through to a bare `exit 0`, same as before.
export SSH_CALLS_LOG="${SANDBOX}/ssh-calls.log"
: >"${SSH_CALLS_LOG}"
write_stub ssh '
printf "%s\n" "$*" >> "${SSH_CALLS_LOG}"
shift 4
host="$1"
shift
cmd="$1"
if printf "%s" "$cmd" | grep -q "VERSION="; then
  echo "VERSION=${SSH_STUB_REMOTE_VERSION:-3.53.3}"
elif printf "%s" "$cmd" | grep -q "df -Pk"; then
  if [ -n "${SSH_STUB_DISK_FAIL:-}" ]; then
    printf "Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/disk1 1000000 999999 1 100%% /\n"
  else
    printf "Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/disk1 1000000000 1000 999999000 1%% /\n"
  fi
fi
exit 0
'
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

echo "=== test: t2 refuses on a local/remote sqlite3_rsync version mismatch ==="
write_conf "${T1_REPLICA}" "op@remotehost:/backups/fixture.db" "${T3_ARCHIVE}"
: >"${SSH_CALLS_LOG}"
if SSH_STUB_REMOTE_VERSION="3.50.1" "${KHIVE_BACKUP_SH}" t2 fixture >/tmp/khive-test-t2-vermismatch.out 2>&1; then
  fail "t2 sync should refuse a local/remote sqlite3_rsync version mismatch"
else
  ok "t2 sync refuses a local/remote sqlite3_rsync version mismatch"
fi
assert_contains "version-preflight-failed logged for the mismatch" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "version-preflight-failed"
assert_contains "mismatch failure names the local version" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "local=3.53.3"
assert_contains "mismatch failure names the mismatched remote version" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "remote (op@remotehost)=3.50.1"
assert_not_contains "remote staging was never touched when the version preflight refuses" \
  "$(cat "${SSH_CALLS_LOG}")" "staging"

echo "=== test: t2 proceeds past the version preflight when local and remote versions match ==="
: >"${SSH_CALLS_LOG}"
if SSH_STUB_REMOTE_VERSION="3.53.3" "${KHIVE_BACKUP_SH}" t2 fixture >/tmp/khive-test-t2-vermatch.out 2>&1; then
  ok "t2 sync proceeds past the version preflight on a matching local/remote pair"
else
  fail "t2 sync should proceed on a matching version pair: $(cat /tmp/khive-test-t2-vermatch.out)"
fi
assert_contains "success row logged for t2 on a matching version pair" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" '"tier":"t2"'
assert_contains "remote staging WAS touched once the preflights pass" \
  "$(cat "${SSH_CALLS_LOG}")" "staging"

echo "=== test: t2 refuses when the remote disk preflight reports insufficient space ==="
: >"${SSH_CALLS_LOG}"
if SSH_STUB_REMOTE_VERSION="3.53.3" SSH_STUB_DISK_FAIL=1 "${KHIVE_BACKUP_SH}" t2 fixture >/tmp/khive-test-t2-diskfail.out 2>&1; then
  fail "t2 sync should refuse when the remote disk preflight reports insufficient space"
else
  ok "t2 sync refuses when the remote disk preflight reports insufficient space"
fi
assert_contains "disk-preflight-failed logged for the remote target" \
  "$(tail -n1 "${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl")" "disk-preflight-failed"
assert_not_contains "remote staging was never touched when the disk preflight refuses" \
  "$(cat "${SSH_CALLS_LOG}")" "staging"
# restore the CHANGE_ME placeholder for the remaining tests in this suite
write_conf "${T1_REPLICA}" "CHANGE_ME@backup-host:/backups/fixture.db" "${T3_ARCHIVE}"

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

echo "=== test: restore drill — capture-replica happy path (marker verified in the replica, not the origin) ==="
# Routine drill flow: the designated sync already ran above (T1_REPLICA is
# in sync with ORIGIN_DB, both carrying marker-1), so capture-replica can
# verify the marker in the replica and capture straight from it.
CR_MANIFEST="${SANDBOX}/drill-manifest-capture-replica.txt"
if "${RESTORE_DRILL_SH}" capture-replica fixture marker-1 "${CR_MANIFEST}" >/tmp/khive-test-drill-cr-capture.out 2>&1; then
  ok "capture-replica succeeds and verifies the marker in the replica"
else
  fail "capture-replica should succeed: $(cat /tmp/khive-test-drill-cr-capture.out)"
fi
assert_file_exists "capture-replica writes the manifest file" "${CR_MANIFEST}"
assert_contains "capture-replica prints MANIFEST_PATH" "$(cat /tmp/khive-test-drill-cr-capture.out)" "MANIFEST_PATH=${CR_MANIFEST}"
assert_contains "capture-replica manifest contains a COUNT row" "$(cat "${CR_MANIFEST}")" "COUNT|notes|"

# Feed the replica-captured manifest straight into validate against that same
# replica — the full routine drill flow (marker -> sync -> capture-replica ->
# validate), deterministic end-to-end on a live store.
CR_VALIDATE_OUT="$(mktemp -d "${SANDBOX}/drill-cr-validate.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${CR_MANIFEST}" "${CR_VALIDATE_OUT}" >/tmp/khive-test-drill-cr-validate.out 2>&1; then
  ok "validate passes end-to-end against a capture-replica manifest (routine drill flow)"
else
  fail "validate should pass against a capture-replica manifest: $(cat /tmp/khive-test-drill-cr-validate.out)"
fi

echo "=== test: restore drill — a passing validate with no explicit scratch-dir removes its own scratch dir too ==="
CR_SUCCESS_STDOUT="$(mktemp "${SANDBOX}/drill-cr-success-stdout.XXXXXX")"
if "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${CR_MANIFEST}" >"${CR_SUCCESS_STDOUT}" 2>&1; then
  ok "validate passes with no explicit scratch-dir argument"
else
  fail "validate should pass: $(cat "${CR_SUCCESS_STDOUT}")"
fi
CR_SUCCESS_SCRATCH_DIR="$(sed -n 's/.*scratch=\(.*\)$/\1/p' "${CR_SUCCESS_STDOUT}" | head -1)"
if [ -z "${CR_SUCCESS_SCRATCH_DIR}" ]; then
  fail "could not parse the auto-created scratch dir from validate's own log line"
elif [ -d "${CR_SUCCESS_SCRATCH_DIR}" ]; then
  fail "scratch dir should be removed after a passing validate run with no explicit scratch-dir argument"
else
  ok "scratch dir is removed after a passing validate run with no explicit scratch-dir argument too"
fi

echo "=== test: restore drill — capture-replica refuses when the marker has not yet reached the replica ==="
sqlite3 "${ORIGIN_DB}" "INSERT INTO notes (id, content) VALUES ('marker-2', 'not yet synced');"
CR_MISSING_MANIFEST="${SANDBOX}/drill-manifest-capture-replica-missing.txt"
if "${RESTORE_DRILL_SH}" capture-replica fixture marker-2 "${CR_MISSING_MANIFEST}" >/tmp/khive-test-drill-cr-missing.out 2>&1; then
  fail "capture-replica should refuse when the marker has not synced to the replica yet"
else
  ok "capture-replica refuses when the marker is absent from the replica"
fi
assert_contains "capture-replica absent-marker error explains the sync ordering" \
  "$(cat /tmp/khive-test-drill-cr-missing.out)" "designated sync has not yet carried it forward"
[ -f "${CR_MISSING_MANIFEST}" ] && fail "capture-replica must not write a manifest file when the marker check fails" \
  || ok "capture-replica writes no manifest file when the marker check fails"
# carry marker-2 to the replica so later store state is consistent for any
# subsequent test in this file that reads T1_REPLICA.
"${KHIVE_BACKUP_SH}" t1 fixture >/dev/null

echo "=== test: restore drill — cleanup-on-fail removes the scratch dir but preserves diff evidence ==="
CLEANUP_BAD_DB="${SANDBOX}/drill-cleanup-bad.db"
cp "${T1_REPLICA}" "${CLEANUP_BAD_DB}"
sqlite3 "${CLEANUP_BAD_DB}" "INSERT INTO notes (id, content) VALUES ('cleanup-extra-row', 'should not be here');"
CLEANUP_STDOUT="$(mktemp "${SANDBOX}/drill-cleanup-stdout.XXXXXX")"
# No explicit scratch-dir argument: validate creates (and, on failure, must
# remove) its own via mktemp — this is the case cleanup-on-fail covers.
if "${RESTORE_DRILL_SH}" validate fixture "${CLEANUP_BAD_DB}" marker-1 "${CR_MANIFEST}" >"${CLEANUP_STDOUT}" 2>&1; then
  fail "validate should fail on the deliberately mismatched cleanup-test copy"
else
  ok "validate fails on the deliberately mismatched cleanup-test copy (cleanup-on-fail case)"
fi
CLEANUP_SCRATCH_DIR="$(sed -n 's/.*scratch=\(.*\)$/\1/p' "${CLEANUP_STDOUT}" | head -1)"
if [ -z "${CLEANUP_SCRATCH_DIR}" ]; then
  fail "could not parse the auto-created scratch dir from validate's own log line"
elif [ -d "${CLEANUP_SCRATCH_DIR}" ]; then
  fail "scratch dir should be removed after a failed validate run with no explicit scratch-dir argument"
else
  ok "scratch dir is removed after a failed validate run with no explicit scratch-dir argument"
fi
FAILED_EVIDENCE_DIR="$(find "${KHIVE_BACKUP_ROOT}/drill" -maxdepth 1 -type d -name 'failed-fixture-*' 2>/dev/null | sort | tail -1)"
if [ -n "${FAILED_EVIDENCE_DIR}" ]; then
  ok "a failed-<store>-<timestamp> evidence dir was created"
else
  fail "no failed-<store>-<timestamp> evidence dir found under ${KHIVE_BACKUP_ROOT}/drill"
fi
assert_file_exists "evidence dir contains the manifest diff" "${FAILED_EVIDENCE_DIR}/manifest.diff"
assert_contains "preserved manifest diff shows the notes table mismatch" \
  "$(cat "${FAILED_EVIDENCE_DIR}/manifest.diff" 2>/dev/null)" "notes"

echo "=== test: restore drill — validate with an explicit scratch-dir is never auto-removed (success or failure) ==="
EXPLICIT_SCRATCH_OK="$(mktemp -d "${SANDBOX}/drill-explicit-ok.XXXXXX")"
# Outcome (pass/fail) is irrelevant to this test — only scratch-dir survival
# is asserted, so the validate exit status is deliberately not checked.
"${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${CR_MANIFEST}" "${EXPLICIT_SCRATCH_OK}" >/tmp/khive-test-drill-explicit-ok.out 2>&1 || true
if [ -d "${EXPLICIT_SCRATCH_OK}" ]; then
  ok "caller-supplied scratch-dir survives (this script never removes a scratch-dir it did not create)"
else
  fail "caller-supplied scratch-dir was removed — cleanup must be scoped to self-created scratch dirs only"
fi

echo "=== test: restore drill validate is fatal by default when kkernel is missing (steps 5-6 required) ==="
# Fresh matching pair: earlier tests in this suite deliberately advance the
# origin/replica past CR_MANIFEST's recorded point (the capture-replica
# absent-marker test below adds marker-2), so re-sync and recapture here
# rather than reusing CR_MANIFEST — this test is about the kkernel gate,
# not manifest equality, and needs a pair that is known to match.
"${KHIVE_BACKUP_SH}" t1 fixture >/dev/null
KKERNEL_GATE_MANIFEST="${SANDBOX}/drill-manifest-kkernel-gate.txt"
"${RESTORE_DRILL_SH}" capture-replica fixture marker-1 "${KKERNEL_GATE_MANIFEST}" >/dev/null
# Temporarily remove the kkernel stub from PATH to simulate the real
# "kkernel not installed on this recovery host" case — the sandbox's
# STUB_BIN normally shadows a real kkernel with an always-succeeding stub.
# A restricted PATH (STUB_BIN plus only the plain system dirs sqlite3 needs)
# is used for these two invocations so a real kkernel binary elsewhere on
# the developer's PATH (e.g. ~/.cargo/bin) cannot leak in and defeat the
# "kkernel not found" simulation.
KKERNEL_STUB_PATH="${STUB_BIN}/kkernel"
KKERNEL_STUB_BACKUP="${SANDBOX}/kkernel-stub-backup"
mv "${KKERNEL_STUB_PATH}" "${KKERNEL_STUB_BACKUP}"
NO_KKERNEL_PATH="${STUB_BIN}:/usr/bin:/bin"
NO_KKERNEL_OUT="$(mktemp "${SANDBOX}/drill-no-kkernel-stdout.XXXXXX")"
if PATH="${NO_KKERNEL_PATH}" "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${KKERNEL_GATE_MANIFEST}" >"${NO_KKERNEL_OUT}" 2>&1; then
  fail "validate should refuse (fatal) when kkernel is missing and KHIVE_BACKUP_ALLOW_PARTIAL_DRILL is unset"
else
  ok "validate fails loudly when kkernel is missing and no partial-drill opt-in is set"
fi
assert_contains "fatal-missing-kkernel error names steps 5-6" "$(cat "${NO_KKERNEL_OUT}")" "steps 5"
assert_contains "fatal-missing-kkernel error cites the ADR acceptance requirement" "$(cat "${NO_KKERNEL_OUT}")" "ADR-100"
assert_not_contains "fatal-missing-kkernel path never prints RESTORE DRILL PASSED" "$(cat "${NO_KKERNEL_OUT}")" "RESTORE DRILL PASSED"

echo "=== test: restore drill validate completes as an explicit PARTIAL drill when the operator opts in ==="
PARTIAL_OUT="$(mktemp "${SANDBOX}/drill-partial-stdout.XXXXXX")"
if PATH="${NO_KKERNEL_PATH}" KHIVE_BACKUP_ALLOW_PARTIAL_DRILL=1 "${RESTORE_DRILL_SH}" validate fixture "${T1_REPLICA}" marker-1 "${KKERNEL_GATE_MANIFEST}" >"${PARTIAL_OUT}" 2>&1; then
  ok "validate completes with KHIVE_BACKUP_ALLOW_PARTIAL_DRILL=1 despite missing kkernel"
else
  fail "validate should complete (exit 0) for an explicit partial drill: $(cat "${PARTIAL_OUT}")"
fi
assert_contains "partial drill prints the PARTIAL line" \
  "$(cat "${PARTIAL_OUT}")" "RESTORE DRILL PARTIAL (steps 5-6 skipped: kkernel not on PATH)"
assert_not_contains "partial drill never prints RESTORE DRILL PASSED" "$(cat "${PARTIAL_OUT}")" "RESTORE DRILL PASSED"
assert_not_contains "partial drill never prints a bare RTO_SECONDS= success token" "$(cat "${PARTIAL_OUT}")" "RTO_SECONDS="
assert_contains "partial drill reports RTO under a partial-qualified key instead" "$(cat "${PARTIAL_OUT}")" "RTO_SECONDS_PARTIAL="

# restore the kkernel stub for any remaining tests in this suite
mv "${KKERNEL_STUB_BACKUP}" "${KKERNEL_STUB_PATH}"

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

echo "=== test: installer renders retention defaults (KHIVE_BACKUP_RETENTION_LOCAL/REMOTE) into the plist ==="
T3_PLIST_DEST="${HOME}/Library/LaunchAgents/com.khive.backup.t3.fixture.plist"
"${INSTALL_SH}" install t3 fixture >/tmp/khive-test-install-retention-default.out 2>&1 \
  || fail "install t3 with no retention flags should succeed: $(cat /tmp/khive-test-install-retention-default.out)"
assert_file_exists "t3 plist installed after install" "${T3_PLIST_DEST}"
RL_DEFAULT="$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:KHIVE_BACKUP_RETENTION_LOCAL' "${T3_PLIST_DEST}" 2>/dev/null || echo '')"
RR_DEFAULT="$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:KHIVE_BACKUP_RETENTION_REMOTE' "${T3_PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "default render writes KHIVE_BACKUP_RETENTION_LOCAL=4" "4" "${RL_DEFAULT}"
assert_eq "default render writes KHIVE_BACKUP_RETENTION_REMOTE=8" "8" "${RR_DEFAULT}"

echo "=== test: installer --retention-remote override renders the explicit value ==="
"${INSTALL_SH}" install t3 fixture --retention-remote 4 >/tmp/khive-test-install-retention-override.out 2>&1 \
  || fail "install t3 --retention-remote 4 should succeed: $(cat /tmp/khive-test-install-retention-override.out)"
RR_OVERRIDE="$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:KHIVE_BACKUP_RETENTION_REMOTE' "${T3_PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "explicit --retention-remote overrides the default" "4" "${RR_OVERRIDE}"

echo "=== test: installer reinstall with no retention flag carries forward the previously-rendered value ==="
"${INSTALL_SH}" install t3 fixture >/tmp/khive-test-install-retention-carry.out 2>&1 \
  || fail "reinstall t3 with no retention flag should succeed: $(cat /tmp/khive-test-install-retention-carry.out)"
RR_CARRY="$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:KHIVE_BACKUP_RETENTION_REMOTE' "${T3_PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "reinstall with no flag preserves the previously-rendered retention (not reset to default 8)" "4" "${RR_CARRY}"
RL_CARRY="$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:KHIVE_BACKUP_RETENTION_LOCAL' "${T3_PLIST_DEST}" 2>/dev/null || echo '')"
assert_eq "reinstall with no flag preserves the untouched retention-local default" "4" "${RL_CARRY}"

echo "=== test: installer status output contains no secrets ==="
STATUS_OUT="$("${INSTALL_SH}" status t1 fixture 2>&1)"
assert_not_contains "status output contains no CHANGE_ME remote credential leakage" "${STATUS_OUT}" "backup-host-password"
assert_contains "status output reports the plist path" "${STATUS_OUT}" "com.khive.backup.t1.fixture.plist"

echo "=== test: installer resolves the default stores.conf when KHIVE_BACKUP_CONF is unset ==="
# Regression: install-backup.sh sources lib.sh, whose resolve_stores_conf
# falls back to "${BACKUP_SCRIPT_DIR}/stores.conf" when KHIVE_BACKUP_CONF is
# not set. The installer must define BACKUP_SCRIPT_DIR (the name lib.sh reads,
# shared with khive-backup.sh / restore-drill.sh) — a mismatched local name
# leaves it unbound, so under `set -u` every installer command that needs the
# registry dies before doing anything. Every other install test exports
# KHIVE_BACKUP_CONF, so only this one exercises the default-path branch. An
# unknown store name is used deliberately: it drives resolve_stores_conf (the
# code that needs BACKUP_SCRIPT_DIR) to completion, then fails cleanly at
# row lookup — no plist is written, so this test mutates nothing.
DEFAULT_CONF_OUT="$(env -u KHIVE_BACKUP_CONF KHIVE_BACKUP_INSTALL_TEST=1 "${INSTALL_SH}" install t1 __nosuch_store__ 2>&1 || true)"
assert_not_contains "installer does not die on an unbound script-dir variable" "${DEFAULT_CONF_OUT}" "unbound variable"
assert_not_contains "installer resolves a real registry path, not the empty-path 'not found' error" "${DEFAULT_CONF_OUT}" "store registry not found:  "
assert_contains "installer reaches store-row lookup against the default registry" "${DEFAULT_CONF_OUT}" "no store named '__nosuch_store__'"

echo "=== test: uninstall removes the plist ==="
"${INSTALL_SH}" uninstall t1 fixture >/tmp/khive-test-uninstall.out 2>&1 \
  || fail "uninstall should succeed: $(cat /tmp/khive-test-uninstall.out)"
if [ -f "${PLIST_DEST}" ]; then
  fail "plist should be removed after uninstall"
else
  ok "plist removed after uninstall"
fi

echo "=== test: manifest capture skips virtual tables but covers their shadow tables ==="
# Production stores carry extension-backed virtual tables (vec0) that the
# stock sqlite3 CLI cannot query. The manifest must enumerate only plain
# tables; the virtual table's shadow tables hold the data and ARE captured.
sqlite3 "${ORIGIN_DB}" "CREATE VIRTUAL TABLE fts_probe USING fts5(body); INSERT INTO fts_probe (body) VALUES ('virtual table row');"
DRILL_VT_MANIFEST="${SANDBOX}/drill-manifest-vt.txt"
if "${RESTORE_DRILL_SH}" capture fixture marker-1 "${DRILL_VT_MANIFEST}" >/tmp/khive-test-drill-vt.out 2>&1; then
  ok "capture succeeds on an origin containing a virtual table"
else
  fail "capture should succeed with a virtual table present: $(cat /tmp/khive-test-drill-vt.out)"
fi
assert_not_contains "manifest omits the virtual table itself" "$(cat "${DRILL_VT_MANIFEST}")" "COUNT|fts_probe|"
assert_contains "manifest covers the virtual table's shadow data table" "$(cat "${DRILL_VT_MANIFEST}")" "COUNT|fts_probe_data|"

echo
echo "[test_backup.sh] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]
