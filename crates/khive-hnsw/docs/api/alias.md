# HNSW Index Alias — Zero-Downtime Migration

The alias manager implements a blue-green deployment pattern for HNSW vector indexes.
When switching embedding models (e.g., from BGE-small to mE5-small), every vector must
be re-embedded and re-indexed. The alias manager allows this without taking the search
service offline.

## Migration Steps

1. `alias("active")` currently points to `collection("index_v1")`
2. Build `collection("index_v2")` in a background thread
3. Validate the new index (recall@k benchmark)
4. Atomic swap: `alias("active")` now points to `collection("index_v2")`
5. In-flight queries on v1 complete on v1; new queries go to v2
6. If no other alias references v1, retire it and deallocate after its readers drain

## Concurrency Model

- **Read path**: `parking_lot::RwLock` read guard (adaptive spinning, no OS block for
  short critical sections)
- **Write path**: Brief exclusive lock transaction for publication and eligible source
  retirement
- **Background build**: `tokio::task::spawn_blocking`, no locks held during build
- **Lock order**: Operations that need both registries always acquire aliases before
  collections
- **Concurrent migration ownership**: Publication is a compare-and-switch against the
  alias target captured before the replacement build. Only one migration that captured a
  given target can publish; a loser removes its unreferenced candidate and returns
  `AliasTargetChanged` without changing the alias. The winning switch and eligible source
  retirement occur under the same aliases-before-collections lock transaction.
- **Drain**: Retirement is rejected while any alias references the collection; after
  retirement, readers drain through async polling of an `AtomicU64` counter

## Module Structure

- `manager`: `IndexAliasManager` — the main entry point
- `drain`: Reader tracking and RAII guard
- `validation`: Pre-swap index quality validation
- `error`: Error types
