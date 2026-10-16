use super::*;

fn input(path: &str, source: &str) -> SourceReadInput {
    SourceReadInput {
        source_path: path.into(),
        source_revision: "revision-a".into(),
        content_hash: content_hash(source),
        language: "rust".into(),
        sweep_time: "2026-10-16T00:00:00+00:00".into(),
        scan: extract_source_reads("rust", source),
    }
}

fn module(input: SourceReadInput) -> ModuleScan {
    ModuleScan {
        source_project: "fixture".into(),
        imports: Vec::new(),
        source_reads: vec![input],
    }
}

#[test]
fn parent_inputs_are_normalized_but_repository_escape_is_refused() {
    let read = |path: &str| SourceRead {
        kind: "include_str".into(),
        path: path.into(),
        base: "file".into(),
    };
    assert_eq!(
        target_path("crates/reader/src/lib.rs", &read("../../input/src/lib.rs")),
        Some("crates/input/src/lib.rs".into())
    );
    assert_eq!(target_path("lib.rs", &read("../outside.rs")), None);
    for path in [
        "/etc/passwd",
        "C:/file.rs",
        "a\\b.rs",
        "*.rs",
        "a//b.rs",
        "a?b.rs",
    ] {
        assert_eq!(target_path("src/lib.rs", &read(path)), None, "{path}");
    }
}

#[test]
fn resolution_uses_only_unique_modules_from_the_same_invocation() {
    let reader = Uuid::from_u128(1);
    let target = Uuid::from_u128(2);
    let mut scans = HashMap::from([
        (
            reader,
            module(input(
                "src/lib.rs",
                "const S: &str = include_str!(\"input.rs\");",
            )),
        ),
        (
            target,
            module(input("src/input.rs", "pub const VALUE: u8 = 1;")),
        ),
    ]);
    let plans = prepare(&scans);
    assert_eq!(
        plans[&reader].edges[&target].len(),
        1,
        "MUST-FAIL control: disabling source-reader resolution loses this edge"
    );
    scans.get_mut(&target).unwrap().source_reads[0].source_revision = "other".into();
    assert!(prepare(&scans)[&reader].edges.is_empty());
    scans.get_mut(&target).unwrap().source_reads[0].source_revision = "revision-a".into();
    let duplicate = module(input("src/input.rs", "pub const VALUE: u8 = 1;"));
    scans.insert(Uuid::from_u128(3), duplicate);
    assert!(
        prepare(&scans)[&reader].edges.is_empty(),
        "ambiguous ownership cannot resolve"
    );
}

#[test]
fn nonmodule_and_dynamic_inputs_remain_explicitly_unresolved() {
    let reader = Uuid::from_u128(1);
    let scans = HashMap::from([(
        reader,
        module(input(
            "src/lib.rs",
            "const S: &str = include_str!(\"schema.sql\"); include!(concat!(\"a\", \".rs\"));",
        )),
    )]);
    let plans = prepare(&scans);
    let plan = &plans[&reader];
    assert!(plan.edges.is_empty());
    assert_eq!(plan.observation["status"], "lexical");
    assert_eq!(plan.observation["observations"][0]["status"], "unresolved");
    assert!(!plan.observation["unresolved"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn module_fingerprint_requires_path_revision_hash_language_and_sweep() {
    let scan = input("src/lib.rs", "fn f() {}");
    let props = json!({"source_path":scan.source_path,"source_revision":scan.source_revision,
        "content_hash":scan.content_hash,"language":scan.language,"last_seen_at":scan.sweep_time});
    assert!(scan.matches(props.as_object().unwrap()));
    for key in [
        "source_path",
        "source_revision",
        "content_hash",
        "language",
        "last_seen_at",
    ] {
        let mut changed = props.clone();
        changed[key] = json!("different");
        assert!(!scan.matches(changed.as_object().unwrap()), "{key}");
    }
}

#[test]
fn import_refresh_preserves_revision_qualified_reader_evidence() {
    let existing = json!({"dependency_kinds":["source_read"],
        "source_read_observations":[{"reader":{"content_hash":"old"}}]});
    let merged = merge_dependency_metadata(Some(&existing), "import", "build", "rust");
    assert_eq!(
        merged["source_read_observations"],
        existing["source_read_observations"]
    );
    assert_eq!(merged["dependency_kinds"], json!(["import", "source_read"]));
    assert_eq!(merged["dependency_scopes"], json!(["build"]));
}

fn runtime(root: &Path) -> (KhiveRuntime, tempfile::TempDir) {
    let locks = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(root.join("map.db")),
        packs: Vec::new(),
        volume_lock_dir: Some(locks.path().to_path_buf()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    (rt, locks)
}

async fn ingest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    root: &Path,
    now: DateTime<Utc>,
) -> CodeSourceIngestReport {
    run_code_ingest(
        rt,
        token,
        CodeSourceIngestOptions {
            path: root,
            languages: ["rust", "python"].into_iter().collect(),
            sweep_time: now,
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .unwrap()
}

fn fixture(root: &Path) {
    for package in ["reader", "input"] {
        let path = root.join("crates").join(package);
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("crates/input/src/lib.rs"),
        "pub const VALUE: u8 = 1;",
    )
    .unwrap();
    fs::write(
        root.join("crates/reader/src/input.rs"),
        "pub const VALUE: u8 = 2;",
    )
    .unwrap();
    fs::write(
        root.join("crates/reader/src/lib.rs"),
        concat!(
            "use crate::input::VALUE;\n#[path = \"input.rs\"] mod input;\n",
            "const S: &str = include_str!(\"input.rs\");\n",
            "const B: &[u8] = include_bytes!(\"input.rs\");\ninclude!(\"input.rs\");\n",
            "const CROSS: &str = include_str!(\"../../input/src/lib.rs\");\n"
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn source_inputs_and_imports_share_one_edge_and_cross_package_reads_are_recorded() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    fs::write(
        root.path().join("pyproject.toml"),
        "[project]\nname = \"reader_scripts\"\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("scripts")).unwrap();
    fs::write(
        root.path().join("scripts/check.py"),
        "source = 'crates/input/src/lib.rs'\nopen(source).read()\n",
    )
    .unwrap();
    let (rt, _locks) = runtime(root.path());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let report = ingest(&rt, &token, root.path(), Utc::now()).await;
    let counts = report.source_reads.unwrap();
    assert_eq!(counts.coverage_stamps_missed, 0);
    assert_eq!(counts.resolved_reads, 6);
    assert!(
        counts.unresolved_reads > 0,
        "Python remains lexical/unknown"
    );
    let reader = module_uuid("reader", "rust", "crate");
    let target = module_uuid("reader", "rust", "input");
    let edge = rt
        .graph(&token)
        .unwrap()
        .get_edge(LinkId::from(edge_uuid(
            EdgeRelation::DependsOn,
            reader,
            target,
        )))
        .await
        .unwrap()
        .unwrap();
    let metadata = edge.metadata.unwrap();
    assert_eq!(
        metadata["dependency_kinds"],
        json!(["import", "source_read"])
    );
    assert_eq!(
        metadata["source_read_observations"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let cross = module_uuid("input", "rust", "crate");
    assert!(
        rt.graph(&token)
            .unwrap()
            .get_edge(LinkId::from(edge_uuid(
                EdgeRelation::DependsOn,
                reader,
                cross
            )))
            .await
            .unwrap()
            .is_some(),
        "MUST-FAIL: Cargo imports alone cannot produce this reader edge"
    );
}

#[tokio::test]
async fn removed_read_retains_history_without_claiming_it_is_current() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let (rt, _locks) = runtime(root.path());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let first = Utc::now();
    ingest(&rt, &token, root.path(), first).await;
    let reader = module_uuid("reader", "rust", "crate");
    let target = module_uuid("input", "rust", "crate");
    let id = LinkId::from(edge_uuid(EdgeRelation::DependsOn, reader, target));
    let before = rt
        .graph(&token)
        .unwrap()
        .get_edge(id)
        .await
        .unwrap()
        .unwrap()
        .metadata;
    fs::write(
        root.path().join("crates/reader/src/lib.rs"),
        "pub fn changed() {}\n",
    )
    .unwrap();
    ingest(
        &rt,
        &token,
        root.path(),
        first + chrono::Duration::seconds(1),
    )
    .await;
    let current = get_entity_opt(&rt, &token, reader)
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap();
    let after = rt
        .graph(&token)
        .unwrap()
        .get_edge(id)
        .await
        .unwrap()
        .unwrap()
        .metadata;
    assert_eq!(after, before, "old observations remain qualified history");
    assert_eq!(current["source_read_scan"]["observations"], json!([]));
    assert_ne!(
        after.unwrap()["source_read_observations"][0]["reader"]["content_hash"],
        current["content_hash"]
    );
}

async fn current_scan(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    source: &str,
) -> ModuleScan {
    let row = get_entity_opt(rt, token, id).await.unwrap().unwrap();
    let props = row.properties.unwrap();
    let mut input = input(props["source_path"].as_str().unwrap(), source);
    input.source_revision = props["source_revision"].as_str().unwrap().into();
    input.content_hash = props["content_hash"].as_str().unwrap().into();
    input.sweep_time = props["last_seen_at"].as_str().unwrap().into();
    module(input)
}

#[tokio::test]
async fn changed_source_binding_counts_both_coverage_misses() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let (rt, _locks) = runtime(root.path());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    ingest(&rt, &token, root.path(), Utc::now()).await;
    let reader = module_uuid("reader", "rust", "crate");
    let source = fs::read_to_string(root.path().join("crates/reader/src/lib.rs")).unwrap();
    let mut scan = current_scan(&rt, &token, reader, &source).await;
    scan.source_reads[0].content_hash = "stale-snapshot".into();
    let scans = HashMap::from([(reader, scan)]);
    let plans = prepare(&scans);
    let mut report = CodeSourceIngestReport::default();
    let stamped = stamp_import_scan_coverage(&rt, &token, scans, &plans, &mut report)
        .await
        .unwrap();
    assert!(stamped.is_empty());
    assert_eq!(report.coverage_stamps_missed, 1);
    assert!(report
        .warnings
        .iter()
        .any(|message| message.contains("changed since")));
    persist_edges(&rt, &token, &plans, &stamped, Utc::now(), &mut report)
        .await
        .unwrap();
    assert_eq!(report.source_reads.unwrap().coverage_stamps_missed, 1);
}

#[tokio::test]
async fn refused_module_observation_cannot_publish_edge_evidence() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let (rt, _locks) = runtime(root.path());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    ingest(&rt, &token, root.path(), Utc::now()).await;
    let reader = module_uuid("reader", "rust", "crate");
    let target = module_uuid("input", "rust", "crate");
    let source = fs::read_to_string(root.path().join("crates/reader/src/lib.rs")).unwrap();
    let target_source = fs::read_to_string(root.path().join("crates/input/src/lib.rs")).unwrap();
    let scans = HashMap::from([
        (reader, current_scan(&rt, &token, reader, &source).await),
        (
            target,
            current_scan(&rt, &token, target, &target_source).await,
        ),
    ]);
    let mut plans = prepare(&scans);
    plans.get_mut(&reader).unwrap().observation["unresolved"] =
        json!(["https://user:pass@example.invalid/private"]);
    let edge_id = LinkId::from(edge_uuid(EdgeRelation::DependsOn, reader, target));
    let before = rt
        .graph(&token)
        .unwrap()
        .get_edge(edge_id)
        .await
        .unwrap()
        .unwrap()
        .metadata;
    let mut report = CodeSourceIngestReport::default();
    let stamped = stamp_import_scan_coverage(&rt, &token, scans, &plans, &mut report)
        .await
        .unwrap();
    assert!(
        !stamped.contains(&reader),
        "MUST-FAIL: runtime secret gate must refuse the observation"
    );
    assert_eq!(report.blocked_count, 1);
    persist_edges(&rt, &token, &plans, &stamped, Utc::now(), &mut report)
        .await
        .unwrap();
    assert_eq!(report.source_reads.as_ref().unwrap().resolved_reads, 0);
    assert_eq!(
        rt.graph(&token)
            .unwrap()
            .get_edge(edge_id)
            .await
            .unwrap()
            .unwrap()
            .metadata,
        before
    );
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("user:pass"));
    let current = get_entity_opt(&rt, &token, reader).await.unwrap().unwrap();
    assert!(!serde_json::to_string(&current.properties)
        .unwrap()
        .contains("user:pass"));
}
