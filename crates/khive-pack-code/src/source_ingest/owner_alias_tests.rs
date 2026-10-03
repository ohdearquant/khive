use super::*;
use khive_runtime::{Namespace, RuntimeConfig};
use tempfile::TempDir;

const SOURCE: &str = "pub fn helper() {}\npub fn caller() { helper(); }\n";

fn file_runtime(path: &Path) -> (KhiveRuntime, NamespaceToken) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed code-map runtime");
    let token = runtime.authorize(Namespace::local()).expect("local token");
    (runtime, token)
}

async fn ingest_at(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    path: &Path,
    stamp: DateTime<Utc>,
) -> CodeSourceIngestReport {
    let report = run_code_ingest(
        rt,
        token,
        CodeSourceIngestOptions {
            path,
            languages: ["rust"].into_iter().collect(),
            sweep_time: stamp,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("natural L2 source ingest");
    assert_eq!(report.blocked_count, 0);
    assert_eq!(report.source_files_refused, 0);
    assert_eq!(report.files_dropped_without_source_path, 0);
    assert_eq!(report.files_skipped_without_module_path, 0);
    assert_eq!(
        report.l2.as_ref().expect("L2 report").symbol_parse_failures,
        0
    );
    report
}

async fn entity(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Entity {
    rt.entities(token)
        .expect("entities")
        .get_entity_including_deleted(id)
        .await
        .expect("stored entity read")
        .expect("ingest created entity")
}

fn property<'a>(entity: &'a Entity, key: &str) -> &'a Value {
    entity
        .properties
        .as_ref()
        .and_then(|properties| properties.get(key))
        .unwrap_or_else(|| panic!("missing {key} in stored entity {}", entity.id))
}

async fn assert_owner_clock(rt: &KhiveRuntime, token: &NamespaceToken, stamp: DateTime<Utc>) {
    let owner = entity(rt, token, project_uuid("proj")).await;
    assert!(owner.deleted_at.is_none());
    assert_eq!(owner.kind, "project");
    assert_eq!(property(&owner, "source_project"), &json!("proj"));
    let actual = property(&owner, "sweep_clock")
        .get("rust")
        .and_then(Value::as_str)
        .expect("persisted Rust owner clock");
    assert_eq!(actual, stamp.to_rfc3339());
    assert_eq!(
        DateTime::parse_from_rfc3339(actual)
            .expect("actual owner clock parses")
            .with_timezone(&Utc),
        stamp
    );
}

async fn natural_call_edge(rt: &KhiveRuntime, token: &NamespaceToken) -> Edge {
    let source = symbol_uuid("proj", "rust", "alpha", "caller", "function");
    let target = symbol_uuid("proj", "rust", "alpha", "helper", "function");
    let expected_id = LinkId::from(edge_uuid(EdgeRelation::DependsOn, source, target));
    let graph = rt.graph(token).expect("graph");
    let row = graph
        .get_edge_including_deleted(expected_id)
        .await
        .expect("original edge including tombstones")
        .expect("original natural call edge still exists");
    assert_eq!(row.id, expected_id);
    assert_eq!(row.source_id, source);
    assert_eq!(row.target_id, target);
    assert_eq!(row.relation, EdgeRelation::DependsOn);
    assert_eq!(row.namespace, token.namespace().as_str());
    assert!(row.deleted_at.is_none(), "original natural edge stays live");
    let live = graph
        .get_edge(expected_id)
        .await
        .expect("live original edge read")
        .expect("original edge passes live filtering");
    let natural = graph
        .get_edge_by_natural_key_including_deleted(
            token.namespace().as_str(),
            source,
            target,
            EdgeRelation::DependsOn,
        )
        .await
        .expect("natural-key row read")
        .expect("natural key still exists");
    assert_eq!(
        natural.id, expected_id,
        "no replacement edge hides the original"
    );
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(&row).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&natural).unwrap(),
        serde_json::to_value(&row).unwrap()
    );
    let metadata = row.metadata.as_ref().expect("natural call metadata");
    assert_eq!(metadata.get("l2_derived"), Some(&json!(true)));
    assert_eq!(metadata.get("language"), Some(&json!("rust")));
    assert_eq!(metadata.get("l2_evidence"), Some(&json!(["call"])));
    row
}

fn assert_edge_stamp(edge: &Edge, expected: DateTime<Utc>) {
    let actual = edge
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("last_seen_at"))
        .and_then(Value::as_str)
        .expect("natural edge last_seen_at");
    assert_eq!(actual, expected.to_rfc3339());
    assert_eq!(
        DateTime::parse_from_rfc3339(actual)
            .expect("natural edge timestamp parses")
            .with_timezone(&Utc),
        expected
    );
}

#[tokio::test]
async fn l2_known_limitation_shared_project_owner_leaves_unchanged_natural_edges_stale() {
    let t1 = DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let t2 = t1 + chrono::Duration::seconds(1);
    let t3 = t2 + chrono::Duration::seconds(1);
    assert!(t1 < t2 && t2 < t3);
    for interleave_shared_owner in [false, true] {
        let fixture = TempDir::new().expect("fixture");
        let a = fixture.path().join("A/proj");
        let b = fixture.path().join("B/proj");
        fs::create_dir_all(&a).expect("A source directory");
        fs::create_dir_all(&b).expect("B source directory");
        fs::write(a.join("alpha.rs"), SOURCE).expect("A real Rust source");
        fs::write(b.join("beta.rs"), SOURCE).expect("B real Rust source");
        assert!(!a.join("Cargo.toml").exists() && !b.join("Cargo.toml").exists());
        let (rt, token) = file_runtime(&fixture.path().join("code-map.db"));

        let first = ingest_at(&rt, &token, &a, t1).await;
        assert_owner_clock(&rt, &token, t1).await;
        assert_eq!(first.projects_created, 1);
        let first_l2 = first.l2.as_ref().unwrap();
        assert_eq!(first_l2.symbols_created, 2);
        assert_eq!(first_l2.symbol_dependencies_unresolved, 0);
        assert_eq!(first_l2.symbol_edges_stamped, 1);
        let initial = natural_call_edge(&rt, &token).await;
        assert_edge_stamp(&initial, t1);
        let original_module = entity(&rt, &token, module_uuid("proj", "rust", "alpha")).await;
        let original_hash = property(&original_module, "l2_content_hash").clone();
        let original_declarations = property(&original_module, "declaration_ids").clone();
        let original_scanner = property(&original_module, "l2_scanner_identity_version").clone();
        assert_eq!(original_declarations.as_array().unwrap().len(), 2);

        if interleave_shared_owner {
            let middle = ingest_at(&rt, &token, &b, t2).await;
            assert_owner_clock(&rt, &token, t2).await;
            assert_eq!(middle.projects_created, 0);
            assert_eq!(middle.projects_updated, 1);
            assert_eq!(middle.l2.as_ref().unwrap().symbols_created, 2);
            let b_module = entity(&rt, &token, module_uuid("proj", "rust", "beta")).await;
            assert_eq!(property(&b_module, "source_project"), &json!("proj"));
            assert_ne!(b_module.id, original_module.id);
            let after_b = natural_call_edge(&rt, &token).await;
            assert_edge_stamp(&after_b, t1);
            assert_eq!(after_b.updated_at, initial.updated_at);
            let a_module = entity(&rt, &token, original_module.id).await;
            assert_eq!(property(&a_module, "last_seen_at"), &json!(t1.to_rfc3339()));
            assert_eq!(property(&a_module, "l2_content_hash"), &original_hash);
            assert_eq!(
                property(&a_module, "declaration_ids"),
                &original_declarations
            );
        } else {
            assert_owner_clock(&rt, &token, t1).await;
        }

        assert_eq!(fs::read_to_string(a.join("alpha.rs")).unwrap(), SOURCE);
        let final_report = ingest_at(&rt, &token, &a, t3).await;
        assert_owner_clock(&rt, &token, t3).await;
        assert_eq!(final_report.projects_created, 0);
        let final_l2 = final_report.l2.as_ref().unwrap();
        assert_eq!(final_l2.symbols_created, 0);
        assert_eq!(final_l2.symbols_updated, 2);
        assert_eq!(final_l2.symbol_dependencies_unresolved, 0);
        let final_module = entity(&rt, &token, original_module.id).await;
        assert_eq!(property(&final_module, "l2_content_hash"), &original_hash);
        assert_eq!(
            property(&final_module, "declaration_ids"),
            &original_declarations
        );
        assert_eq!(
            property(&final_module, "l2_scanner_identity_version"),
            &original_scanner
        );
        assert_eq!(
            property(&final_module, "last_seen_at"),
            &json!(t3.to_rfc3339())
        );
        let final_edge = natural_call_edge(&rt, &token).await;
        assert_eq!(final_edge.id, initial.id);
        assert_eq!(final_edge.created_at, initial.created_at);
        if interleave_shared_owner {
            assert_edge_stamp(&final_edge, t1);
            assert_eq!(final_edge.updated_at, initial.updated_at);
            assert_eq!(final_l2.symbol_edges_stamped, 0);
        } else {
            assert_edge_stamp(&final_edge, t3);
            assert!(final_edge.updated_at >= t3);
            assert_eq!(final_l2.symbol_edges_stamped, 1);
        }
    }
}
