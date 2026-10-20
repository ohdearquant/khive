//! Governance storage is not a caller-selectable generic write destination.

use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::SqlStatement;
use serde_json::{json, Value};
use uuid::Uuid;

const GOVERNANCE_TABLES: [&str; 4] = [
    "edge_governance_decisions",
    "edge_governance_invalidations",
    "edge_governance_active",
    "edge_governance_state",
];

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

async fn governance_counts(runtime: &KhiveRuntime) -> Vec<u64> {
    let mut reader = runtime.sql().reader().await.expect("governance reader");
    let mut counts = Vec::new();
    for table in GOVERNANCE_TABLES {
        // Table names come only from the closed fixture constant above.
        counts.push(
            reader
                .count(SqlStatement::new(
                    format!("SELECT COUNT(*) FROM {table}"),
                    vec![],
                ))
                .await
                .expect("fresh migrated governance table"),
        );
    }
    counts
}

async fn assert_governance_fields_refused(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    verb: &str,
    valid: Value,
) {
    // Establish that setup and the otherwise identical call are valid first.
    registry
        .dispatch(verb, valid.clone())
        .await
        .expect("ordinary generic write succeeds");
    let before = governance_counts(runtime).await;
    assert_eq!(before, [0, 0, 0, 1], "dormant fresh governance state");
    for table in GOVERNANCE_TABLES {
        let mut params = valid.clone();
        params[table] = json!({"decision_id": "caller-forged", "status": "active"});
        let error = registry
            .dispatch(verb, params)
            .await
            .expect_err("caller governance destination must be refused");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("expected parameter refusal, got {error:?}");
        };
        assert!(
            message.contains("unknown field") && message.contains(table),
            "{verb} must reject the named governance field, got {message}"
        );
        assert_eq!(governance_counts(runtime).await, before);
    }
}

#[tokio::test]
async fn generic_create_refuses_governance_table_parameters() {
    let (runtime, registry) = fixture();
    assert_governance_fields_refused(
        &runtime,
        &registry,
        "create",
        json!({"kind": "concept", "name": "ordinary entity", "skip_dedup_check": true}),
    )
    .await;
}

#[tokio::test]
async fn generic_update_refuses_governance_table_parameters() {
    let (runtime, registry) = fixture();
    let entity = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "update target"}),
        )
        .await
        .expect("update fixture entity");
    assert_governance_fields_refused(
        &runtime,
        &registry,
        "update",
        json!({"id": entity["id"], "name": "updated target"}),
    )
    .await;
}

#[tokio::test]
async fn generic_link_refuses_governance_table_parameters() {
    let (runtime, registry) = fixture();
    let source = registry
        .dispatch("create", json!({"kind": "concept", "name": "link source"}))
        .await
        .expect("source fixture entity");
    let target = registry
        .dispatch("create", json!({"kind": "concept", "name": "link target"}))
        .await
        .expect("target fixture entity");
    assert_governance_fields_refused(
        &runtime,
        &registry,
        "link",
        json!({"source_id": source["id"], "target_id": target["id"], "relation": "extends"}),
    )
    .await;
}

#[tokio::test]
async fn generic_import_processes_fixed_substrates_without_governance_writes() {
    let (runtime, _registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let before = governance_counts(&runtime).await;
    assert_eq!(before, [0, 0, 0, 1]);
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let edge_id = Uuid::new_v4();
    let mut claims = json!({});
    for table in GOVERNANCE_TABLES {
        claims[table] = json!({"decision_id":"import-forged", "status":"active"});
    }
    let mut archive = json!({
        "format":"khive-kg", "version":"0.1", "namespace":"local",
        "exported_at":"2026-10-19T00:00:00Z",
        "entities":[
            {"id":source_id, "kind":"concept", "name":"import source", "properties":claims.clone(),
             "created_at":"2026-10-19T00:00:00Z", "updated_at":"2026-10-19T00:00:00Z"},
            {"id":target_id, "kind":"concept", "name":"import target",
             "created_at":"2026-10-19T00:00:00Z", "updated_at":"2026-10-19T00:00:00Z"}
        ],
        "edges":[{"edge_id":edge_id, "source":source_id, "target":target_id,
            "relation":"depends_on", "weight":0.75, "properties":claims.clone(),
            "created_at":"2026-10-19T00:00:00Z", "updated_at":"2026-10-19T00:00:00Z"}]
    });
    for table in GOVERNANCE_TABLES {
        archive[table] = json!({"decision_id":"import-forged", "status":"active"});
    }
    let summary = runtime
        .import_kg_json(&archive.to_string(), &token)
        .await
        .unwrap();
    assert_eq!(summary.entities_imported, 2);
    assert_eq!(summary.edges_imported, 1);
    assert_eq!(summary.edges_skipped, 0);
    assert_eq!(
        runtime.get_entity(&token, source_id).await.unwrap().name,
        "import source"
    );
    assert_eq!(
        runtime.get_entity(&token, target_id).await.unwrap().name,
        "import target"
    );
    let stored = runtime
        .graph(&token)
        .unwrap()
        .get_edge(edge_id.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.source_id, stored.target_id), (source_id, target_id));
    assert_eq!(stored.metadata, Some(claims));
    assert_eq!(
        governance_counts(&runtime).await,
        before,
        "accepted data is not authority"
    );
}
