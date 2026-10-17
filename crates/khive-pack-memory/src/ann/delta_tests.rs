include!("delta_codec_tests.rs");
use super::super::{segment_commit_digest, write_external_ids_sidecar};
use super::*;
use std::fs;

fn persisted_base(dir: &Path) -> AnnBridge {
    let mut base = AnnBridge::build(
        vec![1.0, 0.0, 0.0, 0.0],
        4,
        vec![Uuid::new_v4()],
        std::collections::HashSet::new(),
    )
    .expect("build base");
    base.set_applied_seq(1);
    base.save_atomic(dir).expect("persist base");
    AnnBridge::load(dir).expect("load base")
}

fn publish(dir: &Path, bridge: &mut AnnBridge) -> DeltaPublication {
    let publication = write(dir, bridge).expect("publish delta");
    bridge.mark_delta_checkpoint(&publication);
    publication
}

fn delta_names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("list segment directory")
        .filter_map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .ok()
        })
        .filter(|name| name.starts_with("memory_delta-"))
        .collect()
}

#[test]
fn publication_coalesces_pending_batches_into_one_chunk() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let batches = [
        (
            vec![
                (a, Some(vec![0.0, 1.0, 0.0, 0.0])),
                (b, Some(vec![0.0, 0.0, 1.0, 0.0])),
            ],
            2,
            2,
        ),
        (vec![(a, Some(vec![0.0, 0.0, 0.0, 1.0]))], 3, 3),
        (vec![(b, None)], 4, 1),
    ];
    for (ops, seq, raw) in batches {
        bridge
            .apply_final_ops(ops.clone(), seq)
            .expect("apply batch");
        bridge.record_delta_batch(ops, seq, raw);
    }

    let publication = publish(dir, &mut bridge);
    assert_eq!(bridge.delta_chunks, 1, "pending batches are not chunks");

    // Pin the persisted format independently of writer and reader constants.
    let head_bytes = fs::read(dir.join("memory_delta.head")).expect("read literal HEAD path");
    assert_eq!(&head_bytes[..8], b"KHMDEH01", "persisted HEAD magic");
    let chunk_file = format!("memory_delta-{}.bin", publication.last_nonce);
    let chunk_bytes = fs::read(dir.join(&chunk_file)).expect("read literal chunk path");
    assert_eq!(&chunk_bytes[..8], b"KHMDEC01", "persisted chunk magic");

    assert_eq!(
        delta_names(dir),
        vec![chunk_file],
        "three pending batches publish exactly one chunk"
    );
    let base_digest = segment_commit_digest(dir).unwrap().expect("base commit");
    let overlay = read(dir, &base_digest, 1, 4, 1)
        .expect("read coalesced chain")
        .expect("HEAD exists");
    assert_eq!(overlay.batches.len(), 1);
    let chunk = &overlay.batches[0];
    assert_eq!((chunk.applied_seq, chunk.raw_count), (4, 6));
    assert_eq!(
        chunk.ops,
        vec![(a, Some(vec![0.0, 0.0, 0.0, 1.0])), (b, None)],
        "the last final-state operation per UUID wins"
    );
    let adopted = AnnBridge::load(dir).expect("replay coalesced chunk");
    assert_eq!(adopted.delta_chunks, 1);
    assert_eq!(adopted.commit_digest, Some(publication.identity));
    assert!(adopted.id_map.contains(&a));
}

fn add_delta_op(bridge: &mut AnnBridge, seq: u64) -> Uuid {
    let id = Uuid::new_v4();
    let ops = vec![(id, Some(vec![0.0, 1.0, seq as f32, 0.0]))];
    bridge
        .apply_final_ops(ops.clone(), seq)
        .expect("apply tail");
    bridge.record_delta_batch(ops, seq, 1);
    id
}

fn accept_checkpoint(bridge: &mut AnnBridge, written: super::super::WrittenCheckpoint) -> bool {
    match written {
        super::super::WrittenCheckpoint::Delta(publication) => {
            bridge.mark_delta_checkpoint(&publication);
            bridge.mark_checkpointed();
            false
        }
        super::super::WrittenCheckpoint::Full(digest) => {
            bridge.mark_full_checkpoint_base(digest);
            bridge.mark_checkpointed();
            true
        }
    }
}

#[test]
fn checkpoint_publication_compacts_before_chain_exceeds_cleanup_walk() {
    const WALK_CAP: usize = 2;
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    let base_metadata = fs::read(dir.join("metadata.bin")).expect("base metadata");
    let mut ids = Vec::new();
    for seq in 2..=4 {
        ids.push(add_delta_op(&mut bridge, seq));
    }
    assert_eq!(bridge.delta_batches.len(), 3);
    let first = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
        .expect("publish coalesced chunk");
    assert!(!accept_checkpoint(&mut bridge, first));
    assert_eq!(bridge.delta_chunks, 1);
    assert_eq!(bridge.delta_raw_ops, 3);
    ids.push(add_delta_op(&mut bridge, 5));
    let second = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
        .expect("publish chunk at walk boundary");
    assert!(!accept_checkpoint(&mut bridge, second));
    assert_eq!(bridge.delta_chunks, WALK_CAP);
    assert_eq!(fs::read(dir.join("metadata.bin")).unwrap(), base_metadata);
    let retired = retired_chain_within(dir, WALK_CAP);
    assert_eq!(retired.len(), WALK_CAP);
    assert_eq!(delta_names(dir).len(), WALK_CAP);

    bridge = AnnBridge::load(dir).expect("restart at chain boundary");
    ids.push(add_delta_op(&mut bridge, 6));
    assert!(bridge.delta_raw_ops < compaction_limit(bridge.base_ops));
    let third = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
        .expect("checkpoint after restored chain reaches walk bound");
    let full = accept_checkpoint(&mut bridge, third);
    let observed = retired_chain_within(dir, WALK_CAP + 1);
    assert!(
        observed.len() <= WALK_CAP,
        "PUBLISHED_CHAIN_EXCEEDS_CLEANUP_WALK: {} chunks, cap {WALK_CAP}",
        observed.len()
    );
    assert!(full, "chain bound must select a full checkpoint");
    assert_ne!(fs::read(dir.join("metadata.bin")).unwrap(), base_metadata);
    assert!(!dir.join(HEAD_FILE).exists());
    assert!(delta_names(dir).is_empty(), "full save retires every chunk");
    assert_eq!(bridge.delta_chunks, 0, "FULL_CHUNK_COUNT_RESET");
    assert!(bridge.delta_batches.is_empty());
    assert_eq!(bridge.delta_raw_ops, 0);
    let adopted = AnnBridge::load(dir).expect("adopt full checkpoint");
    assert_eq!(adopted.delta_chunks, 0);
    assert_eq!(adopted.index.last_applied_seq(), Some(6));
    for id in ids {
        assert!(adopted.id_map.contains(&id));
    }

    let next = add_delta_op(&mut bridge, 7);
    let first_new = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
        .expect("first delta over compacted base");
    assert!(!accept_checkpoint(&mut bridge, first_new));
    assert_eq!(bridge.delta_chunks, 1);
    let next_adopter = AnnBridge::load(dir).expect("adopt post-compaction delta");
    assert_eq!(next_adopter.delta_chunks, 1);
    assert!(next_adopter.id_map.contains(&next));
}

#[test]
fn failed_head_publication_keeps_accepted_chunk_count_and_retry_chain() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    add_delta_op(&mut bridge, 2);
    publish(dir, &mut bridge);
    assert_eq!(bridge.delta_chunks, 1);
    let head_before = fs::read(dir.join(HEAD_FILE)).expect("accepted HEAD");
    let identity_before = bridge.commit_digest;
    let nonce_before = bridge.last_delta_nonce;
    let pending = add_delta_op(&mut bridge, 3);
    let blocked_stage = dir.join(format!("{HEAD_FILE}.tmp"));
    fs::create_dir(&blocked_stage).expect("block HEAD staging, retain old HEAD");

    let refusal = write(dir, &bridge)
        .err()
        .expect("HEAD publication must fail");
    assert!(refusal.contains("publish memory delta HEAD"), "{refusal}");
    assert_eq!(bridge.delta_chunks, 1);
    assert_eq!(bridge.commit_digest, identity_before);
    assert_eq!(bridge.last_delta_nonce, nonce_before);
    assert_eq!(bridge.delta_batches.len(), 1);
    assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), head_before);
    assert_eq!(delta_names(dir).len(), 2, "failed HEAD leaves one orphan");
    let before_retry = AnnBridge::load(dir).expect("old accepted chain remains loadable");
    assert_eq!(before_retry.delta_chunks, 1);
    assert!(!before_retry.id_map.contains(&pending));

    fs::remove_dir(&blocked_stage).expect("unblock HEAD staging");
    publish(dir, &mut bridge);
    assert_eq!(bridge.delta_chunks, 2, "orphan must not count as accepted");
    assert_eq!(retired_chain_within(dir, 3).len(), 2);
    assert_eq!(delta_names(dir).len(), 3);
    let adopted = AnnBridge::load(dir).expect("adopt successful retry");
    assert_eq!(adopted.delta_chunks, 2);
    assert!(adopted.id_map.contains(&pending));
}

#[test]
fn failed_full_publication_retains_delta_count_and_pending_batch() {
    const WALK_CAP: usize = 2;
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    for seq in 2..=3 {
        add_delta_op(&mut bridge, seq);
        publish(dir, &mut bridge);
    }
    let pending = add_delta_op(&mut bridge, 4);
    let head_before = fs::read(dir.join(HEAD_FILE)).expect("accepted HEAD");
    let metadata_before = fs::read(dir.join("metadata.bin")).expect("accepted base");
    let identity_before = bridge.commit_digest;
    let invalid_dir = dir.join("not-a-checkpoint-directory");
    fs::write(&invalid_dir, b"regular file").expect("block checkpoint directory");

    assert!(super::super::write_file_checkpoint(&invalid_dir, &bridge, WALK_CAP).is_err());
    assert_eq!(bridge.delta_chunks, WALK_CAP);
    assert_eq!(bridge.delta_batches.len(), 1);
    assert_eq!(bridge.delta_raw_ops, 3);
    assert_eq!(bridge.commit_digest, identity_before);
    assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), head_before);
    assert_eq!(fs::read(dir.join("metadata.bin")).unwrap(), metadata_before);
    let still_published = AnnBridge::load(dir).expect("old accepted checkpoint is intact");
    assert_eq!(still_published.delta_chunks, WALK_CAP);
    assert!(!still_published.id_map.contains(&pending));

    let retry = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
        .expect("full publication retries from retained chain");
    assert!(accept_checkpoint(&mut bridge, retry));
    assert_eq!(bridge.delta_chunks, 0);
    let adopted = AnnBridge::load(dir).expect("adopt full retry");
    assert_eq!(adopted.delta_chunks, 0);
    assert!(adopted.id_map.contains(&pending));
    assert!(delta_names(dir).is_empty());
}

#[test]
fn full_checkpoint_reclaims_retired_chain_longer_than_scan_budget() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    let mut chain = Vec::new();
    for seq in 2..=7u64 {
        let ops = vec![(Uuid::new_v4(), Some(vec![0.0, 1.0, seq as f32, 0.0]))];
        bridge
            .apply_final_ops(ops.clone(), seq)
            .expect("apply tail");
        bridge.record_delta_batch(ops, seq, 1);
        chain.push(chunk_name(publish(dir, &mut bridge).last_nonce));
    }
    assert_eq!(chain.len(), 6);

    // Commit the new base exactly as a full save does, then retire the
    // chain with a scan budget far smaller than the chain.
    bridge.index.save_atomic(dir).expect("commit new base");
    let digest = segment_commit_digest(dir).unwrap().expect("new commit");
    write_external_ids_sidecar(dir, &digest, &bridge.id_map).expect("commit UUID sidecar");
    clear_with_scan_budget(dir, 1).expect("retire chain");

    assert!(!dir.join(HEAD_FILE).exists());
    let left: Vec<_> = chain
        .iter()
        .filter(|name| dir.join(name).exists())
        .collect();
    assert!(
        left.is_empty(),
        "every chunk on the retired chain must be removed, left {left:?}"
    );
    assert!(AnnBridge::load(dir).is_ok(), "the new base adopts cleanly");
}

/// Publish a six-chunk chain, then commit a new base over it exactly as a
/// full save does, leaving the chain retired but not yet cleaned.
fn retired_six_chunk_chain(dir: &Path) -> Vec<String> {
    let mut bridge = persisted_base(dir);
    let mut chain = Vec::new();
    for seq in 2..=7u64 {
        let ops = vec![(Uuid::new_v4(), Some(vec![0.0, 1.0, seq as f32, 0.0]))];
        bridge
            .apply_final_ops(ops.clone(), seq)
            .expect("apply tail");
        bridge.record_delta_batch(ops, seq, 1);
        chain.push(chunk_name(publish(dir, &mut bridge).last_nonce));
    }
    bridge.index.save_atomic(dir).expect("commit new base");
    let digest = segment_commit_digest(dir).unwrap().expect("new commit");
    write_external_ids_sidecar(dir, &digest, &bridge.id_map).expect("commit UUID sidecar");
    chain
}

#[test]
fn retired_chunks_are_removed_before_head() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path().to_path_buf();
    let chain = retired_six_chunk_chain(&dir);
    let observed = dir.clone();
    clear_with_scan_budget_then(&dir, 1, move || {
        let left: Vec<_> = chain
            .iter()
            .filter(|name| observed.join(name).exists())
            .collect();
        assert!(
            left.is_empty(),
            "HEAD must outlive every retired chunk, left at HEAD removal {left:?}"
        );
    })
    .expect("retire chain");
    assert!(!dir.join(HEAD_FILE).exists());
}

#[test]
fn retired_chain_cleanup_resumes_after_a_crash_before_head_removal() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let chain = retired_six_chunk_chain(dir);
    // A crash part-way through cleanup leaves HEAD and the newer chunks,
    // because chunks go oldest first and HEAD goes last.
    for name in &chain[..3] {
        fs::remove_file(dir.join(name)).expect("remove oldest chunk");
    }
    assert!(
        AnnBridge::load(dir).is_ok(),
        "a retired HEAD covered by the new base is ignored by the loader"
    );

    clear_with_scan_budget(dir, 1).expect("resume cleanup");

    assert!(!dir.join(HEAD_FILE).exists());
    let left: Vec<_> = chain
        .iter()
        .filter(|name| dir.join(name).exists())
        .collect();
    assert!(left.is_empty(), "resumed cleanup must remove {left:?}");
}

/// A checksum-valid HEAD naming `nonce` with an arbitrary raw count.
fn head_with_count(nonce: Uuid, raw_count: u64) -> Vec<u8> {
    let mut head = Vec::with_capacity(HEAD_LEN);
    head.extend_from_slice(HEAD_MAGIC);
    head.extend_from_slice(&[7u8; 32]);
    head.extend_from_slice(nonce.as_bytes());
    head.extend_from_slice(&1u64.to_le_bytes());
    head.extend_from_slice(&2u64.to_le_bytes());
    head.extend_from_slice(&raw_count.to_le_bytes());
    let hash = checksum(&head, &[]);
    head.extend_from_slice(&hash);
    head
}

fn write_chunk_linking(dir: &Path, nonce: Uuid, previous: Uuid) {
    let batch = DeltaBatch {
        applied_seq: 2,
        raw_count: 1,
        ops: vec![(Uuid::new_v4(), None)],
    };
    fs::write(
        dir.join(chunk_name(nonce)),
        encode_chunk(&[7u8; 32], nonce, previous, &batch),
    )
    .expect("write linked chunk");
}

#[test]
fn retired_chain_stops_at_a_chunk_linked_to_itself() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let only = Uuid::from_u128(1);
    write_chunk_linking(dir, only, only);
    fs::write(dir.join(HEAD_FILE), head_with_count(only, u64::MAX)).expect("write HEAD");

    assert_eq!(
        retired_chain(dir),
        vec![chunk_name(only)],
        "a self-linked chunk is collected once and the walk ends"
    );
}

#[test]
fn retired_chain_stops_when_two_chunks_link_to_each_other() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
    write_chunk_linking(dir, first, second);
    write_chunk_linking(dir, second, first);
    fs::write(dir.join(HEAD_FILE), head_with_count(first, u64::MAX)).expect("write HEAD");

    assert_eq!(
        retired_chain(dir),
        vec![chunk_name(first), chunk_name(second)],
        "each chunk of a cycle is collected once"
    );
}

#[test]
fn retired_chain_returns_every_chunk_newest_first() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut expected = retired_six_chunk_chain(dir);
    expected.reverse();

    assert_eq!(retired_chain(dir), expected);
}

#[test]
fn retired_chain_stops_at_the_walk_bound() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut newest_first = retired_six_chunk_chain(dir);
    newest_first.reverse();

    assert_eq!(
        retired_chain_within(dir, 3),
        newest_first[..3].to_vec(),
        "only the newest chunks within the bound are collected"
    );
}

#[test]
fn orphan_sweep_removes_stale_chunk_staging_files() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let staged = format!("{}.tmp", chunk_name(Uuid::new_v4()));
    fs::write(dir.join(&staged), b"interrupted staging").expect("stale staging file");
    fs::write(dir.join("unrelated.tmp"), b"keep").expect("unrelated file");

    clear_with_scan_budget(dir, 10).expect("sweep");

    assert!(
        !dir.join(&staged).exists(),
        "stale chunk staging file removed"
    );
    assert!(
        dir.join("unrelated.tmp").exists(),
        "the sweep removes only delta chunk names"
    );
}

#[test]
fn full_save_succeeds_when_cleanup_fails_after_commit() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let mut bridge = persisted_base(dir);
    let before = fs::read(dir.join("metadata.bin")).expect("base metadata");
    // A directory at the HEAD name cannot be unlinked as a file, so
    // cleanup fails only after the new base has committed.
    fs::create_dir(dir.join(HEAD_FILE)).expect("unremovable HEAD entry");
    assert!(clear(dir).is_err(), "control: cleanup itself must fail");
    bridge.set_applied_seq(2);

    let digest = bridge
        .save_atomic(dir)
        .expect("a committed full save reports success");

    assert_ne!(fs::read(dir.join("metadata.bin")).unwrap(), before);
    assert_eq!(segment_commit_digest(dir).unwrap(), Some(digest));
}

#[test]
fn orphan_cleanup_bounds_directory_visits_after_removing_head() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    fs::write(dir.join(HEAD_FILE), b"old HEAD").expect("write old HEAD");
    for nonce in 1..=3 {
        fs::write(dir.join(chunk_name(Uuid::from_u128(nonce))), b"orphan")
            .expect("write orphan chunk");
    }

    clear_with_scan_budget(dir, 2).expect("bounded orphan cleanup");

    assert!(!dir.join(HEAD_FILE).exists(), "HEAD must be removed first");
    let remaining = fs::read_dir(dir)
        .expect("list remaining chunks")
        .filter(|entry| {
            entry
                .as_ref()
                .expect("directory entry")
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("memory_delta-") && name.ends_with(".bin"))
        })
        .count();
    assert_eq!(remaining, 1, "only two directory entries may be visited");

    clear_with_scan_budget(dir, 2).expect("resume orphan cleanup");
    assert_eq!(
        fs::read_dir(dir).expect("list drained directory").count(),
        0
    );
}

#[test]
fn orphan_cleanup_keeps_one_directory_after_path_replacement() {
    let temp = tempfile::tempdir().expect("parent directory");
    let dir = temp.path().join("checkpoint");
    let moved = temp.path().join("moved-checkpoint");
    fs::create_dir(&dir).expect("create checkpoint directory");
    let old_chunk = chunk_name(Uuid::from_u128(1));
    let replacement_chunk = chunk_name(Uuid::from_u128(2));
    fs::write(dir.join(HEAD_FILE), b"old HEAD").expect("write old HEAD");
    fs::write(dir.join(&old_chunk), b"old orphan").expect("write old chunk");

    clear_with_scan_budget_then(&dir, 10, || {
        fs::rename(&dir, &moved).expect("move opened directory");
        fs::create_dir(&dir).expect("replace checkpoint directory");
        fs::write(dir.join(HEAD_FILE), b"replacement HEAD").expect("write replacement HEAD");
        fs::write(dir.join(&replacement_chunk), b"replacement chunk")
            .expect("write replacement chunk");
    })
    .expect("cleanup opened directory");

    assert!(!moved.join(HEAD_FILE).exists());
    assert!(!moved.join(&old_chunk).exists());
    assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), b"replacement HEAD");
    assert_eq!(
        fs::read(dir.join(replacement_chunk)).unwrap(),
        b"replacement chunk"
    );
}
