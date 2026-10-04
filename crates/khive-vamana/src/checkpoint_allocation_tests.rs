use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Default)]
struct AllocationProbe {
    enabled: bool,
    requested_bytes: usize,
    largest_request: usize,
    medoid_comparisons: usize,
    rebuild_active_mappings: Option<usize>,
}

thread_local! {
    static ALLOCATIONS: Cell<AllocationProbe> = const { Cell::new(AllocationProbe {
        enabled: false,
        requested_bytes: 0,
        largest_request: 0,
        medoid_comparisons: 0,
        rebuild_active_mappings: None,
    }) };
    static ACTIVE_MAPPINGS: Cell<usize> = const { Cell::new(0) };
}

struct MeasuredAllocator;

fn record_allocation(size: usize) {
    // Const-initialized TLS and Cell updates allocate nothing. During thread
    // teardown an unavailable TLS slot must not make deallocation panic.
    let _ = ALLOCATIONS.try_with(|cell| {
        let mut probe = cell.get();
        if probe.enabled {
            probe.requested_bytes = probe.requested_bytes.saturating_add(size);
            probe.largest_request = probe.largest_request.max(size);
            cell.set(probe);
        }
    });
}

// SAFETY: every operation delegates its unchanged pointer/layout contract to
// System; the observer neither allocates nor dereferences caller pointers.
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: pointer, layout, and size retain the caller's realloc contract.
        let resized = unsafe { System.realloc(pointer, layout, size) };
        if !resized.is_null() {
            record_allocation(size);
        }
        resized
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout retain the caller's deallocation contract.
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

struct AllocationScope;
impl Drop for AllocationScope {
    fn drop(&mut self) {
        ALLOCATIONS.with(|cell| {
            let mut probe = cell.get();
            probe.enabled = false;
            cell.set(probe);
        });
    }
}

fn measure<T>(operation: impl FnOnce() -> T) -> (T, AllocationProbe) {
    ALLOCATIONS.with(|cell| {
        assert!(!cell.get().enabled, "allocation scopes must not overlap");
        cell.set(AllocationProbe {
            enabled: true,
            ..Default::default()
        });
    });
    let scope = AllocationScope;
    let output = operation();
    drop(scope);
    (output, ALLOCATIONS.with(Cell::get))
}

pub(super) fn record_medoid_comparison() {
    ALLOCATIONS.with(|cell| {
        let mut probe = cell.get();
        if probe.enabled {
            probe.medoid_comparisons += 1;
            cell.set(probe);
        }
    });
}

pub(super) fn record_mapping_opened() {
    ACTIVE_MAPPINGS.with(|cell| cell.set(cell.get() + 1));
}

pub(super) fn record_mapping_closed() {
    ACTIVE_MAPPINGS.with(|cell| cell.set(cell.get().saturating_sub(1)));
}

pub(super) fn record_rebuild() {
    ALLOCATIONS.with(|cell| {
        let mut probe = cell.get();
        if probe.enabled {
            probe.rebuild_active_mappings = Some(ACTIVE_MAPPINGS.with(Cell::get));
            cell.set(probe);
        }
    });
}

fn dense_checkpoint() -> (tempfile::TempDir, VamanaIndex, Vec<f32>) {
    let rows = 64;
    let dimensions = 128;
    let vectors = perf_compat_tests::corpus(rows, dimensions);
    let config = VamanaConfig::with_dimensions(dimensions)
        .with_max_degree(rows - 1)
        .with_search_list_size(rows);
    let mut index = VamanaIndex::build(&vectors, config).unwrap();
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
    (dir, index, vectors)
}

fn segment_allocation_bound(path: &Path) -> usize {
    ["vectors.bin", "graph.bin", "lifecycle.bin", "codes.bin"]
        .into_iter()
        .map(|name| fs::metadata(path.join(name)).unwrap().len() as usize)
        .min()
        .unwrap()
}

#[test]
fn real_v2_load_does_not_allocate_a_whole_checkpoint_segment() {
    let (dir, expected, vectors) = dense_checkpoint();
    let bound = segment_allocation_bound(dir.path());
    let (loaded, allocations) = measure(|| VamanaIndex::load(dir.path()));
    let loaded = loaded.unwrap();
    assert!(
        allocations.largest_request < bound,
        "real load allocated a segment-sized heap buffer: largest={}, bound={bound}",
        allocations.largest_request
    );
    assert_eq!(
        loaded.to_bytes(&[]).unwrap(),
        expected.to_bytes(&[]).unwrap()
    );
    assert_eq!(
        perf_compat_tests::result_bits(loaded.search(&vectors[..128], 7).unwrap()),
        perf_compat_tests::result_bits(expected.search(&vectors[..128], 7).unwrap())
    );
    let (restored, allocations) = measure(|| {
        VamanaIndex::load_or_build_with_sequence(
            dir.path(),
            &vectors,
            expected.config.clone(),
            Some(9),
        )
    });
    assert_eq!(restored.unwrap().last_applied_seq(), Some(7));
    assert!(
        allocations.largest_request < bound,
        "load_or_build fast path allocated a segment-sized heap buffer: largest={}, bound={bound}",
        allocations.largest_request
    );
}

#[test]
fn real_sequence_guard_does_not_allocate_a_whole_checkpoint_segment() {
    let (dir, _, _) = dense_checkpoint();
    let bound = segment_allocation_bound(dir.path());
    let (verdict, allocations) =
        measure(|| reject_checkpoint_sequence_regression(dir.path(), Some(7)));
    verdict.unwrap();
    assert!(
        allocations.largest_request < bound,
        "sequence guard allocated a segment-sized heap buffer: largest={}, bound={bound}",
        allocations.largest_request
    );
    assert!(
        matches!(
            reject_checkpoint_sequence_regression(dir.path(), Some(6)),
            Err(VamanaError::CheckpointSequenceRegression {
                candidate: Some(6),
                incumbent: 7
            })
        ),
        "valid incumbent must refuse a lower-sequence checkpoint"
    );
    assert!(matches!(
        reject_checkpoint_sequence_regression(dir.path(), None),
        Err(VamanaError::CheckpointSequenceRegression {
            candidate: None,
            incumbent: 7
        })
    ));
}

fn star_graph(nodes: usize) -> (VamanaGraph, ParsedLifecycle) {
    let mut graph = VamanaGraph::new(nodes, 0).unwrap();
    graph.adjacency_mut_for_load()[0] = (1..nodes as u32).collect();
    for neighbors in &mut graph.adjacency_mut_for_load()[1..] {
        *neighbors = vec![0];
    }
    let lifecycle = ParsedLifecycle {
        tombstones: vec![0; nodes.div_ceil(64)],
        free_slots: vec![],
        reverse_adj: graph.adjacency().to_vec(),
        ops_since_consolidation: 0,
    };
    (graph, lifecycle)
}

#[test]
fn inverse_validation_uses_counts_without_copying_a_second_inverse() {
    let nodes = 1024;
    let (graph, lifecycle) = star_graph(nodes);
    let original = lifecycle.reverse_adj.clone();
    validate_v2_structural(&graph, &lifecycle, nodes).unwrap();
    let (valid, allocations) = measure(|| validate_v2_structural(&graph, &lifecycle, nodes));
    assert_eq!(valid.unwrap(), 0);
    let expected = nodes * std::mem::size_of::<usize>() + (nodes - 1) * 4;
    assert_eq!(
        allocations.requested_bytes, expected,
        "successful inverse validation must allocate only counts and one medoid forward copy"
    );
    assert_eq!(lifecycle.reverse_adj, original);
}

#[test]
fn inverse_validation_does_not_scan_a_high_degree_medoid_for_each_edge() {
    let nodes = 1024;
    let (graph, lifecycle) = star_graph(nodes);
    validate_v2_structural(&graph, &lifecycle, nodes).unwrap();
    let (valid, allocations) = measure(|| validate_v2_structural(&graph, &lifecycle, nodes));
    valid.unwrap();
    assert!(allocations.medoid_comparisons > 0);
    assert!(
        allocations.medoid_comparisons <= nodes * (nodes.ilog2() as usize + 2),
        "high-degree medoid membership must use logarithmic searches: {} comparisons",
        allocations.medoid_comparisons
    );
}

fn legacy_inverse_error(graph: &VamanaGraph, lifecycle: &ParsedLifecycle) -> Option<String> {
    let mut expected: Vec<Vec<u32>> = vec![Vec::new(); graph.node_count()];
    for (source, neighbors) in graph.adjacency().iter().enumerate() {
        for &target in neighbors {
            expected[target as usize].push(source as u32);
        }
    }
    for (v, (exp, got)) in expected.iter().zip(&lifecycle.reverse_adj).enumerate() {
        let mut got_sorted = got.clone();
        got_sorted.sort_unstable();
        if *exp != got_sorted {
            return Some(format!(
                "lifecycle.bin reverse_adj[{v}] is not the inverse of graph.bin \
                 forward adjacency: expected {exp:?}, got {got_sorted:?}"
            ));
        }
    }
    None
}

#[test]
fn inverse_validation_preserves_missing_and_balanced_phantom_diagnostics() {
    let (graph, mut lifecycle) = star_graph(8);
    lifecycle.reverse_adj[0].reverse();
    validate_v2_structural(&graph, &lifecycle, 8).unwrap();
    for phantom in [false, true] {
        let mut altered = ParsedLifecycle {
            tombstones: lifecycle.tombstones.clone(),
            free_slots: vec![],
            reverse_adj: lifecycle.reverse_adj.clone(),
            ops_since_consolidation: 0,
        };
        if phantom {
            altered.reverse_adj[1] = vec![2];
        } else {
            altered.reverse_adj[1].clear();
        }
        let result = validate_v2_structural(&graph, &altered, 8);
        assert!(
            result.is_err(),
            "inverse validation must refuse {}",
            if phantom {
                "a balanced phantom"
            } else {
                "a missing edge"
            }
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, VamanaError::InvalidFormat { reason }
                if Some(reason.clone()) == legacy_inverse_error(&graph, &altered)),
            "inverse validation must retain the same first-vertex diagnostic: {error}"
        );
    }
}

#[test]
fn inverse_validation_accepts_unsorted_medoid_and_rejects_medoid_phantom() {
    let mut graph = VamanaGraph::new(6, 0).unwrap();
    *graph.adjacency_mut_for_load() = vec![vec![5, 2], vec![3], vec![0], vec![0], vec![1], vec![4]];
    let mut reverse_adj = vec![vec![]; 6];
    for (parent, neighbors) in graph.adjacency().iter().enumerate() {
        for &target in neighbors {
            reverse_adj[target as usize].push(parent as u32);
        }
    }
    let graph = parse_graph(&encode_graph_lossless(&graph).unwrap(), 5, 6).unwrap();
    let mut lifecycle =
        parse_lifecycle(&encode_lifecycle(&[0], &[], &reverse_adj, 0), 6, 5).unwrap();
    assert_eq!(graph.adjacency()[0], [5, 2]);
    assert_eq!(
        validate_v2_structural(&graph, &lifecycle, 6).unwrap(),
        0,
        "a parsed valid inverse accepts a distance-ordered medoid list"
    );

    // Node 3 has one real parent (1), so the replacement keeps its cardinality.
    // Medoid 0 does not point to 3: only medoid membership rejects this phantom.
    lifecycle.reverse_adj[3] = vec![0];
    let lifecycle = parse_lifecycle(
        &encode_lifecycle(&[0], &[], &lifecycle.reverse_adj, 0),
        6,
        5,
    )
    .unwrap();
    let error = validate_v2_structural(&graph, &lifecycle, 6)
        .expect_err("a balanced phantom whose parent is the medoid must be refused");
    assert!(
        matches!(&error, VamanaError::InvalidFormat { reason }
            if Some(reason.clone()) == legacy_inverse_error(&graph, &lifecycle)),
        "medoid phantom retains the first-vertex inverse diagnostic: {error}"
    );
}

#[test]
fn real_v2_load_preserves_unsorted_medoid_adjacency() {
    let (dir, mut index, _) = dense_checkpoint();
    let medoid = index.graph.medoid() as usize;
    index.graph.adjacency_mut_for_load()[medoid].reverse();
    let stored = index.graph.adjacency()[medoid].clone();
    assert!(stored.windows(2).all(|pair| pair[0] > pair[1]));
    index.save_atomic(dir.path()).unwrap();
    let loaded = VamanaIndex::load(dir.path())
        .expect("real checkpoint load accepts an unsorted medoid forward list");
    assert_eq!(loaded.graph.adjacency()[medoid], stored);
    assert_eq!(loaded.to_bytes(&[]).unwrap(), index.to_bytes(&[]).unwrap());
}

#[test]
fn mapped_empty_and_missing_segments_retain_hash_and_format_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.bin");
    assert_eq!(
        map_checkpoint_segment(&path).err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    fs::write(&path, []).unwrap();
    let mapped = map_checkpoint_segment(&path);
    assert!(
        mapped.is_ok(),
        "an empty segment must reach format checks without a mapping error"
    );
    let segment = mapped.unwrap();
    assert!(segment.is_empty());
    assert_eq!(blake3::hash(&segment), blake3::hash(&[]));
    assert!(matches!(
        parse_graph(&segment, 1, 1),
        Err(VamanaError::InvalidFormat { reason }) if reason == "graph.bin too short"
    ));
    assert!(matches!(
        parse_lifecycle(&segment, 1, 1),
        Err(VamanaError::InvalidFormat { .. })
    ));
    assert!(matches!(
        parse_codes_bin(&segment, 1, 1),
        Err(VamanaError::InvalidFormat { reason }) if reason == "codes.bin missing or bad magic"
    ));
}

fn rewrite_segment_checksum(path: &Path, name: &str, data: &[u8]) {
    let mut commit = parse_v2_commit(&fs::read(path.join("metadata.bin")).unwrap()).unwrap();
    let hash = *blake3::hash(data).as_bytes();
    match name {
        "graph.bin" => commit.graph_hash = hash,
        "lifecycle.bin" => commit.lifecycle_hash = hash,
        "codes.bin" => commit.codes_hash = Some(hash),
        _ => unreachable!(),
    }
    write_v2_commit_full(
        &path.join("metadata.bin"),
        &commit.vectors_hash,
        &commit.graph_hash,
        &commit.lifecycle_hash,
        &commit.fingerprint,
        commit.index_meta.num_vectors,
        commit.index_meta.dimensions,
        commit.index_meta.max_degree,
        commit.index_meta.search_list_size,
        commit.index_meta.alpha,
        commit.last_applied_seq,
        commit.codes_hash.as_ref(),
        None,
    )
    .unwrap();
}

#[test]
fn mapped_corrupt_segments_fail_closed_and_release_views_before_repair() {
    for name in ["graph.bin", "lifecycle.bin", "codes.bin"] {
        for corruption in ["checksum", "empty", "truncated"] {
            let (dir, index, vectors) = dense_checkpoint();
            let valid = VamanaIndex::load(dir.path()).expect("valid checkpoint before corruption");
            assert_eq!(valid.last_applied_seq(), Some(7));
            assert_eq!(valid.vectors().unwrap(), vectors);
            drop(valid);
            let path = dir.path().join(name);
            let mut data = fs::read(&path).unwrap();
            match corruption {
                "checksum" => data[0] ^= 1,
                "empty" => data.clear(),
                "truncated" => data.truncate(7),
                _ => unreachable!(),
            }
            fs::write(&path, &data).unwrap();
            if corruption != "checksum" {
                rewrite_segment_checksum(dir.path(), name, &data);
            }
            let strict = VamanaIndex::load(dir.path());
            assert!(
                matches!(&strict, Err(VamanaError::InvalidFormat { .. })),
                "strict load must refuse {corruption} corruption in {name}"
            );
            if name == "codes.bin" && corruption == "empty" {
                assert!(
                    matches!(&strict, Err(VamanaError::InvalidFormat { reason })
                        if reason == "codes.bin missing or bad magic"),
                    "checksum-valid empty codes must reach structural validation"
                );
            }
            if corruption == "checksum" {
                assert!(
                    matches!(strict, Err(VamanaError::InvalidFormat { reason })
                        if reason.contains("checksum")),
                    "checksum refusal must precede segment format checks: {name}"
                );
            }
            assert!(
                reject_checkpoint_sequence_regression(dir.path(), Some(6)).is_ok(),
                "a corrupt incumbent must not block a lower-sequence repair: {name}/{corruption}"
            );
            let (rebuilt, allocations) = measure(|| {
                VamanaIndex::load_or_build_with_sequence(
                    dir.path(),
                    &vectors,
                    index.config.clone(),
                    Some(6),
                )
            });
            assert_eq!(
                allocations.rebuild_active_mappings,
                Some(0),
                "temporary segment views must close before a repair publishes: {name}/{corruption}"
            );
            assert_eq!(rebuilt.unwrap().last_applied_seq(), Some(6));
            let loaded = VamanaIndex::load(dir.path()).unwrap();
            assert_eq!(loaded.last_applied_seq(), Some(6));
            assert_eq!(loaded.vectors().unwrap(), vectors);
        }
    }
}
