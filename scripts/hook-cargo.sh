#!/usr/bin/env bash
# Run one of the pre-commit cargo hooks, serialized behind the machine's build
# locks when the machine declares any.
#
# A commit hook that compiles the workspace is a build like any other. On a
# machine where builds queue behind lock files, a hook that skips the queue runs
# beside the build that holds it, so the hook takes the same locks the build
# scripts take.
#
# That argument reaches clippy and not fmt, so only clippy takes the locks.
# `cargo fmt --all -- --check` compiles nothing: measured on this workspace it
# runs in about seven seconds, spawns rustfmt and never rustc, and leaves
# target/ untouched. Serializing it bought nothing and cost every commit the
# remainder of whatever build held the lock -- observed at eight minutes, with
# two commits queued behind it. An unserialized fmt can now run beside a build,
# which is real but below the noise floor of a machine already compiling; the
# symptom if that is wrong would be build-time variance tracking commit
# frequency.
#
# Which locks, and in what mode, is machine configuration, not a
# property of this repository: the list lives in
# $XDG_CONFIG_HOME/khive/cargo-hook-locks (default ~/.config/khive/...), one
# lock per line as `<shared|exclusive> <path>`, outermost first. Blank lines and
# `#` comments are ignored. No file, or an empty one, means run unserialized,
# which is what CI and a single-developer machine want.
#
# A commit issued from inside a script that already holds one of these locks
# must not queue behind its own ancestor: flock has no reentrancy, so the hook
# would wait out the full timeout and then refuse. Before taking a lock the hook
# lists the lock file's holders and, when one of them is an ancestor of this
# process, runs that step without the lock (it is already inside the queue).
# An unrelated holder still queues as written. Holder detection needs ps and
# lsof; when process inspection is unavailable, the hook takes every declared
# lock instead of inferring that an ancestor already holds it.
#
# usage: scripts/hook-cargo.sh fmt|clippy
set -euo pipefail

case "${1:-}" in
  fmt) cmd=(cargo fmt --all -- --check) ;;
  clippy) cmd=(cargo clippy --workspace --all-targets -- -D warnings) ;;
  *) echo "hook-cargo.sh: expected fmt or clippy, got '${1:-}'" >&2; exit 2 ;;
esac

# Space-separated pids from this process up to init; used to recognise a lock
# held by the script that issued the commit.
ancestors=""
pid=$$
while [ -n "$pid" ] && [ "$pid" -gt 1 ]; do
  ancestors="$ancestors $pid"
  if ! pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' '); then
    # Sandboxed callers may be unable to inspect even their parent. This only
    # disables the reentrancy optimization; the configured locks still apply.
    ancestors=""
    break
  fi
done

lsof_bin=$(command -v lsof || true)
[ -z "$lsof_bin" ] && [ -x /usr/sbin/lsof ] && lsof_bin=/usr/sbin/lsof

# Prints the pid of an ancestor holding $1, or nothing.
held_by_ancestor() {
  [ -n "$lsof_bin" ] && [ -e "$1" ] || return 0
  local holder
  for holder in $("$lsof_bin" -t -- "$1" 2>/dev/null); do
    case " $ancestors " in
      *" $holder "*) echo "$holder"; return 0 ;;
    esac
  done
}

locks="${XDG_CONFIG_HOME:-$HOME/.config}/khive/cargo-hook-locks"
wrapper=()
if [ "$1" = clippy ] && [ -f "$locks" ]; then
  if ! command -v flock >/dev/null 2>&1; then
    echo "hook-cargo.sh: $locks declares locks but flock is not on PATH; refusing to run unserialized" >&2
    exit 3
  fi
  while read -r mode path; do
    case "$mode" in
      ""|\#*) continue ;;
      shared|exclusive) ;;
      *) echo "hook-cargo.sh: $locks: unknown lock mode '$mode' (want shared or exclusive)" >&2; exit 3 ;;
    esac
    if holder=$(held_by_ancestor "$path") && [ -n "$holder" ]; then
      echo "hook-cargo.sh: $path is held by ancestor pid $holder; running $1 inside that hold" >&2
      continue
    fi
    case "$mode" in
      shared) wrapper+=(flock -o -s -w 1800 "$path") ;;
      exclusive) wrapper+=(flock -o -w 1800 "$path") ;;
    esac
  done < "$locks"
fi

cd "$(dirname "$0")/../crates"
if [ "${#wrapper[@]}" -gt 0 ]; then
  exec "${wrapper[@]}" "${cmd[@]}"
fi
exec "${cmd[@]}"
