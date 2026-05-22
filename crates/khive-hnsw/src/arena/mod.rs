//! Bump arena allocator for zero-allocation HNSW search.
//!
//! Provides a fixed-size memory arena with O(1) reset between queries.
//! All per-search allocations (candidates heap, results heap, batch buffer,
//! result buffer) bump from this arena instead of the global allocator.
//!
//! # Design
//!
//! The arena pre-allocates a configurable slab (default 1 MiB). Within a
//! single search query, allocations bump a pointer forward. Between queries,
//! `reset()` sets the pointer back to zero -- O(1), no deallocation, no
//! destructors, no zeroing.
//!
//! # Thread Safety
//!
//! The arena is `!Send` and `!Sync` by design. For concurrent search, each
//! thread should own its own `SearchArena` (via `thread_local!` or explicit
//! per-thread allocation).

#[allow(clippy::module_inception)]
mod arena;
mod arena_heap;
mod arena_vec;

pub use arena::SearchArena;
pub use arena_heap::ArenaBinaryHeap;
pub use arena_vec::ArenaVec;

#[cfg(test)]
mod tests;
