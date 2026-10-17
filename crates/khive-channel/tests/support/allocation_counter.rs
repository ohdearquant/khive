//! Counts allocation requests on the measuring thread, outside fixture setup.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy)]
struct Probe {
    enabled: bool,
    allocations: usize,
}

thread_local! {
    static PROBE: Cell<Probe> = const { Cell::new(Probe {
        enabled: false,
        allocations: 0,
    }) };
}

struct CountedAllocator;

fn record_allocation() {
    // Const TLS and Cell access allocate nothing; unavailable teardown TLS is ignored.
    let _ = PROBE.try_with(|cell| {
        let mut probe = cell.get();
        if probe.enabled {
            probe.allocations = probe.allocations.saturating_add(1);
            cell.set(probe);
        }
    });
}

// SAFETY: all pointer/layout contracts are forwarded unchanged to System. The
// observer never allocates, dereferences caller pointers, or panics in a hook.
unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies GlobalAlloc's layout contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation();
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies GlobalAlloc's layout contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation();
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: pointer, layout and size retain the caller's realloc contract.
        let resized = unsafe { System.realloc(pointer, layout, size) };
        if !resized.is_null() {
            record_allocation();
        }
        resized
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout retain the caller's deallocation contract.
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

struct Measurement;
impl Drop for Measurement {
    fn drop(&mut self) {
        PROBE.with(|cell| {
            let mut probe = cell.get();
            probe.enabled = false;
            cell.set(probe);
        });
    }
}

pub fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    PROBE.with(|cell| {
        assert!(!cell.get().enabled, "allocation measurements cannot nest");
        cell.set(Probe {
            enabled: true,
            allocations: 0,
        });
    });
    let guard = Measurement;
    let output = operation();
    drop(guard);
    (output, PROBE.with(|cell| cell.get().allocations))
}
