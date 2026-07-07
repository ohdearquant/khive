#!/usr/bin/env bash
# Shared shell library for the ADR-100 backup tooling (khive-backup.sh,
# restore-drill.sh). Sourced, never executed directly.
#
# Every external side effect a test needs to intercept (sqlite3_rsync, ssh,
# sqlite3, kkernel) is invoked through a plain command name resolved off
# PATH, so test_backup.sh can stub each one by prepending a sandbox bin dir.

# Guard against double-sourcing.
if [ -n "${KHIVE_BACKUP_LIB_LOADED:-}" ]; then
  return 0 2>/dev/null || exit 0
fi
KHIVE_BACKUP_LIB_LOADED=1

KHIVE_BACKUP_MIN_VERSION="${KHIVE_BACKUP_MIN_VERSION:-3.50.1}"
KHIVE_BACKUP_ROOT="${KHIVE_BACKUP_ROOT:-${HOME}/.khive/backups}"
KHIVE_BACKUP_MARGIN_BYTES="${KHIVE_BACKUP_MARGIN_BYTES:-104857600}" # 100 MiB
KHIVE_BACKUP_RETENTION_LOCAL="${KHIVE_BACKUP_RETENTION_LOCAL:-4}"
KHIVE_BACKUP_RETENTION_REMOTE="${KHIVE_BACKUP_RETENTION_REMOTE:-8}"
KHIVE_BACKUP_ALERT_ACTOR="${KHIVE_BACKUP_ALERT_ACTOR:-lambda:khive}"
SQLITE3_RSYNC_BIN="${KHIVE_BACKUP_SQLITE3_RSYNC:-sqlite3_rsync}"
# shellcheck disable=SC2034 # consumed by khive-backup.sh / restore-drill.sh after sourcing this file
SQLITE3_BIN="${KHIVE_BACKUP_SQLITE3:-sqlite3}"
SSH_BIN="${KHIVE_BACKUP_SSH:-ssh}"

blog() {
  printf '[khive-backup] %s\n' "$*"
}

bdie() {
  printf '[khive-backup] ERROR: %s\n' "$*" >&2
  exit 1
}

# --- path helpers -----------------------------------------------------

# Expand a single leading "~" to $HOME. Config-file paths are read as plain
# text (no shell involved), so "~" never expands on its own.
# shellcheck disable=SC2088 # the "~/"* case body strips a literal leading tilde via parameter expansion; it is not a path needing shell tilde-expansion
expand_path() {
  local p="$1"
  case "${p}" in
    "~") printf '%s' "${HOME}" ;;
    "~/"*) printf '%s' "${HOME}${p#\~}" ;;
    *) printf '%s' "${p}" ;;
  esac
}

get_file_size() {
  local f="$1"
  if [ ! -f "${f}" ]; then
    printf '0'
    return 0
  fi
  if stat -f%z "${f}" >/dev/null 2>&1; then
    stat -f%z "${f}"
  else
    stat -c%s "${f}"
  fi
}

# --- store registry (stores.conf) --------------------------------------
# Row format: name|origin|t1_replica|t2_remote|t3_archive_dir
# t2_remote is "user@host:path". A row starting with '#' or blank is
# skipped. Placeholder value CHANGE_ME in any field means "operator has not
# configured this yet" and preflight for that tier must refuse loudly.

resolve_stores_conf() {
  if [ -n "${KHIVE_BACKUP_CONF:-}" ]; then
    printf '%s' "${KHIVE_BACKUP_CONF}"
    return 0
  fi
  printf '%s' "${BACKUP_SCRIPT_DIR}/stores.conf"
}

load_store_row() {
  local store="$1" conf line name
  conf="$(resolve_stores_conf)"
  [ -f "${conf}" ] || bdie "store registry not found: ${conf} (set KHIVE_BACKUP_CONF to override)"
  while IFS= read -r line || [ -n "${line}" ]; do
    case "${line}" in
      ''|'#'*) continue ;;
    esac
    name="${line%%|*}"
    if [ "${name}" = "${store}" ]; then
      printf '%s' "${line}"
      return 0
    fi
  done <"${conf}"
  return 1
}

# Splits a store row into the STORE_* globals. Callers read these
# immediately after calling; they are not meant to survive further calls.
split_store_row() {
  local row="$1"
  local IFS='|'
  # shellcheck disable=SC2034 # consumed by callers via the STORE_* globals
  read -r STORE_NAME STORE_ORIGIN STORE_T1_REPLICA STORE_T2_REMOTE STORE_T3_ARCHIVE_DIR <<EOF
${row}
EOF
  STORE_ORIGIN="$(expand_path "${STORE_ORIGIN}")"
  STORE_T1_REPLICA="$(expand_path "${STORE_T1_REPLICA}")"
  STORE_T3_ARCHIVE_DIR="$(expand_path "${STORE_T3_ARCHIVE_DIR}")"
}

is_placeholder() {
  case "$1" in
    *CHANGE_ME*) return 0 ;;
    *) return 1 ;;
  esac
}

# --- locking (single per-store lock across all tiers) ------------------
# mkdir is atomic on every POSIX filesystem this ships on; stock macOS has
# no `flock` binary (util-linux only), so a directory-based lock avoids an
# extra dependency. A stale lock (owner pid no longer alive) is reclaimed
# once; a live lock makes the second invocation exit non-zero immediately —
# no queueing, per the ADR's single-writer-per-store requirement.
BACKUP_LOCK_DIR=""

acquire_store_lock() {
  local store="$1" lockdir held_pid
  mkdir -p "${KHIVE_BACKUP_ROOT}/lock"
  lockdir="${KHIVE_BACKUP_ROOT}/lock/${store}.lockdir"
  if mkdir "${lockdir}" 2>/dev/null; then
    echo "$$" >"${lockdir}/pid"
    BACKUP_LOCK_DIR="${lockdir}"
    return 0
  fi
  held_pid="$(cat "${lockdir}/pid" 2>/dev/null || true)"
  if [ -n "${held_pid}" ] && ! kill -0 "${held_pid}" 2>/dev/null; then
    rm -rf "${lockdir}"
    if mkdir "${lockdir}" 2>/dev/null; then
      echo "$$" >"${lockdir}/pid"
      BACKUP_LOCK_DIR="${lockdir}"
      return 0
    fi
  fi
  return 1
}

release_store_lock() {
  if [ -n "${BACKUP_LOCK_DIR}" ]; then
    rm -rf "${BACKUP_LOCK_DIR}"
    BACKUP_LOCK_DIR=""
  fi
}

# --- portable timeout ---------------------------------------------------
# Stock macOS ships no `timeout`(1). Rolled here instead of depending on
# GNU coreutils' timeout/gtimeout, which is Homebrew-only and not
# guaranteed present on an operator's box. Returns 124 on kill, matching
# GNU timeout's convention (used by callers to detect a timeout kill).
run_with_timeout() {
  local secs="$1"
  shift
  "$@" &
  local cmd_pid=$!
  local waited=0
  while kill -0 "${cmd_pid}" 2>/dev/null; do
    if [ "${waited}" -ge "${secs}" ]; then
      kill -TERM "${cmd_pid}" 2>/dev/null || true
      sleep 1
      kill -KILL "${cmd_pid}" 2>/dev/null || true
      wait "${cmd_pid}" 2>/dev/null || true
      return 124
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "${cmd_pid}"
}

# --- version preflight ---------------------------------------------------
# `sqlite3_rsync --version` is observed (Homebrew sqlite-rsync 3.53.3,
# 2026-07) to print only SQLITE_SOURCE_ID (a date + hash, e.g.
# "2026-06-26 20:14:12 d4c0e5...") with NO dotted-triplet semver at all —
# the shape is not the "x.y.z ..." format one might assume. Parse
# defensively: first look for a dotted-triplet anywhere in --version's
# output (covers builds that do print it); if absent, fall back to the
# version string statically linked into the binary's string table (a
# single unique "x.y.z" line, verified present in the Homebrew build).
extract_semver() {
  grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1
}

local_sqlite3_rsync_version() {
  local override="${KHIVE_BACKUP_LOCAL_VERSION:-}"
  if [ -n "${override}" ]; then
    printf '%s' "${override}"
    return 0
  fi
  local bin ver
  bin="$(command -v "${SQLITE3_RSYNC_BIN}" 2>/dev/null || true)"
  [ -n "${bin}" ] || return 1
  ver="$("${bin}" --version 2>&1 | extract_semver || true)"
  if [ -z "${ver}" ] && command -v strings >/dev/null 2>&1; then
    ver="$(strings "${bin}" 2>/dev/null | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | head -1)"
  fi
  [ -n "${ver}" ] || return 1
  printf '%s' "${ver}"
}

# $1 = "user@host" (the host half of a t2_remote spec, path stripped)
remote_sqlite3_rsync_version() {
  local user_host="$1" remote_bin out ver
  remote_bin="${KHIVE_BACKUP_REMOTE_SQLITE3_RSYNC:-sqlite3_rsync}"
  out="$("${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" \
    "v=\$(command -v '${remote_bin}' 2>/dev/null); [ -n \"\$v\" ] || exit 1; \
     out=\$(\"\$v\" --version 2>&1); ver=\$(printf '%s' \"\$out\" | grep -oE '[0-9]+\\.[0-9]+\\.[0-9]+' | head -1); \
     if [ -z \"\$ver\" ] && command -v strings >/dev/null 2>&1; then ver=\$(strings \"\$v\" 2>/dev/null | grep -E '^[0-9]+\\.[0-9]+\\.[0-9]+\$' | head -1); fi; \
     echo \"VERSION=\$ver\"" 2>&1)" || return 1
  ver="$(printf '%s\n' "${out}" | sed -n 's/^VERSION=//p' | tail -1)"
  [ -n "${ver}" ] || return 1
  printf '%s' "${ver}"
}

# Numeric x.y.z >= x.y.z comparison (no reliance on `sort -V`, which is
# GNU-only and absent on stock macOS `sort`).
version_ge() {
  local v1="$1" v2="$2" i
  local IFS=.
  # shellcheck disable=SC2206
  local a=(${v1}) b=(${v2})
  for i in 0 1 2; do
    local x="${a[i]:-0}" y="${b[i]:-0}"
    if [ "${x}" -gt "${y}" ] 2>/dev/null; then return 0; fi
    if [ "${x}" -lt "${y}" ] 2>/dev/null; then return 1; fi
  done
  return 0
}

# --- disk preflight -------------------------------------------------------

preflight_disk() {
  local origin="$1" dest_dir="$2" margin origin_size avail_kb avail_bytes needed
  margin="${KHIVE_BACKUP_MARGIN_BYTES}"
  origin_size="$(get_file_size "${origin}")"
  needed=$((origin_size + margin))
  mkdir -p "${dest_dir}"
  avail_kb="$(df -Pk "${dest_dir}" | awk 'NR==2{print $4}')"
  avail_bytes=$((avail_kb * 1024))
  if [ "${avail_bytes}" -lt "${needed}" ]; then
    blog "disk preflight FAILED: ${dest_dir} has ${avail_bytes} bytes free, need ${needed} (origin ${origin_size} + margin ${margin})"
    return 1
  fi
  return 0
}

# SSH-backed equivalent of preflight_disk for a remote destination
# directory: same origin-size-plus-margin accounting, `df -Pk` (POSIX-fixed
# columns, unlike `df -h` whose units vary by platform) run on the remote
# host via the same ssh invocation pattern used everywhere else in this
# script (BatchMode, ConnectTimeout, single quoted remote command). Returns
# 1 on any ssh failure, missing `df` output, or insufficient free space —
# callers treat all three as "cannot proceed" and refuse before touching
# remote staging or copying.
preflight_disk_remote() {
  local user_host="$1" origin="$2" dest_dir="$3" margin origin_size needed df_out avail_kb avail_bytes
  margin="${KHIVE_BACKUP_MARGIN_BYTES}"
  origin_size="$(get_file_size "${origin}")"
  needed=$((origin_size + margin))
  df_out="$("${SSH_BIN}" -o BatchMode=yes -o ConnectTimeout=10 "${user_host}" \
    "mkdir -p '${dest_dir}' && df -Pk '${dest_dir}'" 2>/dev/null)" || return 1
  avail_kb="$(printf '%s\n' "${df_out}" | awk 'NR==2{print $4}')"
  [ -n "${avail_kb}" ] || return 1
  avail_bytes=$((avail_kb * 1024))
  if [ "${avail_bytes}" -lt "${needed}" ]; then
    blog "remote disk preflight FAILED: ${user_host}:${dest_dir} has ${avail_bytes} bytes free, need ${needed} (origin ${origin_size} + margin ${margin})"
    return 1
  fi
  return 0
}

# --- JSONL event log -------------------------------------------------------

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

log_backup_event() {
  # store tier outcome duration_s bytes wal_before wal_after error
  local store="$1" tier="$2" outcome="$3" duration="$4" bytes="$5" wal_b="$6" wal_a="$7" err="$8"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  mkdir -p "${KHIVE_BACKUP_ROOT}/log"
  printf '{"ts":"%s","store":"%s","tier":"%s","outcome":"%s","duration_s":%s,"bytes":%s,"wal_before_bytes":%s,"wal_after_bytes":%s,"error":"%s"}\n' \
    "${ts}" "$(json_escape "${store}")" "$(json_escape "${tier}")" "$(json_escape "${outcome}")" \
    "${duration}" "${bytes}" "${wal_b}" "${wal_a}" "$(json_escape "${err}")" \
    >>"${KHIVE_BACKUP_ROOT}/log/backup-events.jsonl"
}

# --- alerting ---------------------------------------------------------
# Never put secrets in the message. Fallback on kkernel absence/failure is
# the JSONL row plus this process's own non-zero exit; launchd's
# last-exit-status is the detection path in that case.

send_backup_alert() {
  local msg oneline
  msg="$1"
  oneline="$(printf '%s' "${msg}" | tr '\n' ' ')"
  if command -v kkernel >/dev/null 2>&1; then
    kkernel exec "comm.send(to=\"${KHIVE_BACKUP_ALERT_ACTOR}\", content=\"$(printf '%s' "${oneline}" | sed 's/"/\\"/g')\")" >/dev/null 2>&1 || true
  fi
}

# --- staged sync / promote (local) -----------------------------------
# clone if the filesystem supports cheap CoW (APFS `cp -c`), else a plain
# copy — either way this seeds the staging file with the last promoted
# replica so sqlite3_rsync's page diff against it stays meaningful across
# runs, while the promoted replica itself is never touched until the new
# staging copy has synced and passed its check.
clone_or_copy() {
  local src="$1" dst="$2"
  if [ ! -f "${src}" ]; then
    return 0
  fi
  if cp -c "${src}" "${dst}" 2>/dev/null; then
    return 0
  fi
  cp "${src}" "${dst}"
}

promote_replica_local() {
  local staging="$1" dest="$2" suffix
  mv -f "${staging}" "${dest}"
  for suffix in -wal -shm; do
    if [ -f "${staging}${suffix}" ]; then
      mv -f "${staging}${suffix}" "${dest}${suffix}"
    else
      rm -f "${dest}${suffix}"
    fi
  done
}
