//! knowledge.list must not narrow an unsigned offset into a negative SQL offset.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use serde_json::{json, Value};

const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
const OFFSET_ORDER: &str = "created_at_desc_id_desc";
const CURSOR_CONFLICT: &str = "knowledge.list: `after` and `offset` are mutually exclusive";

async fn fixture() -> VerbRegistry {
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = Default::default();
    config.disk_guard_config = None;
    config.volume_lock_dir = None;
    config.embedding_model = None;
    config.additional_embedding_models.clear();
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = None;
    config.brain_profile = None;
    config.brain = Default::default();
    config.visible_namespaces.clear();
    config.allowed_outbound_namespaces.clear();
    config.packs = vec!["kg".into(), "knowledge".into()];
    let runtime = KhiveRuntime::new(config).expect("explicit memory runtime");
    assert!(runtime.config().db_path.is_none());
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.config().embedding_model.is_none());
    assert!(runtime.config().additional_embedding_models.is_empty());

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("KG and knowledge registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    for (verb, field, description, prefix) in [
        ("knowledge.upsert_atoms", "atoms", "content", "atom"),
        (
            "knowledge.upsert_domains",
            "domains",
            "description",
            "domain",
        ),
    ] {
        let rows: Vec<Value> = (0..2)
            .map(|index| {
                let mut row = json!({
                    "slug": format!("offset-{prefix}-{index}"),
                    "name": format!("Offset {prefix} {index}"),
                });
                row[description] = json!(CONTENT);
                row
            })
            .collect();
        let mut params = json!({});
        params[field] = json!(rows);
        let result = registry.dispatch(verb, params).await.expect("seed corpus");
        assert_eq!(result["created"], 2, "{verb}: {result}");
    }
    registry
}

async fn list(registry: &VerbRegistry, params: Value) -> Value {
    registry
        .dispatch("knowledge.list", params)
        .await
        .expect("list")
}

fn invalid_input(result: Result<Value, RuntimeError>) -> String {
    match result {
        Err(RuntimeError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

async fn ordered_rows(registry: &VerbRegistry, kind: &str) -> Vec<Value> {
    let all = list(registry, json!({"type": kind, "limit": 20})).await;
    assert_eq!(all["total"], 2);
    assert_eq!(all["order"], OFFSET_ORDER);
    let rows = all["results"].as_array().expect("results").clone();
    assert_eq!(rows.len(), 2);
    let mut slugs: Vec<_> = rows
        .iter()
        .map(|row| row["slug"].as_str().unwrap().to_owned())
        .collect();
    slugs.sort_unstable();
    assert_eq!(
        slugs,
        vec![format!("offset-{kind}-0"), format!("offset-{kind}-1")]
    );
    let mut expected = rows.clone();
    expected.sort_by_key(|row| {
        std::cmp::Reverse((
            chrono::DateTime::parse_from_rfc3339(row["created_at"].as_str().unwrap())
                .expect("stored creation time"),
            row["id"].as_str().unwrap().to_owned(),
        ))
    });
    assert_eq!(rows, expected, "declared descending creation/id order");
    assert_ne!(rows[0]["id"], rows[1]["id"]);
    rows
}

#[tokio::test]
async fn valid_offsets_defaults_and_cursor_conflicts_are_preserved() {
    let registry = fixture().await;
    for kind in ["atom", "domain"] {
        let rows = ordered_rows(&registry, kind).await;
        for offset in 0..=2 {
            let page = list(
                &registry,
                json!({"type": kind, "limit": 1, "offset": offset}),
            )
            .await;
            assert_eq!(page["total"], 2);
            assert_eq!(page["limit"], 1);
            assert_eq!(page["offset"], offset);
            assert_eq!(page["order"], OFFSET_ORDER);
            let expected = rows.get(offset).cloned().into_iter().collect::<Vec<_>>();
            assert_eq!(page["results"], json!(expected));
        }
        let zero = list(&registry, json!({"type": kind, "limit": 1, "offset": 0})).await;
        for params in [
            json!({"type": kind, "limit": 1}),
            json!({"type": kind, "limit": 1, "offset": null}),
            json!({"type": kind, "limit": 1, "after": null}),
        ] {
            assert_eq!(list(&registry, params).await, zero);
        }
        let cursor = list(&registry, json!({"type": kind, "limit": 1, "after": ""})).await;
        assert_eq!(cursor["order"], "created_at_asc_id_asc");
        assert_eq!(cursor["results"], json!([rows[1].clone()]));
        assert_eq!(cursor["next_after"], rows[1]["id"]);
        assert!(cursor.get("total").is_none());
        assert!(cursor.get("offset").is_none());
        assert_eq!(
            invalid_input(
                registry
                    .dispatch(
                        "knowledge.list",
                        json!({"type": kind, "after": "", "offset": 0})
                    )
                    .await
            ),
            CURSOR_CONFLICT,
        );
        for offset in [json!(-1), json!(1.5), json!("1")] {
            assert!(invalid_input(
                registry
                    .dispatch("knowledge.list", json!({"type": kind, "offset": offset}))
                    .await
            )
            .starts_with("bad params:"));
        }
        assert_eq!(ordered_rows(&registry, kind).await, rows);
    }
}

#[cfg(target_pointer_width = "64")]
#[tokio::test]
async fn offsets_outside_signed_sql_range_are_rejected_for_both_kinds() {
    let registry = fixture().await;
    for kind in ["atom", "domain"] {
        let rows = ordered_rows(&registry, kind).await;
        let boundary = list(
            &registry,
            json!({"type": kind, "limit": 1, "offset": i64::MAX as u64}),
        )
        .await;
        assert_eq!(boundary["results"], json!([]));
        assert_eq!(boundary["total"], 2);
        assert_eq!(boundary["limit"], 1);
        assert_eq!(boundary["order"], OFFSET_ORDER);
        assert_eq!(boundary["offset"], json!(i64::MAX as u64));
        for offset in [i64::MAX as u64 + 1, u64::MAX] {
            assert_eq!(
                invalid_input(
                    registry
                        .dispatch(
                            "knowledge.list",
                            json!({"type": kind, "limit": 1, "offset": offset})
                        )
                        .await
                ),
                "knowledge.list offset exceeds the supported range",
            );
            assert_eq!(
                invalid_input(
                    registry
                        .dispatch(
                            "knowledge.list",
                            json!({"type": kind, "after": "", "offset": offset})
                        )
                        .await
                ),
                CURSOR_CONFLICT,
            );
        }
        let wide_limit = list(
            &registry,
            json!({"type": kind, "limit": u64::MAX, "offset": 0}),
        )
        .await;
        assert_eq!(wide_limit["limit"], 500);
        assert_eq!(wide_limit["total"], 2);
        assert_eq!(wide_limit["offset"], 0);
        assert_eq!(wide_limit["results"], json!(rows));
        assert_eq!(ordered_rows(&registry, kind).await, rows);
    }
}
