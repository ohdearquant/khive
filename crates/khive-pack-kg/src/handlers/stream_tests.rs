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
    // A batch whose every member is refused has no member to prepare. Both
    // refusal shapes a member can carry, in both modes: atomic reads the
    // refusal before it prepares anything, and per-member prepares an empty
    // set, which must not read a first spec that is not there.
    let (rt, registry) = surface_with_embedding_model();
    let before = population(&rt).await;
    let before_schema = schema(&rt).await;
    for (member, kind, expected) in [
        (
            json!({"op": "write"}),
            "invalid_input",
            "member_unavailable",
        ),
        (json!({"op": "nope"}), "conflict", "unknown_op"),
    ] {
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

#[path = "stream_fence_batch_tests.rs"]
mod fence_batches;
