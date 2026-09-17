//! One isolated allocation measurement, with no wall-clock performance gate.
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use khive_brain_core::brain_state::{AdapterRecord, RouterStateBlob};
use khive_brain_core::{
    BalancedRecallState, BrainState, BrainStateSnapshot, ProfileBinding, ProfileRecord,
    SectionPosteriorState,
};

struct TrackedAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

// All allocations are counted, including ones made before the measurement.
// Measuring growth above a fresh baseline therefore never subtracts an
// allocation that was omitted from the corresponding live-byte counter.
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, layout, new_size) };
        if !result.is_null() {
            if new_size >= layout.size() {
                allocated(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

fn measure(f: impl FnOnce() -> String) -> (String, usize) {
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let value = f();
    let peak_growth = PEAK.load(Ordering::SeqCst).saturating_sub(baseline);
    (value, peak_growth)
}

fn assert_snapshot_equal(mut left: BrainStateSnapshot, mut right: BrainStateSnapshot) {
    assert!(
        left.router_state == right.router_state,
        "opaque router state must round-trip"
    );
    assert!(
        left.adapter_set == right.adapter_set,
        "adapter records must round-trip"
    );
    // Compare opaque buffers as bytes, rather than expanding millions of bytes
    // into serde_json::Value nodes unrelated to the allocation measurement.
    left.router_state.clear();
    right.router_state.clear();
    left.adapter_set.clear();
    right.adapter_set.clear();
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

#[test]
fn borrowed_snapshot_preserves_round_trip_and_reduces_peak_allocation() {
    const PROFILES: usize = 8;
    const BYTES_PER_ROUTER: usize = 1024 * 1024;
    const ROUTER_BYTES: usize = PROFILES * BYTES_PER_ROUTER;
    // Predeclared: save at least 75% of one complete router-byte copy. This
    // leaves room for small harness/projection allocations; it is not an RSS
    // or elapsed-time claim. A clone-based candidate must fail this bound.
    const REQUIRED_SAVING: usize = ROUTER_BYTES * 3 / 4;
    let mut state = BrainState::new(16);
    state.balanced_recall.total_events = 11;
    for i in 0..PROFILES {
        let id = format!("snapshot-profile-{i}");
        let mut record = ProfileRecord::new_balanced_recall(16);
        record.id = id.clone();
        state.profiles.insert(id.clone(), record);
        let mut live = BalancedRecallState::new(16);
        live.total_events = 7 + i as u64;
        state.profile_states.insert(id.clone(), live);
        let mut sections = SectionPosteriorState::new();
        sections.total_events = 13 + i as u64;
        state.section_states.insert(id.clone(), sections);
        state.router_state.insert(
            id.clone(),
            RouterStateBlob {
                schema_version: 1,
                gate_bytes: vec![47; BYTES_PER_ROUTER],
            },
        );
        state.adapter_set.insert(
            id.clone(),
            (0..256)
                .map(|slot| AdapterRecord {
                    adapter_id: format!("adapter-{i}-{slot}"),
                    slot,
                    content_hash: "a".repeat(64),
                })
                .collect(),
        );
        state.bindings.push(ProfileBinding {
            actor: format!("actor-{i}"),
            namespace: "local".into(),
            consumer_kind: "recall".into(),
            profile_id: id,
            priority: 1,
            created_at: chrono::Utc::now(),
        });
    }

    let (owned_json, owned_peak) = measure(|| {
        let mut proposed = BrainState::from_snapshot(state.to_snapshot(), 16);
        proposed.balanced_recall.total_events += 1;
        let snapshot = proposed.to_snapshot();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        black_box(&snapshot);
        black_box(&proposed);
        encoded
    });
    let (borrowed_json, borrowed_peak) = measure(|| {
        let mut proposed = BrainState::from_snapshot(state.to_snapshot(), 16);
        proposed.balanced_recall.total_events += 1;
        let encoded = proposed.to_snapshot_json().unwrap();
        black_box(&proposed);
        encoded
    });
    eprintln!("router_bytes={ROUTER_BYTES} owned_peak_growth={owned_peak} borrowed_peak_growth={borrowed_peak} required_saving={REQUIRED_SAVING}");
    assert!(
        owned_peak >= borrowed_peak + REQUIRED_SAVING,
        "borrowed persistence encoding must avoid a complete router-byte copy"
    );

    let decoded: BrainStateSnapshot = serde_json::from_str(&borrowed_json).unwrap();
    assert_eq!(decoded.profiles["snapshot-profile-0"].total_events, 7);
    assert_eq!(decoded.profiles["balanced-recall-v1"].total_events, 12);
    assert_snapshot_equal(serde_json::from_str(&owned_json).unwrap(), decoded.clone());
    let restored = BrainState::from_snapshot(decoded, 16);
    assert_snapshot_equal(
        serde_json::from_str(&owned_json).unwrap(),
        restored.to_snapshot(),
    );
}
