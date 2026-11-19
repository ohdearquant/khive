//! The shared bridge core keeps the on-disk pairing and the search results of the per-pack
//! bridges it replaced.

#![cfg(feature = "mmap")]

use std::path::Path;

use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{
    read_external_ids_sidecar, segment_commit_digest, write_external_ids_sidecar, VamanaConfig,
    VamanaIndex,
};
use uuid::Uuid;

const DIM: usize = 4;

fn fixture_ids() -> Vec<Uuid> {
    (1..=5u128).map(Uuid::from_u128).collect()
}

fn fixture_vectors() -> Vec<f32> {
    let rows: [[f32; DIM]; 5] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 2.0, 0.0, 0.0],
        [0.0, 0.0, 3.0, 0.0],
        [0.0, 0.0, 0.0, 4.0],
        [1.0, 1.0, 1.0, 0.0],
    ];
    rows.iter().flatten().copied().collect()
}

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|value| value.to_bits()).collect()
}

fn hit_bits(hits: &[(Uuid, f32)]) -> Vec<(Uuid, u32)> {
    hits.iter()
        .map(|(id, distance)| (*id, distance.to_bits()))
        .collect()
}

/// Writes a segment from the underlying primitives alone, in the order the per-pack bridges
/// did: normalize, build, save the segments, digest the commit record, bind the id map to it.
fn write_segment_from_primitives(dir: &Path) -> (VamanaIndex, [u8; 32]) {
    let mut vectors = fixture_vectors();
    for row in vectors.chunks_exact_mut(DIM) {
        l2_normalize(row);
    }
    let cfg = VamanaConfig::with_dimensions(DIM);
    let index = VamanaIndex::build_owned(vectors, cfg).unwrap();
    index.save_atomic(dir).unwrap();
    let digest = segment_commit_digest(dir).unwrap().unwrap();
    write_external_ids_sidecar(dir, &digest, &fixture_ids()).unwrap();
    (index, digest)
}

/// The hits the per-pack bridges returned: normalize the query, search, name each ordinal.
fn reference_hits(index: &VamanaIndex, ids: &[Uuid], query: &[f32], k: usize) -> Vec<(Uuid, f32)> {
    let mut normalized = query.to_vec();
    l2_normalize(&mut normalized);
    index
        .search(&normalized, k)
        .unwrap()
        .into_iter()
        .filter_map(|(ordinal, distance)| ids.get(ordinal as usize).map(|id| (*id, distance)))
        .collect()
}

#[test]
fn core_loads_a_segment_written_from_the_primitives() {
    let dir = tempfile::tempdir().unwrap();
    let (original, digest) = write_segment_from_primitives(dir.path());

    let (core, loaded_digest) = AnnBridgeCore::load(dir.path()).unwrap();

    assert_eq!(loaded_digest, digest);
    assert_eq!(
        core.commit_digest,
        Some(digest),
        "load keeps the commit identity"
    );
    assert_eq!(core.id_map, fixture_ids());
    assert_eq!(
        bits(core.index.vectors().unwrap()),
        bits(original.vectors().unwrap())
    );
    let query = [2.0, 0.5, 0.25, 0.0];
    let expected = reference_hits(&original, &fixture_ids(), &query, 3);
    assert_eq!(
        hit_bits(&core.search_hits(&query, 3).unwrap()),
        hit_bits(&expected)
    );
}

#[test]
fn core_save_atomic_commits_the_pairing_the_primitives_read() {
    let ids = fixture_ids();
    let core = AnnBridgeCore::build(fixture_vectors(), DIM, ids.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();

    let digest = core.save_atomic(dir.path()).unwrap();

    assert_eq!(segment_commit_digest(dir.path()).unwrap(), Some(digest));
    let (sidecar_digest, sidecar_ids) = read_external_ids_sidecar(dir.path()).unwrap();
    assert_eq!(sidecar_digest, digest);
    assert_eq!(sidecar_ids, ids);
    let reopened = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(
        bits(reopened.vectors().unwrap()),
        bits(core.index.vectors().unwrap())
    );
}

#[test]
fn search_hits_normalize_the_query_and_name_the_subjects() {
    let ids = fixture_ids();
    let core = AnnBridgeCore::build(fixture_vectors(), DIM, ids.clone()).unwrap();
    let query = [2.0, 0.5, 0.25, 0.0];

    let hits = core.search_hits(&query, 3).unwrap();

    assert_eq!(hits.len(), 3);
    assert_eq!(
        hit_bits(&hits),
        hit_bits(&reference_hits(&core.index, &ids, &query, 3))
    );
    let scaled = [8.0, 2.0, 1.0, 0.0];
    assert_eq!(
        hit_bits(&core.search_hits(&scaled, 3).unwrap()),
        hit_bits(&hits),
        "a query is compared by direction, not by length"
    );
}
