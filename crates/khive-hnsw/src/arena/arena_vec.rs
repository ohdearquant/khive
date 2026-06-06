//! Arena-backed growable vector (`ArenaVec<'a, T>`) — allocates from `SearchArena`, reset is O(1).

use super::arena::SearchArena;

/// A growable vector backed by arena allocation.
///
/// Elements must be `Copy` because growth copies elements to a new arena
/// region (no drop semantics needed).
pub struct ArenaVec<'a, T: Copy> {
    /// Pointer to the start of the allocated region in the arena.
    ptr: *mut T,
    /// Number of live elements.
    len: usize,
    /// Total capacity (in elements) of the current allocation.
    cap: usize,
    /// Reference to the owning arena (for growth allocation).
    arena: &'a SearchArena,
}

impl<'a, T: Copy> ArenaVec<'a, T> {
    /// Create a new empty `ArenaVec` with the given initial capacity.
    #[inline]
    pub fn new(arena: &'a SearchArena, capacity: usize) -> Self {
        let (ptr, cap) = if capacity > 0 {
            (arena.alloc::<T>(capacity), capacity)
        } else {
            (std::ptr::dangling_mut::<T>(), 0) // dangling
        };
        Self {
            ptr,
            len: 0,
            cap,
            arena,
        }
    }

    /// Number of elements in the vec.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vec is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push an element. Grows if necessary.
    #[inline]
    pub fn push(&mut self, value: T) {
        if self.len == self.cap {
            self.grow();
        }
        // SAFETY: We just ensured len < cap, so ptr.add(len) is within bounds.
        unsafe {
            self.ptr.add(self.len).write(value);
        }
        self.len += 1;
    }

    /// Pop the last element.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: len was > 0, so ptr.add(len) points to the last written element.
        Some(unsafe { self.ptr.add(self.len).read() })
    }

    /// Clear the vec without deallocating (just resets length).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Get element at `index`. Panics if out of bounds (assert, not debug_assert).
    #[inline]
    pub fn get(&self, index: usize) -> &T {
        assert!(index < self.len, "ArenaVec index out of bounds");
        // SAFETY: index < len, and all elements up to len are initialized.
        unsafe { &*self.ptr.add(index) }
    }

    /// Get an optional reference at `index`; returns `None` instead of panicking when out of bounds.
    #[inline]
    pub fn try_get(&self, index: usize) -> Option<&T> {
        if index < self.len {
            // SAFETY: index < len, and all elements up to len are initialized.
            Some(unsafe { &*self.ptr.add(index) })
        } else {
            None
        }
    }

    /// Get mutable reference at `index`. Panics if out of bounds (assert, not debug_assert).
    #[inline]
    pub fn get_mut(&mut self, index: usize) -> &mut T {
        assert!(index < self.len, "ArenaVec index out of bounds");
        // SAFETY: index < len, and all elements up to len are initialized.
        unsafe { &mut *self.ptr.add(index) }
    }

    /// Get an optional mutable reference at `index`; returns `None` instead of panicking.
    #[inline]
    pub fn try_get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index < self.len {
            // SAFETY: index < len, and all elements up to len are initialized.
            Some(unsafe { &mut *self.ptr.add(index) })
        } else {
            None
        }
    }

    /// Get an immutable slice of all elements.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: ptr points to len initialized elements in the arena.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Get a mutable slice of all elements.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: ptr points to len initialized elements in the arena.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Iterate over elements by reference.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Swap-remove the element at `index` (O(1) removal).
    #[inline]
    pub fn swap_remove(&mut self, index: usize) -> T {
        assert!(index < self.len, "ArenaVec swap_remove index out of bounds");
        let last = self.len - 1;
        // SAFETY: both index and last are < len.
        unsafe {
            let val = self.ptr.add(index).read();
            if index != last {
                let last_val = self.ptr.add(last).read();
                self.ptr.add(index).write(last_val);
            }
            self.len -= 1;
            val
        }
    }

    /// Drain all elements, returning an iterator over them.
    /// After drain, the vec is empty.
    pub fn drain(&mut self) -> ArenaVecDrain<'_, T> {
        let len = self.len;
        self.len = 0;
        ArenaVecDrain {
            ptr: self.ptr,
            pos: 0,
            len,
            _marker: std::marker::PhantomData,
        }
    }

    /// Extend from a slice.
    pub fn extend_from_slice(&mut self, slice: &[T]) {
        for &item in slice {
            self.push(item);
        }
    }

    /// Sort elements using the provided comparison function.
    pub fn sort_by<F>(&mut self, compare: F)
    where
        F: FnMut(&T, &T) -> std::cmp::Ordering,
    {
        self.as_mut_slice().sort_by(compare);
    }

    /// Double capacity (minimum 8); old region is leaked in the arena and reclaimed on reset.
    fn grow(&mut self) {
        let new_cap = if self.cap == 0 {
            8
        } else {
            // Use saturating_mul to avoid overflow on pathologically large arenas.
            self.cap.saturating_mul(2)
        };
        let new_ptr = self.arena.alloc::<T>(new_cap);
        if self.len > 0 {
            // SAFETY: copying len elements from old region (ptr, len elements)
            // to new region (new_ptr, new_cap >= len elements). No overlap
            // because the arena only bumps forward.
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len);
            }
        }
        // Old region is leaked -- reclaimed on arena reset.
        self.ptr = new_ptr;
        self.cap = new_cap;
    }
}

/// Index by usize for convenience (read-only).
impl<'a, T: Copy> std::ops::Index<usize> for ArenaVec<'a, T> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &T {
        self.get(index)
    }
}

/// Drain iterator for `ArenaVec`.
pub struct ArenaVecDrain<'a, T: Copy> {
    ptr: *mut T,
    pos: usize,
    len: usize,
    _marker: std::marker::PhantomData<&'a T>,
}

impl<T: Copy> Iterator for ArenaVecDrain<'_, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        if self.pos >= self.len {
            return None;
        }
        // SAFETY: pos < len, and all elements were initialized before drain.
        let val = unsafe { self.ptr.add(self.pos).read() };
        self.pos += 1;
        Some(val)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len - self.pos;
        (remaining, Some(remaining))
    }
}

impl<T: Copy> ExactSizeIterator for ArenaVecDrain<'_, T> {}
