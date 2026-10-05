use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

// libtest's main thread can allocate while its test thread measures synchronous
// checkpoint I/O. Keep a signed per-thread ledger, including outside a measure:
// freeing a pre-existing or transferred allocation must not underflow it.
thread_local! {
    static LIVE_BYTES: Cell<i128> = const { Cell::new(0) };
    static PEAK_BYTES: Cell<i128> = const { Cell::new(0) };
}

pub(super) fn record_allocation(size: usize) {
    // Const-initialized Cell access allocates nothing; teardown may lack TLS.
    let _ = LIVE_BYTES.try_with(|bytes| {
        let live = bytes.get() + size as i128;
        bytes.set(live);
        let _ = PEAK_BYTES.try_with(|peak| peak.set(peak.get().max(live)));
    });
}

pub(super) fn record_deallocation(size: usize) {
    let _ = LIVE_BYTES.try_with(|bytes| bytes.set(bytes.get() - size as i128));
}

pub(super) fn record_reallocation(old_size: usize, new_size: usize) {
    if new_size >= old_size {
        record_allocation(new_size - old_size);
    } else {
        record_deallocation(old_size - new_size);
    }
}

fn live_bytes() -> i128 {
    LIVE_BYTES.with(Cell::get)
}

#[derive(Debug)]
struct HeapPeak {
    baseline: i128,
    peak: i128,
    after: i128,
}

impl HeapPeak {
    fn additional_bytes(&self) -> usize {
        usize::try_from(self.peak - self.baseline).unwrap()
    }
}

fn measure_peak<T>(operation: impl FnOnce() -> T) -> (T, HeapPeak) {
    let baseline = live_bytes();
    PEAK_BYTES.with(|peak| peak.set(baseline));
    let result = operation();
    let peak = HeapPeak {
        baseline,
        peak: PEAK_BYTES.with(Cell::get),
        after: live_bytes(),
    };
    (result, peak)
}

fn run_in_child(name: &str) -> bool {
    const MARKER: &str = "KHIVE_VAMANA_PEAK_TEST";
    let (_, module) = module_path!().split_once("::").unwrap();
    let name = format!("{module}::{name}");
    if std::env::var(MARKER).as_deref() == Ok(&name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(MARKER, &name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.lines().any(|line| line == "running 1 test"),
        "isolated peak measurement failed or selected no test: {name}\n{stdout}\n{stderr}"
    );
    print!("{stdout}");
    true
}

#[test]
fn live_heap_accounts_for_frees_and_successful_and_failed_resizes() {
    if run_in_child("live_heap_accounts_for_frees_and_successful_and_failed_resizes") {
        return;
    }
    let old_layout = Layout::from_size_align(8 * 1024, 8).unwrap();
    // SAFETY: layouts are nonzero and valid, every successful pointer is freed
    // exactly once with its current layout, and failed realloc retains ownership.
    unsafe {
        let old = ALLOCATOR.alloc(old_layout);
        assert!(!old.is_null());
        let ((), peak) = measure_peak(|| {
            ALLOCATOR.dealloc(old, old_layout);
            let layout = Layout::from_size_align(16 * 1024, 8).unwrap();
            let pointer = ALLOCATOR.alloc_zeroed(layout);
            assert!(!pointer.is_null());
            assert!(std::slice::from_raw_parts(pointer, layout.size())
                .iter()
                .all(|byte| *byte == 0));
            let grown = ALLOCATOR.realloc(pointer, layout, 32 * 1024);
            assert!(!grown.is_null());
            let grown_layout = Layout::from_size_align(32 * 1024, 8).unwrap();
            let before_failure = live_bytes();
            // This valid request exceeds the addressable heap; failure must not
            // decrement the still-owned original or account nonexistent bytes.
            let impossible_size = (isize::MAX as usize) & !7;
            let failed = ALLOCATOR.realloc(grown, grown_layout, impossible_size);
            assert!(failed.is_null());
            assert_eq!(live_bytes(), before_failure);
            let shrunk = ALLOCATOR.realloc(grown, grown_layout, 8 * 1024);
            assert!(!shrunk.is_null());
            ALLOCATOR.dealloc(shrunk, old_layout);
        });
        assert_eq!(peak.additional_bytes(), 24 * 1024);
        assert_eq!(peak.after + old_layout.size() as i128, peak.baseline);
    }
}

#[test]
fn live_heap_ignores_concurrent_allocations_on_another_thread() {
    if run_in_child("live_heap_ignores_concurrent_allocations_on_another_thread") {
        return;
    }
    fn wait_for(phase: &AtomicUsize, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while phase.load(Ordering::Acquire) != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "allocation rendezvous timed out"
            );
            std::thread::yield_now();
        }
    }
    let phase = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let other = scope.spawn(|| {
            wait_for(&phase, 1);
            let layout = Layout::from_size_align(128 * 1024, 8).unwrap();
            // SAFETY: the nonzero layout is valid; the successful pointer is
            // freed exactly once, after the measuring thread observes it.
            unsafe {
                let pointer = ALLOCATOR.alloc_zeroed(layout);
                assert!(!pointer.is_null());
                phase.store(2, Ordering::Release);
                wait_for(&phase, 3);
                ALLOCATOR.dealloc(pointer, layout);
            }
            phase.store(4, Ordering::Release);
        });
        let (during_allocation, peak) = measure_peak(|| {
            phase.store(1, Ordering::Release);
            wait_for(&phase, 2);
            let during = live_bytes();
            phase.store(3, Ordering::Release);
            wait_for(&phase, 4);
            during
        });
        other.join().unwrap();
        assert_eq!(
            during_allocation, peak.baseline,
            "another thread entered the live-byte ledger"
        );
        assert_eq!(peak.after, peak.baseline);
        assert_eq!(
            peak.additional_bytes(),
            0,
            "another thread entered the measured peak"
        );
    });
}

fn checkpoint(dimensions: usize) -> (tempfile::TempDir, VamanaIndex) {
    let rows = 64;
    let vectors = perf_compat_tests::corpus(rows, dimensions);
    let config = VamanaConfig::with_dimensions(dimensions)
        .with_max_degree(rows - 1)
        .with_search_list_size(rows);
    let mut index = VamanaIndex::build_owned(vectors, config).unwrap();
    let adjacency: Vec<Vec<u32>> = (0..rows)
        .map(|source| {
            (0..rows as u32)
                .filter(|&target| target as usize != source)
                .collect()
        })
        .collect();
    *index.graph.adjacency_mut_for_load() = adjacency.clone();
    index.graph.restore_reverse_adj(adjacency);
    index.set_last_applied_seq(Some(7));
    let dir = tempfile::tempdir().unwrap();
    index.save_atomic(dir.path()).unwrap();
    (dir, index)
}

const DIMENSIONS: [usize; 2] = [2048, 8192];
// Owned graph/lifecycle lists, paths and metadata remain small at a fixed 64
// rows. Codec minima use 4 bytes/dimension; save also serializes SQ8 code bytes.
const GRAPH_AND_IO_SLACK: usize = 128 * 1024;

#[test]
fn real_v2_load_peak_stays_below_vector_payload() {
    if run_in_child("real_v2_load_peak_stays_below_vector_payload") {
        return;
    }
    let mut measurements = Vec::new();
    for dimensions in DIMENSIONS {
        let (dir, expected) = checkpoint(dimensions);
        let vector_bytes = fs::metadata(dir.path().join("vectors.bin")).unwrap().len() as usize;
        let (loaded, peak) = measure_peak(|| VamanaIndex::load(dir.path()));
        let loaded = loaded.unwrap();
        let additional = peak.additional_bytes();
        assert!(
            peak.after > peak.baseline,
            "loaded owned state must be counted: {peak:?}"
        );
        assert!(peak.peak >= peak.after, "{peak:?}");
        println!("load dimensions={dimensions} vectors={vector_bytes} heap={peak:?}");
        assert!(additional < vector_bytes / 2, "{peak:?}");
        assert!(
            additional <= dimensions * 4 + GRAPH_AND_IO_SLACK,
            "{peak:?}"
        );
        assert_eq!(loaded.vectors().unwrap(), expected.vectors().unwrap());
        assert_eq!(
            loaded.to_bytes(&[]).unwrap(),
            expected.to_bytes(&[]).unwrap()
        );
        assert_eq!(loaded.last_applied_seq(), Some(7));
        let query = &expected.vectors().unwrap()[..dimensions];
        assert_eq!(
            perf_compat_tests::result_bits(loaded.search(query, 7).unwrap()),
            perf_compat_tests::result_bits(expected.search(query, 7).unwrap())
        );
        measurements.push((vector_bytes, additional));
    }
    assert_eq!(measurements[1].0, measurements[0].0 * 4);
    assert!(
        measurements[1].1.saturating_sub(measurements[0].1)
            <= (DIMENSIONS[1] - DIMENSIONS[0]) * 4 + GRAPH_AND_IO_SLACK,
        "peak growth must reflect codec/graph storage, not a vector payload: {measurements:?}"
    );
}

#[test]
fn real_save_atomic_peak_excludes_an_incumbent_vector_copy() {
    if run_in_child("real_save_atomic_peak_excludes_an_incumbent_vector_copy") {
        return;
    }
    let mut measurements = Vec::new();
    for dimensions in DIMENSIONS {
        let (dir, mut index) = checkpoint(dimensions);
        let vector_bytes = fs::metadata(dir.path().join("vectors.bin")).unwrap().len() as usize;
        let codes_bytes = fs::metadata(dir.path().join("codes.bin")).unwrap().len() as usize;
        let (saved, peak) = measure_peak(|| index.save_atomic(dir.path()));
        saved.unwrap();
        let additional = peak.additional_bytes();
        println!("save dimensions={dimensions} vectors={vector_bytes} heap={peak:?}");
        assert!(additional < vector_bytes / 2, "{peak:?}");
        assert!(additional <= codes_bytes + GRAPH_AND_IO_SLACK, "{peak:?}");
        let loaded = VamanaIndex::load(dir.path()).unwrap();
        assert_eq!(loaded.to_bytes(&[]).unwrap(), index.to_bytes(&[]).unwrap());
        assert_eq!(loaded.last_applied_seq(), Some(7));
        drop(loaded);
        measurements.push((vector_bytes, codes_bytes, additional));

        let commit = fs::read(dir.path().join("metadata.bin")).unwrap();
        index.set_last_applied_seq(Some(6));
        let (refused, refusal_peak) = measure_peak(|| index.save_atomic(dir.path()));
        assert!(matches!(
            refused,
            Err(VamanaError::CheckpointSequenceRegression {
                candidate: Some(6),
                incumbent: 7,
            })
        ));
        assert!(
            refusal_peak.additional_bytes() <= dimensions * 4 + GRAPH_AND_IO_SLACK,
            "the public save's refusing incumbent guard must also avoid a vector copy: {refusal_peak:?}"
        );
        assert_eq!(fs::read(dir.path().join("metadata.bin")).unwrap(), commit);
        for name in [
            "vectors.bin.v2new",
            "graph.bin.v2new",
            "lifecycle.bin.v2new",
            "codes.bin.v2new",
            "metadata.bin.tmp",
        ] {
            assert!(!dir.path().join(name).exists());
        }
    }
    assert_eq!(measurements[1].0, measurements[0].0 * 4);
    assert!(
        measurements[1].2.saturating_sub(measurements[0].2)
            <= measurements[1].1 - measurements[0].1 + GRAPH_AND_IO_SLACK,
        "save peak growth must allow codes serialization without an incumbent vector copy: {measurements:?}"
    );
}
