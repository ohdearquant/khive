#!/usr/bin/env bash
# Idempotent installer for the ADR-100 backup LaunchAgents.
#
# Usage:
#   install-backup.sh install <t1|t2|t3> <store-name> [--interval SECONDS]
#   install-backup.sh install-all [--interval-t1 S] [--interval-t2 S] [--interval-t3 S]
#   install-backup.sh status [<t1|t2|t3> <store-name>]
#   install-backup.sh uninstall <t1|t2|t3> <store-name>
#
# Mirrors the render/preserve/refuse pattern from khive-cloud's
# deploy/mini/install.sh (PR #48): this installer owns every rendered value
# in an installed plist. On a rerun, any value not explicitly given (flag or
# env var) is read back from the CURRENTLY INSTALLED plist via PlistBuddy
# and carried forward unchanged — an upgrade never clobbers an
# operator-edited interval. The installer refuses to touch the installed
# plist until a fully valid render succeeds in a temp file first.
#
# Test mode: set KHIVE_BACKUP_INSTALL_TEST=1 (combine with HOME=<sandbox>)
# to skip all launchctl calls (logged, not run) — see test_backup.sh.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
# shellcheck source=./lib.sh
. "${SCRIPT_DIR}/lib.sh"

TEMPLATE="${SCRIPT_DIR}/com.khive.backup.plist.template"
BACKUP_SH="${KHIVE_BACKUP_SCRIPT_PATH:-${SCRIPT_DIR}/khive-backup.sh}"
PLIST_BUDDY="/usr/libexec/PlistBuddy"
LOG_DIR="${HOME}/Library/Logs/khive-backup"
UID_NUM="$(id -u)"
DOMAIN_TARGET="gui/${UID_NUM}"

DEFAULT_INTERVAL_T1=900
DEFAULT_INTERVAL_T2=3600
DEFAULT_INTERVAL_T3=604800

log() { printf '[install-backup] %s\n' "$*"; }
die() {
  printf '[install-backup] ERROR: %s\n' "$*" >&2
  exit 1
}

test_mode() {
  [ "${KHIVE_BACKUP_INSTALL_TEST:-0}" = "1" ]
}

plist_label() {
  local tier="$1" store="$2"
  printf 'com.khive.backup.%s.%s' "${tier}" "${store}"
}

plist_dest() {
  local tier="$1" store="$2"
  printf '%s/Library/LaunchAgents/%s.plist' "${HOME}" "$(plist_label "${tier}" "${store}")"
}

default_interval_for() {
  case "$1" in
    t1) echo "${DEFAULT_INTERVAL_T1}" ;;
    t2) echo "${DEFAULT_INTERVAL_T2}" ;;
    t3) echo "${DEFAULT_INTERVAL_T3}" ;;
    *) die "unknown tier '$1'" ;;
  esac
}

require_launchctl() {
  command -v launchctl >/dev/null 2>&1 || die "launchctl not found; this installer targets macOS."
}

launchctl_reload() {
  local target="$1" dest="$2"
  if test_mode; then
    log "[test mode] skipping launchctl bootout/bootstrap/kickstart for ${target}"
    return 0
  fi
  launchctl bootout "${target}" 2>/dev/null || true
  launchctl bootstrap "${DOMAIN_TARGET}" "${dest}"
  launchctl kickstart -k "${target}"
}

launchctl_bootout_only() {
  local target="$1"
  if test_mode; then
    log "[test mode] skipping launchctl bootout for ${target}"
    return 0
  fi
  launchctl bootout "${target}" 2>/dev/null || true
}

read_installed_interval() {
  local dest="$1"
  [ -f "${dest}" ] || return 0
  "${PLIST_BUDDY}" -c "Print :StartInterval" "${dest}" 2>/dev/null || true
}

sed_escape_replacement() {
  printf '%s' "$1" | sed -e 's/[\&|]/\\&/g'
}

render_plist() {
  local dest_tmp="$1" label="$2" tier="$3" store="$4" interval="$5" script="$6" log_dir="$7" root="$8" conf="$9"
  local esc_label esc_tier esc_store esc_interval esc_script esc_logdir esc_root esc_conf
  esc_label="$(sed_escape_replacement "${label}")"
  esc_tier="$(sed_escape_replacement "${tier}")"
  esc_store="$(sed_escape_replacement "${store}")"
  esc_interval="$(sed_escape_replacement "${interval}")"
  esc_script="$(sed_escape_replacement "${script}")"
  esc_logdir="$(sed_escape_replacement "${log_dir}")"
  esc_root="$(sed_escape_replacement "${root}")"
  esc_conf="$(sed_escape_replacement "${conf}")"
  sed \
    -e "s|__KHIVE_BACKUP_LABEL__|${esc_label}|g" \
    -e "s|__KHIVE_BACKUP_TIER__|${esc_tier}|g" \
    -e "s|__KHIVE_BACKUP_STORE__|${esc_store}|g" \
    -e "s|__KHIVE_BACKUP_INTERVAL__|${esc_interval}|g" \
    -e "s|__KHIVE_BACKUP_SCRIPT__|${esc_script}|g" \
    -e "s|__KHIVE_BACKUP_LOG_DIR__|${esc_logdir}|g" \
    -e "s|__KHIVE_BACKUP_ROOT__|${esc_root}|g" \
    -e "s|__KHIVE_BACKUP_CONF__|${esc_conf}|g" \
    "${TEMPLATE}" >"${dest_tmp}"
}

cmd_install_one() {
  local tier="$1" store="$2" interval_cli="${3:-}"
  local label dest target interval tmp_plist root conf

  case "${tier}" in t1|t2|t3) ;; *) die "unknown tier '${tier}' (expected t1, t2, or t3)" ;; esac
  load_store_row "${store}" >/dev/null || die "no store named '${store}' in $(resolve_stores_conf)"

  label="$(plist_label "${tier}" "${store}")"
  dest="$(plist_dest "${tier}" "${store}")"
  target="${DOMAIN_TARGET}/${label}"
  root="${KHIVE_BACKUP_ROOT}"
  conf="$(resolve_stores_conf)"

  if [ -n "${interval_cli}" ]; then
    interval="${interval_cli}"
  else
    local existing
    existing="$(read_installed_interval "${dest}")"
    if [ -n "${existing}" ]; then
      interval="${existing}"
    else
      interval="$(default_interval_for "${tier}")"
    fi
  fi

  [ -f "${TEMPLATE}" ] || die "plist template not found: ${TEMPLATE}"
  [ -x "${BACKUP_SH}" ] || die "khive-backup.sh not found or not executable: ${BACKUP_SH}"

  mkdir -p "${LOG_DIR}"
  mkdir -p "$(dirname -- "${dest}")"

  tmp_plist="$(mktemp "${TMPDIR:-/tmp}/${label}.XXXXXX.plist")"
  trap 'rm -f "${tmp_plist}"' RETURN

  render_plist "${tmp_plist}" "${label}" "${tier}" "${store}" "${interval}" "${BACKUP_SH}" "${LOG_DIR}" "${root}" "${conf}"

  if ! plutil -lint "${tmp_plist}" >/dev/null; then
    die "rendered plist failed plutil -lint; ${dest} was NOT touched"
  fi
  if grep -q '__KHIVE_BACKUP_' "${tmp_plist}"; then
    die "a template placeholder was not substituted; ${dest} was NOT touched"
  fi

  log "installing ${label} (interval=${interval}s) -> ${dest}"
  mv -f "${tmp_plist}" "${dest}"
  trap - RETURN

  launchctl_reload "${target}" "${dest}"
  log "installed ${label}. Check with: $0 status ${tier} ${store}"
}

cmd_install_all() {
  local interval_t1="${1:-}" interval_t2="${2:-}" interval_t3="${3:-}"
  local conf line name
  conf="$(resolve_stores_conf)"
  [ -f "${conf}" ] || die "store registry not found: ${conf}"
  while IFS= read -r line || [ -n "${line}" ]; do
    case "${line}" in ''|'#'*) continue ;; esac
    name="${line%%|*}"
    cmd_install_one t1 "${name}" "${interval_t1}"
    cmd_install_one t2 "${name}" "${interval_t2}"
    cmd_install_one t3 "${name}" "${interval_t3}"
  done <"${conf}"
}

cmd_status_one() {
  local tier="$1" store="$2" dest target
  dest="$(plist_dest "${tier}" "${store}")"
  target="${DOMAIN_TARGET}/$(plist_label "${tier}" "${store}")"
  log "=== ${tier}/${store} ==="
  log "plist installed: $( [ -f "${dest}" ] && echo yes || echo no ) (${dest})"
  if [ -f "${dest}" ]; then
    log "configured interval: $(read_installed_interval "${dest}")s"
  fi
  if launchctl print "${target}" >/tmp/khive-backup-status.$$ 2>&1; then
    cat /tmp/khive-backup-status.$$
  else
    log "not loaded"
  fi
  rm -f /tmp/khive-backup-status.$$
}

cmd_status_all() {
  local conf line name
  conf="$(resolve_stores_conf)"
  [ -f "${conf}" ] || die "store registry not found: ${conf}"
  while IFS= read -r line || [ -n "${line}" ]; do
    case "${line}" in ''|'#'*) continue ;; esac
    name="${line%%|*}"
    for tier in t1 t2 t3; do
      cmd_status_one "${tier}" "${name}"
    done
  done <"${conf}"
}

cmd_uninstall() {
  local tier="$1" store="$2" dest target
  case "${tier}" in t1|t2|t3) ;; *) die "unknown tier '${tier}'" ;; esac
  dest="$(plist_dest "${tier}" "${store}")"
  target="${DOMAIN_TARGET}/$(plist_label "${tier}" "${store}")"
  log "stopping $(plist_label "${tier}" "${store}")"
  launchctl_bootout_only "${target}"
  rm -f "${dest}"
  log "removed ${dest}"
}

main() {
  require_launchctl
  case "${1:-}" in
    install)
      shift
      [ "$#" -ge 2 ] || die "usage: $0 install <t1|t2|t3> <store-name> [--interval SECONDS]"
      local tier="$1" store="$2" interval=""
      shift 2
      while [ "$#" -gt 0 ]; do
        case "$1" in
          --interval)
            [ "$#" -ge 2 ] || die "--interval requires a value"
            interval="$2"
            shift 2
            ;;
          *) die "unknown argument: $1" ;;
        esac
      done
      cmd_install_one "${tier}" "${store}" "${interval}"
      ;;
    install-all)
      shift
      local it1="" it2="" it3=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          --interval-t1) it1="$2"; shift 2 ;;
          --interval-t2) it2="$2"; shift 2 ;;
          --interval-t3) it3="$2"; shift 2 ;;
          *) die "unknown argument: $1" ;;
        esac
      done
      cmd_install_all "${it1}" "${it2}" "${it3}"
      ;;
    status)
      shift
      if [ "$#" -ge 2 ]; then
        cmd_status_one "$1" "$2"
      else
        cmd_status_all
      fi
      ;;
    uninstall)
      shift
      [ "$#" -ge 2 ] || die "usage: $0 uninstall <t1|t2|t3> <store-name>"
      cmd_uninstall "$1" "$2"
      ;;
    *)
      echo "usage: $0 install <t1|t2|t3> <store-name> [--interval S] | install-all | status [<t1|t2|t3> <store-name>] | uninstall <t1|t2|t3> <store-name>" >&2
      exit 1
      ;;
  esac
}

main "$@"
