#!/usr/bin/env bash
# ADR-100 tier runner. Usage: khive-backup.sh <t1|t2|t3> <store-name>
#
# t1 — every 15 min, sqlite3_rsync to a local replica.
# t2 — hourly, sqlite3_rsync to an off-host replica over SSH.
# t3 — weekly, VACUUM INTO a dated archive, off-host copy, retention prune.
#
# See docs/adr/ADR-100-store-backup-replication.md for the full contract.
# See RUNBOOK.md for the operator procedure this script implements.

set -euo pipefail

BACKUP_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
# shellcheck source=./lib.sh
. "${BACKUP_SCRIPT_DIR}/lib.sh"

usage() {
  echo "usage: $0 <t1|t2|t3> <store-name>" >&2
  exit 1
}

[ "$#" -eq 2 ] || usage
TIER="$1"
STORE="$2"

case "${TIER}" in
  t1|t2|t3) ;;
  *) bdie "unknown tier '${TIER}' (expected t1, t2, or t3)" ;;
esac

TIMEOUT_T1="${KHIVE_BACKUP_TIMEOUT_T1:-120}"
TIMEOUT_T2="${KHIVE_BACKUP_TIMEOUT_T2:-600}"
TIMEOUT_T3="${KHIVE_BACKUP_TIMEOUT_T3:-1800}"

ROW="$(load_store_row "${STORE}")" || bdie "no store named '${STORE}' in $(resolve_stores_conf)"
split_store_row "${ROW}"

if [ ! -f "${STORE_ORIGIN}" ]; then
  bdie "origin database does not exist: ${STORE_ORIGIN}"
fi

if ! acquire_store_lock "${STORE}"; then
  blog "another sync is already running for store '${STORE}' (lock held) — exiting, no queueing"
  exit 1
fi
trap release_store_lock EXIT

START_EPOCH="$(date +%s)"
WAL_BEFORE="$(get_file_size "${STORE_ORIGIN}-wal")"

fail_and_log() {
  local outcome="$1" err="$2"
  local dur wal_after
  dur=$(($(date +%s) - START_EPOCH))
  wal_after="$(get_file_size "${STORE_ORIGIN}-wal")"
  log_backup_event "${STORE}" "${TIER}" "${outcome}" "${dur}" "0" "${WAL_BEFORE}" "${wal_after}" "${err}"
  send_backup_alert "khive-backup ${TIER}/${STORE}: ${outcome} — ${err}"
  exit 1
}

succeed_and_log() {
  local bytes="$1"
  local dur wal_after
  dur=$(($(date +%s) - START_EPOCH))
  wal_after="$(get_file_size "${STORE_ORIGIN}-wal")"
  log_backup_event "${STORE}" "${TIER}" "success" "${dur}" "${bytes}" "${WAL_BEFORE}" "${wal_after}" ""
  blog "${TIER}/${STORE}: success in ${dur}s (${bytes} bytes)"
}

# --- version preflight ---------------------------------------------------

check_local_version() {
  local ver
  ver="$(local_sqlite3_rsync_version)" || fail_and_log "version-preflight-failed" "sqlite3_rsync not found locally (expected ${SQLITE3_RSYNC_BIN} on PATH)"
  if ! version_ge "${ver}" "${KHIVE_BACKUP_MIN_VERSION}"; then
    fail_and_log "version-preflight-failed" "local sqlite3_rsync ${ver} is below the required floor ${KHIVE_BACKUP_MIN_VERSION}"
  fi
  blog "local sqlite3_rsync version ${ver} >= floor ${KHIVE_BACKUP_MIN_VERSION}"
  printf '%s' "${ver}"
}

check_remote_version() {
  local user_host="$1" ver
  ver="$(remote_sqlite3_rsync_version "${user_host}")" || fail_and_log "version-preflight-failed" "sqlite3_rsync not found on remote host ${user_host}"
  if ! version_ge "${ver}" "${KHIVE_BACKUP_MIN_VERSION}"; then
    fail_and_log "version-preflight-failed" "remote sqlite3_rsync ${ver} on ${user_host} is below the required floor ${KHIVE_BACKUP_MIN_VERSION}"
  fi
  blog "remote sqlite3_rsync version ${ver} on ${user_host} >= floor ${KHIVE_BACKUP_MIN_VERSION}"
  printf '%s' "${ver}"
}

# --- tier 1: local differential sync --------------------------------------

run_tier1() {
  check_local_version >/dev/null

  local dest="${STORE_T1_REPLICA}" dest_dir staging rc
  dest_dir="$(dirname -- "${dest}")"
  preflight_disk "${STORE_ORIGIN}" "${dest_dir}" || fail_and_log "disk-preflight-failed" "insufficient free space at ${dest_dir}"

  staging="${dest}.staging"
  rm -f "${staging}" "${staging}-wal" "${staging}-shm"
  clone_or_copy "${dest}" "${staging}"

  rc=0
  run_with_timeout "${TIMEOUT_T1}" "${SQLITE3_RSYNC_BIN}" "${STORE_ORIGIN}" "${staging}" || rc=$?
  if [ "${rc}" -eq 124 ]; then
    rm -f "${staging}" "${staging}-wal" "${staging}-shm"
    fail_and_log "timeout" "sqlite3_rsync exceeded ${TIMEOUT_T1}s and was killed"
  elif [ "${rc}" -ne 0 ]; then
    rm -f "${staging}" "${staging}-wal" "${staging}-shm"
    fail_and_log "sync-failed" "sqlite3_rsync exited ${rc}"
  fi

  if ! "${SQLITE3_BIN}" "${staging}" "PRAGMA quick_check;" >/dev/null 2>&1; then
    rm -f "${staging}" "${staging}-wal" "${staging}-shm"
    fail_and_log "integrity-check-failed" "PRAGMA quick_check failed on staged replica; previous replica left in place"
  fi

  promote_replica_local "${staging}" "${dest}"
  succeed_and_log "$(get_file_size "${dest}")"
}

# --- tier 2: off-host differential sync over SSH --------------------------

run_tier2() {
  is_placeholder "${STORE_T2_REMOTE}" && fail_and_log "not-configured" "t2_remote for store '${STORE}' is still the CHANGE_ME placeholder — edit stores.conf"

  local user_host remote_path staging rc
  user_host="${STORE_T2_REMOTE%%:*}"
  remote_path="${STORE_T2_REMOTE#*:}"
  staging="${remote_path}.staging"

  check_local_version >/dev/null
  check_remote_version "${user_host}" >/dev/null

  # Seed the remote staging file from the last promoted remote replica (if
  # any) so the differential sync stays meaningful, then promote only on
  # success — mirrors run_tier1's local staged-promote, over ssh.
  "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" \
    "rm -f '${staging}' '${staging}-wal' '${staging}-shm'; if [ -f '${remote_path}' ]; then cp -c '${remote_path}' '${staging}' 2>/dev/null || cp '${remote_path}' '${staging}'; fi" \
    || fail_and_log "sync-failed" "failed to seed remote staging path on ${user_host}"

  rc=0
  run_with_timeout "${TIMEOUT_T2}" "${SQLITE3_RSYNC_BIN}" "${STORE_ORIGIN}" "${user_host}:${staging}" || rc=$?
  if [ "${rc}" -eq 124 ]; then
    "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" "rm -f '${staging}' '${staging}-wal' '${staging}-shm'" || true
    fail_and_log "timeout" "sqlite3_rsync exceeded ${TIMEOUT_T2}s and was killed"
  elif [ "${rc}" -ne 0 ]; then
    "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" "rm -f '${staging}' '${staging}-wal' '${staging}-shm'" || true
    fail_and_log "sync-failed" "sqlite3_rsync exited ${rc}"
  fi

  if ! "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" "${SQLITE3_BIN} '${staging}' 'PRAGMA quick_check;'" >/dev/null 2>&1; then
    "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" "rm -f '${staging}' '${staging}-wal' '${staging}-shm'" || true
    fail_and_log "integrity-check-failed" "remote PRAGMA quick_check failed on staged replica; previous replica left in place"
  fi

  "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" \
    "mv -f '${staging}' '${remote_path}'; \
     if [ -f '${staging}-wal' ]; then mv -f '${staging}-wal' '${remote_path}-wal'; else rm -f '${remote_path}-wal'; fi; \
     if [ -f '${staging}-shm' ]; then mv -f '${staging}-shm' '${remote_path}-shm'; else rm -f '${remote_path}-shm'; fi" \
    || fail_and_log "promote-failed" "failed to promote staged replica on ${user_host}"

  succeed_and_log "0"
}

# --- tier 3: dated VACUUM INTO archive + retention -------------------------

prune_local_archives() {
  local dir="$1" keep="$2"
  local count f
  count=0
  # Newest first (lexicographic == chronological given the YYYYMMDD-HHMMSS
  # stamp used below); anything past `keep` is pruned.
  while IFS= read -r f; do
    count=$((count + 1))
    if [ "${count}" -gt "${keep}" ]; then
      rm -f "${f}"
      blog "pruned local archive ${f} (retention ${keep})"
    fi
  done < <(find "${dir}" -maxdepth 1 -type f -name "${STORE}-*.db" 2>/dev/null | sort -r)
}

prune_remote_archives() {
  local user_host="$1" dir="$2" keep="$3"
  "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" \
    "cd '${dir}' 2>/dev/null && ls -1 '${STORE}'-*.db 2>/dev/null | sort -r | tail -n +$((keep + 1)) | xargs -I{} rm -f '${dir}/{}'" \
    || blog "WARNING: remote archive prune on ${user_host} failed (non-fatal)"
}

run_tier3() {
  local archive_dir="${STORE_T3_ARCHIVE_DIR}" stamp tmp_path final_path
  mkdir -p "${archive_dir}"
  preflight_disk "${STORE_ORIGIN}" "${archive_dir}" || fail_and_log "disk-preflight-failed" "insufficient free space at ${archive_dir}"

  stamp="$(date -u +%Y%m%d-%H%M%S)"
  final_path="${archive_dir}/${STORE}-${stamp}.db"
  tmp_path="${final_path}.tmp"
  rm -f "${tmp_path}"

  local rc=0
  run_with_timeout "${TIMEOUT_T3}" "${SQLITE3_BIN}" "${STORE_ORIGIN}" "VACUUM INTO '${tmp_path}';" || rc=$?
  if [ "${rc}" -eq 124 ]; then
    rm -f "${tmp_path}"
    fail_and_log "timeout" "VACUUM INTO exceeded ${TIMEOUT_T3}s and was killed"
  elif [ "${rc}" -ne 0 ]; then
    rm -f "${tmp_path}"
    fail_and_log "sync-failed" "VACUUM INTO exited ${rc}"
  fi

  if ! "${SQLITE3_BIN}" "${tmp_path}" "PRAGMA quick_check;" >/dev/null 2>&1; then
    rm -f "${tmp_path}"
    fail_and_log "integrity-check-failed" "PRAGMA quick_check failed on new archive; not promoted"
  fi

  mv -f "${tmp_path}" "${final_path}"
  prune_local_archives "${archive_dir}" "${KHIVE_BACKUP_RETENTION_LOCAL}"

  if ! is_placeholder "${STORE_T2_REMOTE}"; then
    local user_host remote_replica remote_archive_dir
    user_host="${STORE_T2_REMOTE%%:*}"
    remote_replica="${STORE_T2_REMOTE#*:}"
    remote_archive_dir="$(dirname -- "${remote_replica}")/archive/${STORE}"
    "${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" "mkdir -p '${remote_archive_dir}'" || true
    scp -o BatchMode=yes -o ConnectTimeout=10 -p "${final_path}" "${user_host}:${remote_archive_dir}/" \
      || blog "WARNING: off-host archive copy to ${user_host} failed (local archive still promoted; alert below)"
    prune_remote_archives "${user_host}" "${remote_archive_dir}" "${KHIVE_BACKUP_RETENTION_REMOTE}"
  fi

  succeed_and_log "$(get_file_size "${final_path}")"
}

case "${TIER}" in
  t1) run_tier1 ;;
  t2) run_tier2 ;;
  t3) run_tier3 ;;
esac
