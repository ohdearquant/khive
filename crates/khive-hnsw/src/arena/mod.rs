//! Bump arena allocator for zero-allocation HNSW search.
//!
//! Fixed-size slab with O(1) reset between queries; not thread-safe by design.
//! See `docs/arena.md` for design rationale and thread-safety notes.

// REASON: The inner module is intentionally named `arena` to match the public
// type `SearchArena` exported from it; the name duplication is structural, not accidental.
#[allow(clippy::module_inception)]
mod arena;
mod arena_heap;
mod arena_vec;

pub use arena::SearchArena;
pub use arena_heap::ArenaBinaryHeap;
pub use arena_vec::ArenaVec;

#[cfg(test)]
mod tests;
