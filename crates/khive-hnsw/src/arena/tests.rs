use super::*;

// =========================================================================
// SearchArena tests
// =========================================================================

#[test]
fn test_arena_alloc_and_reset() {
    let arena = SearchArena::new(4096);
    assert_eq!(arena.bytes_used(), 0);

    // Allocate some memory
    let _ptr: *mut u64 = arena.alloc::<u64>(10);
    assert!(arena.bytes_used() > 0);
    let used_after_alloc = arena.bytes_used();

    // Allocate more
    let _ptr2: *mut u32 = arena.alloc::<u32>(20);
    assert!(arena.bytes_used() > used_after_alloc);

    // Reset -- O(1), reclaims all memory
    arena.reset();
    assert_eq!(arena.bytes_used(), 0);

    // Can allocate again after reset
    let _ptr3: *mut u64 = arena.alloc::<u64>(10);
    assert!(arena.bytes_used() > 0);
}

#[test]
fn test_arena_overflow_grows() {
    // Small arena that will need to grow
    let arena = SearchArena::new(1024); // minimum size
    let initial_cap = arena.capacity();

    // Allocate more than capacity
    let _ptr: *mut u8 = arena.alloc::<u8>(2048);

    // Arena should have grown
    assert!(arena.capacity() >= 2048);
    assert!(arena.capacity() > initial_cap);
}

#[test]
fn test_arena_reset_reuse_cycle() {
    let arena = SearchArena::new(4096);

    for _ in 0..100 {
        // Simulate a search query: allocate various buffers
        let _candidates: *mut (f32, usize) = arena.alloc::<(f32, usize)>(64);
        let _results: *mut (f32, usize) = arena.alloc::<(f32, usize)>(64);
        let _batch: *mut (usize, usize) = arena.alloc::<(usize, usize)>(32);

        assert!(arena.bytes_used() > 0);

        // Reset between queries
        arena.reset();
        assert_eq!(arena.bytes_used(), 0);
    }
}

#[test]
fn test_arena_alignment() {
    let arena = SearchArena::new(4096);

    // Allocate a u8 to offset the pointer
    let _: *mut u8 = arena.alloc::<u8>(1);

    // Allocate a u64 -- should be aligned to 8 bytes
    let ptr: *mut u64 = arena.alloc::<u64>(1);
    assert_eq!(ptr as usize % std::mem::align_of::<u64>(), 0);

    // Allocate a u128 -- should be aligned to 16 bytes
    let ptr128: *mut u128 = arena.alloc::<u128>(1);
    assert_eq!(ptr128 as usize % std::mem::align_of::<u128>(), 0);
}

// =========================================================================
// ArenaVec tests
// =========================================================================

#[test]
fn test_arena_vec_push_pop() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 4);

    assert!(vec.is_empty());
    assert_eq!(vec.len(), 0);

    vec.push(10);
    vec.push(20);
    vec.push(30);

    assert_eq!(vec.len(), 3);
    assert!(!vec.is_empty());

    assert_eq!(vec.pop(), Some(30));
    assert_eq!(vec.pop(), Some(20));
    assert_eq!(vec.pop(), Some(10));
    assert_eq!(vec.pop(), None);
    assert!(vec.is_empty());
}

#[test]
fn test_arena_vec_growth() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 2);

    // Push beyond initial capacity
    for i in 0..100 {
        vec.push(i);
    }

    assert_eq!(vec.len(), 100);
    for i in 0..100 {
        assert_eq!(*vec.get(i), i);
    }
}

#[test]
fn test_arena_vec_clear() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(1);
    vec.push(2);
    vec.push(3);
    vec.clear();

    assert!(vec.is_empty());
    assert_eq!(vec.len(), 0);

    // Can push again after clear
    vec.push(4);
    assert_eq!(vec.len(), 1);
    assert_eq!(*vec.get(0), 4);
}

#[test]
fn test_arena_vec_iter() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(10);
    vec.push(20);
    vec.push(30);

    let collected: Vec<i32> = vec.iter().copied().collect();
    assert_eq!(collected, vec![10, 20, 30]);
}

#[test]
fn test_arena_vec_drain() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(1);
    vec.push(2);
    vec.push(3);

    let drained: Vec<i32> = vec.drain().collect();
    assert_eq!(drained, vec![1, 2, 3]);
    assert!(vec.is_empty());
}

#[test]
fn test_arena_vec_as_slice() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(5);
    vec.push(10);
    vec.push(15);

    assert_eq!(vec.as_slice(), &[5, 10, 15]);
}

#[test]
fn test_arena_vec_index() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(100);
    vec.push(200);

    assert_eq!(vec[0], 100);
    assert_eq!(vec[1], 200);
}

#[test]
fn test_arena_vec_sort_by() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(30);
    vec.push(10);
    vec.push(20);

    vec.sort_by(|a, b| a.cmp(b));
    assert_eq!(vec.as_slice(), &[10, 20, 30]);
}

#[test]
fn test_arena_vec_swap_remove() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 8);

    vec.push(10);
    vec.push(20);
    vec.push(30);

    let removed = vec.swap_remove(0);
    assert_eq!(removed, 10);
    assert_eq!(vec.len(), 2);
    // 30 should now be at index 0 (swapped from last)
    assert_eq!(*vec.get(0), 30);
    assert_eq!(*vec.get(1), 20);
}

// =========================================================================
// ArenaBinaryHeap tests
// =========================================================================

#[test]
fn test_arena_heap_max_ordering() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    heap.push(10);
    heap.push(30);
    heap.push(20);
    heap.push(5);
    heap.push(25);

    // Max-heap: should pop in descending order
    assert_eq!(heap.pop(), Some(30));
    assert_eq!(heap.pop(), Some(25));
    assert_eq!(heap.pop(), Some(20));
    assert_eq!(heap.pop(), Some(10));
    assert_eq!(heap.pop(), Some(5));
    assert_eq!(heap.pop(), None);
}

#[test]
fn test_arena_heap_min_ordering_via_reverse() {
    use std::cmp::Reverse;

    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    heap.push(Reverse(10));
    heap.push(Reverse(30));
    heap.push(Reverse(20));
    heap.push(Reverse(5));
    heap.push(Reverse(25));

    // Min-heap via Reverse: should pop in ascending order
    assert_eq!(heap.pop(), Some(Reverse(5)));
    assert_eq!(heap.pop(), Some(Reverse(10)));
    assert_eq!(heap.pop(), Some(Reverse(20)));
    assert_eq!(heap.pop(), Some(Reverse(25)));
    assert_eq!(heap.pop(), Some(Reverse(30)));
    assert_eq!(heap.pop(), None);
}

#[test]
fn test_arena_heap_peek() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    assert_eq!(heap.peek(), None);

    heap.push(10);
    assert_eq!(heap.peek(), Some(&10));

    heap.push(20);
    assert_eq!(heap.peek(), Some(&20));

    heap.push(5);
    assert_eq!(heap.peek(), Some(&20)); // 20 is still max
}

#[test]
fn test_arena_heap_clear() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    heap.push(1);
    heap.push(2);
    heap.push(3);
    heap.clear();

    assert!(heap.is_empty());
    assert_eq!(heap.len(), 0);
    assert_eq!(heap.peek(), None);
}

#[test]
fn test_arena_heap_drain() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    heap.push(10);
    heap.push(20);
    heap.push(30);

    let mut drained: Vec<i32> = heap.drain().collect();
    drained.sort();
    assert_eq!(drained, vec![10, 20, 30]);
    assert!(heap.is_empty());
}

// =========================================================================
// Integration: HNSW-like usage patterns
// =========================================================================

#[test]
fn test_hnsw_search_pattern() {
    use crate::distance::OrderedF32;

    let arena = SearchArena::new(4096);

    // Simulate the HNSW search pattern:
    // candidates = min-heap, results = max-heap

    let mut candidates: ArenaBinaryHeap<std::cmp::Reverse<(OrderedF32, usize)>> =
        ArenaBinaryHeap::new(&arena, 64);
    let mut results: ArenaBinaryHeap<(OrderedF32, usize)> = ArenaBinaryHeap::new(&arena, 64);
    let mut result_buf: ArenaVec<(f32, usize)> = ArenaVec::new(&arena, 64);

    // Insert entry points
    candidates.push(std::cmp::Reverse((OrderedF32(0.5), 0)));
    results.push((OrderedF32(0.5), 0));

    candidates.push(std::cmp::Reverse((OrderedF32(0.3), 1)));
    results.push((OrderedF32(0.3), 1));

    candidates.push(std::cmp::Reverse((OrderedF32(0.7), 2)));
    results.push((OrderedF32(0.7), 2));

    // Pop closest candidate (min-heap)
    let closest = candidates.pop().unwrap();
    assert_eq!(closest.0 .0 .0, 0.3); // OrderedF32(0.3)

    // Peek worst result (max-heap)
    let worst = results.peek().unwrap();
    assert_eq!(worst.0 .0, 0.7); // OrderedF32(0.7)

    // Drain results into result_buf
    for (dist, id) in results.drain() {
        result_buf.push((dist.0, id));
    }
    result_buf.sort_by(|a, b| OrderedF32(a.0).cmp(&OrderedF32(b.0)));

    assert_eq!(result_buf.len(), 3);
    assert_eq!(result_buf[0].0, 0.3);
    assert_eq!(result_buf[1].0, 0.5);
    assert_eq!(result_buf[2].0, 0.7);

    // Reset arena for next query
    arena.reset();
    assert_eq!(arena.bytes_used(), 0);
}

#[test]
fn test_arena_vec_extend_from_slice() {
    let arena = SearchArena::new(4096);
    let mut vec = ArenaVec::new(&arena, 4);

    vec.extend_from_slice(&[1, 2, 3, 4, 5]);
    assert_eq!(vec.as_slice(), &[1, 2, 3, 4, 5]);
}

#[test]
fn test_arena_vec_zero_capacity() {
    let arena = SearchArena::new(4096);
    let mut vec: ArenaVec<i32> = ArenaVec::new(&arena, 0);

    assert!(vec.is_empty());
    vec.push(42);
    assert_eq!(vec.len(), 1);
    assert_eq!(vec[0], 42);
}

#[test]
fn test_arena_heap_single_element() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 4);

    heap.push(42);
    assert_eq!(heap.peek(), Some(&42));
    assert_eq!(heap.pop(), Some(42));
    assert!(heap.is_empty());
}

#[test]
fn test_arena_heap_duplicate_values() {
    let arena = SearchArena::new(4096);
    let mut heap = ArenaBinaryHeap::new(&arena, 8);

    heap.push(5);
    heap.push(5);
    heap.push(5);

    assert_eq!(heap.pop(), Some(5));
    assert_eq!(heap.pop(), Some(5));
    assert_eq!(heap.pop(), Some(5));
    assert_eq!(heap.pop(), None);
}

#[test]
fn test_multiple_reset_cycles_with_collections() {
    let arena = SearchArena::new(4096);

    for cycle in 0..50 {
        let mut vec = ArenaVec::new(&arena, 8);
        let mut heap = ArenaBinaryHeap::new(&arena, 8);

        for i in 0..20 {
            vec.push(cycle * 100 + i);
            heap.push(cycle * 100 + i);
        }

        assert_eq!(vec.len(), 20);
        assert_eq!(heap.len(), 20);

        // Verify max element
        assert_eq!(heap.peek(), Some(&(cycle * 100 + 19)));

        arena.reset();
    }
}

#[test]
fn test_arena_default_capacity() {
    let arena = SearchArena::with_default_capacity();
    assert!(arena.capacity() >= super::arena::DEFAULT_ARENA_SIZE);
}
