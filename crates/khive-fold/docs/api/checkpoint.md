# Checkpoint

Technical reference for the generic snapshot envelope and `InMemoryCheckpointStore`
(`checkpoint.rs`).

## Ordering

`InMemoryCheckpointStore::load_latest` breaks `created_at` ties deterministically by `uuid`:
`max_by_key(|c| (c.created_at, c.uuid))` selects the lexicographically greatest UUID among
equal timestamps. Callers can rely on this deterministic winner.

`sort_checkpoint_keys` is extracted as a standalone helper so it can be unit-tested with
intentionally unsorted input, giving fail-before/pass-after coverage independent of `HashMap`
randomization. The old `HashMap.keys().cloned().collect()` path returned keys in HashMap
iteration order (non-deterministic); the regression test
`sort_checkpoint_keys_produces_lexicographic_order` passes a reverse-sorted vector — the
worst case for an unsorted implementation — to guarantee it fails against any implementation
that skips the sort step.

## Errors

- `FoldError::Serialization` — state serialization failed during checkpoint save.
- `FoldError::IntegrityMismatch` — stored BLAKE3 hash does not match recomputed hash on load.
- `FoldError::CheckpointNotFound` — delete or load of a non-existent checkpoint ID.
- `FoldError::LockPoisoned` — `RwLock` poisoned (thread panic while holding write lock).
