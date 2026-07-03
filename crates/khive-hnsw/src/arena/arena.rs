//! Bump arena allocator for HNSW search operations. Reset is O(1) amortized.
//!
//! Backed by a list of fixed-size chunks rather than one growable buffer:
//! growth allocates a new chunk instead of resizing an existing one, so
//! pointers returned by a prior `alloc()` call are never invalidated by a
//! later `alloc()` call on the same arena (see #412).

use std::cell::{Cell, RefCell};

/// Default arena size: 1 MiB. Sufficient for ef=256 searches (~10 KiB worst-case).
pub const DEFAULT_ARENA_SIZE: usize = 1 << 20; // 1 MiB

/// Bump arena allocator for HNSW search operations. Uses interior mutability for shared allocation.
pub struct SearchArena {
    /// Backing memory chunks. A `Box<[u8]>`'s heap allocation never moves
    /// once created, even when the outer `Vec` reallocates to hold more
    /// chunk handles, so pointers into earlier chunks stay valid.
    chunks: RefCell<Vec<Box<[u8]>>>,
    /// Index of the chunk currently being bumped into.
    current_chunk: Cell<usize>,
    /// Current bump offset into the current chunk.
    offset: Cell<usize>,
    /// Cumulative element bytes allocated since the last reset.
    bytes_used: Cell<usize>,
}

impl SearchArena {
    /// Create a new arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1024); // Minimum 1 KiB
        Self {
            chunks: RefCell::new(vec![vec![0u8; cap].into_boxed_slice()]),
            current_chunk: Cell::new(0),
            offset: Cell::new(0),
            bytes_used: Cell::new(0),
        }
    }

    /// Create a new arena with the default 1 MiB capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_ARENA_SIZE)
    }

    /// Reset the arena in O(1) amortized; all prior allocations become invalid.
    ///
    /// If growth created extra chunks since the last reset, only the
    /// largest (most recently created) chunk is retained, so memory does
    /// not grow unboundedly across repeated grow/reset cycles.
    #[inline]
    pub fn reset(&self) {
        let mut chunks = self.chunks.borrow_mut();
        if chunks.len() > 1 {
            if let Some(last) = chunks.pop() {
                chunks.clear();
                chunks.push(last);
            }
        }
        self.current_chunk.set(0);
        self.offset.set(0);
        self.bytes_used.set(0);
    }

    /// Current number of bytes allocated from this arena.
    #[inline]
    pub fn bytes_used(&self) -> usize {
        self.bytes_used.get()
    }

    /// Total capacity of the arena in bytes (sum of all backing chunks).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.chunks.borrow().iter().map(|c| c.len()).sum()
    }

    /// Allocate `count` elements of type `T`; grows automatically.
    /// The returned pointer is valid until `reset()` is called or the arena is dropped.
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

        let mut chunks = self.chunks.borrow_mut();
        let mut chunk_idx = self.current_chunk.get();
        let mut aligned = {
            let current = self.offset.get();
            (current + align - 1) & !(align - 1)
        };

        let fits = aligned
            .checked_add(size)
            .map(|end| end <= chunks[chunk_idx].len())
            .unwrap_or(false);

        if !fits {
            // Grow by allocating a brand-new chunk sized to fit this
            // request; never resize/move an existing chunk, so pointers
            // returned into earlier chunks remain valid.
            let prev_len = chunks[chunk_idx].len();
            let new_len = prev_len
                .saturating_mul(2)
                .max(size.saturating_add(align))
                .max(1024);
            chunks.push(vec![0u8; new_len].into_boxed_slice());
            chunk_idx = chunks.len() - 1;
            self.current_chunk.set(chunk_idx);
            aligned = 0;
        }

        let ptr = chunks[chunk_idx].as_mut_ptr().wrapping_add(aligned) as *mut T;
        self.offset.set(aligned + size);
        self.bytes_used.set(self.bytes_used.get() + size);
        ptr
    }

    /// Copy `src` into the arena and return a mutable pointer to the copy.
    // REASON: convenience primitive for future arena consumers; avoids re-implementing unsafe copy.
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
