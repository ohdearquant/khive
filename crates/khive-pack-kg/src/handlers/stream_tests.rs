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
        assert!(error.to_string().contains("later slice"), "{error}");
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
    // Amendment 1 acceptance 1, in process: two appends to one stream, an op
    // naming no member operation, and the keyed write member this server
    // refuses until versioned keyed notes land. The read is issued beside it.
    let (_, registry) = surface();
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"ops": [
                {"op": "append", "stream": "b", "record": {"n": 1}},
                {"op": "append", "stream": "b", "record": {"n": 2}},
                {"op": "nope"},
                {"op": "write", "key": "h", "kind": "head", "doc": {}},
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
    assert_eq!(results[3]["kind"], "invalid_input");
    assert_eq!(results[3]["details"]["reason"], "member_unavailable");
    assert_eq!(results[3]["domain_disposition"], "not_committed");
    assert!(results[3]["message"]
        .as_str()
        .unwrap()
        .contains("versioned keyed notes"));
    let read = registry
        .dispatch("stream.read", json!({"stream": "b"}))
        .await
        .unwrap();
    assert_eq!(records(&read), vec![json!({"n": 1}), json!({"n": 2})]);
}

#[tokio::test]
async fn stream_batch_atomic_refusal_writes_nothing() {
    // Amendment 1 acceptance 2, the sequence and member arms; the fence arms
    // wait on versioned keyed notes, and a fence is refused until then.
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
    let fence = json!({"key": "lease", "kind": "head", "expected_version": 1});
    let observed = json!([{"key": "lease", "version": 1}]);
    for args in [
        json!({"ops": [member], "atomic": false, "fence": fence}),
        json!({"ops": [member], "observed": observed}),
        json!({"ops": [member], "observed": observed, "atomic": false}),
        json!({"ops": [member], "fence": fence}),
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
