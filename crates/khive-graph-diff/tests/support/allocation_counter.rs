// Source: crates/khive-pack-code/tests/support/allocation_counter.rs
//! All-thread Rust global allocation requests and incremental live heap; not C heap or RSS.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static BOOKKEEPING: AtomicBool = AtomicBool::new(false);

struct Locked;
impl Drop for Locked {
    fn drop(&mut self) {
        BOOKKEEPING.store(false, Ordering::Release);
    }
}
fn lock() -> Locked {
    while BOOKKEEPING
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop();
    }
    Locked
}

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static IN_USE: AtomicBool = AtomicBool::new(false);

pub struct ObservedSystem;

fn allocated(bytes: usize) {
    let _locked = lock();
    let live = LIVE.fetch_add(bytes, Ordering::SeqCst).wrapping_add(bytes);
    observe(live);
}

// Called only while the bookkeeping lock is held.
fn observe(live: usize) {
    if ACTIVE.load(Ordering::SeqCst) {
        REQUESTS.fetch_add(1, Ordering::SeqCst);
        PEAK.fetch_max(live, Ordering::SeqCst);
    }
}

// Allocation-free, non-panicking bookkeeping uses a short atomic spin lock.
// System calls occur outside the lock. It linearizes start/stop and concurrent
// counter updates; memory instrumentation is never used for latency baselines.
// LIVE tracks all successful Rust global allocations for the entire process,
// even outside observation. Deallocation cannot underflow for valid callers.
// SAFETY: every request forwards the original pointer/layout to System.
// Counters never dereference user memory and bookkeeping never allocates or panics.
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplies a valid nonzero layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplies a valid nonzero layout.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        {
            let _locked = lock();
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        }
        // SAFETY: pointer and layout retain the caller's allocation contract.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forward the caller's live pointer/layout and valid new size.
        let replacement = unsafe { System.realloc(ptr, layout, size) };
        if !replacement.is_null() {
            let _locked = lock();
            // A successful realloc is one request. Its old live block is replaced;
            // any temporary allocator-internal copy is outside this Rust metric.
            let live = if size >= layout.size() {
                LIVE.fetch_add(size - layout.size(), Ordering::SeqCst)
                    .wrapping_add(size - layout.size())
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::SeqCst)
                    .wrapping_sub(layout.size() - size)
            };
            observe(live);
        }
        replacement
    }
}

#[derive(Debug)]
pub struct Measurement {
    pub successful_requests: usize,
    pub peak_incremental_rust_live_bytes: usize,
}

pub struct Observation {
    baseline: usize,
}

impl Observation {
    pub fn begin() -> Self {
        assert!(
            !IN_USE.swap(true, Ordering::SeqCst),
            "one global observer at a time"
        );
        let _locked = lock();
        let baseline = LIVE.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        REQUESTS.store(0, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        Self { baseline }
    }
    pub fn finish(self) -> Measurement {
        let _locked = lock();
        ACTIVE.store(false, Ordering::SeqCst);
        Measurement {
            successful_requests: REQUESTS.load(Ordering::SeqCst),
            peak_incremental_rust_live_bytes: PEAK
                .load(Ordering::SeqCst)
                .saturating_sub(self.baseline),
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        let _locked = lock();
        ACTIVE.store(false, Ordering::SeqCst);
        IN_USE.store(false, Ordering::SeqCst);
    }
}

pub fn integrity_check() {
    // Spawn and synchronization setup are outside observation. The worker holds
    // its live payload across finish; this catches a thread-local-only observer.
    let prepared = std::sync::Arc::new(std::sync::Barrier::new(2));
    let prepared_child = prepared.clone();
    let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
    let allocated = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (r, a, e) = (ready.clone(), allocated.clone(), release.clone());
    let worker = std::thread::spawn(move || {
        prepared_child.wait();
        r.wait();
        let mut bytes = vec![17_u8; 8192];
        bytes.reserve_exact(8192);
        std::hint::black_box(&bytes);
        a.wait();
        e.wait();
        std::hint::black_box(bytes);
    });
    prepared.wait();
    let observation = Observation::begin();
    ready.wait();
    allocated.wait();
    let measured = observation.finish();
    release.wait();
    worker.join().expect("observer integrity worker joined");
    assert!(measured.successful_requests >= 2, "{measured:?}");
    assert!(
        measured.peak_incremental_rust_live_bytes >= 16384,
        "{measured:?}"
    );
}
