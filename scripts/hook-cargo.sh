#!/usr/bin/env bash
# Run one of the pre-commit cargo hooks, serialized behind the machine's build
# locks when the machine declares any.
#
# A commit hook that compiles the workspace is a build like any other. On a
# machine where builds queue behind lock files, a hook that skips the queue runs
# beside the build that holds it, so the hook takes the same locks the build
# scripts take. Which locks, and in what mode, is machine configuration, not a
# property of this repository: the list lives in
# $XDG_CONFIG_HOME/khive/cargo-hook-locks (default ~/.config/khive/...), one
# lock per line as `<shared|exclusive> <path>`, outermost first. Blank lines and
# `#` comments are ignored. No file, or an empty one, means run unserialized,
# which is what CI and a single-developer machine want.
#
# usage: scripts/hook-cargo.sh fmt|clippy
set -euo pipefail

case "${1:-}" in
  fmt) cmd=(cargo fmt --all -- --check) ;;
  clippy) cmd=(cargo clippy --workspace --all-targets -- -D warnings) ;;
  *) echo "hook-cargo.sh: expected fmt or clippy, got '${1:-}'" >&2; exit 2 ;;
esac

locks="${XDG_CONFIG_HOME:-$HOME/.config}/khive/cargo-hook-locks"
wrapper=()
if [ -f "$locks" ]; then
  if ! command -v flock >/dev/null 2>&1; then
    echo "hook-cargo.sh: $locks declares locks but flock is not on PATH; refusing to run unserialized" >&2
    exit 3
  fi
  while read -r mode path; do
    case "$mode" in
      ""|\#*) continue ;;
      shared) wrapper+=(flock -o -s -w 1800 "$path") ;;
      exclusive) wrapper+=(flock -o -w 1800 "$path") ;;
      *) echo "hook-cargo.sh: $locks: unknown lock mode '$mode' (want shared or exclusive)" >&2; exit 3 ;;
    esac
  done < "$locks"
fi

cd "$(dirname "$0")/../crates"
if [ "${#wrapper[@]}" -gt 0 ]; then
  exec "${wrapper[@]}" "${cmd[@]}"
fi
exec "${cmd[@]}"
