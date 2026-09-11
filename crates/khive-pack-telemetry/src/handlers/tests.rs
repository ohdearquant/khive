use super::*;
use khive_runtime::RuntimeConfig;
use khive_storage::{StorageError, WriterTaskRequestState};

#[tokio::test]
async fn all_kinds_read_requires_a_resolvable_non_null_current_policy() {
    for declared in [false, true] {
        // Policy is immutable in-memory configuration, not an I/O service.
        // An absent declaration makes the policy unavailable to the read.
        // Exercise this boundary directly as well as registry activation tests.
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: None,
            brain_profile: None,
            telemetry: khive_runtime::TelemetryConfig {
                default_carrier: declared.then_some(TelemetryCarrier::Ephemeral),
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let token = runtime
            .authorize(khive_runtime::Namespace::local())
            .unwrap();
        let result = read(&runtime, &token, json!({"stream":"events"})).await;
        if declared {
            let value = result.unwrap();
            assert_eq!(value["coverage"]["classification_scope"], "all_kinds");
            assert!(value["coverage"]["ephemeral"].is_null());
            let policy = value["coverage"].get("current_policy").unwrap();
            assert!(policy.is_object());
            assert_eq!(policy["default_carrier"], "ephemeral");
            assert_eq!(policy["channels"], json!([]));
        } else {
            let error = runtime_error_value(result.unwrap_err(), DomainDisposition::Unknown);
            assert!(error["message"]
                .as_str()
                .unwrap()
                .contains("telemetry.default_carrier"));
            assert!(error.get("coverage").is_none());
            assert!(error.get("current_policy").is_none());
        }
    }
}

fn policy(failure_posture: TelemetryFailurePosture) -> TelemetryPolicy {
    TelemetryPolicy {
        carrier: TelemetryCarrier::Durable,
        failure_posture,
        classified: true,
    }
}

#[tokio::test]
async fn proven_append_refusal_drops_in_gap_and_propagates_in_stop() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    for posture in [TelemetryFailurePosture::Stop, TelemetryFailurePosture::Gap] {
        let failure = runtime
            .stream_append_with_outcome(
                &token,
                "events",
                &json!({"kind":"run.started"}),
                Some(99),
                "observation",
                None,
                None,
                Some(false),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.disposition(), StreamAppendDisposition::NotCommitted);
        let result = append_response("events", policy(posture), Err(failure));
        match posture {
            TelemetryFailurePosture::Stop => {
                let source = runtime_error_value(result.unwrap_err(), DomainDisposition::Unknown);
                assert!(source.to_string().contains("sequence"), "{source}");
            }
            TelemetryFailurePosture::Gap => {
                let value = result.unwrap();
                assert_eq!(value["outcome"], "dropped");
                assert_eq!(value["carrier"], "durable");
                assert!(value["receipt_id"].is_null());
                assert!(value.get("error").is_some());
                assert!(value.get("seq").is_none());
                assert!(value.get("dropped").is_none());
            }
        }
        assert_eq!(
            runtime.stream_stat(&token, "events").await.unwrap()["count"],
            0
        );
    }
}

#[test]
fn unknown_append_outcome_preserves_exact_error_and_stop_propagates() {
    fn source() -> RuntimeError {
        RuntimeError::Storage(StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
        })
    }
    let expected = runtime_error_value(source(), DomainDisposition::Unknown);
    let value = append_response(
        "events",
        policy(TelemetryFailurePosture::Gap),
        Err(StreamAppendFailure::unknown(source())),
    )
    .unwrap();
    assert_eq!(value["outcome"], "unknown");
    assert!(value["receipt_id"].is_null());
    assert!(value.get("seq").is_none());
    assert_eq!(value["error"], expected);
    assert_eq!(
        serde_json::to_vec(&value["error"]).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let stopped = append_response(
        "events",
        policy(TelemetryFailurePosture::Stop),
        Err(StreamAppendFailure::unknown(source())),
    )
    .unwrap_err();
    assert_eq!(
        runtime_error_value(stopped, DomainDisposition::Unknown),
        expected
    );
    let lookalike = append_response(
        "events",
        policy(TelemetryFailurePosture::Gap),
        Err(StreamAppendFailure::unknown(RuntimeError::Internal(
            "write queue full".into(),
        ))),
    )
    .unwrap();
    assert_eq!(lookalike["outcome"], "unknown");
}

fn rollup(max_scanned: usize) -> Rollup {
    Rollup {
        since: timestamp("since", "2026-01-02T00:00:00Z").unwrap(),
        until: timestamp("until", "2026-01-03T00:00:00Z").unwrap(),
        group_by: vec!["kind".into()],
        kinds: None,
        scope: ActorScope::all(),
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
