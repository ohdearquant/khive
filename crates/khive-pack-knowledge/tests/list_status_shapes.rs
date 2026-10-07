use std::collections::BTreeSet;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        packs: vec!["kg".into(), "knowledge".into()],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("private memory runtime");
    assert!(runtime.config().db_path.is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
    let registry = builder.build().expect("serving registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn acquisitions(runtime: &KhiveRuntime) -> u64 {
    runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .acquisitions
}

async fn seed(registry: &VerbRegistry) {
    let response = registry.dispatch("knowledge.upsert_atoms", json!({
        "atoms": [
            {
                "slug": "shape-draft", "name": "Shape Draft", "finalized": false,
                "content": "A private corpus fixture distinguishes draft and reviewed atoms when checking exact list filters without any embedding model or external storage service."
            },
            {
                "slug": "shape-reviewed", "name": "Shape Reviewed", "finalized": true,
                "content": "A private corpus fixture distinguishes draft and reviewed atoms when checking exact list filters without any embedding model or external storage service."
            }
        ]
    })).await.expect("seed draft and reviewed atoms");
    assert_eq!(response["created"], 2);
    let response = registry.dispatch("knowledge.upsert_domains", json!({
        "domains": [{"slug": "shape-domain", "name": "Shape Domain", "description": "A private domain fixture supplies enough description words for the ordinary domain mirror validation while keeping list status filtering independent from atom status selection."}]
    })).await.expect("seed a domain and its mirror");
    assert_eq!(response["created"], 1);
}

fn malformed_statuses() -> Vec<Value> {
    vec![
        json!(false),
        json!(42),
        json!(0.5),
        json!({}),
        json!({"status": "draft"}),
        json!([false]),
        json!([null]),
        json!(["draft", false]),
        json!([false, "draft"]),
        json!(["reviewed", 42]),
        json!(["reviewed", {}]),
        json!(["reviewed", ["draft"]]),
    ]
}

fn slugs(response: &Value) -> BTreeSet<String> {
    response["results"]
        .as_array()
        .expect("list results")
        .iter()
        .map(|row| row["slug"].as_str().expect("result slug").to_owned())
        .collect()
}

async fn cursor_slugs(registry: &VerbRegistry, filters: &Value) -> BTreeSet<String> {
    let mut after = String::new();
    let mut seen = BTreeSet::new();
    for _ in 0..3 {
        let mut params = filters.clone();
        params["after"] = json!(after);
        params["limit"] = json!(1);
        params["fields"] = json!(["id", "slug"]);
        let response = registry
            .dispatch("knowledge.list", params)
            .await
            .expect("cursor list");
        assert_eq!(response["order"], "created_at_asc_id_asc");
        assert!(response.get("total").is_none());
        for slug in slugs(&response) {
            assert!(seen.insert(slug), "cursor must not repeat a row");
        }
        match &response["next_after"] {
            Value::Null => return seen,
            Value::String(next) => after = next.clone(),
            other => panic!("unexpected cursor {other}"),
        }
    }
    panic!("the two-atom or one-domain fixture must finish within three pages");
}

#[tokio::test]
async fn malformed_atom_status_refuses_before_any_list_reader() {
    let (runtime, registry) = fixture();
    seed(&registry).await;
    let first = registry
        .dispatch("knowledge.list", json!({"after": "", "limit": 1}))
        .await
        .expect("real cursor boundary");
    let boundary = first["next_after"]
        .as_str()
        .expect("two atoms require continuation");
    for selector in [json!({}), json!({"type": "atom"}), json!({"kind": "atom"})] {
        for pagination in [
            json!({"offset": 0}),
            json!({"after": ""}),
            json!({"after": boundary}),
        ] {
            for status in malformed_statuses() {
                let mut params = selector.clone();
                params
                    .as_object_mut()
                    .unwrap()
                    .extend(pagination.as_object().unwrap().clone());
                params["status"] = status.clone();
                params["exclude_status"] = json!("reviewed");
                let before = acquisitions(&runtime);
                let error = registry
                    .dispatch("knowledge.list", params)
                    .await
                    .expect_err("malformed status must not become a different filter");
                assert!(
                    matches!(error, RuntimeError::InvalidInput(ref message)
                    if message == "status must be a string or an array of strings"),
                    "{status}: {error}"
                );
                assert_eq!(
                    acquisitions(&runtime),
                    before,
                    "status {status} opened a list or cursor reader"
                );
            }
        }
    }
    let before = acquisitions(&runtime);
    let response = registry
        .dispatch("knowledge.list", json!({"status": "draft"}))
        .await
        .expect("valid draft list");
    assert_eq!(slugs(&response), BTreeSet::from(["shape-draft".to_owned()]));
    assert!(
        acquisitions(&runtime) > before,
        "valid list must exercise the same reader counter"
    );
}

#[tokio::test]
async fn valid_atom_statuses_keep_offset_counts_and_cursor_filters() {
    let (_runtime, registry) = fixture();
    seed(&registry).await;
    for (filters, expected) in [
        (json!({}), vec!["shape-draft", "shape-reviewed"]),
        (
            json!({"status": null}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (
            json!({"status": "  "}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (json!({"status": []}), vec!["shape-draft", "shape-reviewed"]),
        (
            json!({"status": [" ", ""]}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (json!({"status": " draft "}), vec!["shape-draft"]),
        (
            json!({"status": ["draft", "reviewed"]}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (
            json!({"status": [" reviewed ", "reviewed", ""]}),
            vec!["shape-reviewed"],
        ),
        (json!({"status": "custom-status"}), vec![]),
        (json!({"status": ["custom-status"]}), vec![]),
        (
            json!({"status": ["draft", "custom-status"]}),
            vec!["shape-draft"],
        ),
        (
            json!({"status": "reviewed", "exclude_status": "reviewed"}),
            vec!["shape-reviewed"],
        ),
        (
            json!({"status": [], "exclude_status": "reviewed"}),
            vec!["shape-draft"],
        ),
        (
            json!({"status": null, "exclude_status": "reviewed"}),
            vec!["shape-draft"],
        ),
        (
            json!({"status": " ", "exclude_status": " draft "}),
            vec!["shape-reviewed"],
        ),
        (
            json!({"exclude_status": " custom-status "}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (
            json!({"exclude_status": ""}),
            vec!["shape-draft", "shape-reviewed"],
        ),
    ] {
        let expected = expected
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let response = registry
            .dispatch("knowledge.list", filters.clone())
            .await
            .expect("valid status shape");
        assert_eq!(
            slugs(&response),
            expected,
            "offset filters {filters}: {response}"
        );
        assert_eq!(response["total"], json!(expected.len()));
        assert_eq!(response["order"], "created_at_desc_id_desc");
        assert_eq!(
            cursor_slugs(&registry, &filters).await,
            expected,
            "cursor filters {filters}"
        );
    }
}

#[tokio::test]
async fn domain_status_is_ignored_for_both_selectors_and_page_modes() {
    let (_runtime, registry) = fixture();
    seed(&registry).await;
    for selector in [json!({"type": "domain"}), json!({"kind": "domain"})] {
        let baseline = registry
            .dispatch("knowledge.list", selector.clone())
            .await
            .expect("domain baseline");
        assert_eq!(
            slugs(&baseline),
            BTreeSet::from(["shape-domain".to_owned()])
        );
        let statuses = malformed_statuses().into_iter().chain([
            json!(null),
            json!("draft"),
            json!(""),
            json!([]),
            json!(["reviewed", ""]),
        ]);
        for status in statuses {
            let mut params = selector.clone();
            params["status"] = status;
            params["exclude_status"] = json!("draft");
            let response = registry
                .dispatch("knowledge.list", params.clone())
                .await
                .expect("domain ignores status");
            assert_eq!(response, baseline);
            assert_eq!(
                cursor_slugs(&registry, &params).await,
                BTreeSet::from(["shape-domain".to_owned()])
            );
        }
    }
}
