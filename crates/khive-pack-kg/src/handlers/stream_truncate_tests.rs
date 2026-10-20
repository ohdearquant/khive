//! ADR-174 A3.2/A12 through the real registry and native SQL guards.
use super::*;

async fn raw(rt: &KhiveRuntime, sql: &str) -> Value {
    let mut reader = rt.sql().reader().await.unwrap();
    serde_json::to_value(
        reader
            .query_all(SqlStatement::new(sql, vec![]))
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn scalar(rt: &KhiveRuntime, sql: &str) -> i64 {
    let mut reader = rt.sql().reader().await.unwrap();
    match reader
        .query_scalar(SqlStatement::new(sql, vec![]))
        .await
        .unwrap()
    {
        Some(SqlValue::Integer(value)) => value,
        value => panic!("expected native integer: {value:?}"),
    }
}

async fn snapshot(rt: &KhiveRuntime) -> Vec<Value> {
    let mut result = Vec::new();
    for sql in [
        "SELECT * FROM notes ORDER BY id",
        "SELECT * FROM note_streams ORDER BY namespace,stream,seq",
        "SELECT rowid,* FROM fts_notes ORDER BY rowid",
        "SELECT * FROM fts_notes_rowids ORDER BY namespace,subject_id",
        "SELECT * FROM graph_edges ORDER BY namespace,id",
        "SELECT * FROM note_stream_floors ORDER BY namespace,stream",
        "SELECT * FROM note_stream_truncations ORDER BY namespace,stream",
        "SELECT * FROM events WHERE kind!='audit' ORDER BY id",
    ] {
        result.push(raw(rt, sql).await);
    }
    result
}

async fn no_permit(rt: &KhiveRuntime) {
    assert_eq!(
        scalar(rt, "SELECT COUNT(*) FROM note_stream_truncations").await,
        0
    );
}

async fn append(
    registry: &VerbRegistry,
    stream: &str,
    namespace: &str,
    record: Value,
    expected: Option<i64>,
) -> Value {
    let mut args = json!({"stream":stream, "namespace":namespace, "record":record});
    if let Some(expected) = expected {
        args["expected_seq"] = json!(expected);
    }
    registry.dispatch("stream.append", args).await.unwrap()
}

async fn stat(registry: &VerbRegistry, stream: &str) -> Value {
    registry
        .dispatch("stream.stat", json!({"stream":stream}))
        .await
        .unwrap()
}

async fn truncate(registry: &VerbRegistry, stream: &str, before: i64) -> Value {
    registry
        .dispatch(
            "stream.truncate",
            json!({"stream":stream,"before_seq":before}),
        )
        .await
        .unwrap()
}

fn exhausted(error: RuntimeError) -> Value {
    let value =
        khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(value["message"], "stream sequence exhausted");
    assert_eq!(value["domain_disposition"], "not_committed");
    value
}

async fn top_fixture(retained: bool) -> (KhiveRuntime, VerbRegistry) {
    let (rt, registry) = surface();
    {
        let mut writer = rt.sql().writer().await.unwrap();
        writer.execute(SqlStatement::new(
            "INSERT INTO note_stream_floors(namespace,stream,floor_seq) VALUES ('local','s',?1)",
            vec![SqlValue::Integer(i64::MAX - 1)],
        )).await.unwrap();
    }
    assert_eq!(
        stat(&registry, "s").await,
        json!({"head_seq":i64::MAX-2,"floor_seq":i64::MAX-1,"count":0})
    );
    if retained {
        assert_eq!(
            append(&registry, "s", "local", json!("last"), Some(i64::MAX - 1)).await["seq"],
            i64::MAX - 1
        );
    }
    (rt, registry)
}

#[tokio::test]
async fn stream_truncate_retains_floor_head_and_independent_streams() {
    for batch_append in [false, true] {
        let (rt, registry) = surface();
        for n in 1..=5 {
            append(
                &registry,
                "s",
                "local",
                json!({"text":"retained lexical entry","n":n}),
                None,
            )
            .await;
        }
        append(&registry, "other", "local", json!(77), None).await;
        append(&registry, "s", "other", json!(88), None).await;
        let independent = registry
            .dispatch("stream.read", json!({"stream":"s","namespace":"other"}))
            .await
            .unwrap();
        assert_eq!(scalar(&rt, "SELECT COUNT(*) FROM fts_notes").await, 7);
        assert_eq!(
            truncate(&registry, "s", 3).await,
            json!({"removed":2,"floor_seq":3,"head_seq":5})
        );
        no_permit(&rt).await;
        assert_eq!(
            stat(&registry, "s").await,
            json!({"head_seq":5,"floor_seq":3,"count":3})
        );
        let page = registry
            .dispatch("stream.read", json!({"stream":"s","after":0,"limit":2}))
            .await
            .unwrap();
        assert_eq!(
            page["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["seq"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(page["next_after"], 4);
        assert_eq!(page["floor_seq"], 3);
        for table in ["notes", "fts_notes", "fts_notes_rowids"] {
            assert_eq!(
                scalar(&rt, &format!("SELECT COUNT(*) FROM {table}")).await,
                5,
                "hard-delete cleanup for {table}"
            );
        }
        assert_eq!(
            truncate(&registry, "s", 6).await,
            json!({"removed":3,"floor_seq":6,"head_seq":5})
        );
        no_permit(&rt).await;
        assert_eq!(
            stat(&registry, "s").await,
            json!({"head_seq":5,"floor_seq":6,"count":0})
        );
        let empty = registry
            .dispatch("stream.read", json!({"stream":"s","after":0}))
            .await
            .unwrap();
        assert_eq!(
            empty,
            json!({"entries":[],"head_seq":5,"floor_seq":6,"next_after":null})
        );
        let next = if batch_append {
            registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":true,"ops":[{"op":"append","stream":"s","record":6}]}),
                )
                .await
                .unwrap()["results"][0]
                .clone()
        } else {
            append(&registry, "s", "local", json!(6), None).await
        };
        assert_eq!(next["seq"], 6, "both allocators must retain the old head");
        for before in [i64::MIN, 0, 6] {
            let before_state = snapshot(&rt).await;
            assert_eq!(
                truncate(&registry, "s", before).await,
                json!({"removed":0,"floor_seq":6,"head_seq":6})
            );
            no_permit(&rt).await;
            assert_eq!(snapshot(&rt).await, before_state);
        }
        assert_eq!(
            truncate(&registry, "s", i64::MAX).await,
            json!({"removed":1,"floor_seq":7,"head_seq":6})
        );
        no_permit(&rt).await;
        assert_eq!(
            truncate(&registry, "unknown", i64::MAX).await,
            json!({"removed":0,"floor_seq":1,"head_seq":0})
        );
        no_permit(&rt).await;
        assert_eq!(
            scalar(
                &rt,
                "SELECT COUNT(*) FROM note_stream_floors WHERE stream='unknown'"
            )
            .await,
            0
        );
        assert_eq!(
            stat(&registry, "other").await,
            json!({"head_seq":1,"floor_seq":1,"count":1})
        );
        assert_eq!(
            registry
                .dispatch("stream.read", json!({"stream":"s","namespace":"other"}))
                .await
                .unwrap(),
            independent
        );
    }
}

#[tokio::test]
async fn stream_truncate_native_note_failure_rolls_back_every_projection_and_retries() {
    let (rt, registry) = surface();
    let mut entries = Vec::new();
    for n in 1..=3 {
        entries.push(
            append(
                &registry,
                "s",
                "local",
                json!({"text":"rollback lexical entry","n":n}),
                None,
            )
            .await,
        );
    }
    let entity = registry
        .dispatch(
            "create",
            json!({"kind":"concept","name":"Unrelated survivor","skip_dedup_check":true}),
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "link",
            json!({"source":entries[0]["id"],"target":entity["id"],"relation":"annotates"}),
        )
        .await
        .unwrap();
    assert_eq!(scalar(&rt, "SELECT COUNT(*) FROM graph_edges").await, 1);
    {
        let mut writer = rt.sql().writer().await.unwrap();
        writer.execute_script(format!(
            "CREATE TRIGGER truncate_native_failure BEFORE DELETE ON notes WHEN OLD.id='{}' BEGIN SELECT RAISE(ABORT,'truncate_native_failure'); END;",
            entries[1]["id"].as_str().unwrap()
        )).await.unwrap();
    }
    let before = snapshot(&rt).await;
    let error = registry
        .dispatch("stream.truncate", json!({"stream":"s","before_seq":3}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("truncate_native_failure"),
        "original native refusal must remain visible: {error}"
    );
    assert_eq!(
        snapshot(&rt).await,
        before,
        "note/ledger/FTS/map/edge/floor/permit/events rollback together"
    );
    no_permit(&rt).await;
    {
        let mut writer = rt.sql().writer().await.unwrap();
        writer
            .execute_script("DROP TRIGGER truncate_native_failure;".into())
            .await
            .unwrap();
    }
    assert_eq!(
        truncate(&registry, "s", 3).await,
        json!({"removed":2,"floor_seq":3,"head_seq":3})
    );
    no_permit(&rt).await;
    for table in ["notes", "fts_notes", "fts_notes_rowids"] {
        assert_eq!(
            scalar(&rt, &format!("SELECT COUNT(*) FROM {table}")).await,
            1,
            "successful retry purges {table}"
        );
    }
    assert_eq!(scalar(&rt, "SELECT COUNT(*) FROM graph_edges").await, 0);
    assert_eq!(
        registry
            .dispatch("get", json!({"id":entity["id"]}))
            .await
            .unwrap()["id"],
        entity["id"]
    );
    for sql in [
        "DELETE FROM note_streams WHERE namespace='local' AND stream='s'",
        "DELETE FROM notes WHERE id IN (SELECT note_id FROM note_streams)",
    ] {
        let mut writer = rt.sql().writer().await.unwrap();
        assert!(
            writer
                .execute(SqlStatement::new(sql, vec![]))
                .await
                .is_err(),
            "direct delete still refused: {sql}"
        );
    }
    assert_eq!(
        stat(&registry, "s").await,
        json!({"head_seq":3,"floor_seq":3,"count":1})
    );
}

#[tokio::test]
async fn stream_truncate_requires_strict_arguments_and_namespace_write_authority() {
    let (rt, registry) = surface();
    append(&registry, "s", "local", json!(1), None).await;
    let before = snapshot(&rt).await;
    for args in [
        json!({"before_seq":2}),
        json!({"stream":"s"}),
        json!({"stream":"s","before_seq":null}),
        json!({"stream":"s","before_seq":1.5}),
        json!({"stream":"s","before_seq":"2"}),
        json!({"stream":"s","before_seq":u64::MAX}),
        json!({"stream":"s","before_seq":2,"unknown":true}),
        json!({"stream":"","before_seq":2}),
        json!({"stream":"x\0y","before_seq":2}),
        json!({"stream":"x".repeat(513),"before_seq":2}),
    ] {
        assert!(matches!(
            registry.dispatch("stream.truncate", args).await,
            Err(RuntimeError::InvalidInput(_))
        ));
        assert_eq!(snapshot(&rt).await, before);
        no_permit(&rt).await;
    }
    #[derive(Debug)]
    struct ReadsOnly;
    impl khive_gate::Gate for ReadsOnly {
        fn check(
            &self,
            request: &khive_gate::GateRequest,
        ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
            Ok(
                if khive_gate::classify_operation(&request.verb)
                    == Some(khive_gate::OperationAccess::Read)
                {
                    khive_gate::GateDecision::allow()
                } else {
                    khive_gate::GateDecision::deny("namespace is read-only")
                },
            )
        }
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    builder.with_gate(std::sync::Arc::new(ReadsOnly));
    let read_only = builder.build().unwrap();
    assert_eq!(stat(&read_only, "s").await["count"], 1);
    let error = read_only
        .dispatch("stream.truncate", json!({"stream":"s","before_seq":2}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::PermissionDenied { .. }),
        "{error}"
    );
    assert_eq!(snapshot(&rt).await, before);
    no_permit(&rt).await;
}

#[tokio::test]
async fn stream_top_sequence_conflict_exhaustion_and_reader_are_exact() {
    let (rt, registry) = top_fixture(false).await;
    append(&registry, "s", "local", json!("last"), Some(i64::MAX - 1)).await;
    let before = snapshot(&rt).await;
    let conflict = reason(
        registry
            .dispatch(
                "stream.append",
                json!({"stream":"s","record":"retry","expected_seq":i64::MAX-1}),
            )
            .await
            .unwrap_err(),
        "seq_conflict",
    );
    assert_eq!(conflict["details"]["next_seq"], i64::MAX.to_string());
    for expected in [None, Some(i64::MAX)] {
        let mut args = json!({"stream":"s","record":"exhausted"});
        if let Some(expected) = expected {
            args["expected_seq"] = json!(expected);
        }
        exhausted(registry.dispatch("stream.append", args).await.unwrap_err());
        assert_eq!(
            snapshot(&rt).await,
            before,
            "exhausted append writes no note"
        );
    }
    let at_top = registry
        .dispatch("stream.read", json!({"stream":"s","after":i64::MAX}))
        .await
        .unwrap();
    assert_eq!(
        at_top,
        json!({"entries":[],"head_seq":i64::MAX-1,"floor_seq":i64::MAX-1,"next_after":null})
    );
    let last = registry
        .dispatch("stream.read", json!({"stream":"s","after":i64::MAX-2}))
        .await
        .unwrap();
    assert_eq!(last["entries"].as_array().unwrap().len(), 1);
    assert_eq!(last["entries"][0]["seq"], i64::MAX - 1);
    assert_eq!(
        truncate(&registry, "s", i64::MAX).await,
        json!({"removed":1,"floor_seq":i64::MAX,"head_seq":i64::MAX-1})
    );
    no_permit(&rt).await;
    assert_eq!(
        stat(&registry, "s").await,
        json!({"count":0,"floor_seq":i64::MAX,"head_seq":i64::MAX-1})
    );
    for after in [0, i64::MAX] {
        assert_eq!(
            registry
                .dispatch("stream.read", json!({"stream":"s","after":after}))
                .await
                .unwrap(),
            json!({"entries":[],"head_seq":i64::MAX-1,"floor_seq":i64::MAX,"next_after":null})
        );
    }
    exhausted(
        registry
            .dispatch(
                "stream.append",
                json!({"stream":"s","record":"after truncate"}),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(scalar(&rt, "SELECT COUNT(*) FROM notes").await, 0);
}

#[tokio::test]
async fn stream_exhaustion_in_atomic_and_per_member_batches_keeps_its_boundary() {
    let (rt, registry) = top_fixture(true).await;
    for stream in ["t", "u"] {
        append(&registry, stream, "local", json!(0), None).await;
    }
    for (ops, member) in [
        (
            json!([{"op":"append","stream":"s","record":1},{"op":"append","stream":"t","record":1}]),
            "0",
        ),
        (
            json!([{"op":"append","stream":"t","record":1},{"op":"append","stream":"s","record":1},{"op":"append","stream":"u","record":1}]),
            "1",
        ),
    ] {
        let before = snapshot(&rt).await;
        let error = exhausted(
            registry
                .dispatch("stream.batch", json!({"atomic":true,"ops":ops}))
                .await
                .unwrap_err(),
        );
        assert_eq!(error["details"]["member"], member);
        assert_eq!(
            snapshot(&rt).await,
            before,
            "earlier atomic members must roll back"
        );
    }
    let before_notes = scalar(&rt, "SELECT COUNT(*) FROM notes").await;
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"atomic":false,"ops":[
                {"op":"append","stream":"t","record":1},
                {"op":"append","stream":"s","record":1},
                {"op":"append","stream":"u","record":1}
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 3);
    assert_eq!(result["results"][0]["seq"], 2);
    assert_eq!(result["results"][1]["message"], "stream sequence exhausted");
    assert_eq!(result["results"][1]["domain_disposition"], "not_committed");
    assert_eq!(result["results"][2]["seq"], 2);
    assert_eq!(
        scalar(&rt, "SELECT COUNT(*) FROM notes").await,
        before_notes + 2
    );
    assert_eq!(stat(&registry, "s").await["count"], 1);
}

#[tokio::test]
async fn stream_batch_checks_exhaustion_for_each_transaction_local_sequence() {
    for atomic in [true, false] {
        let (rt, registry) = top_fixture(false).await;
        append(&registry, "u", "local", json!(0), None).await;
        let before = snapshot(&rt).await;
        let result = registry
            .dispatch(
                "stream.batch",
                json!({"atomic":atomic,"ops":[
                    {"op":"append","stream":"s","record":1,"expected_seq":i64::MAX-1},
                    {"op":"append","stream":"s","record":2,"expected_seq":i64::MAX},
                    {"op":"append","stream":"u","record":1}
                ]}),
            )
            .await;
        if atomic {
            let error = exhausted(result.unwrap_err());
            assert_eq!(error["details"]["member"], "1");
            assert_eq!(snapshot(&rt).await, before);
            assert_eq!(
                stat(&registry, "s").await,
                json!({"head_seq":i64::MAX-2,"floor_seq":i64::MAX-1,"count":0})
            );
        } else {
            let result = result.unwrap();
            assert_eq!(result["results"].as_array().unwrap().len(), 3);
            assert_eq!(result["results"][0]["seq"], i64::MAX - 1);
            assert_eq!(result["results"][1]["message"], "stream sequence exhausted");
            assert_eq!(result["results"][1]["domain_disposition"], "not_committed");
            assert_eq!(result["results"][2]["seq"], 2);
            assert_eq!(scalar(&rt, "SELECT COUNT(*) FROM notes").await, 3);
        }
        no_permit(&rt).await;
    }
}

#[tokio::test]
async fn stream_truncate_records_one_audit_with_original_boundary_and_removed_count() {
    let rt = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    builder.with_runtime_event_store(&rt).unwrap();
    let registry = builder.build().unwrap();
    for record in 1..=5 {
        append(&registry, "s", "local", json!(record), None).await;
    }
    let mut expected = Vec::new();
    for (stream, before, removed, floor, head) in [
        ("s", 100, 5, 6, 5),
        ("s", 0, 0, 6, 5),
        ("unknown", 100, 0, 1, 0),
    ] {
        assert_eq!(
            truncate(&registry, stream, before).await,
            json!({"removed":removed,"floor_seq":floor,"head_seq":head})
        );
        no_permit(&rt).await;
        expected.push((stream, before, removed));
        let mut reader = rt.sql().reader().await.unwrap();
        let rows = reader
            .query_all(SqlStatement::new(
                "SELECT payload,outcome FROM events WHERE kind='audit' AND verb='stream.truncate'",
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            expected.len(),
            "exactly one audit per truncate call"
        );
        let payloads: Vec<Value> = rows
            .iter()
            .map(|row| {
                assert_eq!(row.text("outcome").unwrap(), "success");
                serde_json::from_str(row.text("payload").unwrap()).unwrap()
            })
            .collect();
        for (stream, before, removed) in &expected {
            assert_eq!(
                payloads
                    .iter()
                    .filter(|payload| {
                        payload["stream"] == *stream
                            && payload["before_seq"] == *before
                            && payload["removed"] == *removed
                    })
                    .count(),
                1,
                "audit retains original request boundary and committed count: {payloads:?}"
            );
        }
    }
}

struct TruncateEmbedding {
    dimensions: usize,
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for TruncateEmbedding {
    async fn embed(
        &self,
        texts: &[String],
        _: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(vec![vec![0.5; self.dimensions]; texts.len()])
    }
    fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "stream-truncate-test"
    }
}

struct TruncateProvider {
    name: String,
    dimensions: usize,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for TruncateProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(std::sync::Arc::new(TruncateEmbedding {
            dimensions: self.dimensions,
        }))
    }
}

#[tokio::test]
async fn stream_truncate_purges_both_embedding_models_and_preserves_other_stream() {
    let model = lattice_embed::EmbeddingModel::AllMiniLmL6V2;
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        actor_id: None,
        embedding_model: Some(model),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    for (name, dimensions) in [
        (model.to_string(), model.dimensions()),
        ("stream-truncate-secondary".into(), 4),
    ] {
        rt.register_embedder(TruncateProvider { name, dimensions });
    }
    assert_eq!(rt.registered_embedding_model_names().len(), 2);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    let registry = builder.build().unwrap();
    let mut ids = Vec::new();
    for stream in ["removed", "survivor"] {
        let result = registry.dispatch("stream.append", json!({
            "stream":stream,"record":{"text":"embedded lexical survivor","stream":stream},"embed":true
        })).await.unwrap();
        ids.push(uuid::Uuid::parse_str(result["id"].as_str().unwrap()).unwrap());
    }
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let mut models = rt.registered_embedding_model_names();
    models.sort();
    let mut before_provenance = Vec::new();
    for model in &models {
        let store = rt.vectors_for_model(&token, model).unwrap();
        assert_eq!(store.count().await.unwrap(), 2);
        assert_eq!(store.batch_exists(&ids, "local").await.unwrap().len(), 2);
        assert!(store.provenance(ids[0]).await.unwrap().is_some());
        let provenance = store.provenance(ids[1]).await.unwrap();
        assert!(provenance.is_some(), "a real surviving vector is required");
        before_provenance.push(provenance);
    }
    let survivor_queries = [
        format!("SELECT * FROM notes WHERE id='{}'", ids[1]),
        format!(
            "SELECT rowid,* FROM fts_notes WHERE subject_id='{}'",
            ids[1]
        ),
        format!(
            "SELECT * FROM fts_notes_rowids WHERE subject_id='{}'",
            ids[1]
        ),
        format!(
            "SELECT * FROM ann_write_log WHERE subject_id='{}' ORDER BY seq",
            ids[1]
        ),
    ];
    let mut before_survivor = Vec::new();
    for query in &survivor_queries {
        let rows = raw(&rt, query).await;
        assert!(
            !rows.as_array().unwrap().is_empty(),
            "positive survivor fixture: {query}"
        );
        before_survivor.push(rows);
    }
    assert_eq!(
        truncate(&registry, "removed", 2).await,
        json!({"removed":1,"floor_seq":2,"head_seq":1})
    );
    no_permit(&rt).await;
    for (index, model) in models.iter().enumerate() {
        let store = rt.vectors_for_model(&token, model).unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
        let present = store.batch_exists(&ids, "local").await.unwrap();
        assert_eq!(present, std::collections::HashSet::from([ids[1]]));
        assert!(store.provenance(ids[0]).await.unwrap().is_none());
        assert_eq!(
            store.provenance(ids[1]).await.unwrap(),
            before_provenance[index]
        );
    }
    for (query, before) in survivor_queries.iter().zip(before_survivor) {
        assert_eq!(
            raw(&rt, query).await,
            before,
            "other stream stays unchanged: {query}"
        );
    }
    for table in ["notes", "fts_notes", "fts_notes_rowids"] {
        assert_eq!(
            scalar(
                &rt,
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE {}='{}'",
                    if table == "notes" { "id" } else { "subject_id" },
                    ids[0]
                )
            )
            .await,
            0
        );
    }
    assert_eq!(
        scalar(
            &rt,
            &format!(
                "SELECT COUNT(*) FROM ann_write_log WHERE subject_id='{}' AND op='delete'",
                ids[0]
            )
        )
        .await,
        2
    );
    assert_eq!(
        stat(&registry, "survivor").await,
        json!({"head_seq":1,"floor_seq":1,"count":1})
    );
}
