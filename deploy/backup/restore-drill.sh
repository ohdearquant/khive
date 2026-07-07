#!/usr/bin/env bash
# ADR-100 restore drill (the acceptance test / standing operational drill).
# Implements steps 1-7 of the ADR's "Restore drill (acceptance test)"
# section against a chosen backup copy, comparing it to a manifest captured
# from the ORIGIN — never against the moving origin at compare time.
#
# Usage: restore-drill.sh <store-name> <backup-path> <marker-id> [scratch-dir]
#
# <store-name>  looks up the origin path in stores.conf (KHIVE_BACKUP_CONF
#               to override), so the drill always reads its origin the same
#               way khive-backup.sh resolved it for that store.
# <backup-path> the replica or archive file to validate — the operator's
#               choice of which backup to exercise (tier-2 replica per the
#               ADR's recommendation, but any tier's file works).
# <marker-id>   the id of a marker row the OPERATOR already wrote through
#               the normal write path (per the ADR, step 1) before the sync
#               being validated ran. This script does not create markers —
#               it only verifies one is present and unchanged.
# [scratch-dir] defaults to a mktemp dir under KHIVE_BACKUP_ROOT/drill.
#
# Marker lookup table/column default to "notes"/"id" (the kg substrate);
# override with KHIVE_BACKUP_MARKER_TABLE / KHIVE_BACKUP_MARKER_COLUMN if a
# store's marker lives elsewhere.

set -euo pipefail

BACKUP_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
# shellcheck source=./lib.sh
. "${BACKUP_SCRIPT_DIR}/lib.sh"

usage() {
  echo "usage: $0 <store-name> <backup-path> <marker-id> [scratch-dir]" >&2
  exit 1
}

[ "$#" -ge 3 ] || usage
STORE="$1"
BACKUP_PATH="$2"
MARKER_ID="$3"
SCRATCH_DIR="${4:-}"

MARKER_TABLE="${KHIVE_BACKUP_MARKER_TABLE:-notes}"
MARKER_COLUMN="${KHIVE_BACKUP_MARKER_COLUMN:-id}"

ROW="$(load_store_row "${STORE}")" || bdie "no store named '${STORE}' in $(resolve_stores_conf)"
split_store_row "${ROW}"

[ -f "${STORE_ORIGIN}" ] || bdie "origin database does not exist: ${STORE_ORIGIN}"
[ -f "${BACKUP_PATH}" ] || bdie "backup file does not exist: ${BACKUP_PATH}"

if [ -z "${SCRATCH_DIR}" ]; then
  mkdir -p "${KHIVE_BACKUP_ROOT}/drill"
  SCRATCH_DIR="$(mktemp -d "${KHIVE_BACKUP_ROOT}/drill/${STORE}.XXXXXX")"
fi
mkdir -p "${SCRATCH_DIR}"

RESTORED_DB="${SCRATCH_DIR}/restored.db"
ORIGIN_MANIFEST="${SCRATCH_DIR}/manifest-origin.txt"
RESTORED_MANIFEST="${SCRATCH_DIR}/manifest-restored.txt"

blog "restore drill: store=${STORE} backup=${BACKUP_PATH} marker=${MARKER_ID} scratch=${SCRATCH_DIR}"

# --- manifest capture (step 1 for the origin side; step 4 reuses this on
# the restored copy) --------------------------------------------------
# Single read transaction: table list is queried first (schema, not data),
# then one BEGIN DEFERRED / ROLLBACK bracket runs every per-table
# count/max-rowid/checksum query against one consistent snapshot.

capture_manifest() {
  local db="$1" out="$2" readonly_flag="$3" tables table script raw line

  if [ "${readonly_flag}" = "readonly" ]; then
    tables="$("${SQLITE3_BIN}" -readonly "${db}" "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name;")"
  else
    tables="$("${SQLITE3_BIN}" "${db}" "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name;")"
  fi
  [ -n "${tables}" ] || bdie "no tables found in ${db}"

  script=".bail off
.mode list
BEGIN DEFERRED;
"
  while IFS= read -r table; do
    [ -z "${table}" ] && continue
    script="${script}SELECT 'COUNT|${table}|' || count(*) FROM \"${table}\";
SELECT 'MAXID|${table}|' || ifnull(max(rowid),-1) FROM \"${table}\";
.sha3sum ${table}
"
  done <<EOF
${tables}
EOF
  script="${script}ROLLBACK;
"

  raw="${out}.raw"
  if [ "${readonly_flag}" = "readonly" ]; then
    printf '%s' "${script}" | "${SQLITE3_BIN}" -readonly "${db}" >"${raw}" 2>/dev/null
  else
    printf '%s' "${script}" | "${SQLITE3_BIN}" "${db}" >"${raw}" 2>/dev/null
  fi

  : >"${out}"
  while IFS= read -r line; do
    case "${line}" in
      COUNT\|*|MAXID\|*)
        printf '%s\n' "${line}" >>"${out}"
        ;;
      *"|"*)
        # .sha3sum emits "<hash>|<table>" — tag it CHECKSUM for the diff.
        printf 'CHECKSUM|%s|%s\n' "${line#*|}" "${line%%|*}" >>"${out}"
        ;;
    esac
  done <"${raw}"
  rm -f "${raw}"
  sort -o "${out}" "${out}"
}

check_marker_present() {
  local db="$1" readonly_flag="$2" count
  if [ "${readonly_flag}" = "readonly" ]; then
    count="$("${SQLITE3_BIN}" -readonly "${db}" "SELECT count(*) FROM \"${MARKER_TABLE}\" WHERE \"${MARKER_COLUMN}\" = '${MARKER_ID}';" 2>/dev/null || echo "0")"
  else
    count="$("${SQLITE3_BIN}" "${db}" "SELECT count(*) FROM \"${MARKER_TABLE}\" WHERE \"${MARKER_COLUMN}\" = '${MARKER_ID}';" 2>/dev/null || echo "0")"
  fi
  [ "${count}" = "1" ]
}

DRILL_START="$(date +%s)"

blog "step 1: capturing manifest from origin (read-only, single transaction)"
capture_manifest "${STORE_ORIGIN}" "${ORIGIN_MANIFEST}" "readonly"
check_marker_present "${STORE_ORIGIN}" "readonly" \
  || bdie "marker '${MARKER_ID}' not found in origin ${MARKER_TABLE}.${MARKER_COLUMN} — write the marker row before running the drill"

RESTORE_START="$(date +%s)"

blog "step 2: restoring backup to scratch path"
cp "${BACKUP_PATH}" "${RESTORED_DB}"
if [ -f "${BACKUP_PATH}-wal" ]; then
  cp "${BACKUP_PATH}-wal" "${RESTORED_DB}-wal"
fi
if [ -f "${BACKUP_PATH}-shm" ]; then
  cp "${BACKUP_PATH}-shm" "${RESTORED_DB}-shm"
fi

blog "step 3: PRAGMA integrity_check on the restored copy"
INTEGRITY_RESULT="$("${SQLITE3_BIN}" "${RESTORED_DB}" "PRAGMA integrity_check;" 2>&1)"
if [ "${INTEGRITY_RESULT}" != "ok" ]; then
  bdie "PRAGMA integrity_check FAILED on restored copy: ${INTEGRITY_RESULT}"
fi
blog "integrity_check: ok"

blog "step 4: marker + manifest comparison (exact, no tolerance)"
check_marker_present "${RESTORED_DB}" "rw" \
  || bdie "marker '${MARKER_ID}' missing from restored copy — restore is not equivalent to the manifest point"
capture_manifest "${RESTORED_DB}" "${RESTORED_MANIFEST}" "rw"

if ! diff -u "${ORIGIN_MANIFEST}" "${RESTORED_MANIFEST}" >"${SCRATCH_DIR}/manifest.diff" 2>&1; then
  blog "MANIFEST MISMATCH — see ${SCRATCH_DIR}/manifest.diff"
  cat "${SCRATCH_DIR}/manifest.diff" >&2
  bdie "restored copy does not match the origin manifest exactly"
fi
blog "manifest comparison: exact match"

blog "step 5: booting a runtime against the restored copy and serving live verbs"
STEP5_STATUS="skipped (kkernel not on PATH)"
if command -v kkernel >/dev/null 2>&1; then
  if KHIVE_DB="${RESTORED_DB}" kkernel exec 'stats()' >"${SCRATCH_DIR}/step5-stats.json" 2>&1 \
    && KHIVE_DB="${RESTORED_DB}" kkernel exec 'search(kind="entity", query="restore drill", limit=1)' >"${SCRATCH_DIR}/step5-search.json" 2>&1 \
    && KHIVE_DB="${RESTORED_DB}" kkernel exec 'memory.recall(query="restore drill", limit=1)' >"${SCRATCH_DIR}/step5-recall.json" 2>&1; then
    STEP5_STATUS="ok"
  else
    STEP5_STATUS="FAILED"
  fi
fi
blog "step 5 (stats/search/recall against restored copy): ${STEP5_STATUS}"
if [ "${STEP5_STATUS}" = "FAILED" ]; then
  bdie "step 5 live-verb serving failed against the restored copy — see ${SCRATCH_DIR}/step5-*.json"
fi

blog "step 6: rebuilding the ANN index from the restored database and serving one vector query"
STEP6_STATUS="skipped (kkernel not on PATH)"
if command -v kkernel >/dev/null 2>&1; then
  if KHIVE_DB="${RESTORED_DB}" kkernel reindex >"${SCRATCH_DIR}/step6-reindex.json" 2>&1 \
    && KHIVE_DB="${RESTORED_DB}" kkernel exec 'memory.recall(query="restore drill vector query", limit=1)' >"${SCRATCH_DIR}/step6-vector.json" 2>&1; then
    STEP6_STATUS="ok"
  else
    STEP6_STATUS="FAILED"
  fi
fi
blog "step 6 (ANN rebuild + vector query): ${STEP6_STATUS}"
if [ "${STEP6_STATUS}" = "FAILED" ]; then
  bdie "step 6 ANN rebuild / vector query failed against the restored copy — see ${SCRATCH_DIR}/step6-*.json"
fi

DRILL_END="$(date +%s)"
RTO_SECONDS=$((DRILL_END - RESTORE_START))
TOTAL_SECONDS=$((DRILL_END - DRILL_START))

blog "step 7: RTO (steps 2-6) = ${RTO_SECONDS}s (manifest capture + drill wall time = ${TOTAL_SECONDS}s)"
blog "RESTORE DRILL PASSED for store '${STORE}' — scratch artifacts kept at ${SCRATCH_DIR}"
printf 'RTO_SECONDS=%s\n' "${RTO_SECONDS}"
