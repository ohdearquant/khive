# `git.digest` handler design notes

Extracted from `crates/khive-pack-git/src/handlers.rs` doc-comments.

## `RemoteRecoveryStage` / `RemoteCommitRecovery`

Issue #765 remote-only repair policy: at most one `git fetch --refetch`,
then at most one owned-cache reclone, bounded by `stage` so a persistent or
recurring classified failure surfaces as a terminal error rather than
looping. Local-path sources never construct this — they call public
`run_ingest` directly, which never repairs anything (ADR-088 Amendment 1:
the disposable scratch cache is remote-URL-mode-only).

## `repair`

Advances the bounded repair state machine by one step in response to a
classified `GitLogError` (the caller has already verified
`is_missing_promisor_object()`). Ignores `_repo` — both repair primitives
operate on the cache slot for `canonical_url`, which is the same path
`_repo` already names (`crate::cache`'s slot layout is keyed by URL, not
passed through).

## `handle_digest`

The handler preserves the caller's `include` bits when it enters
`run_ingest`; it never masks issues/pull requests merely because the parsed
source URL is not on GitHub. The handler supplies the canonical source's
expected GitHub slug when available; otherwise the ingest core derives it
from `origin`. The core owns the truthful source-bound `gh` probe and records
requested-but-unusable sources as `Skipped`, which is
required for accurate `history_exhausted` reporting.

The handler serializes `IngestReport` with no `receipt_id`. Both normal and
multi-backend runtime dispatch paths then persist the complete successful
report and inject the durable audit event id before returning. Direct handler
or administrative-ingester callers do not fabricate a receipt. Runtime
presentation classifies `git.digest` as `AlwaysVerbose`, preserving the strict
identity contract: the default MCP result is exactly the stored
`payload.result`, including the full `receipt_id` UUID.
