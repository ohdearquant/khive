#!/usr/bin/env bash
# Publish one or more bench-data/*.jsonl trend-ledger files to the dedicated
# `perf-data` branch (created as an orphan branch on first use).
#
# Usage: publish_ledger.sh <repo-root-relative-file> [<file> ...]
#
# Retries on a push race (a concurrent job's push landed first between our
# fetch and our push - the components/e2e jobs in the same workflow run both
# write to this branch, and a push/nightly overlap can too) by resetting to
# the latest origin/perf-data and re-copying our already-collected local
# files, never a force-push. Every retry re-copies from the SAME local
# source files this invocation was given - it does not re-run any bench, so
# a retry cannot double-count or drop a metric.
#
# The caller's local <file> (e.g. bench-data/components.jsonl) only ever
# holds THIS run's freshly-appended record(s) - it comes from a plain main
# checkout that never saw perf-data's history. A straight `cp` of that file
# into the perf-data worktree would therefore overwrite every prior run's
# history with just the current record. Instead, each destination file is
# MERGED: the worktree's existing content (the real history, just fetched
# from origin/perf-data) is kept, and only lines from the local file that
# are not already present verbatim are appended. JSONL records are
# canonicalized with sort_keys=True (bench_track.py), so an identical record
# always serializes to an identical line, which makes plain line-set dedup
# safe and also makes a retry of this same invocation idempotent. The
# components ledger is additionally migrated into bounded numbered shards;
# its historical flat file is removed from the remote branch on first publish.

set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <file> [<file> ...]" >&2
  exit 2
fi

BRANCH="${PERF_DATA_BRANCH:-perf-data}"
BOT_NAME="${BENCH_BOT_NAME:-khive-bench-bot}"
BOT_EMAIL="${BENCH_BOT_EMAIL:-khive-bench-bot@users.noreply.github.com}"
SHA="$(git rev-parse HEAD)"
WORKTREE_DIR=".perf-data-worktree"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

report_status() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf 'publish_status=%s\n' "$1" >> "$GITHUB_OUTPUT" || true
  fi
}

cleanup() {
  git worktree remove --force "$WORKTREE_DIR" >/dev/null 2>&1 || true
}
trap cleanup EXIT

MAX_ATTEMPTS=5
for attempt in $(seq 1 "$MAX_ATTEMPTS"); do
  git fetch origin "$BRANCH" >/dev/null 2>&1 || true
  git worktree remove --force "$WORKTREE_DIR" >/dev/null 2>&1 || true
  rm -rf "$WORKTREE_DIR"

  if git show-ref --verify --quiet "refs/remotes/origin/$BRANCH"; then
    git worktree add -B "$BRANCH" "$WORKTREE_DIR" "origin/$BRANCH" >/dev/null
  else
    echo "[publish_ledger] origin/$BRANCH not found - creating orphan branch"
    git worktree add --detach "$WORKTREE_DIR" HEAD >/dev/null
    git -C "$WORKTREE_DIR" checkout --orphan "$BRANCH" >/dev/null
    git -C "$WORKTREE_DIR" rm -rf --quiet . >/dev/null 2>&1 || true
  fi

  for f in "$@"; do
    if [ "$f" = "bench-data/components.jsonl" ] || [ "$f" = "./bench-data/components.jsonl" ]; then
      python3 "$SCRIPT_DIR/ledger_shards.py" "$f" "$WORKTREE_DIR/bench-data"
    else
      mkdir -p "$WORKTREE_DIR/$(dirname "$f")"
      if [ -f "$WORKTREE_DIR/$f" ]; then
        # History already present (checked out from origin/perf-data) - union
        # it with this run's local lines, deduping exact-duplicate lines, so
        # every prior run's record survives alongside the new one.
        cat "$WORKTREE_DIR/$f" "$f" | awk '!seen[$0]++' > "$WORKTREE_DIR/$f.merged"
        mv "$WORKTREE_DIR/$f.merged" "$WORKTREE_DIR/$f"
      else
        cp "$f" "$WORKTREE_DIR/$f"
      fi
    fi
  done

  git -C "$WORKTREE_DIR" config user.name "$BOT_NAME"
  git -C "$WORKTREE_DIR" config user.email "$BOT_EMAIL"
  # Components migration removes the flat file and adds numbered paths; the
  # input filename alone would miss both changes.
  git -C "$WORKTREE_DIR" add -A -- bench-data

  if git -C "$WORKTREE_DIR" diff --cached --quiet; then
    echo "[publish_ledger] no changes to commit (attempt $attempt) - already up to date"
    report_status current
    exit 0
  fi

  # KHIVE_ALLOW_DATA=1: the machine-local data-leak-guard hook flags any
  # JSONL outside a bench*/criterion-shaped path; bench-data/*.jsonl IS a
  # benchmark-results ledger (small numeric metric records only, per commit,
  # matching this file's own schema), so this is the hook's own documented,
  # auditable bypass for exactly this case - not a blanket override.
  KHIVE_ALLOW_DATA=1 git -C "$WORKTREE_DIR" commit -q -m "chore(bench-data): append trend record for ${SHA:0:8}"

  if git -C "$WORKTREE_DIR" push origin "HEAD:$BRANCH"; then
    echo "[publish_ledger] pushed on attempt $attempt"
    report_status published
    exit 0
  fi

  if [ "$attempt" -lt "$MAX_ATTEMPTS" ]; then
    delay=$((attempt * 3 + RANDOM % 4))
    echo "[publish_ledger] push rejected on attempt $attempt; retrying in ${delay}s..." >&2
    sleep "$delay"
  fi
done

echo "[publish_ledger] WARNING: failed to push after $MAX_ATTEMPTS attempts; benchmark publication is advisory" >&2
report_status failed
exit 0
