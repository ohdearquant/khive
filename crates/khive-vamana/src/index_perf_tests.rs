use super::*;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};

use super::perf_compat_tests::{config, corpus, fresh_search, result_bits};

#[test]
fn search_reuses_corpus_sized_visited_after_warmup() {
    let vectors = corpus(128, 8);
    let index = VamanaIndex::build(&vectors, config(8)).unwrap();
    assert!(!index.search(&vectors[..8], 7).unwrap().is_empty());
    let warmed = index.search_visited.allocations.load(Ordering::Relaxed);
    assert_eq!(warmed, 1, "warmup must allocate the first visited tracker");
    for query in vectors.chunks_exact(8) {
        assert_eq!(
            result_bits(index.search(query, 7).unwrap()),
            fresh_search(&index, query, 7)
        );
    }
    assert_eq!(
        index.search_visited.allocations.load(Ordering::Relaxed),
        warmed,
        "a warmed sequential search must not allocate another corpus-sized tracker"
    );
}

#[test]
fn concurrent_searches_lease_independent_trackers() {
    let vectors = corpus(64, 8);
    let index = Arc::new(VamanaIndex::build(&vectors, config(8)).unwrap());
    {
        let mut first = index.search_visited.checkout(64);
        let mut second = index.search_visited.checkout(64);
        assert!(first.mark_if_new(0));
        assert!(second.mark_if_new(0), "active leases must not share marks");
        assert!(!first.mark_if_new(0));
        assert!(!second.mark_if_new(0));
        assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 2);
    }
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|offset| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            let query = vectors[offset * 8..(offset + 1) * 8].to_vec();
            let expected = fresh_search(&index, &query, 7);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..16 {
                    assert_eq!(result_bits(index.search(&query, 7).unwrap()), expected);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(index.search_visited.allocations.load(Ordering::Relaxed) <= 4);
}

#[test]
fn build_owned_keeps_buffer_and_matches_borrowed_index() {
    let vectors = corpus(64, 8);
    let borrowed = VamanaIndex::build(&vectors, config(8)).unwrap();
    let pointer = vectors.as_ptr();
    let owned = VamanaIndex::build_owned(vectors, config(8)).unwrap();
    assert_eq!(owned.vectors().unwrap().as_ptr(), pointer);
    assert_eq!(
        owned.to_bytes(&[]).unwrap(),
        borrowed.to_bytes(&[]).unwrap()
    );
    for query in borrowed.vectors().unwrap().chunks_exact(8) {
        assert_eq!(
            result_bits(owned.search(query, 7).unwrap()),
            result_bits(borrowed.search(query, 7).unwrap())
        );
    }
}

#[cfg(feature = "mmap")]
#[derive(Clone, Copy, Default)]
struct VectorHashReadProbe {
    calls: usize,
    bytes: usize,
    largest_request: usize,
}

#[cfg(feature = "mmap")]
thread_local! {
    static VECTOR_HASH_READS: std::cell::Cell<VectorHashReadProbe> = const {
        std::cell::Cell::new(VectorHashReadProbe { calls: 0, bytes: 0, largest_request: 0 })
    };
}

#[cfg(feature = "mmap")]
pub(super) fn record_vector_hash_read(requested: usize, read: usize) {
    VECTOR_HASH_READS.with(|cell| {
        let mut probe = cell.get();
        probe.calls += 1;
        probe.bytes += read;
        probe.largest_request = probe.largest_request.max(requested);
        cell.set(probe);
    });
}

#[cfg(feature = "mmap")]
fn reset_hash_probe() {
    VECTOR_HASH_READS.with(|cell| cell.set(VectorHashReadProbe::default()));
}

#[cfg(feature = "mmap")]
fn assert_bounded_hash_reads(expected_len: usize) {
    VECTOR_HASH_READS.with(|cell| {
        let probe = cell.get();
        assert_eq!(
            probe.bytes, expected_len,
            "the actual vector file must be hashed"
        );
        assert!(
            probe.largest_request <= VECTOR_HASH_CHUNK_BYTES,
            "vector hashing must not request a whole-file buffer"
        );
        assert!(
            probe.calls > 1,
            "the fixture must execute multiple chunk reads"
        );
    });
}

#[cfg(feature = "mmap")]
#[test]
fn streamed_vector_hash_matches_whole_file_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vectors.bin");
    for len in [0, 17, VECTOR_HASH_CHUNK_BYTES * 3 + 17] {
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &bytes).unwrap();
        reset_hash_probe();
        let (hash, actual_len) = hash_vectors_file(&path).unwrap();
        assert_eq!(hash, *blake3::hash(&bytes).as_bytes());
        assert_eq!(actual_len, len);
        if len > VECTOR_HASH_CHUNK_BYTES {
            assert_bounded_hash_reads(len);
        }
    }
}

#[cfg(feature = "mmap")]
#[test]
fn v2_load_and_sequence_guard_stream_vector_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let dim = VECTOR_HASH_CHUNK_BYTES / 4 + 1;
    let vectors = corpus(3, dim);
    let mut index = VamanaIndex::build(&vectors, config(dim)).unwrap();
    index.set_last_applied_seq(Some(7));
    index.save_atomic(dir.path()).unwrap();
    let vector_len = fs::metadata(dir.path().join("vectors.bin")).unwrap().len() as usize;
    assert!(vector_len > VECTOR_HASH_CHUNK_BYTES);

    reset_hash_probe();
    let loaded = VamanaIndex::load(dir.path()).unwrap();
    assert_bounded_hash_reads(vector_len);
    assert_eq!(loaded.last_applied_seq(), Some(7));
    assert_eq!(loaded.to_bytes(&[]).unwrap(), index.to_bytes(&[]).unwrap());

    reset_hash_probe();
    loaded.save_atomic(dir.path()).unwrap();
    assert_bounded_hash_reads(vector_len);
    index.set_last_applied_seq(Some(6));
    assert!(matches!(
        index.save_atomic(dir.path()),
        Err(VamanaError::CheckpointSequenceRegression {
            candidate: Some(6),
            incumbent: 7
        })
    ));
}
