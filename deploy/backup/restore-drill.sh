#!/usr/bin/env bash
# ADR-100 restore drill (the acceptance test / standing operational drill).
#
# Two capture modes, one validate subcommand:
#
#   restore-drill.sh capture-replica <store-name> <marker-id> [out-path]
#   restore-drill.sh capture <store-name> <marker-id> [out-path]
#   restore-drill.sh validate <store-name> <backup-path> <marker-id> <manifest-path> [scratch-dir]
#
# capture-replica (ROUTINE drill — deterministic on a live, multi-writer
# store; amendment 2026-07-07): captures the manifest from the store's
# freshly-synced tier-1 replica (`t1_replica` in stores.conf) instead of the
# origin. Live evidence on a 4.1GB production store showed the origin-exact
# ordering below is unsatisfiable there: an audit `events` table takes a row
# from every client dispatch on a ~10s cadence, versus a ~60s capture-to-sync
# window, so five controlled origin-capture attempts each failed with a
# manifest diff that isolated exactly the writes landing inside that window.
# The differ was correct every time; the contract (capture the moving
# origin, then compare a later replica to it) was the thing racing.
#
# capture-replica still requires the marker round-trip: the marker was
# written to the ORIGIN before the sync (operator step 1, unchanged), so its
# presence in the REPLICA is the proof the sync actually carried the origin's
# state forward — without that check, a routine drill would only prove "the
# replica equals itself", which is not a restore drill at all. Routine flow:
#   write marker to origin -> run the designated sync -> capture-replica
#   (verifies marker in the replica, captures the manifest FROM the replica)
#   -> validate (unchanged; feeds the replica-captured manifest)
#
# capture (ORIGIN-EXACT — retained for maintenance-window drills only): the
# manifest is captured immediately before the DESIGNATED sync, straight from
# the origin, and stored beside the sync metrics; validation later compares
# a restored copy against that RECORDED file, never against the
# (by-then-moved-on) origin. This ordering is only valid when the origin is
# quiescent for the capture-to-sync window (a real maintenance window, or a
# store with no live writers) — on a live multi-writer store, in-window
# writes will show up as spurious manifest mismatches, per the evidence
# above. Bound to the schema-migration re-drill cadence the ADR already
# names: run it once in the first genuine maintenance window and record the
# result.
#
# capture-replica / capture:
#   <store-name>  looks up the origin (and, for capture-replica, the t1
#                 replica) path in stores.conf (KHIVE_BACKUP_CONF to
#                 override).
#   <marker-id>   the id of a marker row the OPERATOR already wrote through
#                 the normal write path to the ORIGIN (ADR step 1). This
#                 script does not create markers — it only verifies one is
#                 present (in the origin for `capture`, in the replica for
#                 `capture-replica`).
#   [out-path]    defaults to KHIVE_BACKUP_ROOT/drill/manifests/<store>-<UTC
#                 stamp>.manifest. Printed on success as MANIFEST_PATH=<path>.
#
# validate:
#   <backup-path>    the replica or archive file to validate — the
#                    operator's choice of which backup to exercise (tier-2
#                    replica per the ADR's recommendation, but any tier's
#                    file works).
#   <marker-id>      same marker id used for `capture`/`capture-replica`.
#   <manifest-path>  the file `capture`/`capture-replica` produced. Must
#                    exist and be well-formed; validate refuses otherwise
#                    rather than silently treating a missing file as "no
#                    prior state".
#   [scratch-dir]    defaults to a mktemp dir under KHIVE_BACKUP_ROOT/drill.
#                    On validate failure, small evidence (the manifest diff
#                    and the restored manifest) is preserved under
#                    KHIVE_BACKUP_ROOT/drill/failed-<store>-<UTC stamp>/ and
#                    the (multi-GB) scratch dir is removed — but only when
#                    this script created scratch-dir itself (no explicit
#                    scratch-dir argument was passed); an operator-supplied
#                    scratch-dir is left for the caller to manage.
#
# Marker lookup table/column default to "notes"/"id" (the kg substrate);
# override with KHIVE_BACKUP_MARKER_TABLE / KHIVE_BACKUP_MARKER_COLUMN if a
# store's marker lives elsewhere.

set -euo pipefail

BACKUP_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
# shellcheck source=./lib.sh
. "${BACKUP_SCRIPT_DIR}/lib.sh"

usage() {
  cat >&2 <<'EOF'
usage:
  restore-drill.sh capture-replica <store-name> <marker-id> [out-path]
      routine drill: capture the manifest from the freshly-synced t1
      replica (deterministic on a live, multi-writer store). Marker must
      already be present in the replica (i.e. run AFTER the designated
      sync that carried it there).
  restore-drill.sh capture <store-name> <marker-id> [out-path]
      origin-exact drill: capture the manifest from the origin. Only valid
      on a quiescent store (maintenance-window drills) — see file header.
  restore-drill.sh validate <store-name> <backup-path> <marker-id> <manifest-path> [scratch-dir]
EOF
  exit 1
}

MARKER_TABLE="${KHIVE_BACKUP_MARKER_TABLE:-notes}"
MARKER_COLUMN="${KHIVE_BACKUP_MARKER_COLUMN:-id}"

# --- manifest capture (used by both subcommands; `validate` runs it only
# against the restored copy, never the origin) ---------------------------
# Single read transaction: table list is queried first (schema, not data),
# then one BEGIN DEFERRED / ROLLBACK bracket runs every per-table
# count/max-rowid/checksum query against one consistent snapshot.

capture_manifest() {
  local db="$1" out="$2" readonly_flag="$3" tables table script raw line

  # Virtual tables are excluded: the sqlite3 CLI may lack their extension
  # module (e.g. vec0), and their content lives in plain shadow tables that
  # ARE enumerated and checksummed here. Each row is "name|rowid" or
  # "name|norowid" — WITHOUT ROWID tables (e.g. fts5 config shadow tables)
  # cannot answer max(rowid); COUNT + CHECKSUM still cover them fully.
  local table_sql="SELECT name || '|' || (CASE WHEN sql LIKE '%WITHOUT ROWID%' THEN 'norowid' ELSE 'rowid' END) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND (sql IS NULL OR sql NOT LIKE 'CREATE VIRTUAL TABLE%') ORDER BY name;"
  if [ "${readonly_flag}" = "readonly" ]; then
    tables="$("${SQLITE3_BIN}" -readonly "${db}" "${table_sql}")"
  else
    tables="$("${SQLITE3_BIN}" "${db}" "${table_sql}")"
  fi
  [ -n "${tables}" ] || bdie "no tables found in ${db}"

  script=".bail on
.mode list
BEGIN DEFERRED;
"
  local rowid_kind
  while IFS= read -r table; do
    [ -z "${table}" ] && continue
    rowid_kind="${table##*|}"
    table="${table%|*}"
    script="${script}SELECT 'COUNT|${table}|' || count(*) FROM \"${table}\";
"
    if [ "${rowid_kind}" = "rowid" ]; then
      script="${script}SELECT 'MAXID|${table}|' || ifnull(max(rowid),-1) FROM \"${table}\";
"
    fi
    script="${script}.sha3sum ${table}
"
  done <<EOF
${tables}
EOF
  script="${script}ROLLBACK;
"

  raw="${out}.raw"
  if [ "${readonly_flag}" = "readonly" ]; then
    printf '%s' "${script}" | "${SQLITE3_BIN}" -readonly "${db}" >"${raw}" 2>"${raw}.err" \
      || bdie "manifest capture failed against ${db}: $(cat "${raw}.err")"
  else
    printf '%s' "${script}" | "${SQLITE3_BIN}" "${db}" >"${raw}" 2>"${raw}.err" \
      || bdie "manifest capture failed against ${db}: $(cat "${raw}.err")"
  fi
  rm -f "${raw}.err"

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

# A well-formed manifest is non-empty and every line is KIND|table|value
# with KIND in {COUNT,MAXID,CHECKSUM}. Guards `validate` against a missing,
# truncated, or hand-edited manifest silently comparing against garbage.
validate_manifest_file() {
  local path="$1" line
  [ -f "${path}" ] || bdie "manifest file not found: ${path} — run 'restore-drill.sh capture' before validating"
  [ -s "${path}" ] || bdie "manifest file is empty: ${path}"
  while IFS= read -r line; do
    case "${line}" in
      COUNT\|*\|*|MAXID\|*\|*|CHECKSUM\|*\|*) ;;
      *) bdie "manifest file is malformed at line: '${line}' (expected COUNT|MAXID|CHECKSUM prefix) — ${path}" ;;
    esac
  done <"${path}"
}

# --- capture subcommand ---------------------------------------------------

cmd_capture() {
  [ "$#" -ge 2 ] || usage
  local store="$1" marker_id="$2" out_path="${3:-}"
  local row

  row="$(load_store_row "${store}")" || bdie "no store named '${store}' in $(resolve_stores_conf)"
  split_store_row "${row}"
  [ -f "${STORE_ORIGIN}" ] || bdie "origin database does not exist: ${STORE_ORIGIN}"

  MARKER_ID="${marker_id}"

  if [ -z "${out_path}" ]; then
    mkdir -p "${KHIVE_BACKUP_ROOT}/drill/manifests"
    out_path="${KHIVE_BACKUP_ROOT}/drill/manifests/${store}-$(date -u +%Y%m%d-%H%M%S).manifest"
  fi
  mkdir -p "$(dirname -- "${out_path}")"

  blog "capture: verifying marker '${marker_id}' is present in origin ${STORE_ORIGIN}"
  check_marker_present "${STORE_ORIGIN}" "readonly" \
    || bdie "marker '${marker_id}' not found in origin ${MARKER_TABLE}.${MARKER_COLUMN} — write the marker row before capturing"

  blog "capture: capturing manifest from origin (read-only, single transaction)"
  capture_manifest "${STORE_ORIGIN}" "${out_path}" "readonly"

  blog "capture: manifest written to ${out_path}"
  printf 'MANIFEST_PATH=%s\n' "${out_path}"
}

# --- capture-replica subcommand (routine drill; see file header) ----------

cmd_capture_replica() {
  [ "$#" -ge 2 ] || usage
  local store="$1" marker_id="$2" out_path="${3:-}"
  local row

  row="$(load_store_row "${store}")" || bdie "no store named '${store}' in $(resolve_stores_conf)"
  split_store_row "${row}"
  [ -n "${STORE_T1_REPLICA}" ] || bdie "no t1_replica configured for store '${store}' in $(resolve_stores_conf)"
  is_placeholder "${STORE_T1_REPLICA}" && bdie "t1_replica for store '${store}' is still the CHANGE_ME placeholder"
  [ -f "${STORE_T1_REPLICA}" ] || bdie "t1 replica does not exist: ${STORE_T1_REPLICA} — run the designated sync before capture-replica"

  MARKER_ID="${marker_id}"

  if [ -z "${out_path}" ]; then
    mkdir -p "${KHIVE_BACKUP_ROOT}/drill/manifests"
    out_path="${KHIVE_BACKUP_ROOT}/drill/manifests/${store}-$(date -u +%Y%m%d-%H%M%S).manifest"
  fi
  mkdir -p "$(dirname -- "${out_path}")"

  blog "capture-replica: verifying marker '${marker_id}' is present in replica ${STORE_T1_REPLICA}"
  check_marker_present "${STORE_T1_REPLICA}" "readonly" \
    || bdie "marker '${marker_id}' not found in replica ${MARKER_TABLE}.${MARKER_COLUMN} — the marker was written to the origin before the sync (ADR step 1); its absence from the replica means the designated sync has not yet carried it forward. Run the sync, then retry."

  blog "capture-replica: capturing manifest from the freshly-synced replica (read-only, single transaction)"
  capture_manifest "${STORE_T1_REPLICA}" "${out_path}" "readonly"

  blog "capture-replica: manifest written to ${out_path}"
  printf 'MANIFEST_PATH=%s\n' "${out_path}"
}

# --- validate subcommand ---------------------------------------------------

# EXIT-trap cleanup for a scratch dir this script created itself (see
# cmd_validate's own_scratch guard — never called against a caller-supplied
# scratch-dir). On failure (rc != 0), ALL small evidence files (the manifest
# diff, the restored copy's manifest, and the step 5/6 runtime-boot outputs —
# plain text/JSON, never the multi-GB restored database) are copied out to a
# dated failed-<store>-* dir under KHIVE_BACKUP_ROOT/drill before the scratch
# dir is removed — "fail loud" and "clean up" both hold. The step that failed
# is the one whose evidence matters, so the step5-*/step6-* outputs must
# survive the scratch removal. On success, the scratch dir (including the
# restored database copy) is removed outright.
validate_cleanup_scratch() {
  local rc="$1" store="$2" scratch_dir="$3" fail_dir f
  if [ "${rc}" -ne 0 ]; then
    fail_dir="${KHIVE_BACKUP_ROOT}/drill/failed-${store}-$(date -u +%Y%m%d-%H%M%S)"
    mkdir -p "${fail_dir}"
    for f in "${scratch_dir}/manifest.diff" "${scratch_dir}/manifest-restored.txt" \
      "${scratch_dir}"/step5-*.json "${scratch_dir}"/step6-*.json; do
      [ -f "${f}" ] && cp "${f}" "${fail_dir}/$(basename "${f}")"
    done
    blog "validate FAILED — evidence preserved at ${fail_dir}; removing scratch dir ${scratch_dir}"
  fi
  rm -rf "${scratch_dir}"
}

cmd_validate() {
  [ "$#" -ge 4 ] || usage
  local store="$1" backup_path="$2" marker_id="$3" manifest_path="$4" scratch_dir="${5:-}"
  local row restored_db restored_manifest own_scratch=0

  row="$(load_store_row "${store}")" || bdie "no store named '${store}' in $(resolve_stores_conf)"
  split_store_row "${row}"

  MARKER_ID="${marker_id}"

  [ -f "${backup_path}" ] || bdie "backup file does not exist: ${backup_path}"
  validate_manifest_file "${manifest_path}"

  if [ -z "${scratch_dir}" ]; then
    mkdir -p "${KHIVE_BACKUP_ROOT}/drill"
    scratch_dir="$(mktemp -d "${KHIVE_BACKUP_ROOT}/drill/${store}.XXXXXX")"
    own_scratch=1
  fi
  mkdir -p "${scratch_dir}"

  # Only a scratch dir this script created (via its own mktemp above) is
  # ever removed here. A caller-supplied scratch-dir is left untouched on
  # both success and failure — cleanup of it is the caller's responsibility.
  if [ "${own_scratch}" -eq 1 ]; then
    # shellcheck disable=SC2064 # store/scratch_dir must expand now, not at trap-fire time
    trap "validate_cleanup_scratch \$? '${store}' '${scratch_dir}'" EXIT
  fi

  restored_db="${scratch_dir}/restored.db"
  restored_manifest="${scratch_dir}/manifest-restored.txt"

  blog "restore drill validate: store=${store} backup=${backup_path} marker=${marker_id} manifest=${manifest_path} scratch=${scratch_dir}"

  RESTORE_START="$(date +%s)"

  blog "step 2: restoring backup to scratch path"
  cp "${backup_path}" "${restored_db}"
  if [ -f "${backup_path}-wal" ]; then
    cp "${backup_path}-wal" "${restored_db}-wal"
  fi
  if [ -f "${backup_path}-shm" ]; then
    cp "${backup_path}-shm" "${restored_db}-shm"
  fi

  blog "step 3: PRAGMA integrity_check on the restored copy"
  local integrity_result
  integrity_result="$("${SQLITE3_BIN}" "${restored_db}" "PRAGMA integrity_check;" 2>&1)"
  if [ "${integrity_result}" != "ok" ]; then
    bdie "PRAGMA integrity_check FAILED on restored copy: ${integrity_result}"
  fi
  blog "integrity_check: ok"

  blog "step 4: marker + manifest comparison against the RECORDED manifest (exact, no tolerance)"
  check_marker_present "${restored_db}" "rw" \
    || bdie "marker '${marker_id}' missing from restored copy — restore is not equivalent to the recorded manifest point"
  capture_manifest "${restored_db}" "${restored_manifest}" "rw"

  if ! diff -u "${manifest_path}" "${restored_manifest}" >"${scratch_dir}/manifest.diff" 2>&1; then
    blog "MANIFEST MISMATCH — see ${scratch_dir}/manifest.diff"
    cat "${scratch_dir}/manifest.diff" >&2
    bdie "restored copy does not match the recorded manifest exactly"
  fi
  blog "manifest comparison: exact match"

  blog "step 5: booting a runtime against the restored copy and serving live verbs"
  # kkernel refuses the KHIVE_DB override when the host config declares
  # [[backends]] ("cannot be combined with [[backends]]: N backend(s) are
  # already declared in khive.toml" — an ambiguity guard, observed live).
  # Run every kkernel invocation under an isolated HOME inside the scratch
  # dir: config discovery finds nothing, KHIVE_DB is accepted, and the drill
  # runtime provably cannot discover (or touch) the host's real stores.
  local drill_home="${scratch_dir}/kkernel-home"
  mkdir -p "${drill_home}"
  local step5_status="skipped (kkernel not on PATH)"
  if command -v kkernel >/dev/null 2>&1; then
    if HOME="${drill_home}" KHIVE_DB="${restored_db}" kkernel exec 'stats()' >"${scratch_dir}/step5-stats.json" 2>&1 \
      && HOME="${drill_home}" KHIVE_DB="${restored_db}" kkernel exec 'search(kind="entity", query="restore drill", limit=1)' >"${scratch_dir}/step5-search.json" 2>&1 \
      && HOME="${drill_home}" KHIVE_DB="${restored_db}" kkernel exec 'memory.recall(query="restore drill", limit=1)' >"${scratch_dir}/step5-recall.json" 2>&1; then
      step5_status="ok"
    else
      step5_status="FAILED"
    fi
  fi
  blog "step 5 (stats/search/recall against restored copy): ${step5_status}"
  if [ "${step5_status}" = "FAILED" ]; then
    bdie "step 5 live-verb serving failed against the restored copy — see ${scratch_dir}/step5-*.json"
  fi

  blog "step 6: rebuilding the ANN index from the restored database and serving one vector query"
  local step6_status="skipped (kkernel not on PATH)"
  if command -v kkernel >/dev/null 2>&1; then
    if HOME="${drill_home}" KHIVE_DB="${restored_db}" kkernel reindex >"${scratch_dir}/step6-reindex.json" 2>&1 \
      && HOME="${drill_home}" KHIVE_DB="${restored_db}" kkernel exec 'memory.recall(query="restore drill vector query", limit=1)' >"${scratch_dir}/step6-vector.json" 2>&1; then
      step6_status="ok"
    else
      step6_status="FAILED"
    fi
  fi
  blog "step 6 (ANN rebuild + vector query): ${step6_status}"
  if [ "${step6_status}" = "FAILED" ]; then
    bdie "step 6 ANN rebuild / vector query failed against the restored copy — see ${scratch_dir}/step6-*.json"
  fi

  local drill_end rto_seconds
  drill_end="$(date +%s)"
  rto_seconds=$((drill_end - RESTORE_START))

  blog "step 7: RTO (steps 2-6) = ${rto_seconds}s"
  if [ "${own_scratch}" -eq 1 ]; then
    blog "RESTORE DRILL PASSED for store '${store}' — scratch dir ${scratch_dir} will be removed (pass an explicit scratch-dir argument to retain it)"
  else
    blog "RESTORE DRILL PASSED for store '${store}' — scratch artifacts kept at ${scratch_dir} (caller-supplied scratch-dir)"
  fi
  printf 'RTO_SECONDS=%s\n' "${rto_seconds}"
}

case "${1:-}" in
  capture)
    shift
    cmd_capture "$@"
    ;;
  capture-replica)
    shift
    cmd_capture_replica "$@"
    ;;
  validate)
    shift
    cmd_validate "$@"
    ;;
  *)
    usage
    ;;
esac
