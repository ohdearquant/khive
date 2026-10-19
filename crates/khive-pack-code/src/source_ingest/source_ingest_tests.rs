use super::*;
use khive_db::StorageBackend;
use khive_runtime::{Namespace, RuntimeConfig};
use tempfile::TempDir;

#[tokio::test(flavor = "current_thread")]
async fn blocking_worker_yields_the_async_executor() {
    let worker = blocking_io(|| std::thread::sleep(std::time::Duration::from_millis(200)));
    tokio::pin!(worker);
    tokio::select! {
        biased;
        _ = &mut worker => panic!("blocking work completed before the executor timer"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
    }
    worker.await.expect("blocking worker completed");
}

fn runtime_on(db_path: &Path) -> (KhiveRuntime, NamespaceToken) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("target runtime opens");
    let token = runtime.authorize(Namespace::local()).expect("token");
    (runtime, token)
}

#[derive(Clone, Copy)]
pub(super) enum TestJournalMode {
    Wal,
    Delete,
}

impl TestJournalMode {
    fn label(self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Delete => "delete",
        }
    }

    fn wal_mode(self) -> bool {
        matches!(self, Self::Wal)
    }
}

pub(super) fn runtime_on_with_mode(
    db_path: &Path,
    mode: TestJournalMode,
) -> (KhiveRuntime, NamespaceToken) {
    // Lock files beside the database: the fixture's own lease slot, so a
    // test pausing an ingest inside the lease stalls only its own store.
    runtime_on_with_mode_in(db_path, mode, db_path.with_extension("volume-locks"))
}

pub(super) fn runtime_on_with_mode_in(
    db_path: &Path,
    mode: TestJournalMode,
    volume_lock_dir: std::path::PathBuf,
) -> (KhiveRuntime, NamespaceToken) {
    let backend = Arc::new(
        StorageBackend::sqlite_for_test_with_journal_mode_in(
            db_path,
            mode.wal_mode(),
            std::time::Duration::from_secs(5),
            volume_lock_dir,
        )
        .expect("target backend opens"),
    );
    backend.prepare_core_schema().expect("fresh schema");
    let runtime = KhiveRuntime::from_prepared_backend(
        backend,
        RuntimeConfig {
            db_path: Some(db_path.to_path_buf()),
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        },
    )
    .expect("target runtime opens");
    let writer = runtime.backend().pool().writer().expect("writer");
    let (actual_mode, busy_timeout_ms): (String, i64) = writer
        .autocommit(|conn| {
            let actual_mode = conn
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .expect("journal mode");
            let busy_timeout_ms = conn
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .expect("busy timeout");
            Ok((actual_mode, busy_timeout_ms))
        })
        .expect("inspect writer settings");
    assert_eq!(actual_mode.to_ascii_lowercase(), mode.label());
    assert_eq!(busy_timeout_ms, 5_000);
    let token = runtime.authorize(Namespace::local()).expect("token");
    (runtime, token)
}

#[test]
fn rust_l2_safety_guard_ignores_literals_and_comments() {
    let braces = "{".repeat(RUST_L2_MAX_DELIMITER_DEPTH + 1);
    let source = format!(
        "// {braces}\n/* {braces} */\nconst RAW: &str = r#\"{braces}\"#;\nconst QUOTED: &str = \"{braces}\";\nfn valid<'a>(value: &'a str) {{ let _brace = '{{'; let _ = value; }}\n"
    );
    assert!(check_rust_l2_nesting(&source).is_ok());
    assert!(check_rust_l2_nesting("#!/usr/bin/env rust-script ]\nfn valid() {}\n").is_ok());

    let nested = format!("{}0{}", "(".repeat(65), ")".repeat(65));
    assert!(check_rust_l2_nesting(&nested)
        .unwrap_err()
        .contains("scanner safety limit"));
    let generic = format!("type Deep = {}u8{};", "Vec<".repeat(65), ">".repeat(65));
    assert!(check_rust_l2_nesting(&generic)
        .unwrap_err()
        .contains("generic nesting"));
    let unary = format!("fn f() {{ let _ = {}true; }}", "!".repeat(129));
    assert!(check_rust_l2_nesting(&unary)
        .unwrap_err()
        .contains("expression is too complex"));
}

#[test]
fn rust_l2_safety_guard_accepts_large_flat_item() {
    let mut source = String::from("pub fn many_statements() {\n");
    for _ in 0..1_024 {
        source.push_str("let _ = 0;\n");
    }
    source.push_str("}\n");

    assert!(check_rust_l2_nesting(&source).is_ok());
}

#[test]
fn rust_l2_oversized_file_is_refused_without_reading_it() {
    let root = TempDir::new().expect("tempdir");
    let path = root.path().join("oversized.rs");
    let source = " ".repeat(RUST_L2_MAX_SOURCE_BYTES + 1);
    fs::write(&path, &source).expect("source file");
    let L2Source::Refused { hash, reason } =
        read_l2_source(&root.path().canonicalize().expect("canonical root"), &path)
            .expect("bounded read")
    else {
        panic!("oversized source must not be retained for parsing");
    };
    assert_eq!(hash, format!("refused:size:{}", source.len()));
    assert!(reason.contains("scanner safety limit"));
}

#[cfg(unix)]
#[test]
fn import_scan_reader_refuses_opened_source_outside_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("ingest root");
    let outside = TempDir::new().expect("outside root");
    let source = outside.path().join("outside.py");
    fs::write(&source, "import must_not_ingest\n").expect("outside source");
    let link = root.path().join("source.py");
    symlink(&source, &link).expect("outside link");
    assert!(matches!(
        scan_import_source(&root.path().canonicalize().expect("canonical root"), &link, "python"),
        Err(SourceReadError::Refused(reason)) if reason.contains("escapes the canonical ingest root")
    ));
}

#[cfg(unix)]
#[test]
fn rust_l2_reader_refuses_opened_source_outside_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("ingest root");
    let outside = TempDir::new().expect("outside root");
    let source = outside.path().join("outside.rs");
    fs::write(&source, "pub fn must_not_ingest() {}\n").expect("outside source");
    let link = root.path().join("source.rs");
    symlink(&source, &link).expect("outside link");
    assert!(matches!(
        read_l2_source(&root.path().canonicalize().expect("canonical root"), &link),
        Err(SourceReadError::Refused(reason)) if reason.contains("escapes the canonical ingest root")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn rust_l2_scanner_runs_off_the_ingest_thread() {
    let root = TempDir::new().expect("tempdir");
    let project = root.path().join("worker_probe");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"worker_probe\"\n",
    )
    .expect("manifest");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn l2_worker_probe_3292() {}\n",
    )
    .expect("source");
    let (runtime, token) = runtime_on(&root.path().join("worker.db"));
    let (send, receive) = std::sync::mpsc::channel();
    *scanner_thread_observer()
        .lock()
        .expect("scanner observer lock") = Some(send);

    let caller = std::thread::current().id();
    let report = run_code_ingest(
        &runtime,
        &token,
        CodeSourceIngestOptions {
            path: &project,
            languages: ["rust"].into_iter().collect(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("L2 ingest");
    let (scanner, name) = receive
        .try_recv()
        .expect("valid Rust source reaches scanner");
    assert_ne!(
        scanner, caller,
        "source parsing must leave the ingest thread"
    );
    assert_eq!(name.as_deref(), Some("khive-rust-l2-scanner"));
    assert_eq!(report.l2.expect("L2 report").symbol_parse_failures, 0);
}

#[test]
fn invalid_declaration_ids_force_reparse() {
    let hash = "0123456789abcdef";

    assert!(l2_needs_reparse(
        Some(hash),
        None,
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
    assert!(l2_needs_reparse(
        Some(hash),
        Some(&Value::Null),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
    assert!(l2_needs_reparse(
        Some(hash),
        Some(&json!("not-an-array")),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
    assert!(l2_needs_reparse(
        Some(hash),
        Some(&json!(["not-a-uuid"])),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
    assert!(!l2_needs_reparse(
        Some(hash),
        Some(&json!([])),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
    assert!(!l2_needs_reparse(
        Some(hash),
        Some(&json!([Uuid::nil().to_string()])),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
}

#[test]
fn old_scanner_identity_stamp_forces_reparse_of_unchanged_file() {
    let hash = "0123456789abcdef";
    let declarations = json!([Uuid::nil().to_string()]);
    assert!(l2_needs_reparse(
        Some(hash),
        Some(&declarations),
        None,
        hash
    ));
    assert!(l2_needs_reparse(
        Some(hash),
        Some(&declarations),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION - 1),
        hash
    ));
    assert!(!l2_needs_reparse(
        Some(hash),
        Some(&declarations),
        Some(RUST_L2_SCANNER_IDENTITY_VERSION),
        hash
    ));
}

#[tokio::test]
async fn stamp_skips_non_object_properties_without_rebuilding() {
    let root = TempDir::new().expect("temporary database directory");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(root.path().join("stamp.db")),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("target runtime opens");
    let token = rt.authorize(Namespace::local()).expect("token");
    let module_id = module_uuid("fixture", "rust", "crate");
    let mut module = Entity::new(token.namespace().as_str(), "concept", "crate")
        .with_entity_type(Some("module"));
    module.id = module_id;
    module.properties = Some(json!("corrupt"));
    let original_properties = module.properties.clone();
    rt.entities(&token)
        .expect("entity store")
        .upsert_entity(module)
        .await
        .expect("direct entity write");

    let mut module_scans = HashMap::new();
    module_scans.insert(
        module_id,
        ModuleScan {
            source_project: "fixture".to_string(),
            imports: Vec::new(),
            source_reads: Vec::new(),
        },
    );
    let mut report = CodeSourceIngestReport::default();
    stamp_import_scan_coverage(&rt, &token, module_scans, &BTreeMap::new(), &mut report)
        .await
        .expect("stamp path completes");

    let stored = rt
        .entities(&token)
        .expect("entity store")
        .get_entity(module_id)
        .await
        .expect("fetch stamped module")
        .expect("module remains present");
    assert_eq!(stored.properties, original_properties);
    assert_eq!(report.coverage_stamps_missed, 1);
    assert!(report.warnings.iter().any(|warning| {
        warning.contains("F2 contract violation") && warning.contains("coverage stamp skipped")
    }));
}

#[tokio::test]
async fn symbol_fts_failure_aborts_without_incrementing_success_counters() {
    let root = TempDir::new().expect("temporary database directory");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(root.path().join("fts-failure.db")),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("target runtime opens");
    let token = rt.authorize(Namespace::local()).expect("token");
    rt.sql()
        .writer()
        .await
        .expect("writer")
        .execute_script(
            "DROP TABLE fts_entities; \
             CREATE TABLE fts_entities (broken_column TEXT);"
                .to_string(),
        )
        .await
        .expect("replace temporary FTS table with an incompatible schema");

    let declaration = ExtractedDeclaration {
        kind: DeclKind::Function,
        name: "symbol".to_string(),
        description: None,
        content_hash: "0123456789abcdef".to_string(),
        calls: Vec::new(),
        module_segments: Vec::new(),
        type_refs: Vec::new(),
    };
    let mut report = CodeSourceIngestReport {
        l2: Some(CodeSourceIngestL2Report::default()),
        ..Default::default()
    };
    let error = upsert_declaration(
        &rt,
        &token,
        "fixture",
        "rust",
        "crate",
        &declaration,
        "src/lib.rs",
        "unversioned",
        Utc::now(),
        Uuid::new_v4(),
        "src/lib.rs",
        &mut report,
    )
    .await
    .expect_err("symbol FTS failure must abort");
    assert!(error.to_string().contains("entity FTS indexing"));
    assert_eq!(report.fts_indexed, 0);
    assert_eq!(report.l2.expect("L2 report").symbols_created, 0);
}

async fn concurrent_unresolved_additions_rebase_without_losing_either_specifier_in_mode(
    mode: TestJournalMode,
) {
    let root = TempDir::new().expect("temporary database directory");
    let db_path = root.path().join(format!("entity-race-{}.db", mode.label()));
    let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
    let entity_id = project_uuid("race-fixture");
    let mut entity = Entity::new(token_a.namespace().as_str(), "project", "race-fixture");
    entity.id = entity_id;
    entity.properties = Some(json!({"source_project": "race-fixture"}));
    let seed_revision = entity.updated_at;
    runtime_a
        .entities(&token_a)
        .expect("entity store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
    let specifier_a = UnresolvedSpec {
        specifier: "alpha".to_string(),
        target_kind: "project".to_string(),
        dependency_kind: "dependencies".to_string(),
        dependency_scope: "normal".to_string(),
        language: "rust".to_string(),
    };
    let specifier_b = UnresolvedSpec {
        specifier: "beta".to_string(),
        target_kind: "project".to_string(),
        dependency_kind: "dev-dependencies".to_string(),
        dependency_scope: "dev".to_string(),
        language: "rust".to_string(),
    };
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let pause_a = std::sync::Arc::new(race_seam::OneShotPause::new(std::sync::Arc::clone(
        &barrier,
    )));
    let pause_b = std::sync::Arc::new(race_seam::OneShotPause::new(barrier));
    let mut report_a = CodeSourceIngestReport::default();
    let mut report_b = CodeSourceIngestReport::default();
    let pending_a = [PendingUnresolved {
        spec: specifier_a.clone(),
        file: "alpha.rs".to_string(),
    }];
    let pending_b = [PendingUnresolved {
        spec: specifier_b.clone(),
        file: "beta.rs".to_string(),
    }];

    let (result_a, result_b) = tokio::join!(
        race_seam::AFTER_ROW_READ.scope(
            pause_a,
            record_unresolved_batch(&runtime_a, &token_a, entity_id, &pending_a, &mut report_a),
        ),
        race_seam::AFTER_ROW_READ.scope(
            pause_b,
            record_unresolved_batch(&runtime_b, &token_b, entity_id, &pending_b, &mut report_b),
        ),
    );
    result_a.expect("writer A completes");
    result_b.expect("writer B completes");

    let stored = runtime_a
        .entities(&token_a)
        .expect("entity store")
        .get_entity(entity_id)
        .await
        .expect("read entity")
        .expect("entity remains");
    let specifiers = stored
        .properties
        .as_ref()
        .map(read_unresolved)
        .unwrap_or_default();
    assert_eq!(
        specifiers.len(),
        2,
        "two additions derived from one entity revision must both survive: {specifiers:?}"
    );
    assert!(specifiers.contains(&specifier_a));
    assert!(specifiers.contains(&specifier_b));
    assert!(stored.updated_at > seed_revision);
    assert_eq!(report_a.unresolved_recorded, 1);
    assert_eq!(report_b.unresolved_recorded, 1);
    assert_eq!(report_a.fts_indexed, 1);
    assert_eq!(report_b.fts_indexed, 1);
}

#[tokio::test]
async fn concurrent_unresolved_additions_rebase_without_losing_either_specifier() {
    for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
        concurrent_unresolved_additions_rebase_without_losing_either_specifier_in_mode(mode).await;
    }
}

#[tokio::test]
async fn unresolved_batch_keeps_order_and_dedup_with_one_owner_write() {
    let root = TempDir::new().expect("temporary database directory");
    let (runtime, token) = runtime_on(&root.path().join("unresolved-batch.db"));
    let entity_id = project_uuid("batch-fixture");
    let existing = UnresolvedSpec {
        specifier: "existing".to_string(),
        target_kind: "project".to_string(),
        dependency_kind: "dependencies".to_string(),
        dependency_scope: "normal".to_string(),
        language: "rust".to_string(),
    };
    let mut entity = Entity::new(token.namespace().as_str(), "project", "batch-fixture");
    entity.id = entity_id;
    entity.properties = Some(json!({
        "source_project": "batch-fixture",
        "unresolved_specifiers": [existing],
    }));
    runtime
        .entities(&token)
        .expect("entity store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let spec = |name: String| UnresolvedSpec {
        specifier: name,
        target_kind: "project".to_string(),
        dependency_kind: "dependencies".to_string(),
        dependency_scope: "normal".to_string(),
        language: "rust".to_string(),
    };
    let mut pending: Vec<_> = (0..64)
        .map(|i| PendingUnresolved {
            spec: spec(format!("missing_{i:02}")),
            file: "Cargo.toml".to_string(),
        })
        .collect();
    pending.insert(
        1,
        PendingUnresolved {
            spec: spec("existing".to_string()),
            file: "Cargo.toml".to_string(),
        },
    );
    pending.push(PendingUnresolved {
        spec: spec("missing_00".to_string()),
        file: "Cargo.toml".to_string(),
    });
    pending.push(PendingUnresolved {
        spec: spec("scheme://user:pass@host".to_string()),
        file: "blocked.toml".to_string(),
    });
    let mut report = CodeSourceIngestReport::default();
    record_unresolved_batch(&runtime, &token, entity_id, &pending, &mut report)
        .await
        .expect("batch appends safe siblings");

    let stored = runtime
        .entities(&token)
        .expect("entity store")
        .get_entity(entity_id)
        .await
        .expect("read entity")
        .expect("entity remains");
    let list = read_unresolved(stored.properties.as_ref().expect("properties"));
    let expected: Vec<_> = std::iter::once(spec("existing".to_string()))
        .chain((0..64).map(|i| spec(format!("missing_{i:02}"))))
        .collect();
    assert_eq!(list, expected, "append order and dedup must be stable");
    assert_eq!(report.unresolved_recorded, 64);
    assert_eq!(report.fts_indexed, 1, "one owner gets one FTS upsert");
    assert_eq!(report.blocked_count, 1);
    assert_eq!(report.blocked[0].file, "blocked.toml");
}

async fn concurrent_dependency_evidence_rebases_without_losing_either_kind_in_mode(
    mode: TestJournalMode,
) {
    let root = TempDir::new().expect("temporary database directory");
    let db_path = root.path().join(format!("edge-race-{}.db", mode.label()));
    let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
    let source_id = project_uuid("source");
    let target_id = project_uuid("target");
    for (id, name) in [(source_id, "source"), (target_id, "target")] {
        let mut entity = Entity::new(token_a.namespace().as_str(), "project", name);
        entity.id = id;
        runtime_a
            .entities(&token_a)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("seed endpoint");
    }
    let seed_time = Utc::now();
    let mut seed_report = CodeSourceIngestReport::default();
    upsert_dependency_edge(
        &runtime_a,
        &token_a,
        source_id,
        target_id,
        "dependencies",
        "normal",
        "rust",
        seed_time,
        &mut seed_report,
    )
    .await
    .expect("seed dependency edge");

    let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let pause_a = std::sync::Arc::new(race_seam::OneShotPause::new(std::sync::Arc::clone(
        &barrier,
    )));
    let pause_b = std::sync::Arc::new(race_seam::OneShotPause::new(barrier));
    let mut report_a = CodeSourceIngestReport::default();
    let mut report_b = CodeSourceIngestReport::default();
    let update_time = seed_time + chrono::Duration::seconds(1);

    let (result_a, result_b) = tokio::join!(
        race_seam::AFTER_ROW_READ.scope(
            pause_a,
            upsert_dependency_edge(
                &runtime_a,
                &token_a,
                source_id,
                target_id,
                "dev-dependencies",
                "dev",
                "rust",
                update_time,
                &mut report_a,
            ),
        ),
        race_seam::AFTER_ROW_READ.scope(
            pause_b,
            upsert_dependency_edge(
                &runtime_b,
                &token_b,
                source_id,
                target_id,
                "build-dependencies",
                "build",
                "rust",
                update_time,
                &mut report_b,
            ),
        ),
    );
    result_a.expect("writer A completes");
    result_b.expect("writer B completes");

    let edge = runtime_a
        .graph(&token_a)
        .expect("graph store")
        .get_edge(LinkId::from(edge_uuid(
            EdgeRelation::DependsOn,
            source_id,
            target_id,
        )))
        .await
        .expect("read dependency edge")
        .expect("dependency edge remains");
    let kinds: BTreeSet<String> = edge
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("dependency_kinds"))
        .and_then(Value::as_array)
        .expect("dependency kinds")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert_eq!(
        kinds,
        [
            "build-dependencies".to_string(),
            "dependencies".to_string(),
            "dev-dependencies".to_string(),
        ]
        .into_iter()
        .collect(),
        "two evidence additions derived from one edge revision must both survive"
    );
    assert_eq!(report_a.edges_updated, 1);
    assert_eq!(report_b.edges_updated, 1);
    assert!(edge.updated_at > update_time);
}

#[tokio::test]
async fn concurrent_dependency_evidence_rebases_without_losing_either_kind() {
    for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
        concurrent_dependency_evidence_rebases_without_losing_either_kind_in_mode(mode).await;
    }
}

#[tokio::test]
async fn concurrent_conditional_inserts_preserve_entity_and_edge_winners() {
    for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
        let root = TempDir::new().expect("temporary database directory");
        let db_path = root.path().join(format!("insert-race-{}.db", mode.label()));
        let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
        let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
        let entity_id = project_uuid("insert-race");
        let mut entity_a = Entity::new("local", "project", "first-candidate");
        entity_a.id = entity_id;
        entity_a.properties = Some(json!({"candidate": "a"}));
        let mut entity_b = entity_a.clone();
        entity_b.name = "second-candidate".to_string();
        entity_b.properties = Some(json!({"candidate": "b"}));

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let entity_store_a = runtime_a.entities(&token_a).expect("entity store A");
        let entity_store_b = runtime_b.entities(&token_b).expect("entity store B");
        let (inserted_a, inserted_b) = tokio::join!(
            async {
                barrier.wait().await;
                entity_store_a
                    .insert_entity_if_absent(entity_a.clone())
                    .await
            },
            async {
                barrier.wait().await;
                entity_store_b
                    .insert_entity_if_absent(entity_b.clone())
                    .await
            },
        );
        let inserted_a = inserted_a.expect("entity insert A");
        let inserted_b = inserted_b.expect("entity insert B");
        assert_ne!(
            inserted_a,
            inserted_b,
            "exactly one entity insert wins in {} mode",
            mode.label()
        );
        let stored_entity = entity_store_a
            .get_entity(entity_id)
            .await
            .expect("read entity")
            .expect("one entity remains");
        let winner = if inserted_a { &entity_a } else { &entity_b };
        assert_eq!(stored_entity.name, winner.name, "{} mode", mode.label());
        assert_eq!(
            stored_entity.properties,
            winner.properties,
            "{} mode",
            mode.label()
        );

        let source_id = project_uuid("insert-source");
        let target_id = project_uuid("insert-target");
        for (id, name) in [(source_id, "insert-source"), (target_id, "insert-target")] {
            let mut endpoint = Entity::new("local", "project", name);
            endpoint.id = id;
            entity_store_a
                .upsert_entity(endpoint)
                .await
                .expect("seed endpoint");
        }
        let now = Utc::now();
        let edge_a = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: "local".to_string(),
            source_id,
            target_id,
            relation: EdgeRelation::DependsOn,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: Some(json!({"candidate": "a"})),
            target_backend: None,
        };
        let edge_b = Edge {
            id: LinkId::from(Uuid::new_v4()),
            weight: 0.25,
            metadata: Some(json!({"candidate": "b"})),
            ..edge_a.clone()
        };
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let edge_store_a = runtime_a.graph(&token_a).expect("edge store A");
        let edge_store_b = runtime_b.graph(&token_b).expect("edge store B");
        let (inserted_a, inserted_b) = tokio::join!(
            async {
                barrier.wait().await;
                edge_store_a.insert_edge_if_absent(edge_a.clone()).await
            },
            async {
                barrier.wait().await;
                edge_store_b.insert_edge_if_absent(edge_b.clone()).await
            },
        );
        let inserted_a = inserted_a.expect("edge insert A");
        let inserted_b = inserted_b.expect("edge insert B");
        assert_ne!(
            inserted_a,
            inserted_b,
            "exactly one natural-key edge insert wins in {} mode",
            mode.label()
        );
        let (winning_edge, losing_edge) = if inserted_a {
            (&edge_a, &edge_b)
        } else {
            (&edge_b, &edge_a)
        };
        let stored_edge = edge_store_a
            .get_edge(winning_edge.id)
            .await
            .expect("read edge")
            .expect("one edge remains");
        assert_eq!(stored_edge.id, winning_edge.id, "{} mode", mode.label());
        assert_eq!(
            stored_edge.weight,
            winning_edge.weight,
            "{} mode",
            mode.label()
        );
        assert_eq!(
            stored_edge.metadata,
            winning_edge.metadata,
            "{} mode",
            mode.label()
        );
        assert!(
            edge_store_a
                .get_edge(losing_edge.id)
                .await
                .expect("read loser")
                .is_none(),
            "losing edge must be absent in {} mode",
            mode.label()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_journal_busy_begin_retries_after_other_runtime_releases_lock() {
    let root = TempDir::new().expect("temporary database directory");
    let db_path = root.path().join("busy-delete.db");
    // The runtimes take different lock directories, so the second one stands
    // in for a writer outside this process's lease and meets the first
    // one's reserved lock at BEGIN, as SQLITE_BUSY.
    let (_runtime_a, _) = runtime_on_with_mode(&db_path, TestJournalMode::Delete);
    let (runtime_b, token_b) = runtime_on_with_mode_in(
        &db_path,
        TestJournalMode::Delete,
        root.path().join("other-writer-locks"),
    );
    let pool_b = runtime_b.backend().pool();
    let writer_task = pool_b
        .writer_task_handle()
        .expect("writer task handle")
        .expect("file-backed writer task");
    writer_task
        .send_top_level(|conn| {
            conn.busy_handler(None)
                .map_err(|error| khive_storage::StorageError::Internal(error.to_string()))
        })
        .await
        .expect("disable SQLite wait on first BEGIN");

    let lock_holder = rusqlite::Connection::open(&db_path).expect("external lock holder");
    lock_holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("reserve rollback-journal writer lock");
    let id = project_uuid("busy-retry");
    let mut report = CodeSourceIngestReport::default();
    let write = mutate_entity(&runtime_b, &token_b, id, "busy.rs", &mut report, |_| {
        let mut entity = Entity::new("local", "project", "busy-retry");
        entity.id = id;
        Some(entity)
    });
    let release = async {
        let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while pool_b.writer_acquisition_snapshot().writer_task_begin_busy == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await;
        lock_holder
            .execute_batch("ROLLBACK")
            .expect("release rollback-journal writer lock");
        observed.expect("the other runtime must observe a real SQLITE_BUSY refusal");
    };
    let (result, ()) = tokio::join!(write, release);
    assert_eq!(
        result.expect("busy BEGIN retries after release"),
        RowMutationOutcome::Created
    );
    assert_eq!(report.fts_indexed, 1);
    let counters = pool_b.writer_acquisition_snapshot();
    assert!(counters.writer_task_begin_busy >= 1);
    assert!(counters.writer_task_begin_busy_absorbed >= 1);
}

fn git_in(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .expect("git command starts");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output is UTF-8")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual A8 WAL/DELETE wall-clock comparison"]
async fn measure_concurrent_code_map_ingest_a8_32_commits() {
    const COMMITS: usize = 32;
    const REPO_NAME: &str = "code-map-a8-32";
    let root = TempDir::new().expect("temporary measurement directory");
    let repo = root.path().join(REPO_NAME);
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("source directory");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"code-map-a8-32\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("stable manifest");
    fs::write(src.join("lib.rs"), "").expect("initial library");
    git_in(&repo, &["init", "-q"]);
    for i in 0..COMMITS {
        fs::write(
            src.join(format!("module_{i:02}.rs")),
            format!("pub fn value_{i:02}() -> usize {{ {i} }}\n"),
        )
        .expect("small module");
        let mut lib = fs::read_to_string(src.join("lib.rs")).expect("read library");
        lib.push_str(&format!("pub mod module_{i:02};\n"));
        fs::write(src.join("lib.rs"), lib).expect("extend library");
        git_in(&repo, &["add", "-A"]);
        let message = format!("add module {i:02}");
        git_in(
            &repo,
            &[
                "-c",
                "user.name=A8 Fixture",
                "-c",
                "user.email=a8@example.invalid",
                "commit",
                "-q",
                "-m",
                &message,
            ],
        );
    }
    assert_eq!(
        git_in(&repo, &["rev-list", "--count", "HEAD"]).trim(),
        COMMITS.to_string()
    );

    for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
        let db_path = root.path().join(format!("map-{}.db", mode.label()));
        let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
        let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let sweep_time = Utc::now();
        let run_a = async {
            barrier.wait().await;
            run_code_ingest(
                &runtime_a,
                &token_a,
                CodeSourceIngestOptions {
                    path: &repo,
                    languages: ["rust"].into_iter().collect(),
                    sweep_time,
                    enable_l1: true,
                    enable_l1_5: true,
                    enable_l2: false,
                },
            )
            .await
        };
        let run_b = async {
            barrier.wait().await;
            run_code_ingest(
                &runtime_b,
                &token_b,
                CodeSourceIngestOptions {
                    path: &repo,
                    languages: ["rust"].into_iter().collect(),
                    sweep_time,
                    enable_l1: true,
                    enable_l1_5: true,
                    enable_l2: false,
                },
            )
            .await
        };
        let started = std::time::Instant::now();
        let (report_a, report_b) = tokio::join!(run_a, run_b);
        let wall = started.elapsed();
        let report_a = report_a.expect("concurrent ingest A");
        let report_b = report_b.expect("concurrent ingest B");
        assert_eq!(report_a.source_revision, report_b.source_revision);
        println!(
            "A8_MEASURE mode={} repo={REPO_NAME} commits={COMMITS} wall_ms={}",
            mode.label(),
            wall.as_millis()
        );
    }
}
