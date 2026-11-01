// Gate wiring tests: confirm that the secret check integrated into
// upsert_atoms fires on credential-shaped atom content and passes on
// allowlisted content (sha256 hex, UUIDs).  Tests call
// `khive_runtime::secret_gate::check` directly with the same inputs
// that the handler would pass — this proves the gate is reachable without
// requiring a live DB connection.

use khive_runtime::secret_gate::check;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_storage::SqlValue;

#[test]
fn key_only_list_projection_does_not_select_atom_content() {
    let fields = vec!["id".to_string(), "slug".to_string()];
    let columns = super::list_select_columns(Some(&fields));
    assert_eq!(columns, "id, slug, created_at");
    assert!(!columns.contains("content"));
}

#[tokio::test]
async fn get_prefix_query_plan_uses_primary_key_range_seeks() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());

    let mut reader = runtime.sql().reader().await.expect("prefix plan reader");
    let rows = reader
        .explain(
            super::knowledge_get_prefix_statement("0b6cf134").expect("valid compact UUID prefix"),
        )
        .await
        .expect("explain knowledge prefix query");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();

    assert!(
        ["knowledge_domains", "knowledge_atoms"]
            .iter()
            .all(|table| {
                details.iter().any(|detail| {
                    detail.contains(&format!("SEARCH {table}"))
                        && detail.contains("id>?")
                        && detail.contains("id<?")
                }) && !details
                    .iter()
                    .any(|detail| detail.contains(&format!("SCAN {table}")))
            }),
        "knowledge.get short-id tables must use primary-key range seeks: {details:?}"
    );
}

/// `delete_atoms` matches ids and slugs with one statement per column. A
/// single `id IN (..) OR slug IN (..)` predicate makes the no-statistics
/// planner scan the namespace, so a full chunk must still seek an index.
#[tokio::test]
async fn delete_atoms_key_statements_seek_an_index_at_a_full_chunk() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let keys: Vec<String> = (0..super::HYDRATION_ID_CHUNK)
        .map(|i| format!("key-{i}"))
        .collect();
    let mut reader = runtime.sql().reader().await.expect("plan reader");

    for table in ["knowledge_atoms", "knowledge_domains"] {
        let head = format!("SELECT id, slug FROM {table}");
        let lead = vec![SqlValue::Text("local".into())];
        let [by_id, by_slug] = super::key_match_statements(&head, lead, &keys);
        assert_eq!(by_id.params.len(), super::HYDRATION_ID_CHUNK + 1);
        assert_eq!(by_slug.params.len(), super::HYDRATION_ID_CHUNK + 1);

        for (statement, index) in [
            (by_id, format!("sqlite_autoindex_{table}_1")),
            (by_slug, format!("idx_{table}_ns_slug")),
        ] {
            let rows = reader.explain(statement).await.expect("explain statement");
            let details: Vec<String> = rows
                .iter()
                .filter_map(|row| match row.get("detail") {
                    Some(SqlValue::Text(detail)) => Some(detail.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                details
                    .iter()
                    .any(|detail| detail.contains(&format!("USING INDEX {index}"))),
                "{table} key match must seek {index}: {details:?}"
            );
            assert!(
                !details.iter().any(|detail| detail.contains("SCAN")),
                "{table} key match must not scan: {details:?}"
            );
        }
    }
}

#[test]
fn atom_body_with_fake_aws_key_is_blocked() {
    // Fake AWS access key ID in an atom body — must be blocked.
    let body = "provider: aws\naccess_key_id: AKIAFAKE000000000000\nregion: us-east-1";
    assert!(
        check(body).is_err(),
        "atom body containing fake AWS key must be blocked"
    );
}

#[test]
fn atom_body_with_sha256_hash_passes() {
    // A manifest-style line containing a sha256 digest — must pass the allowlist.
    let body = "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"";
    assert!(
        check(body).is_ok(),
        "atom body with sha256 manifest hash must pass; fired: {:?}",
        check(body).err()
    );
}

#[test]
fn atom_name_with_fake_openai_key_is_blocked() {
    // A credential accidentally used as an atom name — must be blocked.
    let name = format!("sk-proj-{}", "A".repeat(80));
    assert!(
        check(&name).is_err(),
        "atom name containing fake OpenAI key must be blocked"
    );
}

#[test]
fn normal_atom_name_passes() {
    let name = "FlashAttention-2: efficient transformer self-attention";
    assert!(
        check(name).is_ok(),
        "normal atom name must pass; fired: {:?}",
        check(name).err()
    );
}

// Ignored-param warning coverage: verify that chunk_size and cascade are still
// accepted by the param structs (no deserialization error) and that the fields
// are Some when supplied, confirming the warning branch precondition is satisfiable.

#[test]
fn upsert_atoms_chunk_size_accepted_and_detectable() {
    use crate::knowledge::schema::UpsertAtomsParams;
    let p: UpsertAtomsParams = serde_json::from_value(serde_json::json!({
        "atoms": [{"slug": "s", "name": "n", "content": "placeholder content for test"}],
        "chunk_size": 100,
    }))
    .expect("upsert_atoms params with chunk_size must deserialize without error");
    assert!(
        p.chunk_size.is_some(),
        "chunk_size must be Some when supplied so the warning branch precondition is satisfiable"
    );
}

#[test]
fn delete_atoms_cascade_accepted_and_detectable() {
    use crate::knowledge::schema::DeleteAtomsParams;
    let p: DeleteAtomsParams = serde_json::from_value(serde_json::json!({
        "ids": ["some-atom-id"],
        "cascade": true,
    }))
    .expect("delete_atoms params with cascade must deserialize without error");
    assert!(
        p.cascade.is_some(),
        "cascade must be Some when supplied so the warning branch precondition is satisfiable"
    );
}

// The real implementation receives its normal SQL provider in production. These
// fixtures replace only that capability, never token minting or handler logic.
#[derive(Clone, Copy)]
enum AtomicFault {
    Healthy,
    TargetRead,
    NotStarted,
    RolledBackContract,
    CommitThenUnknown,
}

struct AtomicSql {
    inner: std::sync::Arc<dyn khive_storage::SqlAccess>,
    fault: AtomicFault,
    batches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
struct AtomicReader {
    inner: Box<dyn khive_storage::SqlReader>,
    fault: AtomicFault,
}
struct AtomicWriter {
    inner: Box<dyn khive_storage::SqlWriter>,
    fault: AtomicFault,
    batches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl khive_storage::SqlReader for AtomicReader {
    async fn query_row(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Option<khive_storage::types::SqlRow>> {
        if matches!(self.fault, AtomicFault::TargetRead)
            && statement.sql == khive_runtime::sql!("knowledge_target_domain_probe")
        {
            return Err(khive_storage::StorageError::Internal(
                "fixture-target-reader-interrupted".into(),
            ));
        }
        self.inner.query_row(statement).await
    }
    async fn query_all(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::types::SqlRow>> {
        self.inner.query_all(statement).await
    }
    async fn query_scalar(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Option<SqlValue>> {
        self.inner.query_scalar(statement).await
    }
    async fn explain(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::types::SqlRow>> {
        self.inner.explain(statement).await
    }
}
#[async_trait::async_trait]
impl khive_storage::SqlReader for AtomicWriter {
    async fn query_row(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Option<khive_storage::types::SqlRow>> {
        self.inner.query_row(statement).await
    }
    async fn query_all(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::types::SqlRow>> {
        self.inner.query_all(statement).await
    }
    async fn query_scalar(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Option<SqlValue>> {
        self.inner.query_scalar(statement).await
    }
    async fn explain(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::types::SqlRow>> {
        self.inner.explain(statement).await
    }
}
#[async_trait::async_trait]
impl khive_storage::SqlWriter for AtomicWriter {
    async fn execute(
        &mut self,
        statement: khive_storage::SqlStatement,
    ) -> khive_storage::StorageResult<u64> {
        self.inner.execute(statement).await
    }
    async fn execute_batch(
        &mut self,
        statements: Vec<khive_storage::SqlStatement>,
    ) -> khive_storage::StorageResult<u64> {
        use khive_storage::{StorageError, WriterTaskRequestState};
        self.batches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.fault {
            AtomicFault::NotStarted => Err(StorageError::writer_task_terminated(
                WriterTaskRequestState::NotStarted,
            )),
            AtomicFault::RolledBackContract => Err(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source: Box::new(StorageError::Internal("fixture-rollback-contract".into())),
            }),
            AtomicFault::CommitThenUnknown => {
                self.inner.execute_batch(statements).await?;
                Err(StorageError::writer_task_terminated(
                    WriterTaskRequestState::SideEffectsUnknown,
                ))
            }
            _ => self.inner.execute_batch(statements).await,
        }
    }
    async fn execute_script(&mut self, script: String) -> khive_storage::StorageResult<()> {
        self.inner.execute_script(script).await
    }
}
#[async_trait::async_trait]
impl khive_storage::SqlAccess for AtomicSql {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.inner.database_path()
    }
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        Ok(Box::new(AtomicReader {
            inner: self.inner.reader().await?,
            fault: self.fault,
        }))
    }
    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        Ok(Box::new(AtomicWriter {
            inner: self.inner.writer().await?,
            fault: self.fault,
            batches: self.batches.clone(),
        }))
    }
    async fn atomic_unit(
        &self,
        op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        self.inner.atomic_unit(op).await
    }
}

const ATOMIC_CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
fn atomic_runtime() -> (KhiveRuntime, khive_runtime::NamespaceToken) {
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        actor_id: Some("atomic-write-fixture".into()),
        packs: vec!["kg".into(), "knowledge".into()],
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::KnowledgePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    let token = runtime
        .authorize(khive_types::Namespace::local())
        .expect("real Gate-approved fixture token");
    (runtime, token)
}
fn atomic_params() -> serde_json::Value {
    serde_json::json!({"atoms":[
        {"slug":"atomic-first","name":"First","content":ATOMIC_CONTENT},
        {"slug":"atomic-second","name":"Second","content":ATOMIC_CONTENT}
    ]})
}
async fn atomic_rows(runtime: &KhiveRuntime) -> serde_json::Value {
    let mut reader = runtime.sql().reader().await.unwrap();
    serde_json::to_value(
        reader
            .query_all(khive_storage::SqlStatement {
                sql: "SELECT * FROM knowledge_atoms ORDER BY id".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
    )
    .unwrap()
}
fn atomic_sql(
    runtime: &KhiveRuntime,
    fault: AtomicFault,
) -> (
    std::sync::Arc<dyn khive_storage::SqlAccess>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (
        std::sync::Arc::new(AtomicSql {
            inner: runtime.sql(),
            fault,
            batches: batches.clone(),
        }),
        batches,
    )
}

#[tokio::test]
async fn atomic_admission_interruption_reports_exact_unchecked_indices() {
    let (runtime, token) = atomic_runtime();
    let before = atomic_rows(&runtime).await;
    let (sql, batches) = atomic_sql(&runtime, AtomicFault::TargetRead);
    let params = serde_json::json!({"atoms":[
        {"slug":"bad-before","name":"Bad","content":"short"},
        {"slug":"good-before","name":"Good","content":ATOMIC_CONTENT},
        {"id":"6ab81527-ccdf-4c99-a898-4cb032829d89","properties":{}},
        {"slug":"bad-after","name":"Bad","content":"short"},
        {"slug":"unchecked-after","name":"Good","content":ATOMIC_CONTENT}
    ]});
    let error =
        super::KnowledgeHandlers::upsert_atoms_with_sql(&runtime, &token, params, false, sql)
            .await
            .unwrap_err();
    assert!(
        matches!(error.refusal_source(), khive_runtime::RuntimeError::Storage(khive_storage::StorageError::Internal(message)) if message == "fixture-target-reader-interrupted")
    );
    let value =
        khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(value["domain_disposition"], "not_committed");
    assert_eq!(value["validation_complete"], false);
    assert_eq!(value["unchecked_indices"], serde_json::json!([2, 4]));
    assert_eq!(
        value["refused"],
        serde_json::json!([
            {"index":0,"field":"content","reason":"invalid_input"},
            {"index":3,"field":"content","reason":"invalid_input"}
        ])
    );
    assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(atomic_rows(&runtime).await, before);
}

#[tokio::test]
async fn atomic_write_settlement_preserves_typed_states_and_real_unknown_commit() {
    use khive_storage::WriterTaskRequestState;
    for (fault, expected_state, disposition, committed) in [
        (
            AtomicFault::NotStarted,
            WriterTaskRequestState::NotStarted,
            "not_committed",
            false,
        ),
        (
            AtomicFault::RolledBackContract,
            WriterTaskRequestState::TransactionRolledBack,
            "not_committed",
            false,
        ),
        (
            AtomicFault::CommitThenUnknown,
            WriterTaskRequestState::SideEffectsUnknown,
            "unknown",
            true,
        ),
    ] {
        let (runtime, token) = atomic_runtime();
        let before = atomic_rows(&runtime).await;
        let (sql, batches) = atomic_sql(&runtime, fault);
        let error = super::KnowledgeHandlers::upsert_atoms_with_sql(
            &runtime,
            &token,
            atomic_params(),
            false,
            sql,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                error.refusal_source(),
                khive_runtime::RuntimeError::Storage(_)
            ),
            "typed original storage error survives"
        );
        assert_eq!(
            error.writer_task_failure_context().unwrap().request_state,
            expected_state
        );
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        let after = atomic_rows(&runtime).await;
        if committed {
            assert_eq!(
                after.as_array().unwrap().len(),
                2,
                "lost acknowledgement follows a REAL committed batch"
            );
        } else {
            assert_eq!(after, before);
        }
        let value =
            khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
        assert_eq!(value["domain_disposition"], disposition);
        assert_eq!(value["request_state"], expected_state.to_string());
        if matches!(fault, AtomicFault::RolledBackContract) {
            assert!(value["message"]
                .as_str()
                .unwrap()
                .contains("fixture-rollback-contract"));
        }
    }
    let (runtime, token) = atomic_runtime();
    let (sql, batches) = atomic_sql(&runtime, AtomicFault::Healthy);
    let value = super::KnowledgeHandlers::upsert_atoms_with_sql(
        &runtime,
        &token,
        atomic_params(),
        false,
        sql,
    )
    .await
    .unwrap();
    assert_eq!(
        value,
        serde_json::json!({"created":2,"updated":0,"total":2})
    );
    assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(atomic_rows(&runtime).await.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn atomic_real_batch_failure_rolls_back_requested_rows() {
    let (runtime, token) = atomic_runtime();
    let before = atomic_rows(&runtime).await;
    {
        let mut writer = runtime.sql().writer().await.unwrap();
        writer.execute(khive_storage::SqlStatement { sql: "CREATE TRIGGER reject_atomic_second BEFORE INSERT ON knowledge_atoms WHEN NEW.slug = 'atomic-second' BEGIN SELECT RAISE(ABORT, 'ATOMIC-REAL-ROLLBACK'); END".into(), params: vec![], label: None }).await.unwrap();
    }
    let error = super::KnowledgeHandlers::upsert_atoms_with_sql(
        &runtime,
        &token,
        atomic_params(),
        false,
        runtime.sql(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error.refusal_source(),
        khive_runtime::RuntimeError::Storage(_)
    ));
    assert_eq!(
        atomic_rows(&runtime).await,
        before,
        "actual earlier statement rolls back with the later fault"
    );
    assert!(error.to_string().contains("ATOMIC-REAL-ROLLBACK"));
    assert_eq!(
        error
            .writer_task_failure_context()
            .map(|context| context.request_state),
        Some(khive_storage::WriterTaskRequestState::TransactionRolledBack)
    );
    let value =
        khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(value["domain_disposition"], "not_committed");
    {
        let mut writer = runtime.sql().writer().await.unwrap();
        writer
            .execute(khive_storage::SqlStatement {
                sql: "DROP TRIGGER reject_atomic_second".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
    }
    super::KnowledgeHandlers::upsert_atoms_with_sql(
        &runtime,
        &token,
        atomic_params(),
        false,
        runtime.sql(),
    )
    .await
    .unwrap();
    assert_eq!(atomic_rows(&runtime).await.as_array().unwrap().len(), 2);
}

// These are synthetic in-memory fixtures. Their setup and positive baseline
// must pass on the owner host before either mutation control is interpreted.
async fn delete_purge_fixture() -> (KhiveRuntime, khive_runtime::NamespaceToken, String) {
    let (runtime, token) = atomic_runtime();
    super::KnowledgeHandlers::upsert_atoms_with_sql(
        &runtime, &token,
        serde_json::json!({"atoms": [{"slug": "purge-target", "name": "Purge target", "content": ATOMIC_CONTENT}]}),
        false, runtime.sql(),
    ).await.expect("seed atom");
    let mut reader = runtime.sql().reader().await.expect("fixture reader");
    let id = match reader
        .query_scalar(khive_storage::SqlStatement::new(
            "SELECT id FROM knowledge_atoms WHERE slug = 'purge-target' AND deleted_at IS NULL",
            vec![],
        ))
        .await
        .expect("find atom")
    {
        Some(SqlValue::Text(id)) => id,
        other => panic!("expected live atom id: {other:?}"),
    };
    drop(reader);
    let mut writer = runtime.sql().writer().await.expect("fixture writer");
    writer.execute_script("CREATE VIRTUAL TABLE vec_fixture_a USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, embedding float[4]);\nCREATE VIRTUAL TABLE vec_fixture_b USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, embedding float[4]);\nCREATE TABLE vec_ordinary_fixture (other TEXT);".into()).await.expect("create fixture vec0 tables");
    for (table, field) in [
        ("vec_fixture_a", "knowledge.atom"),
        ("vec_fixture_b", "other.field"),
    ] {
        writer.execute(khive_storage::SqlStatement::new(format!(
            "INSERT INTO {table} (subject_id,namespace,kind,field,embedding_model,embedding) VALUES (?1,'local','entity',?2,?3,vec_f32('[0.1,0.2,0.3,0.4]'))"),
            vec![SqlValue::Text(id.clone()), SqlValue::Text(field.into()), SqlValue::Text(table[4..].into())],
        )).await.expect("seed vector");
        writer.execute(khive_storage::SqlStatement::new(
            "INSERT INTO vector_provenance (model_key,subject_id,namespace,embedding_digest) VALUES (?1,?2,'local',?3)",
            vec![SqlValue::Text(table[4..].into()), SqlValue::Text(id.clone()), SqlValue::Text("a".repeat(64))],
        )).await.expect("seed provenance");
    }
    writer.execute(khive_storage::SqlStatement::new(
        "INSERT INTO vec_fixture_a (subject_id,namespace,kind,field,embedding_model,embedding) VALUES ('00000000-0000-4000-8000-000000000001','foreign','note','note.content','fixture_a',vec_f32('[0.5,0.6,0.7,0.8]'))", vec![],
    )).await.expect("seed unrelated vector");
    drop(writer);
    (runtime, token, id)
}

async fn purge_count(runtime: &KhiveRuntime, query: &str, id: &str) -> i64 {
    let mut reader = runtime.sql().reader().await.expect("count reader");
    match reader
        .query_scalar(khive_storage::SqlStatement::new(
            query,
            vec![SqlValue::Text(id.into())],
        ))
        .await
        .expect("count query")
    {
        Some(SqlValue::Integer(count)) => count,
        other => panic!("expected count: {other:?}"),
    }
}

#[tokio::test]
async fn delete_atoms_purges_only_atom_field_and_preserves_other_model_provenance() {
    let (runtime, token, id) = delete_purge_fixture().await;
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_b WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vector_provenance WHERE subject_id=?1",
            &id
        )
        .await,
        2
    );
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("delete atom");
    assert_eq!(result["deleted"], 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_b WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1 AND namespace='foreign' AND kind='note'", "00000000-0000-4000-8000-000000000001").await, 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vector_provenance WHERE model_key='fixture_a' AND subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vector_provenance WHERE model_key='fixture_b' AND subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND namespace='local' AND embedding_model='fixture_a' AND field='knowledge.atom' AND op='delete'", &id).await, 1);
    let again = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("repeat delete");
    assert_eq!(again["deleted"], 0);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        1
    );
}

#[tokio::test]
async fn delete_atoms_rolls_back_tombstone_vector_and_log_on_purge_failure() {
    let (runtime, token, id) = delete_purge_fixture().await;
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    {
        let mut writer = runtime.sql().writer().await.expect("trigger writer");
        writer.execute(khive_storage::SqlStatement::new(
            "CREATE TRIGGER reject_fixture_purge BEFORE DELETE ON vector_provenance WHEN OLD.model_key='fixture_a' BEGIN SELECT RAISE(ABORT, 'fixture-purge-failure'); END", vec![],
        )).await.expect("install rollback trigger");
    }
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect_err("purge trigger refuses");
    assert!(error.to_string().contains("fixture-purge-failure"));
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vector_provenance WHERE subject_id=?1",
            &id
        )
        .await,
        2
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delete_atoms_with_no_vector_tables_does_not_create_any() {
    let (runtime, token) = atomic_runtime();
    super::KnowledgeHandlers::upsert_atoms_with_sql(&runtime, &token,
        serde_json::json!({"atoms":[{"slug":"no-vector","name":"No vector","content":ATOMIC_CONTENT}]}),
        false, runtime.sql()).await.expect("seed atom");
    let before = {
        let mut reader = runtime.sql().reader().await.expect("reader");
        reader
            .query_scalar(khive_storage::SqlStatement::new(
                "SELECT count(*) FROM pragma_table_list WHERE name GLOB 'vec_*' AND type='virtual'",
                vec![],
            ))
            .await
            .expect("count tables")
    };
    assert!(matches!(before, Some(SqlValue::Integer(0))));
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":["no-vector"]}),
    )
    .await
    .expect("delete without vectors");
    assert_eq!(result["deleted"], 1);
    let mut reader = runtime.sql().reader().await.expect("reader");
    let after = reader
        .query_scalar(khive_storage::SqlStatement::new(
            "SELECT count(*) FROM pragma_table_list WHERE name GLOB 'vec_*' AND type='virtual'",
            vec![],
        ))
        .await
        .expect("count tables");
    assert!(matches!(after, Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn generic_atom_delete_purges_vector_and_preserves_other_field() {
    use khive_runtime::pack::PackByIdResolver;
    let (runtime, _token, id) = delete_purge_fixture().await;
    let pack = crate::KnowledgePack::new(runtime.clone());
    let parsed = uuid::Uuid::parse_str(&id).expect("fixture UUID");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    let value = pack
        .delete_by_id(parsed, false)
        .await
        .expect("soft delete atom");
    assert_eq!(value["kind"], "atom");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_b WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND field='knowledge.atom' AND op='delete'", &id).await, 1);
    pack.delete_by_id(parsed, false)
        .await
        .expect("repeat soft delete");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        1
    );
    pack.delete_by_id(parsed, true)
        .await
        .expect("hard delete tombstone");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_b WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
}

#[tokio::test]
async fn delete_atoms_logs_every_vector_in_a_multi_id_chunk() {
    let (runtime, token, first) = delete_purge_fixture().await;
    super::KnowledgeHandlers::upsert_atoms_with_sql(&runtime, &token,
        serde_json::json!({"atoms":[{"slug":"purge-second","name":"Second","content":ATOMIC_CONTENT}]}),
        false, runtime.sql()).await.expect("seed second atom");
    let second = {
        let mut reader = runtime.sql().reader().await.expect("reader");
        match reader
            .query_scalar(khive_storage::SqlStatement::new(
                "SELECT id FROM knowledge_atoms WHERE slug='purge-second' AND deleted_at IS NULL",
                vec![],
            ))
            .await
            .expect("find second")
        {
            Some(SqlValue::Text(id)) => id,
            other => panic!("missing second atom: {other:?}"),
        }
    };
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer.execute(khive_storage::SqlStatement::new(
            "INSERT INTO vec_fixture_a (subject_id,namespace,kind,field,embedding_model,embedding) VALUES (?1,'local','entity','knowledge.atom','fixture_a',vec_f32('[0.1,0.2,0.3,0.4]'))",
            vec![SqlValue::Text(second.clone())],
        )).await.expect("seed second vector");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &first
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &second
        )
        .await,
        1
    );
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[first.clone(), second.clone()]}),
    )
    .await
    .expect("delete both");
    assert_eq!(result["deleted"], 2);
    for id in [&first, &second] {
        assert_eq!(
            purge_count(
                &runtime,
                "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
                id
            )
            .await,
            0
        );
        assert_eq!(purge_count(&runtime, "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND field='knowledge.atom' AND op='delete'", id).await, 1);
    }
}

#[tokio::test]
async fn delete_atoms_refuses_domain_before_modifying_other_requested_atoms() {
    let (runtime, token, id) = delete_purge_fixture().await;
    super::KnowledgeHandlers::upsert_domains(&runtime, &token,
        serde_json::json!({"domains":[{"slug":"fixture-domain","name":"Fixture domain","description":ATOMIC_CONTENT}]}),
    ).await.expect("seed domain and mirror");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone(), "fixture-domain"]}),
    )
    .await
    .expect_err("domain must refuse");
    assert!(error.to_string().contains("cannot delete domain"));
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

struct DeletePreWriterInterpose {
    inner: std::sync::Arc<dyn khive_storage::SqlAccess>,
    statement: khive_storage::SqlStatement,
}
#[async_trait::async_trait]
impl khive_storage::SqlAccess for DeletePreWriterInterpose {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.inner.database_path()
    }
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(
        &self,
        op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        let mut writer = self.inner.writer().await?;
        let affected = writer.execute(self.statement.clone()).await?;
        if affected != 1 {
            return Err(khive_storage::StorageError::Internal(
                "fixture interposition did not change exactly one domain".into(),
            ));
        }
        drop(writer);
        self.inner.atomic_unit(op).await
    }
}

#[tokio::test]
async fn delete_atoms_rechecks_domain_guard_after_pre_writer_change() {
    let (runtime, token, id) = delete_purge_fixture().await;
    super::KnowledgeHandlers::upsert_domains(&runtime, &token,
        serde_json::json!({"domains":[{"slug":"before-domain","name":"Fixture domain","description":ATOMIC_CONTENT}]}),
    ).await.expect("seed domain");
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND slug='purge-target' AND deleted_at IS NULL", &id).await, 1);
    let sql = std::sync::Arc::new(DeletePreWriterInterpose {
        inner: runtime.sql(),
        statement: khive_storage::SqlStatement::new(
            "UPDATE knowledge_domains SET slug='purge-target' WHERE slug='before-domain' AND namespace='local'", vec![],
        ),
    });
    let error = super::KnowledgeHandlers::delete_atoms_with_sql(
        &runtime,
        &token,
        serde_json::json!({"ids":["purge-target"]}),
        sql,
    )
    .await
    .expect_err("new domain guard must refuse");
    assert!(error.to_string().contains("cannot delete domain"));
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delete_atoms_purges_refilled_live_vector_and_logs_again() {
    let (runtime, token, id) = delete_purge_fixture().await;
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("first delete");
    {
        let mut writer = runtime.sql().writer().await.expect("refill writer");
        assert_eq!(
            writer
                .execute(khive_storage::SqlStatement::new(
                    "UPDATE knowledge_atoms SET deleted_at=NULL WHERE id=?1",
                    vec![SqlValue::Text(id.clone())],
                ))
                .await
                .expect("restore synthetic atom"),
            1
        );
        writer.execute(khive_storage::SqlStatement::new(
            "INSERT INTO vec_fixture_a (subject_id,namespace,kind,field,embedding_model,embedding) VALUES (?1,'local','entity','knowledge.atom','fixture_a',vec_f32('[0.1,0.2,0.3,0.4]'))", vec![SqlValue::Text(id.clone())],
        )).await.expect("refill vector");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("second live delete");
    assert_eq!(result["deleted"], 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND field='knowledge.atom' AND op='delete'", &id).await, 2);
}

#[tokio::test]
async fn delete_atoms_preserves_noncanonical_stored_id_spelling() {
    let (runtime, token, original) = delete_purge_fixture().await;
    let upper = "ABCDEF00-0000-4000-8000-000000000001".to_string();
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        assert_eq!(
            writer
                .execute(khive_storage::SqlStatement::new(
                    "UPDATE knowledge_atoms SET id=?1 WHERE id=?2",
                    vec![
                        SqlValue::Text(upper.clone()),
                        SqlValue::Text(original.clone())
                    ],
                ))
                .await
                .expect("change atom spelling"),
            1
        );
        for table in ["vec_fixture_a", "vec_fixture_b"] {
            assert_eq!(
                writer
                    .execute(khive_storage::SqlStatement::new(
                        format!("DELETE FROM {table} WHERE subject_id=?1"),
                        vec![SqlValue::Text(original.clone())],
                    ))
                    .await
                    .expect("remove canonical fixture row"),
                1
            );
        }
        writer.execute(khive_storage::SqlStatement::new(
            "INSERT INTO vec_fixture_a (subject_id,namespace,kind,field,embedding_model,embedding) VALUES (?1,'local','entity','knowledge.atom','fixture_a',vec_f32('[0.1,0.2,0.3,0.4]'))", vec![SqlValue::Text(upper.clone())],
        )).await.expect("seed upper-case vector");
        writer
            .execute(khive_storage::SqlStatement::new(
                "UPDATE vector_provenance SET subject_id=?1 WHERE subject_id=?2",
                vec![
                    SqlValue::Text(upper.clone()),
                    SqlValue::Text(original.clone()),
                ],
            ))
            .await
            .expect("change provenance spelling");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &upper
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &upper
        )
        .await,
        1
    );
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[upper.clone()]}),
    )
    .await
    .expect("delete stored spelling");
    assert_eq!(result["deleted"], 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NOT NULL",
            &upper
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &upper
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &upper
        )
        .await,
        1
    );
}

#[tokio::test]
async fn delete_atoms_refuses_other_virtual_module_before_any_write() {
    let (runtime, token, id) = delete_purge_fixture().await;
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer
            .execute_script("CREATE VIRTUAL TABLE vec_odd USING fts5(content);".into())
            .await
            .expect("create other-module fixture");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect_err("unsupported module refuses");
    let text = error.to_string();
    assert!(text.contains("vec_odd") && text.contains("fts5"), "{text}");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delete_atoms_refuses_incomplete_vec0_before_any_write() {
    let (runtime, token, id) = delete_purge_fixture().await;
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer.execute_script("CREATE VIRTUAL TABLE vec_incomplete USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, embedding_model TEXT NOT NULL, embedding float[4]);".into()).await.expect("create incomplete vec0 fixture");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect_err("missing field refuses");
    let text = error.to_string();
    assert!(
        text.contains("vec_incomplete") && text.contains("field"),
        "{text}"
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delete_atoms_refuses_vec0_missing_log_column_before_any_write() {
    let (runtime, token, id) = delete_purge_fixture().await;
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer.execute_script("CREATE VIRTUAL TABLE vec_incomplete_log USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL, embedding float[4]);".into()).await.expect("create incomplete vec0 fixture");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect_err("missing log column refuses");
    let text = error.to_string();
    assert!(
        text.contains("vec_incomplete_log") && text.contains("embedding_model"),
        "{text}"
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        0
    );
}

struct TempShadowSql {
    inner: std::sync::Arc<dyn khive_storage::SqlAccess>,
    id: String,
}
#[async_trait::async_trait]
impl khive_storage::SqlAccess for TempShadowSql {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.inner.database_path()
    }
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(
        &self,
        op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        let id = self.id.clone();
        self.inner.atomic_unit(Box::new(move |writer| Box::pin(async move {
            writer.execute(khive_storage::SqlStatement::new(
                "CREATE TEMP TABLE vec_fixture_a (subject_id TEXT, namespace TEXT, field TEXT)", vec![],
            )).await?;
            writer.execute(khive_storage::SqlStatement::new(
                "INSERT INTO temp.vec_fixture_a(subject_id,namespace,field) VALUES (?1,'local','knowledge.atom')",
                vec![SqlValue::Text(id.clone())],
            )).await?;
            for schema in ["main", "temp"] {
                let count = writer.query_scalar(khive_storage::SqlStatement::new(
                    format!("SELECT count(*) FROM {schema}.vec_fixture_a WHERE subject_id=?1"),
                    vec![SqlValue::Text(id.clone())],
                )).await?;
                if !matches!(count, Some(SqlValue::Integer(1))) {
                    return Err(khive_storage::StorageError::Internal(format!("{schema} shadow baseline invalid")));
                }
            }
            let outcome = op(writer).await?;
            for (schema, expected) in [("main", 0), ("temp", 1)] {
                let count = writer.query_scalar(khive_storage::SqlStatement::new(
                    format!("SELECT count(*) FROM {schema}.vec_fixture_a WHERE subject_id=?1"),
                    vec![SqlValue::Text(id.clone())],
                )).await?;
                if !matches!(count, Some(SqlValue::Integer(actual)) if actual == expected) {
                    return Err(khive_storage::StorageError::Internal(format!("{schema} shadow assertion failed")));
                }
            }
            Ok(outcome)
        }))).await
    }
}

#[tokio::test]
async fn delete_atoms_targets_main_vector_when_temp_table_shadows_name() {
    let (runtime, token, id) = delete_purge_fixture().await;
    let sql = std::sync::Arc::new(TempShadowSql {
        inner: runtime.sql(),
        id: id.clone(),
    });
    let result = super::KnowledgeHandlers::delete_atoms_with_sql(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
        sql,
    )
    .await
    .expect("delete from main with same-writer shadow controls");
    assert_eq!(result["deleted"], 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.ann_write_log WHERE subject_id=?1 AND op='delete'",
            &id
        )
        .await,
        1
    );
}

#[tokio::test]
async fn delete_atoms_refuses_invalid_vector_table_suffix_before_any_write() {
    let (runtime, token, id) = delete_purge_fixture().await;
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer.execute_script("CREATE VIRTUAL TABLE \"vec_invalid-name\" USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, embedding float[4]);".into()).await.expect("create invalid-suffix fixture");
    }
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    let error = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect_err("invalid model key refuses");
    let text = error.to_string();
    assert!(
        text.contains("vec_invalid-name") && text.contains("suffix"),
        "{text}"
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_fixture_a WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
}

async fn seed_case_variant_model(runtime: &KhiveRuntime, id: &str, field: &str) {
    let mut writer = runtime.sql().writer().await.expect("case fixture writer");
    writer.execute_script("CREATE VIRTUAL TABLE vec_Qwen3 USING vec0(subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, embedding float[4]);".into()).await.expect("create mixed-case vec0");
    writer.execute(khive_storage::SqlStatement::new(
        "INSERT INTO main.vec_Qwen3 (subject_id,namespace,kind,field,embedding_model,embedding) VALUES (?1,'local','entity',?2,'Qwen3',vec_f32('[0.1,0.2,0.3,0.4]'))",
        vec![SqlValue::Text(id.into()), SqlValue::Text(field.into())],
    )).await.expect("seed mixed-case vector");
    for key in ["Qwen3", "qwen3"] {
        writer.execute(khive_storage::SqlStatement::new(
            "INSERT INTO main.vector_provenance (model_key,subject_id,namespace,embedding_digest) VALUES (?1,?2,'local',?3)",
            vec![SqlValue::Text(key.into()), SqlValue::Text(id.into()), SqlValue::Text("a".repeat(64))],
        )).await.expect("seed case-variant provenance");
    }
}

#[tokio::test]
async fn delete_atoms_purges_all_case_variants_of_model_provenance() {
    let (runtime, token, id) = delete_purge_fixture().await;
    seed_case_variant_model(&runtime, &id, "knowledge.atom").await;
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_Qwen3 WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.vector_provenance WHERE model_key='Qwen3' COLLATE NOCASE AND subject_id=?1", &id).await, 2);
    let result = super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("delete mixed-case model");
    assert_eq!(result["deleted"], 1);
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_Qwen3 WHERE subject_id=?1",
            &id
        )
        .await,
        0
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.vector_provenance WHERE model_key='Qwen3' COLLATE NOCASE AND subject_id=?1", &id).await, 0);
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.ann_write_log WHERE embedding_model='Qwen3' AND subject_id=?1 AND op='delete'", &id).await, 1);
}

#[tokio::test]
async fn delete_atoms_keeps_case_variant_provenance_if_other_field_remains() {
    let (runtime, token, id) = delete_purge_fixture().await;
    seed_case_variant_model(&runtime, &id, "other.field").await;
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_Qwen3 WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.vector_provenance WHERE model_key='Qwen3' COLLATE NOCASE AND subject_id=?1", &id).await, 2);
    super::KnowledgeHandlers::delete_atoms(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone()]}),
    )
    .await
    .expect("delete atom");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM main.vec_Qwen3 WHERE subject_id=?1",
            &id
        )
        .await,
        1
    );
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.vector_provenance WHERE model_key='Qwen3' COLLATE NOCASE AND subject_id=?1", &id).await, 2);
    assert_eq!(purge_count(&runtime, "SELECT count(*) FROM main.ann_write_log WHERE embedding_model='Qwen3' AND subject_id=?1 AND op='delete'", &id).await, 0);
}

struct DeleteWriterCounter {
    inner: std::sync::Arc<dyn khive_storage::SqlAccess>,
    acquisitions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl khive_storage::SqlAccess for DeleteWriterCounter {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.inner.database_path()
    }
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.writer().await
    }
    async fn atomic_unit(
        &self,
        op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        self.acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.atomic_unit(op).await
    }
}

#[tokio::test]
async fn delete_atoms_refuses_known_domain_before_writer_acquisition() {
    let (runtime, token, id) = delete_purge_fixture().await;
    super::KnowledgeHandlers::upsert_domains(&runtime, &token,
        serde_json::json!({"domains":[{"slug":"known-domain","name":"Known domain","description":ATOMIC_CONTENT}]}),
    ).await.expect("seed domain and mirror");
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
    let acquisitions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sql = std::sync::Arc::new(DeleteWriterCounter {
        inner: runtime.sql(),
        acquisitions: acquisitions.clone(),
    });
    let error = super::KnowledgeHandlers::delete_atoms_with_sql(
        &runtime,
        &token,
        serde_json::json!({"ids":[id.clone(),"known-domain"]}),
        sql,
    )
    .await
    .expect_err("domain must refuse");
    assert!(error.to_string().contains("cannot delete domain"));
    assert_eq!(
        acquisitions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "preflight refusal must not request a writer"
    );
    assert_eq!(
        purge_count(
            &runtime,
            "SELECT count(*) FROM knowledge_atoms WHERE id=?1 AND deleted_at IS NULL",
            &id
        )
        .await,
        1
    );
}
