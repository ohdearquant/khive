#!/usr/bin/env bash
# bootstrap.sh: off-Studio Linux box bootstrap for the khive substrate.
#
# Idempotent: safe to re-run. Each stage checks current state before acting so a
# second invocation (e.g. to pick up a new release) only redoes the parts that
# changed (git update, rebuild, binary install, systemd restart).
#
# Usage:
#   scripts/box/bootstrap.sh [REPO_DIR]
#
# Env overrides:
#   REPO_DIR  Local checkout path (default: ~/khive; overridden by $1 if given)
#   REPO_URL  Git remote to clone (default: https://github.com/ohdearquant/khive.git)
#   REF       Branch/tag/commit to build (default: main)
#
# This script only touches this box: apt packages, ~/.cargo, a checkout under
# REPO_DIR, ~/.cargo/bin/kkernel, and a systemd --user unit. It does not touch
# ~/.khive/khive.db or ~/.khive/.env. See scripts/box/README.md for the DB
# migration and secrets setup steps that precede/follow this script.

set -euo pipefail

stage() {
  printf '\n==> [%s] %s\n' "$(date -u +%H:%M:%S)" "$1"
}

# Extract `id` from the `[actor]` TOML table in a khive config file. Prints the
# value (unquoted, trimmed) on stdout and returns 0 when found and non-empty;
# returns 1 (silently) otherwise. This is a lightweight section-scoped scan for
# a bootstrap sanity check, not a full TOML parser: it does not handle inline
# comments after the value, multi-line strings, or a re-opened [actor] table
# further down the file (first occurrence wins, matching how the runtime's
# TOML deserializer would also reject a duplicate table).
actor_id_from_config() {
  local cfg="$1" raw val
  [ -f "$cfg" ] || return 1
  raw="$(awk '
    /^[[:space:]]*\[actor\][[:space:]]*$/ { insec=1; next }
    /^[[:space:]]*\[/ { insec=0 }
    insec && /^[[:space:]]*id[[:space:]]*=/ { print; exit }
  ' "$cfg")"
  [ -n "$raw" ] || return 1
  val="${raw#*=}"
  val="${val#"${val%%[![:space:]]*}"}"
  val="${val%"${val##*[![:space:]]}"}"
  val="${val%\"}"; val="${val#\"}"
  val="${val%\'}"; val="${val#\'}"
  [ -n "$val" ] || return 1
  printf '%s\n' "$val"
}

# Resolve one of the three KHIVE_EMAIL_OAUTH_* values the way
# load_khive_dotenv() would (crates/kkernel/src/main.rs): a real process
# environment variable wins; otherwise fall back to parsing (never sourcing)
# the given dotenv-style file, so a value containing shell metacharacters
# cannot execute code. dotenvy::from_path() never overrides an already-set
# variable and, for a key repeated in the file, applies the first occurrence
# (dotenvy 0.15.7 src/iter.rs Iter::load). This helper mirrors both rules.
# Prints the resolved value on stdout and returns 0 when found and non-empty;
# returns 1 (silently) otherwise.
oauth_var() {
  local name="$1" file="$2" existing raw val
  case "$name" in
    KHIVE_EMAIL_OAUTH_CLIENT_ID) existing="${KHIVE_EMAIL_OAUTH_CLIENT_ID:-}" ;;
    KHIVE_EMAIL_OAUTH_CLIENT_SECRET) existing="${KHIVE_EMAIL_OAUTH_CLIENT_SECRET:-}" ;;
    KHIVE_EMAIL_OAUTH_TENANT_ID) existing="${KHIVE_EMAIL_OAUTH_TENANT_ID:-}" ;;
    *) existing="" ;;
  esac
  if [ -n "$existing" ]; then
    printf '%s\n' "$existing"
    return 0
  fi
  [ -f "$file" ] || return 1
  raw="$(grep -E "^[[:space:]]*${name}[[:space:]]*=" "$file" 2>/dev/null | head -n 1)"
  [ -n "$raw" ] || return 1
  val="${raw#*=}"
  val="${val#"${val%%[![:space:]]*}"}"
  val="${val%"${val##*[![:space:]]}"}"
  case "$val" in
    \"*\") val="${val#\"}"; val="${val%\"}" ;;
    \'*\') val="${val#\'}"; val="${val%\'}" ;;
  esac
  [ -n "$val" ] || return 1
  printf '%s\n' "$val"
}

# Mask a GUID-shaped value for display: first 6 characters, then an ellipsis
# marker. Only ever called on a client_id or tenant_id (see the email auth
# smoke below); never called on the client secret.
mask_value() {
  printf '%s...\n' "${1:0:6}"
}

# Replace every literal occurrence of `value` in `text` with its masked
# form. Used only to scrub a client_id or tenant_id out of Microsoft's
# error/error_description text before printing it, since that text
# sometimes echoes a configured id verbatim (e.g. "Application with
# identifier 'xxx' was not found in the directory"). Client ids and tenant
# ids are Microsoft-issued GUIDs, which cannot contain glob metacharacters,
# so bash's `${text//pattern/repl}` pattern matching is a safe literal
# replacement here.
scrub_value() {
  local text="$1" value="$2" masked
  if [ -z "$value" ]; then
    printf '%s\n' "$text"
    return 0
  fi
  masked="$(mask_value "$value")"
  printf '%s\n' "${text//$value/$masked}"
}

# Extract a top-level string field from a JSON object. Prefers python3
# (correct quote/escape handling); falls back to a grep/sed pass that
# assumes a flat, single-line-ish body with no escaped quotes or embedded
# newlines inside the value, which is what Microsoft's token endpoint
# returns for its error/error_description fields. Prints the value on
# stdout and returns 0 when found and non-empty; returns 1 (silently)
# otherwise.
extract_json_field() {
  local json="$1" field="$2"
  if command -v python3 >/dev/null 2>&1; then
    printf '%s' "$json" | python3 -c '
import json, sys
try:
    data = json.loads(sys.stdin.read())
except Exception:
    sys.exit(1)
value = data.get(sys.argv[1])
if not isinstance(value, str) or not value:
    sys.exit(1)
sys.stdout.write(value)
' "$field"
    return $?
  fi
  local match
  match="$(printf '%s' "$json" | grep -o "\"${field}\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -n 1)"
  [ -n "$match" ] || return 1
  match="${match#*:}"
  match="${match#"${match%%[![:space:]]*}"}"
  match="${match#\"}"
  match="${match%\"}"
  [ -n "$match" ] || return 1
  printf '%s\n' "$match"
}

# --- Guard: Linux only ------------------------------------------------------
if [ "$(uname -s)" != "Linux" ]; then
  echo "ERROR: bootstrap.sh targets Linux (systemd --user units, apt). Detected: $(uname -s)." >&2
  echo "This kit is for the off-Studio VPS box, not the Mac dev machine." >&2
  exit 1
fi

REPO_DIR="${1:-${REPO_DIR:-$HOME/khive}}"
REPO_URL="${REPO_URL:-https://github.com/ohdearquant/khive.git}"
REF="${REF:-main}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

echo "==> khive box bootstrap"
echo "    REPO_DIR: $REPO_DIR"
echo "    REPO_URL: $REPO_URL"
echo "    REF:      $REF"

# --- Stage (a): apt dependencies --------------------------------------------
stage "Installing apt dependencies"
if command -v apt-get >/dev/null 2>&1; then
  $SUDO apt-get update
  $SUDO apt-get install -y \
    build-essential clang pkg-config git sqlite3 curl ca-certificates
else
  echo "ERROR: apt-get not found. This script targets Debian/Ubuntu." >&2
  echo "Install manually and re-run: build-essential clang pkg-config git sqlite3 curl ca-certificates" >&2
  exit 1
fi

# --- Stage (b): rustup stable -----------------------------------------------
stage "Ensuring Rust toolchain"
if command -v cargo >/dev/null 2>&1; then
  echo "Rust toolchain already present: $(cargo --version)"
else
  echo "Installing rustup (stable toolchain)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
fi
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

# --- Stage (c): clone or update repo checkout -------------------------------
stage "Syncing repo checkout at $REPO_DIR (ref: $REF)"
if [ -d "$REPO_DIR/.git" ]; then
  echo "Existing checkout found, updating..."
  git -C "$REPO_DIR" fetch origin "$REF"
  git -C "$REPO_DIR" checkout "$REF" 2>/dev/null || git -C "$REPO_DIR" checkout -b "$REF" "origin/$REF"
  git -C "$REPO_DIR" reset --hard "origin/$REF" 2>/dev/null || git -C "$REPO_DIR" reset --hard "$REF"
else
  echo "No checkout found, cloning..."
  git clone "$REPO_URL" "$REPO_DIR"
  git -C "$REPO_DIR" checkout "$REF"
fi
# REPO_DIR is a deployment checkout, not a workspace for local edits. The
# reset --hard on update is intentional so re-running this script always
# converges on the exact state of $REF upstream.

# --- Stage (d): release build (must include channel-email) -----------------
stage "Building kkernel (release, channel-email); this can take several minutes"
(cd "$REPO_DIR/crates" && cargo build --release -p kkernel --features channel-email)

SRC="$REPO_DIR/crates/target/release/kkernel"
if [ ! -f "$SRC" ]; then
  echo "ERROR: build artifact $SRC missing after build." >&2
  exit 1
fi

# --- Stage (e): install binary ----------------------------------------------
stage "Installing kkernel to ~/.cargo/bin"
mkdir -p "$HOME/.cargo/bin"
DEST="$HOME/.cargo/bin/kkernel"
# Atomic mv into place. Unlike the Mac `make local` target, this does NOT pkill
# running kkernel processes first. On Linux, replacing the inode under a running
# process's feet is safe (the old process keeps its already-open file handle to
# the old inode until it exits/restarts). The systemd restart in stage (f) is
# what actually swaps the running process over to the new binary.
cp "$SRC" "$DEST.new"
chmod +x "$DEST.new"
mv "$DEST.new" "$DEST"
echo "Installed: $DEST ($("$DEST" --version 2>&1 || echo 'version check failed'))"

# --- Stage (f): systemd user unit -------------------------------------------
stage "Installing systemd --user unit"
mkdir -p "$HOME/.config/systemd/user"
mkdir -p "$HOME/.khive"
cp "$SCRIPT_DIR/kkernel.service" "$HOME/.config/systemd/user/kkernel.service"
systemctl --user daemon-reload
systemctl --user enable kkernel.service
systemctl --user restart kkernel.service
echo "systemd --user unit installed and (re)started."
echo "NOTE: for this unit to survive logout and start on boot, run once:"
echo "      loginctl enable-linger $USER"

# --- Stage (g): smoke test ---------------------------------------------------
stage "Smoke test"
SOCK="$HOME/.khive/khived.sock"
echo "Waiting up to 10s for daemon socket at $SOCK..."
for _ in $(seq 1 10); do
  [ -S "$SOCK" ] && break
  sleep 1
done

if "$DEST" exec 'stats()'; then
  echo "Smoke test passed: kkernel exec stats() responded."
else
  echo "WARNING: smoke test call failed. Check: systemctl --user status kkernel.service" >&2
  echo "         and: journalctl --user -u kkernel.service -n 50 --no-pager" >&2
fi

# --- Check: actor identity (issue #200) -------------------------------------
# comm.send stamps from_actor from the dispatch token's actor. A daemon with no
# [actor] id configured mints anonymous tokens, so comm.send stamps
# from_actor="local"; a reply routed back to that outbound note then resolves
# to_actor="local" instead of this box's inbox: a silent misroute, not an
# error either side would notice on its own. This check only verifies a human
# has deliberately set an id in this daemon's config; it does not invent one
# (the right id is deployment-specific, see scripts/box/README.md).
CONFIG_TOML="$HOME/.khive/config.toml"
ACTOR_ID="$(actor_id_from_config "$CONFIG_TOML" || true)"
if [ -z "$ACTOR_ID" ]; then
  echo "" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  echo "WARNING: no [actor] id set in $CONFIG_TOML" >&2
  echo "         This daemon will mint anonymous dispatch tokens. comm.send" >&2
  echo "         will stamp from_actor=\"local\", and replies routed back to" >&2
  echo "         it will silently misroute instead of reaching this box's" >&2
  echo "         inbox (issue #200). Fix: create/edit $CONFIG_TOML with:" >&2
  echo "           [actor]" >&2
  echo "           id = \"lambda:<you>\"" >&2
  echo "         then: systemctl --user restart kkernel.service" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
else
  echo "Actor identity: [actor] id = \"$ACTOR_ID\" ($CONFIG_TOML)"
fi

# --- Check: email channel auth smoke (config.rs OAuth contract) -------------
# The email channel disables itself with a warning log, not a crash, when its
# OAuth config is incomplete or the client secret is invalid or expired
# (crates/khive-channel-email/src/config.rs). On a headless box nobody is
# tailing journalctl by default, so a rotated or expired secret otherwise
# means email quietly stops working with nothing loud to notice it. This
# check calls the same client_credentials token endpoint the daemon itself
# would use and makes that failure loud at bootstrap time. It never prints
# the client secret, in any form, on any path.
stage "Email channel auth smoke"

# The whole stage is best-effort: a curl failure, a malformed response, or a
# missing python3 fallback path are all reported as warnings, matching the
# non-fatal convention of the stats() smoke test and the actor-identity check
# above (a box without email configured, or one hitting a transient network
# blip during bootstrap, is not a bootstrap failure). set +e removes any risk
# that a failing command in this stage trips the script's set -e and aborts
# bootstrap; it is restored immediately after.
set +e

DOTENV="$HOME/.khive/.env"
EMAIL_CLIENT_ID="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$DOTENV")"
EMAIL_CLIENT_SECRET="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_SECRET "$DOTENV")"
EMAIL_TENANT_ID="$(oauth_var KHIVE_EMAIL_OAUTH_TENANT_ID "$DOTENV")"

EMAIL_OAUTH_PRESENT=0
[ -n "$EMAIL_CLIENT_ID" ] && EMAIL_OAUTH_PRESENT=$((EMAIL_OAUTH_PRESENT + 1))
[ -n "$EMAIL_CLIENT_SECRET" ] && EMAIL_OAUTH_PRESENT=$((EMAIL_OAUTH_PRESENT + 1))
[ -n "$EMAIL_TENANT_ID" ] && EMAIL_OAUTH_PRESENT=$((EMAIL_OAUTH_PRESENT + 1))

if [ "$EMAIL_OAUTH_PRESENT" -eq 0 ]; then
  echo "email OAuth not configured; skipping auth smoke"
elif [ "$EMAIL_OAUTH_PRESENT" -lt 3 ]; then
  EMAIL_MISSING=""
  [ -z "$EMAIL_CLIENT_ID" ] && EMAIL_MISSING="$EMAIL_MISSING KHIVE_EMAIL_OAUTH_CLIENT_ID"
  [ -z "$EMAIL_CLIENT_SECRET" ] && EMAIL_MISSING="$EMAIL_MISSING KHIVE_EMAIL_OAUTH_CLIENT_SECRET"
  [ -z "$EMAIL_TENANT_ID" ] && EMAIL_MISSING="$EMAIL_MISSING KHIVE_EMAIL_OAUTH_TENANT_ID"
  echo "" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  echo "WARNING: partial email OAuth config in $DOTENV" >&2
  echo "         missing:$EMAIL_MISSING" >&2
  echo "         KHIVE_EMAIL_OAUTH_CLIENT_ID, KHIVE_EMAIL_OAUTH_CLIENT_SECRET," >&2
  echo "         and KHIVE_EMAIL_OAUTH_TENANT_ID must be set together or not" >&2
  echo "         at all (crates/khive-channel-email/src/config.rs). With only" >&2
  echo "         some of the three set, the email channel disables itself" >&2
  echo "         with a warning log rather than crashing the daemon, so this" >&2
  echo "         is easy to miss on a headless box. Set all three, or remove" >&2
  echo "         all three to use Basic auth (KHIVE_EMAIL_PASSWORD) instead," >&2
  echo "         then:" >&2
  echo "           systemctl --user restart kkernel.service" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
else
  TOKEN_URL="https://login.microsoftonline.com/${EMAIL_TENANT_ID}/oauth2/v2.0/token"
  EMAIL_RESPONSE="$(curl -sS --max-time 15 \
    --data-urlencode "grant_type=client_credentials" \
    --data-urlencode "scope=https://outlook.office365.com/.default" \
    --data-urlencode "client_id=${EMAIL_CLIENT_ID}" \
    --data-urlencode "client_secret=${EMAIL_CLIENT_SECRET}" \
    "$TOKEN_URL" 2>&1)"
  EMAIL_CURL_RC=$?

  if [ "$EMAIL_CURL_RC" -ne 0 ]; then
    echo "" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "WARNING: email auth smoke could not reach the token endpoint (curl exit $EMAIL_CURL_RC)" >&2
    echo "         Check outbound HTTPS access to login.microsoftonline.com" >&2
    echo "         from this box." >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  elif printf '%s' "$EMAIL_RESPONSE" | grep -q '"access_token"'; then
    echo "Email channel auth smoke: PASS (client_id $(mask_value "$EMAIL_CLIENT_ID"))"
  else
    EMAIL_ERR="$(extract_json_field "$EMAIL_RESPONSE" "error")"
    EMAIL_ERR_DESC="$(extract_json_field "$EMAIL_RESPONSE" "error_description")"
    EMAIL_ERR="$(scrub_value "$EMAIL_ERR" "$EMAIL_CLIENT_ID")"
    EMAIL_ERR="$(scrub_value "$EMAIL_ERR" "$EMAIL_TENANT_ID")"
    EMAIL_ERR_DESC="$(scrub_value "$EMAIL_ERR_DESC" "$EMAIL_CLIENT_ID")"
    EMAIL_ERR_DESC="$(scrub_value "$EMAIL_ERR_DESC" "$EMAIL_TENANT_ID")"
    echo "" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "WARNING: email channel auth smoke FAILED" >&2
    echo "         error: ${EMAIL_ERR:-<unparsed response>}" >&2
    echo "         error_description: ${EMAIL_ERR_DESC:-<unparsed response>}" >&2
    echo "         AADSTS7000222 = expired client secret. AADSTS7000215 =" >&2
    echo "         invalid client secret. Entra portal: App registrations >" >&2
    echo "         (this app) > Certificates & secrets. Client secrets have" >&2
    echo "         bounded lifetimes and this one may have expired. After" >&2
    echo "         rotating it, update $DOTENV and re-run:" >&2
    echo "           scripts/box/bootstrap.sh" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  fi
fi

set -e

cat <<'EOF'

==> Bootstrap complete. Next steps:

  1. Place secrets:
       cp scripts/box/env.template ~/.khive/.env
       # edit ~/.khive/.env with real values (see scripts/box/README.md)
       scripts/box/bootstrap.sh
       # re-running exercises the email auth smoke against the new secrets.
       # systemctl --user restart kkernel.service also picks up the change,
       # but only bootstrap.sh runs the smoke.

  2. If this box is replacing the Mac as the primary substrate host, copy the
     existing khive.db over (see scripts/box/README.md "Moving the database").

  3. Verify boot persistence:
       loginctl enable-linger $USER

  4. From the Mac, forward the daemon socket over SSH if you need direct access
     (see scripts/box/README.md "Mac-side access").

  5. Set this box's actor identity if the check above warned about it
     (see scripts/box/README.md "Actor identity").

EOF
