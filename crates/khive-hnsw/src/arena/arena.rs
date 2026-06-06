//! Core bump arena allocator.
//!
//! Pre-allocates a contiguous memory slab and bumps a pointer for each
//! allocation. Reset is O(1) -- just set the bump offset back to zero.
//!
//! # Memory Layout
//!
//! ```text
//! [---- slab (1 MiB default) ----]
//!  ^                ^             ^
//!  base             offset        capacity
//! ```
//!
//! Each `alloc<T>(count)` bumps `offset` by `count * size_of::<T>()` (with
//! alignment padding). If `offset` would exceed `capacity`, the arena grows
//! by allocating a new, larger slab.
//!
//! # Safety Invariants
//!
//! 1. The slab is a `Vec<u8>` owned by the arena. All pointers derived from
//!    it are valid as long as the arena is alive and has not been reset or grown.
//! 2. `ArenaVec` and `ArenaBinaryHeap` hold an `&SearchArena` reference,
//!    tying their lifetime to the arena. After `reset()`, all prior allocations
//!    are logically invalid -- the type system enforces this via lifetimes.
//! 3. Growth invalidates all prior pointers. This is safe because growth only
//!    happens during `alloc`, and all live `ArenaVec`/`ArenaBinaryHeap` objects
//!    manage their own pointer + length, requesting new allocations as needed
//!    via copy-on-grow.

use std::cell::Cell;

/// Default arena size: 1 MiB. More than sufficient for ef=256 searches.
///
/// Worst-case per-search memory for ef=256, M=16:
///
/// - candidates heap: 256 * 12 = 3,072 bytes
/// - results heap:    256 * 12 = 3,072 bytes
/// - batch buffer:    16 * 32 = 512 bytes
/// - result\_buf:      256 * 12 = 3,072 bytes
/// - overhead/alignment: ~1,000 bytes
///
/// Total: ~10,728 bytes (~10 KiB)
///
/// 1 MiB gives ~100x headroom.
pub const DEFAULT_ARENA_SIZE: usize = 1 << 20; // 1 MiB

/// Bump arena allocator for HNSW search operations.
///
/// All allocations within a search query bump from this arena. Between
/// queries, call `reset()` to reclaim all memory in O(1).
///
/// The arena uses interior mutability (`Cell`) for the bump offset so that
/// multiple `ArenaVec` instances can allocate from the same `&SearchArena`.
pub struct SearchArena {
    /// Backing memory slab.
    slab: Cell<Vec<u8>>,
    /// Current bump offset into the slab.
    offset: Cell<usize>,
}

impl SearchArena {
    /// Create a new arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1024); // Minimum 1 KiB
        Self {
            slab: Cell::new(vec![0u8; cap]),
            offset: Cell::new(0),
        }
    }

    /// Create a new arena with the default 1 MiB capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_ARENA_SIZE)
    }

    /// Reset the arena in O(1). All prior allocations become invalid.
    ///
    /// This is the key performance win: no deallocation, no destructors,
    /// no zeroing. Just reset the bump pointer.
    #[inline]
    pub fn reset(&self) {
        self.offset.set(0);
    }

    /// Current number of bytes allocated from this arena.
    #[inline]
    pub fn bytes_used(&self) -> usize {
        self.offset.get()
    }

    /// Total capacity of the arena in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        // SAFETY: We take the slab out, read its capacity, and put it back.
        // This is safe because we don't keep any references across the take/set.
        let slab = self.slab.take();
        let cap = slab.capacity();
        self.slab.set(slab);
        cap
    }

    /// Allocate `count` elements of type `T` from the arena.
    ///
    /// Returns a pointer to the allocated memory. The caller is responsible
    /// for writing to this memory before reading.
    ///
    /// # Panics
    ///
    /// Never panics. If the arena is full, it grows automatically.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid until `reset()` is called or the arena
    /// is dropped. The caller must not use the pointer after either event.
    /// This is enforced by the lifetime parameter on `ArenaVec`.
    pub(super) fn alloc<T>(&self, count: usize) -> *mut T {
        // Use checked arithmetic to avoid overflow: size_of::<T>() * count can
        // overflow for large `count` values on 32-bit platforms or huge allocations.
        let size = std::mem::size_of::<T>()
            .checked_mul(count)
            .expect("arena alloc size overflow");
        let align = std::mem::align_of::<T>();

        if size == 0 {
            return std::ptr::dangling_mut::<T>(); // ZST: return aligned dangling pointer
        }

        let mut current = self.offset.get();

        // Align up
        let aligned = (current + align - 1) & !(align - 1);
        let new_offset = aligned + size;

        // Take slab, work with it, put it back
        let mut slab = self.slab.take();

        if new_offset > slab.len() {
            // Grow: double or fit, whichever is larger
            let new_cap = (slab.len() * 2).max(new_offset).max(slab.len() + size);
            slab.resize(new_cap, 0);
            // Recompute alignment in case resize moved the buffer
            current = self.offset.get();
            let aligned = (current + align - 1) & !(align - 1);
            let new_offset = aligned + size;
            let ptr = slab.as_mut_ptr().wrapping_add(aligned) as *mut T;
            self.offset.set(new_offset);
            self.slab.set(slab);
            return ptr;
        }

        let ptr = slab.as_mut_ptr().wrapping_add(aligned) as *mut T;
        self.offset.set(new_offset);
        self.slab.set(slab);
        ptr
    }

    /// Copy `src` slice into the arena and return a mutable pointer to the copy.
    ///
    /// Useful for bulk-copying data into the arena. Allocates space for
    /// `src.len()` elements, copies them in, and returns a pointer to the copy.
    // REASON: `alloc_copy` is a convenience primitive available for future
    // arena consumers (e.g. arena-pinned neighbor buffers). Currently unused
    // but kept to avoid re-implementing the unsafe copy pattern at each call site.
    #[allow(dead_code)]
    pub(super) fn alloc_copy<T: Copy>(&self, src: &[T]) -> *mut T {
        if src.is_empty() {
            return self.alloc::<T>(0);
        }
        let ptr = self.alloc::<T>(src.len());
        // SAFETY: `ptr` points to freshly allocated arena memory with enough
        // space for `src.len()` elements. `src` is a valid slice. No overlap
        // because arena memory is freshly bumped.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), ptr, src.len());
        }
        ptr
    }
}
