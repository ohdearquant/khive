use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use khive_pack_git::GitPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

use super::*;
use crate::pack::WorkspacePack;

async fn bounded(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(10), test)
        .await
        .expect("workspace cursor test exceeded its async deadline");
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    builder.register(WorkspacePack::new(runtime.clone()));
    builder.build().expect("workspace registry")
}

fn initialize(runtime: &KhiveRuntime) {
    runtime.backend().prepare_core_schema().expect("core schema before pack plans");
    registry(runtime)
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("fallible writable pack schema initialization");
}

fn fixture() -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
    initialize(&runtime);
    runtime
}

async fn read(runtime: &KhiveRuntime, path: &str) -> Option<CursorRow> {
    let mut reader = runtime.sql().reader().await.expect("cursor reader");
    get(&mut *reader, path).await.expect("cursor get")
}

async fn rows(runtime: &KhiveRuntime) -> Vec<CursorRow> {
    let mut reader = runtime.sql().reader().await.expect("cursor list reader");
    list(&mut *reader).await.expect("cursor list")
}

async fn put(
    runtime: &KhiveRuntime,
    path: &str,
    mtime: Option<i64>,
    hash: Option<&str>,
    synced: Option<i64>,
) {
    let path = path.to_owned();
    let hash = hash.map(str::to_owned);
    runtime
        .sql()
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                upsert_on_writer(writer, &path, mtime, hash.as_deref(), synced).await?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })
        }))
        .await
        .expect("cursor commit");
}

#[tokio::test]
async fn fresh_registry_initialization_installs_exact_four_columns() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
        let mut reader = runtime.sql().reader().await.unwrap();
        let absent = reader
            .query_row(SqlStatement::new(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                vec![SqlValue::Text("workspace_mirror_cursor".into())],
            ))
            .await
            .unwrap();
        assert!(
            absent.is_none(),
            "fixture must start before workspace schema initialization"
        );
        drop(reader);
        initialize(&runtime);
        let mut reader = runtime.sql().reader().await.unwrap();
        let columns = reader
            .query_all(SqlStatement::new(
                "PRAGMA table_info(workspace_mirror_cursor)",
                vec![],
            ))
            .await
            .unwrap();
        let shape: Vec<_> = columns
            .iter()
            .map(|column| {
                (
                    column.text("name").unwrap(),
                    column.text("type").unwrap(),
                    column.i64("notnull").unwrap(),
                    column.i64("pk").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("path", "TEXT", 0, 1),
                ("last_mtime", "INTEGER", 0, 0),
                ("last_hash", "TEXT", 0, 0),
                ("last_synced_at", "INTEGER", 0, 0),
            ],
            "cursor schema must retain exactly the accepted four-column shape"
        );
        assert!(list(&mut *reader).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn existing_store_and_cursor_rows_survive_initialization_and_reapplication() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
        // Model an already initialized database that predates the workspace table.
        // This is not a claim of opening a historical core-version fixture.
        let mut writer = runtime.sql().writer().await.unwrap();
        writer
            .execute_script("CREATE TABLE cursor_prior_fixture (value TEXT);".into())
            .await
            .unwrap();
        writer
            .execute(SqlStatement::new(
                "INSERT INTO cursor_prior_fixture(value) VALUES (?1)",
                vec![SqlValue::Text("existing row".into())],
            ))
            .await
            .unwrap();
        drop(writer);
        initialize(&runtime);
        put(
            &runtime,
            "prior/path",
            Some(-7),
            Some("opaque hash"),
            Some(0),
        )
        .await;
        let before = rows(&runtime).await;
        initialize(&runtime);
        assert_eq!(
            rows(&runtime).await,
            before,
            "idempotent initialization erased cursor data"
        );
        let mut reader = runtime.sql().reader().await.unwrap();
        let prior = reader
            .query_all(SqlStatement::new(
                "SELECT value FROM cursor_prior_fixture",
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].text("value").unwrap(), "existing row");
    })
    .await;
}

#[tokio::test]
async fn exact_path_replacement_preserves_other_paths_and_opaque_nullable_values() {
    bounded(async {
        let runtime = fixture();
        assert_eq!(read(&runtime, "a/../b").await, None);
        put(
            &runtime,
            "a/../b",
            Some(i64::MIN),
            Some("not-a-generated-hash"),
            Some(-1),
        )
        .await;
        put(&runtime, "b", Some(0), Some(""), Some(i64::MAX)).await;
        let other = read(&runtime, "b").await.unwrap();
        assert_eq!(other.last_mtime, Some(0));
        assert_eq!(other.last_hash.as_deref(), Some(""));
        assert_eq!(other.last_synced_at, Some(i64::MAX));
        let first = read(&runtime, "a/../b").await.unwrap();
        assert_eq!(first.last_mtime, Some(i64::MIN));
        assert_eq!(first.last_hash.as_deref(), Some("not-a-generated-hash"));
        assert_eq!(first.last_synced_at, Some(-1));
        put(&runtime, "a/../b", None, None, None).await;
        assert_eq!(
            read(&runtime, "a/../b").await,
            Some(CursorRow {
                path: Some("a/../b".into()),
                last_mtime: None,
                last_hash: None,
                last_synced_at: None,
            }),
            "replacement must preserve explicit NULL, not coalesce old values"
        );
        assert_eq!(read(&runtime, "b").await, Some(other.clone()));
        let all = rows(&runtime).await;
        assert_eq!(
            all.len(),
            2,
            "replace must not insert another row or normalize paths"
        );
        assert_eq!(all[0].path.as_deref(), Some("a/../b"));
        assert_eq!(all[1], other);
    })
    .await;
}

#[tokio::test]
async fn list_retains_schema_permitted_null_path_rows() {
    bounded(async {
        let runtime = fixture();
        runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement::new(
                            "INSERT INTO workspace_mirror_cursor \
                             (path, last_mtime, last_hash, last_synced_at) VALUES (?1, ?2, ?3, ?4)",
                            vec![
                                SqlValue::Null,
                                SqlValue::Integer(-8),
                                SqlValue::Text("".into()),
                                SqlValue::Null,
                            ],
                        ))
                        .await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await
            .unwrap();
        assert_eq!(
            rows(&runtime).await,
            vec![CursorRow {
                path: None,
                last_mtime: Some(-8),
                last_hash: Some("".into()),
                last_synced_at: None,
            }]
        );
        assert_eq!(
            read(&runtime, "").await,
            None,
            "NULL path must not become empty text"
        );
    })
    .await;
}

#[tokio::test]
async fn atomic_error_restores_existing_cursor_and_removes_new_cursor() {
    bounded(async {
        let runtime = fixture();
        put(&runtime, "existing", Some(1), Some("before"), Some(2)).await;
        put(&runtime, "unrelated", Some(3), Some("other"), Some(4)).await;
        let before = rows(&runtime).await;
        let error = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "existing", Some(9), Some("after"), Some(10)).await?;
                    upsert_on_writer(writer, "new", Some(11), Some("new"), Some(12)).await?;
                    Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: "cursor rollback fixture".into(),
                        message: "abort after both cursor statements".into(),
                    })
                })
            }))
            .await
            .expect_err("deliberate transaction failure must propagate");
        assert!(error
            .to_string()
            .contains("abort after both cursor statements"));
        assert_eq!(
            rows(&runtime).await,
            before,
            "failed atomic unit advanced progress"
        );
        assert_eq!(read(&runtime, "new").await, None);
    })
    .await;
}

#[tokio::test]
async fn actual_sql_write_failure_rolls_back_cursor_changes() {
    bounded(async {
        let runtime = fixture();
        put(&runtime, "existing", Some(1), Some("before"), Some(2)).await;
        let before = rows(&runtime).await;
        let result = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "existing", Some(99), Some("bad pass"), Some(100))
                        .await?;
                    upsert_on_writer(writer, "new", Some(99), Some("bad pass"), Some(100)).await?;
                    writer
                        .execute(SqlStatement::new(
                            "INSERT INTO cursor_fixture_missing_table(value) VALUES (1)",
                            vec![],
                        ))
                        .await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
        assert!(result.is_err(), "actual failing SQL must fail the unit");
        assert_eq!(rows(&runtime).await, before);
        assert_eq!(read(&runtime, "new").await, None);
    })
    .await;
}

#[tokio::test]
async fn missing_table_errors_are_not_empty_cursor_results() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated uninitialized runtime");
        let mut reader = runtime.sql().reader().await.unwrap();
        assert!(get(&mut *reader, "path").await.is_err());
        assert!(list(&mut *reader).await.is_err());
        drop(reader);
        let result = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "path", None, None, None).await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
        assert!(result.is_err(), "write must not hide missing schema");
    })
    .await;
}

#[tokio::test]
async fn malformed_stored_values_propagate_decode_errors() {
    bounded(async {
        let runtime = fixture();
        let mut writer = runtime.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement::new(
                "INSERT INTO workspace_mirror_cursor(path, last_mtime) VALUES (?1, ?2)",
                vec![
                    SqlValue::Text("malformed".into()),
                    SqlValue::Text("not an integer".into()),
                ],
            ))
            .await
            .unwrap();
        drop(writer);
        let mut reader = runtime.sql().reader().await.unwrap();
        let error = get(&mut *reader, "malformed").await.unwrap_err();
        assert!(error.to_string().contains("last_mtime"));
        assert!(
            list(&mut *reader).await.is_err(),
            "malformed row must not be silently skipped"
        );
    })
    .await;
}

// The config constructor is public from_env only. Each new case runs in its
// own exact-test child with an explicit environment, never process-global mutation.
struct MirrorTree(std::path::PathBuf);
impl MirrorTree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "workspace mirror {}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .expect("private mirror fixture directory");
        Self(path)
    }
    fn write(&self, relative: &str, body: impl AsRef<[u8]>) -> std::path::PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }
}
impl Drop for MirrorTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn mirror_child(test: &str) -> bool {
    const CHILD: &str = "KHIVE_WORKSPACE_MIRROR_TEST_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(test) {
        return false;
    }
    let completion = MirrorTree::new();
    let marker = completion.0.join("completed");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("cursor::tests::{test}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, test)
        .env("KHIVE_WORKSPACE_MIRROR_TEST_DONE", &marker)
        .env("KHIVE_MIRROR_WORKSPACE_POLL_SECS", "2")
        .env_remove("KHIVE_MIRROR_WORKSPACE_INCLUDE")
        .env_remove("KHIVE_MIRROR_WORKSPACE_EXCLUDE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn isolated mirror fixture child");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("observe mirror fixture child") {
            assert!(status.success(), "mirror fixture child failed: {status}");
            assert_eq!(
                std::fs::read_to_string(&marker)
                    .expect("exact child test must complete; zero tests are invalid"),
                test
            );
            return true;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mirror fixture exceeded real-wall deadline; timeout is not acceptance");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn mirror_done(test: &str) {
    std::fs::write(
        std::env::var_os("KHIVE_WORKSPACE_MIRROR_TEST_DONE").expect("child completion path"),
        test,
    )
    .unwrap();
}
async fn mirror_bounded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(45), future)
        .await
        .expect("mirror async fixture deadline");
}
fn mirror_fixture() -> (KhiveRuntime, VerbRegistry, khive_runtime::NamespaceToken) {
    let runtime = KhiveRuntime::from_backend(
        std::sync::Arc::new(khive_runtime::StorageBackend::memory().unwrap()),
        khive_runtime::RuntimeConfig::no_embeddings(),
    );
    initialize(&runtime);
    let registry = registry(&runtime);
    runtime.install_edge_rules(registry.all_edge_rules());
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    (runtime, registry, token)
}
async fn mirror_documents(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
) -> Vec<serde_json::Value> {
    let store = runtime.entities(token).unwrap();
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = store
            .query_entities(
                token.namespace().as_str(),
                khive_storage::EntityFilter {
                    kinds: vec!["document".into()],
                    ..Default::default()
                },
                khive_storage::types::PageRequest { offset, limit: 50 },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        values.extend(
            page.items
                .into_iter()
                .map(|entity| serde_json::to_value(entity).unwrap()),
        );
    }
    values
}
async fn mirror_edges(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
) -> Vec<serde_json::Value> {
    let graph = runtime.graph(token).unwrap();
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = graph
            .query_edges_in_namespaces(
                &[token.namespace().as_str().to_owned()],
                khive_storage::types::EdgeFilter {
                    ids: vec![],
                    source_ids: vec![],
                    target_ids: vec![],
                    relations: vec![khive_types::EdgeRelation::Supersedes],
                    min_weight: None,
                    max_weight: None,
                    created_at: None,
                },
                vec![khive_storage::types::SortOrder {
                    field: khive_storage::types::EdgeSortField::CreatedAt,
                    direction: khive_storage::types::SortDirection::Asc,
                }],
                khive_storage::types::PageRequest { offset, limit: 50 },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        values.extend(
            page.items
                .into_iter()
                .map(|edge| serde_json::to_value(edge).unwrap()),
        );
    }
    values
}
fn mirror_slot(
    tree: &MirrorTree,
    relative: &str,
    token: &khive_runtime::NamespaceToken,
) -> (String, String) {
    let uri =
        crate::mirror::file_uri(&std::fs::canonicalize(&tree.0).unwrap().join(relative)).unwrap();
    let key = crate::mirror::cursor_key(token.namespace().as_str(), &uri);
    (uri, key)
}

async fn mirror_lexical_rows(runtime: &KhiveRuntime) -> (i64, i64) {
    let mut reader = runtime.sql().reader().await.unwrap();
    let row = reader.query_row(SqlStatement::new(
        "SELECT (SELECT COUNT(*) FROM fts_entities) AS documents, (SELECT COUNT(*) FROM fts_entities_rowids) AS mappings",
        vec![],
    )).await.unwrap().unwrap();
    (row.i64("documents").unwrap(), row.i64("mappings").unwrap())
}

fn mirror_no_cursor<'w>(
    _writer: &'w mut dyn khive_storage::SqlWriter,
) -> std::pin::Pin<Box<dyn Future<Output = StorageResult<()>> + Send + 'w>> {
    Box::pin(async { Ok(()) })
}

async fn mirror_script(runtime: &KhiveRuntime, sql: &str) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script(sql.into())
        .await
        .expect("install/remove actual fixture trigger");
}

#[tokio::test]
async fn mirror_selected_files_are_documents_with_searchable_properties() {
    const NAME: &str = "mirror_selected_files_are_documents_with_searchable_properties";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        let tree = MirrorTree::new();
        let fixtures = [
            (
                ".khive/notes/one.md",
                "mirroruniqueword one prose",
                "text/markdown",
            ),
            (
                ".khive/notes/deep/two.md",
                "mirroruniqueword two prose",
                "text/markdown",
            ),
            (
                ".khive/reports/plain.txt",
                "mirroruniqueword plain prose",
                "text/plain",
            ),
            (
                ".khive/workspaces/w/artifacts/report.md",
                "mirroruniqueword report prose",
                "text/markdown",
            ),
        ];
        for (path, body, _) in fixtures {
            tree.write(path, body);
        }
        tree.write(".khive/scripts/skip.md", "not selected");
        tree.write(".khive/kg/schema.yaml", [0xff, 0xfe]);
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("selected file pass must commit");
        assert_eq!(summary.committed, fixtures.len());
        let documents = mirror_documents(&runtime, &token).await;
        assert_eq!(
            documents.len(),
            fixtures.len(),
            "one canonical document per selected file"
        );
        for (relative, body, source_type) in fixtures {
            let (uri, key) = mirror_slot(&tree, relative, &token);
            assert!(
                uri.contains("%20"),
                "space-containing root must have a stable escaped URI"
            );
            let document = documents
                .iter()
                .find(|document| document["properties"]["source_uri"] == uri)
                .expect("selected source URI");
            assert_eq!(document["kind"], "document");
            assert_eq!(document["description"], body);
            assert_eq!(document["properties"]["source_type"], source_type);
            let hash = khive_types::Hash32::from_blake3(body.as_bytes()).to_string();
            assert_eq!(document["properties"]["checksum"], hash);
            assert_eq!(
                read(&runtime, &key)
                    .await
                    .expect("committed document must have a cursor")
                    .last_hash
                    .as_deref(),
                Some(hash.as_str())
            );
        }
        let found = registry
            .dispatch(
                "search",
                serde_json::json!({"kind":"document", "query":"mirroruniqueword", "limit":20}),
            )
            .await
            .expect("ordinary graph search");
        let hits = found
            .as_array()
            .expect("ordinary entity search result array");
        assert_eq!(
            hits.len(),
            fixtures.len(),
            "canonical lexical index must expose every selected document"
        );
        for document in &documents {
            assert!(
                hits.iter().any(|hit| hit["id"] == document["id"]),
                "ordinary search omitted a committed document"
            );
        }
    })
    .await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_secrets_are_masked_without_rejecting_prose() {
    const NAME: &str = "mirror_secrets_are_masked_without_rejecting_prose";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        let tree = MirrorTree::new();
        const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let body = format!("Mirror prose before {SECRET} prose after.");
        assert_ne!(
            khive_runtime::secret_gate::mask_secrets(&body).as_ref(),
            body,
            "synthetic credential must actually activate the canonical masker"
        );
        tree.write(".khive/notes/secret.md", &body);
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("credential file must be accepted after masking");
        assert_eq!(summary.committed, 1);
        let documents = mirror_documents(&runtime, &token).await;
        assert_eq!(documents.len(), 1);
        let description = documents[0]["description"].as_str().unwrap();
        assert!(
            !description.contains(SECRET),
            "stored description retained a credential"
        );
        assert!(description.starts_with("Mirror prose before "));
        assert!(description.ends_with(" prose after."));
        assert_eq!(
            description,
            khive_runtime::secret_gate::mask_secrets(&body).as_ref()
        );
    })
    .await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_versions_noop_and_complete_history_head() {
    const NAME: &str = "mirror_versions_noop_and_complete_history_head";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        let tree = MirrorTree::new();
        let path = ".khive/notes/version.md";
        tree.write(path, "first version prose");
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        assert_eq!(
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .unwrap()
                .committed,
            1
        );
        let old = mirror_documents(&runtime, &token).await.pop().unwrap();
        tree.write(path, "second version prose");
        assert_eq!(
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .unwrap()
                .committed,
            1
        );
        let documents = mirror_documents(&runtime, &token).await;
        assert_eq!(documents.len(), 2);
        assert!(
            documents.iter().any(|document| document == &old),
            "previous version body must remain intact"
        );
        let new = documents
            .iter()
            .find(|document| document["id"] != old["id"])
            .unwrap();
        let edges = mirror_edges(&runtime, &token).await;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["source_id"], new["id"]);
        assert_eq!(edges[0]["target_id"], old["id"]);
        let (_, key) = mirror_slot(&tree, path, &token);
        let before = read(&runtime, &key).await;
        let noop =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .unwrap();
        assert_eq!(
            noop.committed, 0,
            "unchanged pass must not create another version"
        );
        assert_eq!(noop.unchanged, 1);
        assert_eq!(mirror_documents(&runtime, &token).await, documents);
        assert_eq!(mirror_edges(&runtime, &token).await, edges);
        assert_eq!(
            read(&runtime, &key).await,
            before,
            "unchanged pass modified cursor values"
        );

        // An older unlinked head beyond the first page must not be ignored.
        let tree = MirrorTree::new();
        tree.write(path, "ambiguous source must not be versioned");
        let (runtime, registry, token) = mirror_fixture();
        let (uri, key) = mirror_slot(&tree, path, &token);
        let orphan_checksum = khive_types::Hash32::from_blake3(b"orphan history body").to_string();
        let chain_checksum = khive_types::Hash32::from_blake3(b"linked history body").to_string();
        let (orphan_id, effects) = runtime
            .create_workspace_document_with_cursor(
                &registry,
                &token,
                ("old orphan", "orphan history body"),
                (&uri, "text/markdown", &orphan_checksum),
                None,
                mirror_no_cursor,
            )
            .await
            .expect("seed orphan document");
        effects.expect("orphan seed effects");
        let mut previous = None;
        for index in 0..=crate::mirror::HISTORY_PAGE_SIZE {
            let name = format!("chain document {index}");
            let (id, effects) = runtime
                .create_workspace_document_with_cursor(
                    &registry,
                    &token,
                    (&name, "linked history body"),
                    (&uri, "text/markdown", &chain_checksum),
                    previous,
                    mirror_no_cursor,
                )
                .await
                .expect("seed complete history chain");
            effects.expect("chain seed effects");
            previous = Some(id);
        }
        let documents = mirror_documents(&runtime, &token).await;
        assert_eq!(
            documents.len(),
            crate::mirror::HISTORY_PAGE_SIZE as usize + 2,
            "history fixture must exceed the actual lookup page"
        );
        assert!(documents
            .iter()
            .all(|document| document["properties"]["source_uri"] == uri));
        let first_page = runtime
            .entities(&token)
            .unwrap()
            .query_entities(
                token.namespace().as_str(),
                khive_storage::EntityFilter {
                    kinds: vec!["document".into()],
                    ..Default::default()
                }
                .property_eq("$.source_uri", SqlValue::Text(uri.clone())),
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: crate::mirror::HISTORY_PAGE_SIZE,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            first_page.items.len(),
            crate::mirror::HISTORY_PAGE_SIZE as usize
        );
        assert!(
            first_page
                .items
                .iter()
                .all(|entity| entity.id.as_bytes() != orphan_id.as_bytes()),
            "old orphan must lie beyond the first actual lookup page"
        );
        let edges = mirror_edges(&runtime, &token).await;
        assert_eq!(edges.len(), crate::mirror::HISTORY_PAGE_SIZE as usize);
        let heads = documents
            .iter()
            .filter(|document| !edges.iter().any(|edge| edge["target_id"] == document["id"]))
            .count();
        assert_eq!(
            heads, 2,
            "history fixture must have two genuine unlinked heads"
        );
        assert_eq!(read(&runtime, &key).await, None);
        let error =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect_err("complete history must refuse ambiguous heads");
        assert!(error.to_string().contains("no unique current document"));
        assert_eq!(mirror_documents(&runtime, &token).await, documents);
        assert_eq!(read(&runtime, &key).await, None);
    })
    .await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_fts_and_cursor_tail_failure_roll_back_and_retry() {
    const NAME: &str = "mirror_fts_and_cursor_tail_failure_roll_back_and_retry";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        for fault in ["fts", "cursor", "fence"] {
            let tree = MirrorTree::new();
            let path = ".khive/notes/failure.md";
            tree.write(path, "before failure prose");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            assert_eq!(crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config).await.expect("positive caller baseline before fault").committed, 1);
            let documents = mirror_documents(&runtime, &token).await;
            let lexical = mirror_lexical_rows(&runtime).await;
            assert_eq!(lexical, (1, 1), "positive baseline must have both lexical rows");
            let (_, key) = mirror_slot(&tree, path, &token);
            let before = read(&runtime, &key).await;
            let (sql, marker) = match fault {
                "fts" => ("CREATE TRIGGER mirror_fault BEFORE INSERT ON fts_entities_rowids BEGIN SELECT RAISE(ABORT, 'workspace-mirror-fts-fault'); END;".to_owned(), "workspace-mirror-fts-fault"),
                "cursor" => ("CREATE TRIGGER mirror_fault BEFORE INSERT ON workspace_mirror_cursor BEGIN SELECT RAISE(ABORT, 'workspace-mirror-cursor-fault'); END;".to_owned(), "workspace-mirror-cursor-fault"),
                _ => (format!("CREATE TRIGGER mirror_fault AFTER INSERT ON entities WHEN NEW.kind='document' BEGIN UPDATE workspace_mirror_cursor SET last_hash='moved-by-fixture' WHERE path='{}'; END;", key.replace('\'', "''")), "cursor changed before commit"),
            };
            mirror_script(&runtime, &sql).await;
            tree.write(path, "after failure prose");
            let error = crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config).await.expect_err("required file write/fence failure must propagate");
            assert!(error.to_string().contains(marker), "wrong failure stage: {error}");
            if fault == "fts" {
                assert!(
                    matches!(error.runtime_error(), Some(khive_runtime::RuntimeError::Storage(_))),
                    "FTS_ABORT_PRESERVES_NATIVE_STORAGE_ERROR"
                );
            }
            assert_eq!(error.commit_state, crate::mirror::FileCommitState::Unconfirmed);
            assert_eq!(read(&runtime, &key).await, before, "failed file advanced its cursor");
            assert_eq!(mirror_documents(&runtime, &token).await, documents, "failed file left a partial document");
            assert_eq!(mirror_lexical_rows(&runtime).await, lexical, "failed file left a partial FTS or rowid-map write");
            assert!(mirror_edges(&runtime, &token).await.is_empty(), "failed file left a partial supersedes edge");
            mirror_script(&runtime, "DROP TRIGGER mirror_fault;").await;
            assert_eq!(crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config).await.expect("retry after removing fixture fault").committed, 1);
            assert_eq!(mirror_documents(&runtime, &token).await.len(), 2, "retry must add exactly one version");
            assert_eq!(mirror_lexical_rows(&runtime).await, (2, 2));
            assert_eq!(mirror_edges(&runtime, &token).await.len(), 1);
        }
    }).await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_document_failure_preserves_existing_and_absent_cursors() {
    const NAME: &str = "mirror_document_failure_preserves_existing_and_absent_cursors";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        for existing in [false, true] {
            let tree = MirrorTree::new();
            tree.write(".khive/notes/ready.md", "positive baseline body");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            assert_eq!(crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config).await.expect("positive baseline before document fault").committed, 1);
            let path = if existing { ".khive/notes/ready.md" } else { ".khive/notes/new.md" };
            tree.write(path, "changed or new document body");
            let (_, key) = mirror_slot(&tree, path, &token);
            let before = read(&runtime, &key).await;
            assert_eq!(before.is_some(), existing, "cursor fixture premise");
            let documents = mirror_documents(&runtime, &token).await;
            mirror_script(&runtime, "CREATE TRIGGER mirror_document_fault BEFORE INSERT ON entities WHEN NEW.kind='document' BEGIN SELECT RAISE(ABORT, 'workspace-mirror-doc-fault'); END;").await;
            let error = crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config).await.expect_err("actual document insert must fail");
            assert!(error.to_string().contains("workspace-mirror-doc-fault"), "wrong failure stage: {error}");
            assert_eq!(read(&runtime, &key).await, before, "document failure changed existing/absent progress");
            assert_eq!(mirror_documents(&runtime, &token).await, documents);
            assert!(mirror_edges(&runtime, &token).await.is_empty());
        }
    }).await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_secondary_backend_is_refused_before_tree_io() {
    const NAME: &str = "mirror_secondary_backend_is_refused_before_tree_io";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        use std::sync::Arc;
        use khive_runtime::{BackendId, RuntimeConfig, StorageBackend};
        let main = Arc::new(StorageBackend::memory().unwrap());
        let main_runtime = KhiveRuntime::from_backend(Arc::clone(&main), RuntimeConfig::no_embeddings());
        initialize(&main_runtime);
        let auxiliary = Arc::new(StorageBackend::memory().unwrap());
        let secondary = KhiveRuntime::from_backend(auxiliary, RuntimeConfig { backend_id: BackendId::parse("mirror-secondary").unwrap(), ..RuntimeConfig::no_embeddings() }).with_core_backend(main);
        initialize(&secondary);
        let registry = registry(&secondary);
        let token = secondary.authorize(khive_runtime::Namespace::local()).unwrap();
        let tree = MirrorTree::new();
        let missing = tree.0.join("root-does-not-exist");
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let error = crate::mirror::mirror_directory_once(&secondary, &registry, &token, &missing, &config).await.expect_err("secondary backend must be refused before tree IO");
        assert!(matches!(error.runtime_error(), Some(khive_runtime::RuntimeError::InvalidInput(message)) if message.contains("mirror-secondary")), "must retain typed assigned-backend refusal: {error}");
        assert_eq!(error.commit_state, crate::mirror::FileCommitState::NotAttempted);
        assert_eq!(error.progress, crate::mirror::MirrorPassSummary::default());
        assert!(mirror_documents(&secondary, &token).await.is_empty());
        let main_token = main_runtime.authorize(khive_runtime::Namespace::local()).unwrap();
        assert!(mirror_documents(&main_runtime, &main_token).await.is_empty());
        let uri = crate::mirror::file_uri(&missing.join(".khive/notes/new.md")).unwrap();
        let key = crate::mirror::cursor_key(token.namespace().as_str(), &uri);
        assert_eq!(read(&secondary, &key).await, None, "assigned-backend cursor must remain absent");
        assert!(rows(&secondary).await.is_empty());
    }).await;
    mirror_done(NAME);
}
