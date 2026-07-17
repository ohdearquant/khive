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
6. After drain (all v1 readers dropped), deallocate v1

## Concurrency Model

- **Read path**: `parking_lot::RwLock` read guard (adaptive spinning, no OS block for
  short critical sections)
- **Write path**: Brief exclusive lock for pointer swap only
- **Background build**: `tokio::task::spawn_blocking`, no locks held during build
- **Drain**: Async poll via `AtomicU64` reader counter

## Module Structure

- `manager`: `IndexAliasManager` — the main entry point
- `drain`: Reader tracking and RAII guard
- `validation`: Pre-swap index quality validation
- `error`: Error types
