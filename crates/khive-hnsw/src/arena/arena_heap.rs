//! Arena-backed binary heap backed by `ArenaVec` (min or max, via `Ord` / `Reverse`).

use super::arena::SearchArena;
use super::arena_vec::ArenaVec;

/// A max-heap backed by arena allocation; wrap in `Reverse` for a min-heap.
/// Elements must be `Copy + Ord`.
pub struct ArenaBinaryHeap<'a, T: Copy + Ord> {
    data: ArenaVec<'a, T>,
}

impl<'a, T: Copy + Ord> ArenaBinaryHeap<'a, T> {
    /// Create a new empty heap with the given initial capacity.
    #[inline]
    pub fn new(arena: &'a SearchArena, capacity: usize) -> Self {
        Self {
            data: ArenaVec::new(arena, capacity),
        }
    }

    /// Number of elements in the heap.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the heap is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Peek at the maximum element (for max-heap) without removing it.
    #[inline]
    pub fn peek(&self) -> Option<&T> {
        if self.data.is_empty() {
            None
        } else {
            Some(self.data.get(0))
        }
    }

    /// Push an element onto the heap. O(log n).
    #[inline]
    pub fn push(&mut self, value: T) {
        self.data.push(value);
        self.sift_up(self.data.len() - 1);
    }

    /// Remove and return the maximum element. O(log n).
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.data.is_empty() {
            return None;
        }
        let len = self.data.len();
        if len == 1 {
            return self.data.pop();
        }
        // Swap root with last, pop last, sift down root.
        let root = *self.data.get(0);
        let last = *self.data.get(len - 1);
        *self.data.get_mut(0) = last;
        self.data.pop(); // removes last element
        if !self.data.is_empty() {
            self.sift_down(0);
        }
        Some(root)
    }

    /// Clear the heap without deallocating.
    #[inline]
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Drain all elements in storage order (not heap order).
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.data.drain()
    }

    /// Sift element at `pos` up to restore heap property.
    #[inline]
    fn sift_up(&mut self, mut pos: usize) {
        while pos > 0 {
            let parent = (pos - 1) / 2;
            if *self.data.get(pos) > *self.data.get(parent) {
                // Swap child with parent
                let child_val = *self.data.get(pos);
                let parent_val = *self.data.get(parent);
                *self.data.get_mut(pos) = parent_val;
                *self.data.get_mut(parent) = child_val;
                pos = parent;
            } else {
                break;
            }
        }
    }

    /// Sift element at `pos` down to restore heap property.
    #[inline]
    fn sift_down(&mut self, mut pos: usize) {
        let len = self.data.len();
        loop {
            let left = 2 * pos + 1;
            let right = 2 * pos + 2;
            let mut largest = pos;

            if left < len && *self.data.get(left) > *self.data.get(largest) {
                largest = left;
            }
            if right < len && *self.data.get(right) > *self.data.get(largest) {
                largest = right;
            }

            if largest == pos {
                break;
            }

            // Swap
            let pos_val = *self.data.get(pos);
            let largest_val = *self.data.get(largest);
            *self.data.get_mut(pos) = largest_val;
            *self.data.get_mut(largest) = pos_val;
            pos = largest;
        }
    }
}
