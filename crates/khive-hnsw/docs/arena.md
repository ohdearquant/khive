# HNSW Bump Arena Allocator

The arena in `src/arena/` provides zero-allocation per-search memory for HNSW queries.

## Design

The arena pre-allocates a configurable slab (default 1 MiB). Within a single search query,
allocations bump a pointer forward. Between queries, `reset()` sets the pointer back to
zero — O(1), no deallocation, no destructors, no zeroing.

All per-search allocations (candidates heap, results heap, batch buffer, result buffer) use
this arena instead of the global allocator.

## Thread Safety

The arena is `!Send` and `!Sync` by design. For concurrent search, each thread must own
its own `SearchArena` (via `thread_local!` or explicit per-thread allocation).
