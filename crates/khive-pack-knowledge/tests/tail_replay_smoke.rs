#[path = "../benches/support/tail_replay.rs"]
mod support;

#[tokio::test]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn knowledge_tail_replay_one_index_write_replays_one_subject() {
    let sample = support::measure(64, support::Arm::OneIndex, 1, 1000.0, true).await;
    assert!(
        sample.raw_rows > 0,
        "one actual index write must append a real tail"
    );
    assert_eq!(sample.distinct_subjects, 1);
    assert_eq!(
        sample.replay_point_reads, 1,
        "one index write must replay its one final subject after slot invalidation"
    );
    assert_eq!(sample.final_state_scans, 1);
    assert!(
        sample.report["post_search_watermark"].as_u64().unwrap()
            >= sample.report["last_workload_seq"].as_u64().unwrap()
    );
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn knowledge_tail_rewrites_coalesce_by_subject_without_losing_raw_rows() {
    let same = support::measure(64, support::Arm::SameSubject, 4, 1000.0, true).await;
    let distinct = support::measure(64, support::Arm::DistinctSubjects, 4, 1000.0, true).await;
    assert_eq!(same.raw_rows, distinct.raw_rows);
    assert!(same.raw_rows >= 4);
    assert_eq!(same.distinct_subjects, 1);
    assert_eq!(distinct.distinct_subjects, 4);
    assert_eq!(same.replay_point_reads, 1);
    assert_eq!(distinct.replay_point_reads, 4);
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn knowledge_tail_delete_distinguishes_atom_verb_from_vector_tail() {
    let verb = support::measure(64, support::Arm::VerbDelete, 1, 1000.0, true).await;
    let composed = support::measure(64, support::Arm::ComposedVectorDelete, 1, 1000.0, true).await;
    assert_eq!(
        verb.raw_rows, 0,
        "the actual atom verb does not emit a vector delete tail"
    );
    assert_eq!(verb.distinct_subjects, 0);
    assert!(composed.raw_rows > 0);
    assert_eq!(composed.distinct_subjects, 1);
    assert_eq!(
        composed.replay_point_reads, 0,
        "delete final states require no embedding point read"
    );
}
