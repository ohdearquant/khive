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

async fn heads(registry: &VerbRegistry, streams: &[&str]) -> Vec<i64> {
    let mut out = Vec::new();
    for stream in streams {
        let stat = registry
            .dispatch("stream.stat", json!({"stream": stream}))
            .await
            .unwrap();
        out.push(stat["head_seq"].as_i64().unwrap());
    }
    out
}

fn seqs(result: &Value) -> Vec<i64> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["seq"].as_i64().unwrap())
        .collect()
}

fn records(page: &Value) -> Vec<Value> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["record"].clone())
        .collect()
}

#[tokio::test]
async fn stream_batch_per_member_order_values_and_refusals_as_values() {
    // Interleaved appends and a keyed write retain their list positions;
    // an unknown operation is a refusal value in per-member mode.
    let (_, registry) = surface();
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"ops": [
                {"op": "append", "stream": "b", "record": {"n": 1}},
                {"op": "append", "stream": "b", "record": {"n": 2}},
                {"op": "nope"},
                {"op": "write", "key": "h", "kind": "observation", "doc": {}},
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    let results = result["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["seq"], 1);
    assert_eq!(results[1]["seq"], 2);
    uuid::Uuid::parse_str(results[0]["id"].as_str().unwrap()).unwrap();
    assert_eq!(results[2]["kind"], "conflict");
    assert_eq!(results[2]["details"]["reason"], "unknown_op");
    assert_eq!(results[2]["details"]["op"], "nope");
    assert_eq!(results[2]["domain_disposition"], "not_committed");
    assert!(results[2]["details"].get("member").is_none());
    assert_eq!(results[3]["version"], 1);
    uuid::Uuid::parse_str(results[3]["id"].as_str().unwrap()).unwrap();
    let written = registry
        .dispatch("get", json!({"key": "h", "kind": "observation"}))
        .await
        .unwrap();
    assert_eq!(written["id"], results[3]["id"]);
    assert_eq!(written["content"], "{}");
    let read = registry
        .dispatch("stream.read", json!({"stream": "b"}))
        .await
        .unwrap();
    assert_eq!(records(&read), vec![json!({"n": 1}), json!({"n": 2})]);
}

#[tokio::test]
async fn stream_batch_atomic_refusal_writes_nothing() {
    // Sequence and unknown-operation failures roll back earlier appends.
    let (rt, registry) = surface();
    registry
        .dispatch("stream.append", json!({"stream": "a", "record": 0}))
        .await
        .unwrap();
    let before = population(&rt).await;
    let heads_before = heads(&registry, &["a", "c"]).await;
    let stale = json!({"atomic": true, "ops": [
        {"op": "append", "stream": "a", "record": 1},
        {"op": "append", "stream": "c", "record": 2, "expected_seq": 9},
        {"op": "append", "stream": "a", "record": 3},
    ]});
    let error = reason(
        registry.dispatch("stream.batch", stale).await.unwrap_err(),
        "seq_conflict",
    );
    assert_eq!(error["details"]["member"], "1");
    assert_eq!(error["details"]["stream"], "c");
    assert_eq!(error["details"]["expected_seq"], "9");
    assert_eq!(error["details"]["next_seq"], "1");
    assert!(error.get("committed").is_none());
    assert_eq!(population(&rt).await, before);
    assert_eq!(heads(&registry, &["a", "c"]).await, heads_before);
    let refused_member = json!({"atomic": true, "ops": [
        {"op": "append", "stream": "a", "record": 1},
        {"op": "nope"},
    ]});
    let error = reason(
        registry
            .dispatch("stream.batch", refused_member)
            .await
            .unwrap_err(),
        "unknown_op",
    );
    assert_eq!(error["details"]["member"], "1");
    assert_eq!(population(&rt).await, before);
    assert_eq!(heads(&registry, &["a", "c"]).await, heads_before);
    // Control: the same list carrying the numbers this transaction assigns
    // commits, consecutive per stream (member 2 is a's third entry, after
    // member 0).
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"atomic": true, "ops": [
                {"op": "append", "stream": "a", "record": 1},
                {"op": "append", "stream": "c", "record": 2, "expected_seq": 1},
                {"op": "append", "stream": "a", "record": 3, "expected_seq": 3},
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(seqs(&result), vec![2, 1, 3]);
    assert_eq!(heads(&registry, &["a", "c"]).await, vec![3, 1]);
}

#[tokio::test]
async fn stream_batch_per_member_refusal_keeps_siblings() {
    // Amendment 1 acceptance 4.
    let (_, registry) = surface();
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"ops": [
                {"op": "append", "stream": "p", "record": 1},
                {"op": "append", "stream": "p", "record": 2, "expected_seq": 9},
                {"op": "append", "stream": "p", "record": 3},
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    let results = result["results"].as_array().unwrap();
    assert_eq!(results[0]["seq"], 1);
    assert_eq!(results[1]["kind"], "conflict");
    assert_eq!(results[1]["details"]["reason"], "seq_conflict");
    assert_eq!(results[1]["details"]["next_seq"], "2");
    assert_eq!(results[1]["domain_disposition"], "not_committed");
    assert!(results[1]["details"].get("member").is_none());
    assert_eq!(results[2]["seq"], 2);
    let read = registry
        .dispatch("stream.read", json!({"stream": "p"}))
        .await
        .unwrap();
    assert_eq!(records(&read), vec![json!(1), json!(3)]);
    assert_eq!(read["head_seq"], 2);
}

#[tokio::test]
async fn stream_batch_authority_is_checked_once_before_any_member() {
    // Amendment 1 acceptance 5: the gate refuses the request in both modes
    // before any member runs; a control with authority commits the same list.
    use khive_gate::{Gate, GateDecision, GateError, GateRequest};
    use khive_runtime::RuntimeConfig;
    use std::sync::Arc;

    #[derive(Debug)]
    struct NoBatchAuthority;
    impl Gate for NoBatchAuthority {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(if req.verb == "stream.batch" {
                GateDecision::deny("no write authority for the batch")
            } else {
                GateDecision::allow()
            })
        }
    }
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    // The gate the registry consults on dispatch is the builder's, not the
    // runtime config's: a config-only gate leaves this arm passing vacuously.
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(NoBatchAuthority));
    builder.register(crate::KgPack::new(rt.clone()));
    let denied = builder.build().unwrap();
    let ops = json!([
        {"op": "append", "stream": "auth", "record": 1},
        {"op": "append", "stream": "auth", "record": 2},
    ]);
    let before = population(&rt).await;
    for atomic in [true, false] {
        let error = denied
            .dispatch("stream.batch", json!({"ops": ops, "atomic": atomic}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no write authority"), "{error}");
        assert_eq!(population(&rt).await, before);
    }
    assert_eq!(heads(&denied, &["auth"]).await, vec![0]);
    let (_, allowed) = surface();
    for (atomic, expected) in [(true, vec![1, 2]), (false, vec![3, 4])] {
        let result = allowed
            .dispatch("stream.batch", json!({"ops": ops, "atomic": atomic}))
            .await
            .unwrap();
        assert_eq!(seqs(&result), expected);
    }
}

#[tokio::test]
async fn stream_batch_mode_and_shape_refusals_write_nothing() {
    // Amendment 1 acceptance 8, plus the shape rules common to both modes: a
    // refusal here is the whole batch's, and it lands before the first write.
    let (rt, registry) = surface();
    let before = population(&rt).await;
    let member = json!({"op": "append", "stream": "m", "record": null});
    let fence = json!({"key": "lease", "kind": "observation", "expected_version": 1});
    let observed = json!([{"key": "lease", "version": 1}]);
    for args in [
        json!({"ops": [member], "atomic": false, "fence": fence}),
        json!({"ops": [member], "observed": observed}),
        json!({"ops": [member], "observed": observed, "atomic": false}),
        json!({"ops": [member], "fence": [fence]}),
        json!({"ops": [member], "atomic": true, "observed": observed}),
        json!({"ops": "not a list"}),
        json!({"ops": [member, "not an object"]}),
        json!({"ops": [member, {"stream": "m", "record": null}]}),
        json!({"ops": [member, {"op": "append", "stream": "m"}]}),
        json!({"ops": [member, {"op": "append", "stream": "a\0b", "record": null}]}),
        json!({"ops": [member, {"op": "append", "stream": "m", "record": null, "tags": ["x"]}]}),
        json!({"ops": [member], "atomic": true, "extra": 1}),
    ] {
        let error = registry
            .dispatch("stream.batch", args.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{args}: {error}"
        );
        assert_eq!(population(&rt).await, before, "{args}");
    }
    assert_eq!(heads(&registry, &["m"]).await, vec![0]);
    // atomic=true without a fence is the atomic mode of acceptance 2.
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"ops": [member, member], "atomic": true}),
        )
        .await
        .unwrap();
    assert_eq!(seqs(&result), vec![1, 2]);
    // A JSON null fence or observation set is absent, so the mode defaults
    // to per-member and a stale member is a value.
    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "ops": [
                    member,
                    {"op": "append", "stream": "m", "record": null, "expected_seq": 9},
                    member,
                ],
                "fence": null,
                "observed": null,
            }),
        )
        .await
        .unwrap();
    let results = result["results"].as_array().unwrap();
    assert_eq!(results[0]["seq"], 3);
    assert_eq!(results[1]["details"]["reason"], "seq_conflict");
    assert_eq!(results[2]["seq"], 4);
}

#[tokio::test]
async fn stream_batch_fence_and_observed_predicates_run_before_members() {
    let (rt, registry) = surface();
    registry
        .dispatch(
            "stream.batch",
            json!({"ops": [
                {"op": "write", "key": "lease", "kind": "observation", "doc": {"held": true}}
            ]}),
        )
        .await
        .unwrap();
    // Advance the held revision so a null observation cannot accidentally
    // behave like an assertion against only the first revision.
    for version in 1..5 {
        registry
            .dispatch(
                "stream.batch",
                json!({"ops": [
                    {"op": "write", "key": "lease", "kind": "observation", "doc": version,
                     "expected_version": version}
                ]}),
            )
            .await
            .unwrap();
    }
    let member = json!({"op": "append", "stream": "predicated", "record": true});
    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "fence": {"key": "lease", "kind": "observation", "expected_version": 5},
                "observed": [
                    {"key": "lease", "kind": "observation", "version": 5},
                    {"key": "absent", "kind": "observation", "version": null},
                    {"key": "lease", "kind": "insight", "version": null}
                ],
                "ops": [member]
            }),
        )
        .await
        .unwrap();
    assert_eq!(seqs(&result), vec![1]);
    let before = population(&rt).await;
    for (predicate, expected_reason, index, key) in [
        (
            json!({"fence": {"key": "lease", "kind": "observation", "expected_version": 4}}),
            "fence_conflict",
            None,
            "lease",
        ),
        (
            json!({"fence": {"key": "absent", "kind": "observation", "expected_version": 1}}),
            "fence_conflict",
            None,
            "absent",
        ),
        (
            json!({"observed": [
               {"key": "absent", "kind": "observation", "version": null},
               {"key": "lease", "kind": "observation", "version": 4}
            ]}),
            "version_conflict",
            Some("1"),
            "lease",
        ),
        (
            json!({"observed": [{"key": "lease", "kind": "observation", "version": null}]}),
            "version_conflict",
            Some("0"),
            "lease",
        ),
        (
            json!({"observed": [{"key": "absent", "kind": "observation", "version": 1}]}),
            "version_conflict",
            Some("0"),
            "absent",
        ),
    ] {
        let mut args = predicate;
        args["atomic"] = json!(true);
        args["ops"] = json!([member]);
        let error = reason(
            registry.dispatch("stream.batch", args).await.unwrap_err(),
            expected_reason,
        );
        assert_eq!(error["details"]["key"], key);
        assert_eq!(error["details"].get("index").and_then(Value::as_str), index);
        assert!(error["details"].get("member").is_none());
        if key == "lease" {
            assert_eq!(error["details"]["current_version"], "5");
        }
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["predicated"]).await, vec![1]);
    }
    // Observing absence is based on live rows, not historical key ownership.
    let lease = registry
        .dispatch("get", json!({"key": "lease", "kind": "observation"}))
        .await
        .unwrap();
    registry
        .dispatch("delete", json!({"id": lease["id"]}))
        .await
        .unwrap();
    registry
        .dispatch(
            "stream.batch",
            json!({"atomic": true,
                "observed": [{"key": "lease", "kind": "observation", "version": null}],
                "ops": [member]
            }),
        )
        .await
        .unwrap();
    assert_eq!(heads(&registry, &["predicated"]).await, vec![2]);
}

#[tokio::test]
async fn stream_batch_late_write_refusal_obeys_transaction_mode() {
    for atomic in [true, false] {
        for update_first in [true, false] {
            let (rt, registry) = surface();
            registry
                .dispatch(
                    "stream.batch",
                    json!({"ops": [
                        {"op": "write", "key": "stale", "kind": "observation", "doc": "original"}
                    ]}),
                )
                .await
                .unwrap();
            let mut first =
                json!({"op": "write", "key": "candidate", "kind": "observation", "doc": "new"});
            if update_first {
                registry
                    .dispatch(
                        "stream.batch",
                        json!({"ops": [
                            {"op": "write", "key": "candidate", "kind": "observation", "doc": "old"}
                        ]}),
                    )
                    .await
                    .unwrap();
                first["expected_version"] = json!(1);
            }
            let before = population(&rt).await;
            let outcome = registry.dispatch("stream.batch", json!({"atomic": atomic, "ops": [
                first,
                {"op": "append", "stream": "rollback", "record": "before failure"},
                {"op": "write", "key": "stale", "kind": "observation", "doc": "wrong", "expected_version": 9},
                {"op": "append", "stream": "rollback", "record": "after failure"}
            ]})).await;
            if atomic {
                let error = reason(outcome.unwrap_err(), "version_conflict");
                assert_eq!(error["details"]["member"], "2");
                assert_eq!(population(&rt).await, before);
            } else {
                let result = outcome.unwrap();
                assert_eq!(
                    result["results"][0]["version"],
                    if update_first { 2 } else { 1 }
                );
                assert_eq!(result["results"][1]["seq"], 1);
                assert_eq!(
                    result["results"][2]["details"]["reason"],
                    "version_conflict"
                );
                assert_eq!(result["results"][2]["domain_disposition"], "not_committed");
                assert!(result["results"][2]["details"].get("member").is_none());
                assert_eq!(result["results"][3]["seq"], 2);
            }
            assert_eq!(
                heads(&registry, &["rollback"]).await,
                vec![if atomic { 0 } else { 2 }]
            );
            let candidate = registry
                .dispatch("get", json!({"key": "candidate", "kind": "observation"}))
                .await;
            if atomic && !update_first {
                assert!(
                    candidate.is_err(),
                    "rolled-back keyed creation must not survive"
                );
            } else {
                let candidate = candidate.unwrap();
                assert_eq!(
                    candidate["content"],
                    if atomic { "\"old\"" } else { "\"new\"" }
                );
                assert_eq!(
                    candidate["version"],
                    if !atomic && update_first { 2 } else { 1 }
                );
            }
            let stale = registry
                .dispatch("get", json!({"key": "stale", "kind": "observation"}))
                .await
                .unwrap();
            assert_eq!(stale["version"], 1);
            assert_eq!(stale["content"], "\"original\"");
        }
    }
}

struct MemberRefusalEmbeddingService;

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for MemberRefusalEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "stream-batch-member-refusal"
    }
}

struct MemberRefusalEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for MemberRefusalEmbedderProvider {
    fn name(&self) -> &str {
        "stream-batch-member-refusal"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
    {
        Ok(std::sync::Arc::new(MemberRefusalEmbeddingService))
    }
}

/// `surface()` registers no embedding model, and with none registered a
/// prepared note set is never read for a token. A registered model is the
/// condition under which preparation reads the first spec, so it is the
/// condition every arm below needs.
fn surface_with_embedding_model() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(MemberRefusalEmbedderProvider);
    assert!(
        !rt.registered_embedding_model_names().is_empty(),
        "the arm needs a registered model to reach the preparation path"
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    (rt, builder.build().unwrap())
}

/// Schema rows, so an arm asserting that nothing was written also covers the
/// lazy vector-table create preparation performs.
async fn schema(rt: &KhiveRuntime) -> i64 {
    let mut reader = rt.sql().reader().await.unwrap();
    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM sqlite_master".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    let Some(SqlValue::Integer(n)) = value else {
        panic!("integer count")
    };
    n
}

#[tokio::test]
async fn stream_batch_all_refused_members_prepare_nothing() {
    // No embedding setup is needed when every member is already refused.
    let (rt, registry) = surface_with_embedding_model();
    let before = population(&rt).await;
    let before_schema = schema(&rt).await;
    let (member, kind, expected) = (json!({"op": "nope"}), "conflict", "unknown_op");
    {
        let atomic_error = registry
            .dispatch("stream.batch", json!({"ops": [member], "atomic": true}))
            .await
            .unwrap_err();
        let RuntimeError::Khive(atomic_error) = atomic_error else {
            panic!("structured refusal: {atomic_error:?}")
        };
        let value = serde_json::to_value(atomic_error).unwrap();
        assert_eq!(value["kind"], kind, "{member}");
        assert_eq!(value["details"]["reason"], expected, "{member}");
        assert_eq!(value["details"]["member"], "0", "{member}");

        let per_member = registry
            .dispatch("stream.batch", json!({"ops": [member], "atomic": false}))
            .await
            .unwrap();
        let results = per_member["results"].as_array().unwrap();
        assert_eq!(results[0]["kind"], kind, "{member}");
        assert_eq!(results[0]["details"]["reason"], expected, "{member}");
        assert_eq!(
            results[0]["domain_disposition"], "not_committed",
            "{member}"
        );
        assert!(results[0]["details"].get("member").is_none(), "{member}");
    }
    assert_eq!(population(&rt).await, before);
    assert_eq!(schema(&rt).await, before_schema);
}

#[tokio::test]
async fn stream_batch_refuses_an_empty_member_list() {
    // An empty list is a shape refusal, not a batch that takes the writer to
    // commit nothing and reports `committed: true`.
    let (rt, registry) = surface_with_embedding_model();
    let before = population(&rt).await;
    let before_schema = schema(&rt).await;
    for atomic in [true, false] {
        let error = registry
            .dispatch("stream.batch", json!({"ops": [], "atomic": atomic}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "atomic={atomic}: {error}"
        );
        assert_eq!(population(&rt).await, before, "atomic={atomic}");
        assert_eq!(schema(&rt).await, before_schema, "atomic={atomic}");
    }
}

#[tokio::test]
async fn stream_batch_atomic_refuses_before_it_prepares_a_good_member() {
    // An atomic refusal writes nothing, and preparing a member is a write: it
    // embeds the record and lazily creates the vector table that embedding
    // needs. So a batch that is going to refuse must read the refusal before
    // it prepares the members that were fine, and the schema count is what
    // sees the difference.
    let (rt, registry) = surface_with_embedding_model();
    let before = population(&rt).await;
    let before_schema = schema(&rt).await;
    let error = registry
        .dispatch(
            "stream.batch",
            json!({"ops": [
                {"op": "append", "stream": "guard", "record": 1},
                {"op": "nope"},
            ], "atomic": true}),
        )
        .await
        .unwrap_err();
    let RuntimeError::Khive(error) = error else {
        panic!("structured refusal: {error:?}")
    };
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["details"]["reason"], "unknown_op");
    assert_eq!(value["details"]["member"], "1");
    assert_eq!(population(&rt).await, before);
    assert_eq!(schema(&rt).await, before_schema);
    assert_eq!(heads(&registry, &["guard"]).await, vec![0]);
}

#[tokio::test]
async fn stream_batch_write_shapes_validate_the_entire_list_before_preparation() {
    for atomic in [true, false] {
        let (rt, registry) = surface_with_embedding_model();
        let before = population(&rt).await;
        let before_schema = schema(&rt).await;
        let before_writers = rt.backend().pool().writer_acquisition_snapshot();
        let mut malformed = vec![
            json!({"op": "write", "key": "bad", "kind": "head"}),
            json!({"op": "write", "kind": "head", "doc": null}),
            json!({"op": "write", "key": "bad", "doc": null}),
        ];
        for (field, values) in [
            (
                "key",
                vec![json!(null), json!(5), json!("a\0b"), json!("k".repeat(513))],
            ),
            (
                "kind",
                vec![
                    json!(null),
                    json!(5),
                    json!(""),
                    json!("no-such-kind"),
                    json!("concept"),
                ],
            ),
            (
                "expected_version",
                vec![
                    json!(0),
                    json!(-1),
                    json!(1.5),
                    json!("1"),
                    json!(true),
                    json!(u64::MAX),
                ],
            ),
            ("tags", vec![json!("tag"), json!([1])]),
            ("embed", vec![json!(1), json!("false")]),
            ("extra", vec![json!(true)]),
        ] {
            for value in values {
                let mut member = json!({"op": "write", "key": "bad", "kind": "head", "doc": null});
                member[field] = value;
                malformed.push(member);
            }
        }
        for member in malformed {
            let args = json!({"atomic": atomic, "ops": [
                {"op": "append", "stream": "shape", "record": 1},
                {"op": "write", "key": "valid", "kind": "head", "doc": null, "embed": true},
                {"op": "nope"},
                member,
            ]});
            let error = registry
                .dispatch("stream.batch", args.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::InvalidInput(_)),
                "{args}: {error}"
            );
            assert_eq!(population(&rt).await, before, "{args}");
            assert_eq!(schema(&rt).await, before_schema, "{args}");
            assert_eq!(
                rt.backend().pool().writer_acquisition_snapshot(),
                before_writers,
                "{args}"
            );
        }
        let result = registry
            .dispatch("stream.batch", json!({"atomic": atomic, "ops": [
                {"op": "write", "key": "null-doc", "kind": "head", "doc": null, "expected_version": null},
                {"op": "append", "stream": "shape", "record": null},
                {"op": "append", "stream": "shape", "record": 2},
            ]}))
            .await
            .unwrap();
        assert_eq!(result["results"][0]["version"], 1);
        assert_eq!(result["results"][1]["seq"], 1);
        assert_eq!(result["results"][2]["seq"], 2);
        let note = registry
            .dispatch("get", json!({"key": "null-doc", "kind": "head"}))
            .await
            .unwrap();
        assert_eq!(note["content"], "null");
        assert_eq!(heads(&registry, &["shape"]).await, vec![2]);
    }
}

#[derive(Debug)]
struct StreamBatchPolicyProbe {
    allow_batch: bool,
    allow_list: bool,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl khive_gate::Gate for StreamBatchPolicyProbe {
    fn check(
        &self,
        request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        self.seen
            .lock()
            .unwrap()
            .push((request.verb.clone(), request.args.clone()));
        Ok(if request.verb == "stream.batch" && !self.allow_batch {
            khive_gate::GateDecision::deny("batch denied by test policy")
        } else if request.verb == "list" && !self.allow_list {
            khive_gate::GateDecision::deny("listing denied by test policy")
        } else {
            khive_gate::GateDecision::allow()
        })
    }
}

#[derive(Clone)]
struct StreamBatchEmbeddingProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for StreamBatchEmbeddingProbe {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "stream-batch-validation-probe"
    }
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for StreamBatchEmbeddingProbe {
    fn name(&self) -> &str {
        "stream-batch-validation-probe"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(std::sync::Arc::new(self.clone()))
    }
}

#[tokio::test]
async fn stream_batch_duplicate_writes_follow_policy_and_precede_preparation() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    for atomic in [true, false] {
        for allow_batch in [false, true] {
            let rt = KhiveRuntime::memory().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            rt.register_embedder(StreamBatchEmbeddingProbe(calls.clone()));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut builder = VerbRegistryBuilder::new();
            builder.with_gate(Arc::new(StreamBatchPolicyProbe {
                allow_batch,
                allow_list: true,
                seen: seen.clone(),
            }));
            builder.register(crate::KgPack::new(rt.clone()));
            let registry = builder.build().unwrap();
            let before = population(&rt).await;
            let before_schema = schema(&rt).await;
            let before_writers = rt.backend().pool().writer_acquisition_snapshot();
            for other_kind in ["head", " HEAD "] {
                seen.lock().unwrap().clear();
                let args = json!({"atomic": atomic, "ops": [
                    {"op": "append", "stream": "duplicates", "record": 1},
                    {"op": "write", "key": "same", "kind": "head", "doc": null, "embed": true},
                    {"op": "append", "stream": "duplicates", "record": 2},
                    {"op": "write", "key": "same", "kind": other_kind, "doc": null, "expected_version": 1, "embed": true},
                ]});
                let error = registry
                    .dispatch("stream.batch", args.clone())
                    .await
                    .unwrap_err();
                if allow_batch {
                    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
                    assert!(
                        error.to_string().contains("repeats write target"),
                        "{error}"
                    );
                } else {
                    assert!(
                        matches!(error, RuntimeError::PermissionDenied { .. }),
                        "{error}"
                    );
                }
                assert_eq!(
                    *seen.lock().unwrap(),
                    vec![("stream.batch".to_owned(), args)]
                );
                assert_eq!(population(&rt).await, before);
                assert_eq!(schema(&rt).await, before_schema);
                assert_eq!(
                    rt.backend().pool().writer_acquisition_snapshot(),
                    before_writers
                );
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
            if allow_batch {
                let result = registry.dispatch("stream.batch", json!({"atomic": atomic, "ops": [
                    {"op": "append", "stream": "duplicates", "record": 1},
                    {"op": "write", "key": "same", "kind": "head", "doc": null, "embed": true},
                    {"op": "append", "stream": "duplicates", "record": 2},
                    {"op": "write", "key": "same", "kind": "observation", "doc": null, "embed": true},
                ]})).await.unwrap();
                assert_eq!(result["results"][0]["seq"], 1);
                assert_eq!(result["results"][2]["seq"], 2);
                assert_eq!(result["results"][1]["version"], 1);
                assert_eq!(result["results"][3]["version"], 1);
                assert_ne!(result["results"][1]["id"], result["results"][3]["id"]);
                for (kind, member) in [("head", 1), ("observation", 3)] {
                    let note = registry
                        .dispatch("get", json!({"key": "same", "kind": kind}))
                        .await
                        .unwrap();
                    assert_eq!(note["id"], result["results"][member]["id"]);
                    assert_eq!(note["content"], "null");
                }
                assert!(calls.load(Ordering::SeqCst) > 0);
                assert!(schema(&rt).await > before_schema);
                assert!(
                    rt.backend()
                        .pool()
                        .writer_acquisition_snapshot()
                        .acquisitions
                        > before_writers.acquisitions
                );
            }
        }
    }
}

#[tokio::test]
async fn stream_batch_key_conflicts_apply_list_disclosure_to_the_refused_member() {
    use std::sync::{Arc, Mutex};

    for atomic in [true, false] {
        for allow_list in [true, false] {
            let rt = KhiveRuntime::memory().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut builder = VerbRegistryBuilder::new();
            builder.with_gate(Arc::new(StreamBatchPolicyProbe {
                allow_batch: true,
                allow_list,
                seen: seen.clone(),
            }));
            builder.register(crate::KgPack::new(rt.clone()));
            let registry = builder.build().unwrap();
            let holder = registry
                .dispatch(
                    "create",
                    json!({"kind": "head", "key": "private/key", "content": "{}"}),
                )
                .await
                .unwrap();
            let before = population(&rt).await;
            seen.lock().unwrap().clear();
            let args = json!({"atomic": atomic, "ops": [
                {"op": "append", "stream": "disclosure", "record": 1},
                {"op": "write", "key": "new/key", "kind": "head", "doc": {"existing_id": "document-value"}},
                {"op": "write", "key": "private/key", "kind": " HEAD ", "doc": null},
                {"op": "append", "stream": "disclosure", "record": 2},
            ]});
            let outcome = registry.dispatch("stream.batch", args.clone()).await;
            let conflict = if atomic {
                let conflict = reason(outcome.unwrap_err(), "key_conflict");
                assert_eq!(conflict["details"]["member"], "2");
                assert_eq!(population(&rt).await, before);
                conflict
            } else {
                let result = outcome.unwrap();
                assert_eq!(result["committed"], true);
                let members = result["results"].as_array().unwrap();
                assert_eq!(members.len(), 4);
                assert_eq!(members[0]["seq"], 1);
                assert_eq!(members[1]["version"], 1);
                assert_eq!(members[3]["seq"], 2);
                for index in [0, 1, 3] {
                    uuid::Uuid::parse_str(members[index]["id"].as_str().unwrap()).unwrap();
                    assert!(members[index].get("details").is_none());
                }
                assert_eq!(members[2]["domain_disposition"], "not_committed");
                assert!(members[2]["details"].get("member").is_none());
                members[2].clone()
            };
            assert_eq!(conflict["kind"], "conflict");
            assert_eq!(conflict["details"]["reason"], "key_conflict");
            assert_eq!(conflict["details"]["key"], "private/key");
            assert_eq!(
                conflict["details"].get("existing_id"),
                allow_list.then_some(&holder["id"])
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    ("stream.batch".to_owned(), args),
                    (
                        "list".to_owned(),
                        json!({"kind": "note", "note_kind": "head", "key_prefix": "private/key"})
                    ),
                ]
            );
            assert_eq!(
                heads(&registry, &["disclosure"]).await,
                vec![if atomic { 0 } else { 2 }]
            );
            let unchanged = registry
                .dispatch("get", json!({"key": "private/key", "kind": "head"}))
                .await
                .unwrap();
            assert_eq!(unchanged["version"], 1);
            assert_eq!(unchanged["content"], "{}");
            let created = registry
                .dispatch("get", json!({"key": "new/key", "kind": "head"}))
                .await;
            if atomic {
                assert!(created.is_err());
            } else {
                let created = created.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(created["content"].as_str().unwrap()).unwrap(),
                    json!({"existing_id": "document-value"})
                );
            }
        }
    }
}
#[path = "stream_fence_batch_tests.rs"]
mod fence_batches;

#[path = "stream_mixed_fence_tests.rs"]
mod mixed_fences;

#[path = "stream_expiry_tests.rs"]
mod expiry_tests;
