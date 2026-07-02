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

cat <<'EOF'

==> Bootstrap complete. Next steps:

  1. Place secrets:
       cp scripts/box/env.template ~/.khive/.env
       # edit ~/.khive/.env with real values (see scripts/box/README.md)
       systemctl --user restart kkernel.service

  2. If this box is replacing the Mac as the primary substrate host, copy the
     existing khive.db over (see scripts/box/README.md "Moving the database").

  3. Verify boot persistence:
       loginctl enable-linger $USER

  4. From the Mac, forward the daemon socket over SSH if you need direct access
     (see scripts/box/README.md "Mac-side access").

  5. Set this box's actor identity if the check above warned about it
     (see scripts/box/README.md "Actor identity").

EOF
