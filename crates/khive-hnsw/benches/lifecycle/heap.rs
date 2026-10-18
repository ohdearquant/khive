use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static LIVE: Cell<i128> = const { Cell::new(0) };
    static PEAK: Cell<i128> = const { Cell::new(0) };
}

pub struct MeasuredAllocator;

fn allocated(size: usize) {
    // Signed, allocation-free TLS also handles freeing an existing allocation
    // or one transferred from another thread without unsigned underflow.
    let _ = LIVE.try_with(|current| {
        let live = current.get() + size as i128;
        current.set(live);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live)));
    });
}

fn freed(size: usize) {
    let _ = LIVE.try_with(|current| current.set(current.get() - size as i128));
}

// SAFETY: every pointer/layout contract is delegated unchanged to System.
// The observer only updates const-initialized TLS, without allocating.
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: layout retains the caller's GlobalAlloc contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: layout retains the caller's GlobalAlloc contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout retain the caller's deallocation contract.
        unsafe { System.dealloc(pointer, layout) };
        freed(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the original pointer/layout and new size retain the caller's contract.
        let resized = unsafe { System.realloc(pointer, layout, size) };
        if !resized.is_null() {
            if size >= layout.size() {
                allocated(size - layout.size());
            } else {
                freed(layout.size() - size);
            }
        }
        resized
    }
}

pub fn measure_peak<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(baseline));
    let result = operation();
    let additional = PEAK.with(Cell::get) - baseline;
    (result, usize::try_from(additional).unwrap())
}
