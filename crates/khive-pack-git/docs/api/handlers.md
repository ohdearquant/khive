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
