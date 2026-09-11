use super::*;
use khive_storage::{StorageError, WriterTaskRequestState};

#[test]
fn gap_only_converts_typed_pre_execution_refusals_and_stop_preserves_them() {
    let stopped = append_response(
        "events",
        TelemetryPolicy {
            carrier: TelemetryCarrier::Durable,
            failure_posture: TelemetryFailurePosture::Stop,
        },
        Err(RuntimeError::Storage(StorageError::WriteQueueFull {
            timeout_ms: 10,
        })),
    )
    .unwrap_err();
    assert!(matches!(
        stopped,
        RuntimeError::Storage(StorageError::WriteQueueFull { timeout_ms: 10 })
    ));
    let dropped = append_response(
        "events",
        TelemetryPolicy {
            carrier: TelemetryCarrier::Durable,
            failure_posture: TelemetryFailurePosture::Gap,
        },
        Err(RuntimeError::Storage(StorageError::WriteQueueFull {
            timeout_ms: 10,
        })),
    )
    .unwrap();
    assert_eq!(dropped["carrier"], "durable");
    assert_eq!(dropped["accepted"], false);
    assert_eq!(dropped["dropped"], true);
    assert_eq!(dropped["gap"], true);
    assert_eq!(dropped["domain_disposition"], "not_committed");
    assert_eq!(dropped["receipt_persisted"], false);
    assert!(dropped.get("seq").is_none());
}

#[test]
fn unknown_outcomes_propagate_unchanged_under_both_failure_postures() {
    for failure_posture in [TelemetryFailurePosture::Stop, TelemetryFailurePosture::Gap] {
        let unknown = append_response(
            "events",
            TelemetryPolicy {
                carrier: TelemetryCarrier::Durable,
                failure_posture,
            },
            Err(RuntimeError::Storage(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            })),
        )
        .unwrap_err();
        assert!(matches!(
            unknown,
            RuntimeError::Storage(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown
            })
        ));
        let untyped = append_response(
            "events",
            TelemetryPolicy {
                carrier: TelemetryCarrier::Durable,
                failure_posture,
            },
            Err(RuntimeError::Internal("write queue full".into())),
        )
        .unwrap_err();
        assert!(matches!(untyped, RuntimeError::Internal(_)));
    }
}

fn rollup(max_scanned: usize) -> Rollup {
    Rollup {
        since: timestamp("since", "2026-01-02T00:00:00Z").unwrap(),
        until: timestamp("until", "2026-01-03T00:00:00Z").unwrap(),
        group_by: vec!["kind".into()],
        kinds: None,
        head: None,
        after: 0,
        scanned: 0,
        max_scanned,
        total: 0,
        groups: BTreeMap::new(),
    }
}

fn entry(seq: i64, at: &str) -> StreamEntry {
    StreamEntry {
        seq,
        id: Uuid::new_v4().to_string(),
        record: json!({"kind":"run.started"}),
        created_at: timestamp("created_at", at).unwrap(),
    }
}

fn page(entries: Vec<StreamEntry>, head_seq: i64, next_after: Option<i64>) -> StreamPage {
    StreamPage {
        entries,
        head_seq,
        next_after,
    }
}

#[test]
fn initial_head_excludes_later_appends_and_sequence_holes_do_not_skip_rows() {
    let mut rollup = rollup(10);
    assert!(!rollup
        .consume(page(vec![entry(2, "2026-01-02T00:00:00Z")], 5, Some(2)))
        .unwrap());
    assert!(rollup
        .consume(page(
            vec![
                entry(4, "2026-01-02T00:00:00Z"),
                entry(6, "2026-01-02T00:00:00Z")
            ],
            6,
            None
        ))
        .unwrap());
    assert_eq!(rollup.total, 2);
    assert_eq!(rollup.scanned, 2);
    assert_eq!(rollup.head, Some(5));
    assert_eq!(rollup.after, 4);
}

#[test]
fn excluded_rows_consume_the_scan_bound_and_cannot_produce_partial_success() {
    let mut rollup = rollup(2);
    assert!(!rollup
        .consume(page(
            vec![
                entry(1, "2026-01-01T00:00:00Z"),
                entry(2, "2026-01-02T00:00:00Z")
            ],
            3,
            Some(2)
        ))
        .unwrap());
    let error = rollup
        .consume(page(vec![entry(3, "2026-01-03T00:00:00Z")], 3, None))
        .unwrap_err();
    assert!(error.to_string().contains("scan exceeds 2"));
}

#[test]
fn exactly_the_scan_bound_is_complete_and_empty_deleted_tail_terminates() {
    let mut rollup = rollup(2);
    assert!(!rollup
        .consume(page(
            vec![
                entry(1, "2026-01-02T00:00:00Z"),
                entry(2, "2026-01-03T00:00:00Z")
            ],
            4,
            Some(2)
        ))
        .unwrap());
    assert!(rollup.consume(page(vec![], 4, None)).unwrap());
    assert_eq!(rollup.total, 1);
    assert_eq!(rollup.scanned, 2);
}

#[test]
fn kind_exclusions_advance_the_cursor_before_filtering() {
    let mut rollup = rollup(10);
    rollup.kinds = Some(HashSet::from(["run.completed".into()]));
    assert!(!rollup
        .consume(page(vec![entry(2, "2026-01-02T00:00:00Z")], 3, Some(2)))
        .unwrap());
    assert_eq!(rollup.after, 2);
    assert_eq!(rollup.total, 0);
    let mut matching = entry(3, "2026-01-02T00:00:00Z");
    matching.record["kind"] = json!("run.completed");
    assert!(rollup.consume(page(vec![matching], 3, None)).unwrap());
    assert_eq!(rollup.total, 1);
}

#[test]
fn group_cardinality_and_key_size_are_bounded() {
    let mut by_kind = rollup(MAX_GROUPS + 1);
    let entries: Vec<_> = (1..=MAX_GROUPS + 1)
        .map(|seq| {
            let mut entry = entry(seq as i64, "2026-01-02T00:00:00Z");
            entry.record["kind"] = json!(format!("kind-{seq}"));
            entry
        })
        .collect();
    assert!(by_kind
        .consume(page(entries, (MAX_GROUPS + 1) as i64, None))
        .unwrap_err()
        .to_string()
        .contains("groups"));
    let mut oversized = rollup(10);
    let mut large = entry(1, "2026-01-02T00:00:00Z");
    large.record["kind"] = json!("x".repeat(MAX_GROUP_KEY_BYTES));
    assert!(oversized
        .consume(page(vec![large], 1, None))
        .unwrap_err()
        .to_string()
        .contains("group key"));
}

#[test]
fn dimensions_reject_non_scalar_values_and_invalid_paths() {
    let mut rollup = rollup(10);
    let mut nested = entry(1, "2026-01-02T00:00:00Z");
    nested.record["kind"] = json!([1]);
    assert!(rollup
        .consume(page(vec![nested], 1, None))
        .unwrap_err()
        .to_string()
        .contains("scalar"));
    for fields in [
        vec![],
        vec!["a..b".into()],
        vec!["x".repeat(129)],
        vec!["x".into(); 9],
    ] {
        assert!(dimensions(Some(fields)).is_err());
    }
}
