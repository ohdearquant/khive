//! Ordered-stream acceptance through the real registry; no new API required to compile the baseline.
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

fn surface() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    (rt, builder.build().unwrap())
}

async fn population(rt: &KhiveRuntime) -> Vec<Value> {
    let mut reader = rt.sql().reader().await.unwrap();
    // Include domain audit only: registry gate audits are allowed on refusal.
    let mut out = Vec::new();
    for sql in [
        "SELECT COUNT(*) FROM notes",
        "SELECT COUNT(*) FROM note_streams",
        "SELECT COUNT(*) FROM fts_notes",
        "SELECT COUNT(*) FROM events WHERE kind != 'audit'",
    ] {
        let value = reader
            .query_scalar(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        let Some(SqlValue::Integer(n)) = value else {
            panic!("integer count")
        };
        out.push(json!(n));
    }
    out
}

fn reason(error: RuntimeError, expected: &str) -> Value {
    let RuntimeError::Khive(error) = error else {
        panic!("structured conflict: {error:?}")
    };
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["kind"], "conflict");
    assert_eq!(value["details"]["reason"], expected);
    value
}

async fn lease(registry: &VerbRegistry, key: &str) -> Value {
    registry
        .dispatch("create", json!({"kind":"head", "key":key, "content":"{}"}))
        .await
        .unwrap()
}

fn fence(key: &str, version: i64) -> Value {
    json!({"key":key, "kind":"head", "expected_version":version})
}

#[tokio::test]
async fn ordered_fences_commit_objects_and_lists_on_notes_and_streams() {
    let (_, registry) = surface();
    let a = lease(&registry, "lease/a").await;
    let b = lease(&registry, "lease/b").await;
    for (index, fences) in [
        fence("lease/a", 1),
        json!([fence("lease/a", 1)]),
        json!([fence("lease/a", 1), fence("lease/b", 1)]),
    ]
    .into_iter()
    .enumerate()
    {
        let target = registry
            .dispatch(
                "create",
                json!({"kind":"head", "content":"{}", "fence":fences}),
            )
            .await
            .unwrap();
        assert_eq!(target["version"], 1);
        registry.dispatch("update", json!({"id":target["id"], "content":"{\"written\":true}", "expected_version":1, "fence":fences})).await.unwrap();
        let after = registry
            .dispatch("get", json!({"id":target["id"]}))
            .await
            .unwrap();
        assert_eq!(after["version"], 2);
        assert_eq!(after["content"], "{\"written\":true}");
        let appended = registry
            .dispatch(
                "stream.append",
                json!({"stream":"fenced", "record":index, "fence":fences}),
            )
            .await
            .unwrap();
        assert_eq!(appended["seq"], index + 1);
    }
    for original in [a, b] {
        let after = registry
            .dispatch("get", json!({"id":original["id"]}))
            .await
            .unwrap();
        assert_eq!(after["version"], original["version"]);
        assert_eq!(after["content"], original["content"]);
    }
}

#[tokio::test]
async fn ordered_fences_refuse_first_stale_index_without_mutation() {
    let (rt, registry) = surface();
    let a = lease(&registry, "lease/a").await;
    lease(&registry, "lease/b").await;
    let target = lease(&registry, "target").await;
    for (fences, key, current, index) in [
        (fence("lease/a", 2), "lease/a", Some("1"), None),
        (
            json!([fence("lease/a", 2)]),
            "lease/a",
            Some("1"),
            Some("0"),
        ),
        (
            json!([fence("lease/a", 1), fence("lease/b", 2)]),
            "lease/b",
            Some("1"),
            Some("1"),
        ),
        (
            json!([fence("lease/a", 1), fence("missing", 2)]),
            "missing",
            None,
            Some("1"),
        ),
        (
            json!([fence("lease/a", 2), fence("lease/b", 2)]),
            "lease/a",
            Some("1"),
            Some("0"),
        ),
    ] {
        for (verb, args) in [
            (
                "create",
                json!({"kind":"head", "content":"{}", "fence":fences}),
            ),
            (
                "update",
                json!({"id":target["id"], "content":"{\"bad\":true}", "fence":fences}),
            ),
            (
                "stream.append",
                json!({"stream":"refused", "record":null, "fence":fences}),
            ),
        ] {
            let before = population(&rt).await;
            let error = reason(
                registry.dispatch(verb, args).await.unwrap_err(),
                "fence_conflict",
            );
            let mut expected = json!({"reason":"fence_conflict","key":key,"expected_version":"2"});
            if let Some(current) = current {
                expected["current_version"] = json!(current);
            }
            if let Some(index) = index {
                expected["index"] = json!(index);
            }
            assert_eq!(error["details"], expected);
            assert_eq!(population(&rt).await, before);
            assert_eq!(
                registry
                    .dispatch("stream.stat", json!({"stream":"refused"}))
                    .await
                    .unwrap()["head_seq"],
                0
            );
            for original in [&a, &target] {
                let after = registry
                    .dispatch("get", json!({"id":original["id"]}))
                    .await
                    .unwrap();
                assert_eq!(after["version"], original["version"]);
                assert_eq!(after["content"], original["content"]);
            }
        }
    }
}

#[tokio::test]
async fn ordered_fences_reject_malformed_input_without_domain_writes() {
    let (rt, registry) = surface();
    let target = lease(&registry, "target").await;
    for fences in [
        Value::Null,
        json!([]),
        json!([fence("x", 1), fence("x", 2)]),
        json!([fence("x", 0)]),
        json!([{"kind":"head","key":"x","expected_version":1,"extra":true}]),
        json!([null]),
        json!([{"kind":"","key":"x","expected_version":1}]),
        json!([fence("x\0y", 1)]),
    ] {
        for (verb, args) in [
            (
                "create",
                json!({"kind":"head","content":"{}","fence":fences}),
            ),
            (
                "update",
                json!({"id":target["id"],"content":"{}","fence":fences}),
            ),
            (
                "stream.append",
                json!({"stream":"invalid","record":null,"fence":fences}),
            ),
        ] {
            let before = population(&rt).await;
            let error = registry.dispatch(verb, args).await.unwrap_err();
            assert!(
                matches!(error, RuntimeError::InvalidInput(_)),
                "{verb}: {error:?}"
            );
            if fences.as_array().is_some_and(|items| items.len() == 2) {
                assert!(
                    error.to_string().contains("0") && error.to_string().contains("1"),
                    "{error}"
                );
            }
            assert_eq!(population(&rt).await, before);
        }
    }
}

#[tokio::test]
async fn stream_dense_per_stream_and_json_values() {
    let (_, registry) = surface();
    let values = [
        Value::Null,
        json!("scalar"),
        json!(3),
        json!(false),
        json!([1, {"n": 2}]),
    ];
    for (index, record) in values.iter().enumerate() {
        for stream in ["a", "b"] {
            let appended = registry
                .dispatch("stream.append", json!({"stream": stream, "record": record}))
                .await
                .unwrap();
            assert_eq!(appended["seq"], index + 1);
            uuid::Uuid::parse_str(appended["id"].as_str().unwrap()).unwrap();
            assert!(!appended["created_at"].is_null());
        }
    }
    for stream in ["a", "b"] {
        let read = registry
            .dispatch("stream.read", json!({"stream": stream}))
            .await
            .unwrap();
        let entries = read["entries"].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|v| v["record"].clone())
                .collect::<Vec<_>>(),
            values
        );
        assert_eq!(
            entries
                .iter()
                .map(|v| v["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            (1..=5).collect::<Vec<_>>()
        );
        assert_eq!(read["head_seq"], 5);
        assert!(read["next_after"].is_null());
    }
}

#[tokio::test]
async fn stream_expected_sequence_conflict_and_reconciliation() {
    let (rt, registry) = surface();
    registry
        .dispatch(
            "stream.append",
            json!({"stream": "run", "record": {"token": "ours"}, "expected_seq": 1}),
        )
        .await
        .unwrap();
    let before = population(&rt).await;
    for expected in [1, 3] {
        let error = registry
            .dispatch(
                "stream.append",
                json!({"stream": "run", "record": {"token": "ours"}, "expected_seq": expected}),
            )
            .await
            .unwrap_err();
        let error = reason(error, "seq_conflict");
        assert_eq!(error["details"]["stream"], "run");
        assert_eq!(error["details"]["expected_seq"], expected.to_string());
        assert_eq!(error["details"]["next_seq"], "2");
        assert_eq!(population(&rt).await, before);
    }
    let reconciled = registry
        .dispatch(
            "stream.read",
            json!({"stream": "run", "after": 0, "limit": 1}),
        )
        .await
        .unwrap();
    assert_eq!(reconciled["entries"][0]["record"], json!({"token": "ours"}));
    registry
        .dispatch(
            "stream.append",
            json!({"stream": "run", "record": {"token": "competitor"}, "expected_seq": 2}),
        )
        .await
        .unwrap();
    reason(
        registry
            .dispatch(
                "stream.append",
                json!({"stream": "run", "record": {"token": "ours"}, "expected_seq": 2}),
            )
            .await
            .unwrap_err(),
        "seq_conflict",
    );
    let competitor = registry
        .dispatch(
            "stream.read",
            json!({"stream": "run", "after": 1, "limit": 1}),
        )
        .await
        .unwrap();
    assert_ne!(competitor["entries"][0]["record"], json!({"token": "ours"}));
}

#[tokio::test]
async fn stream_pagination_unknown_and_after_head() {
    let (_, registry) = surface();
    for n in 1..=25 {
        registry
            .dispatch("stream.append", json!({"stream": "pages", "record": n}))
            .await
            .unwrap();
    }
    let mut records = Vec::new();
    for (after, next) in [(0, json!(10)), (10, json!(20)), (20, Value::Null)] {
        let page = registry
            .dispatch(
                "stream.read",
                json!({"stream": "pages", "after": after, "limit": 10}),
            )
            .await
            .unwrap();
        assert_eq!(page["next_after"], next);
        assert_eq!(page["head_seq"], 25);
        records.extend(
            page["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["record"].as_u64().unwrap()),
        );
    }
    assert_eq!(records, (1..=25).collect::<Vec<_>>());
    for (stream, after, head) in [("unknown", 0, 0), ("pages", 100, 25)] {
        assert_eq!(
            registry
                .dispatch("stream.read", json!({"stream": stream, "after": after}))
                .await
                .unwrap(),
            json!({"entries": [], "head_seq": head, "next_after": null})
        );
    }
    assert_eq!(
        registry
            .dispatch("stream.stat", json!({"stream": "pages"}))
            .await
            .unwrap(),
        json!({"count": 25, "head_seq": 25})
    );
}

#[tokio::test]
async fn stream_member_refuses_record_changes_but_allows_metadata() {
    let (rt, registry) = surface();
    let appended = registry
        .dispatch(
            "stream.append",
            json!({"stream": "immutable", "record": "original", "tags": ["fixed"]}),
        )
        .await
        .unwrap();
    let id = &appended["id"];
    let before = population(&rt).await;
    for (verb, args) in [
        ("update", json!({"id": id, "content": "changed"})),
        (
            "update",
            json!({"id": id, "properties": {"tags": ["changed"]}}),
        ),
        ("delete", json!({"id": id})),
        ("delete", json!({"id": id, "hard": true})),
    ] {
        let error = reason(
            registry.dispatch(verb, args).await.unwrap_err(),
            "stream_member",
        );
        assert_eq!(error["details"]["id"], *id);
        assert_eq!(error["details"]["seq"], "1");
        assert_eq!(error["details"]["stream"], "immutable");
        assert_eq!(population(&rt).await, before);
    }
    registry
        .dispatch(
            "update",
            json!({"id": id, "salience": 0.7, "decay_factor": 0.1, "name": "display"}),
        )
        .await
        .unwrap();
    let got = registry.dispatch("get", json!({"id": id})).await.unwrap();
    assert_eq!(got["salience"], 0.7);
    assert_eq!(got["content"], "\"original\"");
    let ordinary = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "mutable", "skip_dedup_check": true}),
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "update",
            json!({"id": ordinary["id"], "content": "changed"}),
        )
        .await
        .unwrap();
    registry
        .dispatch("delete", json!({"id": ordinary["id"], "hard": true}))
        .await
        .unwrap();
}

#[tokio::test]
async fn stream_arguments_validate_presence_and_utf8_bytes() {
    let (_, registry) = surface();
    for fence in [Value::Null, json!({"key": "lease", "expected_version": 1})] {
        let error = registry
            .dispatch(
                "stream.append",
                json!({"stream": "s", "record": null, "fence": fence}),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
    }
    for args in [
        json!({"stream": "s"}),
        json!({"stream": "x".repeat(513), "record": null}),
        json!({"stream": "é".repeat(257), "record": null}),
        json!({"stream": "a\0b", "record": null}),
    ] {
        assert!(registry.dispatch("stream.append", args).await.is_err());
    }
    registry
        .dispatch(
            "stream.append",
            json!({"stream": "é".repeat(256), "record": null}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn stream_ledger_failure_rolls_back_note_and_indexes() {
    let (rt, registry) = surface();
    registry
        .dispatch("stream.append", json!({"stream": "control", "record": 0}))
        .await
        .unwrap();
    let before = population(&rt).await;
    rt.sql().writer().await.unwrap().execute_script("CREATE TRIGGER force_stream_insert_failure BEFORE INSERT ON note_streams BEGIN SELECT RAISE(ABORT, 'forced_stream_failure'); END;".into()).await.unwrap();
    assert!(registry
        .dispatch(
            "stream.append",
            json!({"stream": "fail", "record": "must roll back"})
        )
        .await
        .is_err());
    assert_eq!(population(&rt).await, before);
}
