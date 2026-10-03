use super::*;

fn lifecycle_index() -> VamanaIndex {
    let vectors = [1.0, 0.0, 0.0, 1.0, -1.0, 0.0, 0.0, -1.0];
    let mut index = VamanaIndex::build(&vectors, VamanaConfig::with_dimensions(2)).unwrap();
    index.tombstone(1).unwrap();
    index.set_last_applied_seq(Some(17));
    index
}

#[test]
fn maintenance_fork_preserves_owned_lifecycle_and_isolates_mutations() {
    let original = lifecycle_index();
    let mut fork = original.fork_for_maintenance();
    assert_eq!(fork.graph, original.graph);
    assert_eq!(fork.tombstones, original.tombstones);
    assert_eq!(fork.free_slots, original.free_slots);
    assert_eq!(
        fork.ops_since_consolidation,
        original.ops_since_consolidation
    );
    assert_eq!(fork.consolidation_tau, original.consolidation_tau);
    assert_eq!(fork.last_applied_seq, original.last_applied_seq);
    assert_eq!(fork.vectors().unwrap(), original.vectors().unwrap());
    assert_eq!(fork.gs_codec.min, original.gs_codec.min);
    assert_eq!(fork.gs_codec.gs, original.gs_codec.gs);
    for ordinal in 0..original.num_vectors {
        assert_eq!(
            fork.gs_codes.view().code(ordinal),
            original.gs_codes.view().code(ordinal)
        );
    }
    let mut sequential = original.fork_for_maintenance();
    assert_eq!(fork.insert(&[0.6, 0.8]).unwrap(), 1);
    sequential.insert(&[0.6, 0.8]).unwrap();
    assert_eq!(
        fork.consolidate().unwrap(),
        sequential.consolidate().unwrap()
    );
    assert_eq!(fork.graph, sequential.graph);
    assert_eq!(fork.vectors().unwrap(), sequential.vectors().unwrap());
    assert!(original.is_tombstoned(1));
    assert_eq!(original.last_applied_seq(), Some(17));
}

#[cfg(feature = "mmap")]
#[test]
fn maintenance_fork_shares_vector_and_sq8_mappings_until_mutation() {
    let dir = tempfile::tempdir().unwrap();
    lifecycle_index().save_atomic(dir.path()).unwrap();
    let original = VamanaIndex::load(dir.path()).unwrap();
    let mut fork = original.fork_for_maintenance();
    match (&original.vectors, &fork.vectors) {
        (VectorStorage::Mmap { mmap: old, .. }, VectorStorage::Mmap { mmap: new, .. }) => {
            assert!(std::sync::Arc::ptr_eq(old, new));
        }
        _ => panic!("maintenance fork must retain mapped vectors"),
    }
    match (&original.gs_codes, &fork.gs_codes) {
        (CodeStore::Mmap { mmap: old, .. }, CodeStore::Mmap { mmap: new, .. }) => {
            assert!(std::sync::Arc::ptr_eq(old, new));
        }
        _ => panic!("maintenance fork must retain mapped SQ8 codes"),
    }
    assert_eq!(fork.graph, original.graph);
    assert_eq!(fork.tombstones, original.tombstones);
    assert_eq!(fork.free_slots, original.free_slots);
    let before = original.search(&[1.0, 0.0], 4).unwrap();
    fork.insert(&[0.6, 0.8]).unwrap();
    assert!(matches!(fork.vectors, VectorStorage::Owned(_)));
    assert!(matches!(fork.gs_codes, CodeStore::Owned(_)));
    assert!(matches!(original.vectors, VectorStorage::Mmap { .. }));
    assert!(matches!(original.gs_codes, CodeStore::Mmap { .. }));
    assert_eq!(original.search(&[1.0, 0.0], 4).unwrap(), before);
    assert!(original.is_tombstoned(1));
}
