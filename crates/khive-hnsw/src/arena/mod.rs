//! Bump arena allocator for zero-allocation HNSW search.
//! Fixed-size slab with O(1) reset; not thread-safe by design.

// REASON: inner module named `arena` to match the exported `SearchArena` type.
#[allow(clippy::module_inception)]
mod arena;
mod arena_heap;
mod arena_vec;

pub use arena::SearchArena;
pub use arena_heap::ArenaBinaryHeap;
pub use arena_vec::ArenaVec;

#[cfg(test)]
mod tests;
