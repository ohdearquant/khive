use std::alloc::{GlobalAlloc, Layout};

#[path = "../benches/lifecycle/heap.rs"]
mod heap;

#[global_allocator]
static ALLOCATOR: heap::MeasuredAllocator = heap::MeasuredAllocator;

#[test]
fn allocation_peak_accounts_for_frees_and_resizes() {
    let small = Layout::from_size_align(8 * 1024, 8).unwrap();
    let medium = Layout::from_size_align(16 * 1024, 8).unwrap();
    let large = Layout::from_size_align(32 * 1024, 8).unwrap();
    // SAFETY: all layouts are valid and nonzero. Every successful pointer is
    // deallocated once with its current layout; resized pointers replace ownership.
    unsafe {
        let old = ALLOCATOR.alloc(small);
        assert!(!old.is_null());
        let ((), peak) = heap::measure_peak(|| {
            ALLOCATOR.dealloc(old, small);
            let fresh = ALLOCATOR.alloc_zeroed(medium);
            assert!(!fresh.is_null());
            assert!(std::slice::from_raw_parts(fresh, medium.size())
                .iter()
                .all(|byte| *byte == 0));
            let grown = ALLOCATOR.realloc(fresh, medium, large.size());
            assert!(!grown.is_null());
            let shrunk = ALLOCATOR.realloc(grown, large, small.size());
            assert!(!shrunk.is_null());
            ALLOCATOR.dealloc(shrunk, small);
        });
        assert_eq!(peak, 24 * 1024);
        assert_eq!(heap::measure_peak(|| ()).1, 0);
    }
}

#[test]
fn allocation_peak_starts_at_existing_live_bytes() {
    let layout = Layout::from_size_align(32 * 1024, 8).unwrap();
    // SAFETY: the valid, nonzero allocation is freed once with its original layout.
    unsafe {
        let existing = ALLOCATOR.alloc(layout);
        assert!(!existing.is_null());
        let (_, peak) = heap::measure_peak(|| ALLOCATOR.dealloc(existing, layout));
        assert_eq!(peak, 0);
    }
}
