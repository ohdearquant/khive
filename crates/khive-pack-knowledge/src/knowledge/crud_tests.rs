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
