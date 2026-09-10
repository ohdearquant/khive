use super::*;

#[tokio::test]
async fn expiry_arm7_writer_clock_statement_trace_is_once_before_observations() {
    let (runtime, token, registry) = fixture();
    for expired in [false, true] {
        let prepared = runtime
            .prepare_stream_batch(&token, vec![append("expiry-trace", None)], &registry)
            .await
            .unwrap();
        let trace = TraceAccess(Arc::new(Mutex::new(vec![])));
        let observations = vec![
            StreamObservation {
                key: "first".into(),
                kind: "head".into(),
                version: Some(1),
                live_until: Some("expires_at".into()),
            },
            StreamObservation {
                key: if expired { "expired" } else { "second" }.into(),
                kind: "head".into(),
                version: Some(1),
                live_until: Some("expires_at".into()),
            },
        ];
        let result = run_prepared_stream_batch(
            &trace,
            token.namespace().as_str().into(),
            prepared,
            None,
            observations,
        )
        .await;
        let trace = trace.0.lock().unwrap();
        assert_eq!(trace[0].sql, "BEGIN");
        let clocks: Vec<_> = trace
            .iter()
            .enumerate()
            .filter(|(_, s)| s.label.as_deref() == Some("stream-batch-clock"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(clocks.len(), 1, "one clock serves every observed entry");
        let checks: Vec<_> = trace
            .iter()
            .enumerate()
            .filter(|(_, s)| s.label.as_deref() == Some("stream-batch-observed"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(checks.len(), 2);
        assert!(clocks[0] > 0 && clocks[0] < checks[0]);
        assert_eq!(trace[clocks[0]].sql, "SELECT khive_now_micros()");
        assert!(
            trace[clocks[0]].params.is_empty(),
            "the clock is evaluated by SQL, not passed from arrival time"
        );
        if expired {
            let RuntimeError::Khive(error) = result.err().expect("equal deadline must refuse")
            else {
                panic!("structured refusal")
            };
            assert_eq!(error.details().unwrap().get("reason"), Some("expired"));
            assert_eq!(error.details().unwrap().get("key"), Some("expired"));
            assert_eq!(
                error.details().unwrap().get("now"),
                Some("1970-01-01T00:00:00.000001Z")
            );
        } else {
            assert!(result.unwrap().is_ok());
        }
        let insert = trace.iter().position(|s| s.sql.starts_with("INSERT"));
        if expired {
            assert!(insert.is_none());
            assert_eq!(trace.last().unwrap().sql, "ROLLBACK");
        } else {
            assert!(checks[1] < insert.unwrap());
            assert_eq!(trace.last().unwrap().sql, "COMMIT");
        }
    }
}

#[tokio::test]
async fn expiry_arm7_real_writer_clock_on_memory_and_file_connections() {
    let (memory, token, registry) = fixture();
    let (_dir, file, _peer, file_token, file_registry) = file_fixture();
    for (runtime, token, registry) in [
        (&memory, &token, &registry),
        (&file, &file_token, &file_registry),
    ] {
        let mut spec = write("clock", None);
        spec.doc = json!({"expires_at":"2000-01-01T00:00:00Z"});
        batch_write(runtime, token, registry, spec).await;
        let before = chrono::Utc::now();
        let result = runtime
            .stream_batch_atomic(
                token,
                vec![append("expired", None)],
                None,
                vec![StreamObservation {
                    key: "clock".into(),
                    kind: "head".into(),
                    version: Some(1),
                    live_until: Some("expires_at".into()),
                }],
                registry,
            )
            .await;
        let after = chrono::Utc::now();
        let RuntimeError::Khive(error) = result.err().unwrap() else {
            panic!("expired")
        };
        assert_eq!(error.details().unwrap().get("reason"), Some("expired"));
        let now =
            chrono::DateTime::parse_from_rfc3339(error.details().unwrap().get("now").unwrap())
                .unwrap();
        assert!(
            now.timestamp_micros() >= before.timestamp_micros()
                && now.timestamp_micros() <= after.timestamp_micros(),
            "SQL function uses the note stamp's UTC clock"
        );
        assert_eq!(
            runtime.stream_stat(token, "expired").await.unwrap()["count"],
            0
        );
    }
}

#[tokio::test]
async fn expiry_arm5_runtime_null_version_before_preparation() {
    let (runtime, token, registry) = fixture();
    let before = stream_store_snapshot(&runtime).await;
    let error = runtime
        .stream_batch_atomic(
            &token,
            vec![append("bad", None)],
            None,
            vec![StreamObservation {
                key: "absent".into(),
                kind: "head".into(),
                version: None,
                live_until: Some("expires_at".into()),
            }],
            &registry,
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert_eq!(stream_store_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn expiry_arm8_timestamp_survives_later_writer_before_reply() {
    let (_dir, runtime, peer, token, registry) = file_fixture();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let captured = observed.clone();
    let other = peer.clone();
    runtime.install_note_mutation_hook(Arc::new(move |_: String, id: Uuid| {
        let peer = other.clone();
        let captured = captured.clone();
        Box::pin(async move {
            let token = peer.authorize(Namespace::local()).unwrap();
            let prior = peer
                .get_note_by_key(&token, "result-time", Some("head"), false)
                .await
                .unwrap();
            assert_eq!(prior.id, id);
            // A genuine second connection writes after commit and before the
            // first caller receives its reply (post-commit hook boundary).
            peer.sql()
                .writer()
                .await
                .unwrap()
                .execute(statement(
                    "UPDATE notes SET content=?1, updated_at=?2 WHERE id=?3",
                    vec![
                        SqlValue::Text("{\"later\":true}".into()),
                        SqlValue::Integer(prior.updated_at + 100),
                        SqlValue::Text(id.to_string()),
                    ],
                ))
                .await
                .unwrap();
            captured.lock().unwrap().push(prior);
        })
    }));
    for expected in [None, Some(2)] {
        let result = batch_write(&runtime, &token, &registry, write("result-time", expected)).await;
        let prior = observed.lock().unwrap().last().unwrap().clone();
        let current = peer
            .get_note_by_key(&token, "result-time", Some("head"), false)
            .await
            .unwrap();
        assert_eq!(result["version"], prior.version);
        assert_eq!(result["updated_at"], micros_to_iso(prior.updated_at));
        assert_eq!(current.version, prior.version + 1);
        assert_ne!(result["updated_at"], micros_to_iso(current.updated_at));
    }
    assert_eq!(
        observed.lock().unwrap().len(),
        2,
        "create and update each saw the later writer"
    );
}
