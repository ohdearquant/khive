//! Bump arena allocator for HNSW search operations. Reset is O(1).

use std::cell::Cell;

/// Default arena size: 1 MiB. Sufficient for ef=256 searches (~10 KiB worst-case).
pub const DEFAULT_ARENA_SIZE: usize = 1 << 20; // 1 MiB

/// Bump arena allocator for HNSW search operations. Uses interior mutability for shared allocation.
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

    /// Reset the arena in O(1); all prior allocations become invalid.
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
