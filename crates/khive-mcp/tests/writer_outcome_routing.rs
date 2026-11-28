//! Real writer reply loss through MCP request envelopes, with independent disk reads.
//! The caller stays alive: only the exact matched writer request's sender is suppressed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use khive_db::pool::RuntimeWriteOperation;
use khive_db::test_hooks::{CommitReplySuppressionProbe, TestHooks};
use khive_db::{ConnectionPool, PoolConfig, SqliteError, StorageBackend, WriterTaskHandle};
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{StorageCapability, StorageError};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

const TARGET: &str = "outcome:target";
const HEALTHY: &str = "outcome:healthy";
const PAYLOAD: &str = "persisted domain payload";
const MARKER: &str = "committed literal witness";

fn driver(error: rusqlite::Error) -> StorageError {
    StorageError::driver(StorageCapability::Sql, "writer_outcome_fixture", error)
}

fn independent_reader(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("independent read-only connection to the real database")
}

async fn request(server: &KhiveMcpServer, ops: String) -> Value {
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        server.dispatch_request_local(RequestParams {
            ops,
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("live request must receive a bounded response, not be cancelled")
    .expect("valid request produces its actual operation envelope");
    serde_json::from_str(&response).expect("actual MCP response JSON")
}

fn create(key: &str) -> String {
    format!(
        "create(kind=\"note\",note_kind=\"observation\",key={},content={},skip_dedup_check=true)",
        json!(key),
        json!(PAYLOAD)
    )
}

struct Fixture {
    server: KhiveMcpServer,
    writer: WriterTaskHandle,
    path: PathBuf,
    _runtime: KhiveRuntime,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let path = directory.path().join("domain.db");
        let locks = directory.path().join("volume-locks");
        std::fs::create_dir_all(&locks).expect("private lease directory");
        let backend = Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode_in(
                &path,
                true,
                Duration::from_secs(1),
                locks,
            )
            .expect("explicitly queued private file backend"),
        );
        backend.prepare_core_schema().expect("valid core schema");
        let runtime = KhiveRuntime::from_backend(
            Arc::clone(&backend),
            RuntimeConfig {
                db_path: Some(path.clone()),
                actor_id: Some("actor:writer-outcome".into()),
                events_split: None,
                brain_profile: None,
                ..RuntimeConfig::no_embeddings()
            },
        );
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("actor:writer-outcome".into()));
        builder.with_default_namespace("local");
        builder.register_trusted(KgPack::new(runtime.clone()));
        let registry = builder.build().expect("actual KG registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), &backend)
            .expect("actual registered schema");
        let writer = backend
            .pool()
            .writer_task_handle()
            .expect("writer lookup")
            .expect("fixture explicitly enables its writer");
        writer
            .send_top_level(|connection| {
                // A literal marker avoids guessing the generated note's hidden rowid.
                // The exact-key trigger runs in the real domain transaction, not in
                // a reply/projection callback or a second transaction.
                connection
                    .execute_batch(
                        "CREATE TABLE writer_outcome_probe (
                            id INTEGER PRIMARY KEY, literal TEXT NOT NULL
                         );
                         CREATE TRIGGER writer_outcome_note_marker AFTER INSERT ON notes
                         WHEN NEW.key = 'outcome:target'
                         BEGIN
                            INSERT INTO writer_outcome_probe(id, literal)
                            VALUES (1, 'committed literal witness');
                         END;",
                    )
                    .map_err(driver)
            })
            .await
            .expect("fixture setup completes before arming suppression");
        Self {
            server: KhiveMcpServer::from_registry(registry),
            writer,
            path,
            _runtime: runtime,
            _directory: directory,
        }
    }

    async fn arm(&self) -> CommitReplySuppressionProbe {
        self.writer
            .send_top_level(|connection| {
                Ok(connection.test_suppress_commit_reply_after_update("writer_outcome_probe", 1))
            })
            .await
            .expect("arm the exact connection/table/INSERT/rowid matcher")
    }

    fn assert_note(&self, key: &str, kind: &str, content: &str) {
        let reader = independent_reader(&self.path);
        let mut statement = reader
            .prepare("SELECT kind, content FROM notes WHERE namespace = 'local' AND key = ?1")
            .expect("independent note query");
        let rows = statement
            .query_map([key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(kind.to_owned(), content.to_owned())],
            "the actual domain write must be independently committed exactly once"
        );
    }

    fn assert_marker(&self) {
        let reader = independent_reader(&self.path);
        let rows: (i64, String) = reader
            .query_row(
                "SELECT count(*), min(literal) FROM writer_outcome_probe WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("independent literal marker query");
        assert_eq!(
            rows,
            (1, MARKER.into()),
            "real committed marker, not an acknowledgement inference"
        );
    }

    fn assert_suppression(&self, probe: &CommitReplySuppressionProbe) {
        self.assert_marker();
        let observed = probe.snapshot();
        assert_eq!(
            observed.matched_requests, 1,
            "only the intended writer request matched"
        );
        assert!(observed.commit_issued, "production COMMIT was issued");
        assert!(observed.commit_succeeded, "production COMMIT succeeded");
        assert!(
            observed.autocommit,
            "writer observed settled autocommit before reply loss"
        );
        assert_eq!(
            observed.reply_suppressed, 1,
            "only that committed request lost its reply"
        );
    }
}

fn assert_unknown(entry: &Value) {
    assert_eq!(
        entry["ok"], false,
        "actual operation failed to acknowledge: {entry}"
    );
    assert_eq!(
        entry["error"]["code"], "write_outcome_unknown",
        "issued COMMIT reply loss keeps its own public code: {entry}"
    );
    assert_eq!(
        entry["error"]["retryable"], false,
        "ordinary uncertain writes must not be retried: {entry}"
    );
    assert_eq!(entry["error"]["request_state"], "side_effects_unknown");
    assert_eq!(entry["error"]["task_terminated"], false);
    assert_eq!(
        entry["domain_disposition"], "unknown",
        "operation disposition: {entry}"
    );
    assert_eq!(
        entry["error"]["domain_disposition"], "unknown",
        "typed error disposition: {entry}"
    );
    assert!(
        entry.get("result").is_none(),
        "no fabricated success result: {entry}"
    );
    assert!(
        entry["error"].get("domain_result").is_none(),
        "no fabricated domain result: {entry}"
    );
}

#[tokio::test]
async fn committed_note_lost_reply_is_unknown_in_single_mcp_request() {
    let fixture = Fixture::new().await;
    let probe = fixture.arm().await;
    let body = request(&fixture.server, create(TARGET)).await;
    fixture.assert_note(TARGET, "observation", PAYLOAD);
    fixture.assert_suppression(&probe);
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_unknown(&body["results"][0]);
}

#[tokio::test]
async fn committed_note_lost_reply_is_local_to_parallel_mcp_member() {
    let fixture = Fixture::new().await;
    let probe = fixture.arm().await;
    let body = request(
        &fixture.server,
        format!("[{},{}]", create(TARGET), create(HEALTHY)),
    )
    .await;
    fixture.assert_note(TARGET, "observation", PAYLOAD);
    fixture.assert_note(HEALTHY, "observation", PAYLOAD);
    fixture.assert_suppression(&probe);
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_unknown(&body["results"][0]);
    assert_eq!(
        body["results"][1]["ok"], true,
        "healthy sibling keeps its acknowledgement: {body}"
    );
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["summary"]["failed"], 1);
}

#[tokio::test]
async fn committed_note_lost_reply_preserves_prior_chain_success() {
    let fixture = Fixture::new().await;
    let probe = fixture.arm().await;
    let body = request(
        &fixture.server,
        format!("{} | {}", create(HEALTHY), create(TARGET)),
    )
    .await;
    fixture.assert_note(HEALTHY, "observation", PAYLOAD);
    fixture.assert_note(TARGET, "observation", PAYLOAD);
    fixture.assert_suppression(&probe);
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["results"][0]["ok"], true,
        "earlier chain result remains successful: {body}"
    );
    assert_unknown(&body["results"][1]);
}

#[tokio::test]
async fn committed_atomic_stream_unit_lost_reply_is_unknown_not_rolled_back() {
    let fixture = Fixture::new().await;
    let probe = fixture.arm().await;
    let doc = json!({"payload": PAYLOAD});
    let ops = json!([
        {"op":"write", "kind":"head", "key":TARGET, "doc":doc, "embed":false},
        {"op":"write", "kind":"head", "key":HEALTHY, "doc":doc, "embed":false}
    ]);
    let body = request(
        &fixture.server,
        format!("stream.batch(atomic=true,ops={ops})"),
    )
    .await;
    let content = serde_json::to_string(&doc).unwrap();
    fixture.assert_note(TARGET, "head", &content);
    fixture.assert_note(HEALTHY, "head", &content);
    fixture.assert_suppression(&probe);
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_unknown(&body["results"][0]);
}

#[tokio::test]
async fn unarmed_writer_keeps_normal_mcp_success_and_stored_note() {
    let fixture = Fixture::new().await;
    let body = request(&fixture.server, create(TARGET)).await;
    fixture.assert_note(TARGET, "observation", PAYLOAD);
    fixture.assert_marker();
    assert_eq!(
        body["results"][0]["ok"], true,
        "unarmed successful writer is unchanged: {body}"
    );
    assert_eq!(body["results"][0]["result"]["created"], true);
}

#[derive(Debug)]
struct FullEvidence {
    codes: (i32, i32),
    began: bool,
    auto_rolled_back: bool,
}

struct FullPack {
    writer: WriterTaskHandle,
    observed: Arc<Mutex<Option<FullEvidence>>>,
}

impl Pack for FullPack {
    const NAME: &'static str = "writer_outcome";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "writer_outcome.native_full",
        description: "write through the actual writer with a bounded SQLite page limit",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for FullPack {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        assert_eq!(verb, "writer_outcome.native_full");
        let observed = Arc::clone(&self.observed);
        self.writer
            .send(move |connection| {
                let began = !connection.is_autocommit();
                match connection.execute(
                    "INSERT INTO native_payload(id, bytes) VALUES (2, zeroblob(1048576))",
                    [],
                ) {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        if let rusqlite::Error::SqliteFailure(code, _) = &error {
                            if code.code == rusqlite::ErrorCode::DiskFull {
                                *observed.lock().unwrap() = Some(FullEvidence {
                                    codes: (code.extended_code & 0xff, code.extended_code),
                                    began,
                                    auto_rolled_back: connection.is_autocommit(),
                                });
                            }
                        }
                        Err(driver(error))
                    }
                }
            })
            .await?;
        Ok(json!({"unexpected_success":true}))
    }
}

#[tokio::test]
async fn actual_native_full_before_commit_keeps_public_precedence_and_no_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("full.db");
    let locks = directory.path().join("volume-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
        &path,
        true,
        Duration::from_secs(1),
        locks,
    )
    .unwrap();
    let writer = backend.pool().writer_task_handle().unwrap().unwrap();
    let (pages, cap) = writer.send_top_level(|connection| {
        connection.execute_batch("CREATE TABLE native_payload(id INTEGER PRIMARY KEY, bytes BLOB NOT NULL); INSERT INTO native_payload VALUES (1, X'616263');").map_err(driver)?;
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0)).map_err(driver)?;
        let cap: i64 = connection.query_row(&format!("PRAGMA max_page_count = {}", pages + 1), [], |row| row.get(0)).map_err(driver)?;
        Ok((pages, cap))
    }).await.expect("configure actual writer page limit before dispatch");
    assert_eq!(cap, pages + 1);
    let observed = Arc::new(Mutex::new(None));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(FullPack {
        writer,
        observed: Arc::clone(&observed),
    });
    let server = KhiveMcpServer::from_registry(builder.build().unwrap());
    let body = request(&server, "writer_outcome.native_full()".into()).await;
    let observed = observed.lock().unwrap();
    let observed = observed
        .as_ref()
        .expect("actual native FULL, not a setup or fabricated failure");
    assert!(observed.began);
    assert!(observed.auto_rolled_back);
    assert_eq!(observed.codes.0, rusqlite::ffi::SQLITE_FULL);
    let reader = independent_reader(&path);
    let rows: (i64, Vec<u8>) = reader
        .query_row(
            "SELECT count(*), min(bytes) FROM native_payload",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        rows,
        (1, b"abc".to_vec()),
        "failed pre-COMMIT statement leaves the literal baseline only"
    );
    let entry = &body["results"][0];
    assert_eq!(entry["ok"], false);
    assert_eq!(
        entry["error"]["code"], "sqlite_disk_full",
        "native FULL outranks generic writer uncertainty: {entry}"
    );
    assert_eq!(entry["error"]["sqlite_primary_code"], observed.codes.0);
    assert_eq!(entry["error"]["sqlite_extended_code"], observed.codes.1);
    assert_eq!(
        entry["error"]["sqlite_write_stage"],
        "sqlite_statement_failure"
    );
    assert_eq!(entry["error"]["retryable"], false);
    assert_eq!(entry["domain_disposition"], "unknown");
    assert_eq!(entry["error"]["domain_disposition"], "unknown");
    assert_ne!(
        entry["error"]["code"], "write_outcome_unknown",
        "no COMMIT was issued for the failed body"
    );
}

#[derive(Clone, Copy)]
enum Route {
    MissingOwner,
    RawTransaction,
    RawAutocommit,
}

#[derive(Debug)]
struct OwnerCause {
    has_source: bool,
    pool_source: bool,
    source_retryable: bool,
}

struct RoutingPack {
    pool: Arc<ConnectionPool>,
    route: Route,
    callbacks: Arc<AtomicUsize>,
    cause: Arc<Mutex<Option<OwnerCause>>>,
}

impl Pack for RoutingPack {
    const NAME: &'static str = "writer_routing";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "writer_routing.attempt",
        description: "attempt one real strict writer route",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for RoutingPack {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        assert_eq!(verb, "writer_routing.attempt");
        let callbacks = Arc::clone(&self.callbacks);
        let write = move |connection: &Connection| -> Result<(), SqliteError> {
            callbacks.fetch_add(1, Ordering::SeqCst);
            connection.execute(
                "INSERT INTO routing_payload VALUES (2, 'forbidden write')",
                [],
            )?;
            Ok(())
        };
        match self.route {
            Route::MissingOwner => {
                let resolved = self
                    .pool
                    .writer_task_for_runtime_write(RuntimeWriteOperation::MergeEntity);
                if let Err(error @ StorageError::WriterOwnerUnavailable { source, .. }) = &resolved
                {
                    *self.cause.lock().unwrap() = Some(OwnerCause {
                        has_source: std::error::Error::source(error).is_some(),
                        pool_source: matches!(source.as_ref(), StorageError::Pool { .. }),
                        source_retryable: source.is_retryable(),
                    });
                }
                match resolved? {
                    Some(writer) => {
                        writer
                            .send(move |connection| {
                                write(connection).map_err(|error| {
                                    error.into_storage_error(
                                        StorageCapability::Sql,
                                        "routing_fixture",
                                    )
                                })
                            })
                            .await?
                    }
                    None => self.pool.writer()?.autocommit(write)?,
                }
            }
            Route::RawTransaction => self.pool.writer()?.transaction(write)?,
            Route::RawAutocommit => self.pool.writer()?.autocommit(write)?,
        }
        Ok(json!({"unexpected_success":true}))
    }
}

async fn routing_refusal(route: Route) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("routing.db");
    let locks = directory.path().join("volume-locks");
    std::fs::create_dir_all(&locks).unwrap();
    // Setup predates construction of the strict pool; no public strict bypass
    // or lifecycle permission is invented to seed the fixture.
    let setup = Connection::open(&path).unwrap();
    setup.execute_batch("CREATE TABLE routing_payload(id INTEGER PRIMARY KEY, literal TEXT NOT NULL); INSERT INTO routing_payload VALUES (1, 'stored baseline');").unwrap();
    setup.close().unwrap();
    let enabled = !matches!(route, Route::MissingOwner);
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(enabled),
            write_routing_strict: true,
            volume_lock_dir: Some(locks),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    if enabled {
        pool.writer_task_handle()
            .unwrap()
            .expect("live strict owner")
            .send(|connection| {
                connection
                    .execute(
                        "UPDATE routing_payload SET literal = 'queued baseline' WHERE id = 1",
                        [],
                    )
                    .map_err(driver)?;
                Ok(())
            })
            .await
            .expect("real queued write remains admitted under strict routing");
    }
    let callbacks = Arc::new(AtomicUsize::new(0));
    let cause = Arc::new(Mutex::new(None));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RoutingPack {
        pool,
        route,
        callbacks: Arc::clone(&callbacks),
        cause: Arc::clone(&cause),
    });
    let server = KhiveMcpServer::from_registry(builder.build().unwrap());
    let body = request(&server, "writer_routing.attempt()".into()).await;
    let entry = &body["results"][0];
    let code = if enabled {
        "writer_route_violation"
    } else {
        "writer_owner_unavailable"
    };
    assert_eq!(
        entry["ok"], false,
        "operative strict routing must refuse: {body}"
    );
    assert_eq!(
        entry["error"]["code"], code,
        "actual strict route preserves its own public code: {entry}"
    );
    assert_eq!(entry["error"]["request_state"], "not_started");
    assert_eq!(entry["error"]["task_terminated"], false);
    assert_eq!(entry["error"]["retryable"], !enabled);
    assert_eq!(entry["domain_disposition"], "not_committed");
    assert_eq!(entry["error"]["domain_disposition"], "not_committed");
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "strict refusal precedes the real mutation callback"
    );
    let reader = independent_reader(&path);
    let stored: (i64, String) = reader
        .query_row(
            "SELECT count(*), min(literal) FROM routing_payload",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            1,
            if enabled {
                "queued baseline"
            } else {
                "stored baseline"
            }
            .into()
        ),
        "strict refusal leaves the independent stored baseline unchanged"
    );
    if !enabled {
        let cause = cause.lock().unwrap();
        let cause = cause
            .as_ref()
            .expect("real unavailable-owner typed cause retained");
        assert!(
            cause.has_source && cause.pool_source && cause.source_retryable,
            "original Pool source and retry policy: {cause:?}"
        );
    }
}

#[tokio::test]
async fn strict_missing_writer_owner_has_public_code_and_no_effects() {
    routing_refusal(Route::MissingOwner).await;
}

#[tokio::test]
async fn strict_raw_transaction_has_public_route_violation_and_no_effects() {
    routing_refusal(Route::RawTransaction).await;
}

#[tokio::test]
async fn strict_raw_autocommit_has_public_route_violation_and_no_effects() {
    routing_refusal(Route::RawAutocommit).await;
}
