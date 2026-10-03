use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    HandlerDef, KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{EntityKind, EntityTypeDef, Pack};
use serde_json::{json, Value};

struct RegisteredSubtypeFixture;

impl Pack for RegisteredSubtypeFixture {
    const NAME: &'static str = "registered_subtype_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[
        EntityTypeDef {
            kind: EntityKind::Artifact,
            type_name: "brain_profile",
            aliases: &[],
        },
        // The tool pack registers this Project subtype; its spelling also
        // predates the pack as a KG Resource alias.
        EntityTypeDef {
            kind: EntityKind::Project,
            type_name: "skill",
            aliases: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for RegisteredSubtypeFixture {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }

    fn entity_types(&self) -> &'static [EntityTypeDef] {
        Self::ENTITY_TYPES
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "RegisteredSubtypeFixture does not handle {verb:?}"
        )))
    }
}

fn registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.register(RegisteredSubtypeFixture);
    builder.build().expect("registry builds")
}

fn bare_registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.build().expect("registry builds")
}

async fn create(registry: &VerbRegistry, kind: &str, name: &str) -> Value {
    registry
        .dispatch(
            "create",
            json!({"kind": kind, "name": name, "skip_dedup_check": true}),
        )
        .await
        .expect("create entity")
}

#[tokio::test]
async fn registry_kind_tokens_preserve_their_subtypes() {
    let registry = registry();
    let paper = create(&registry, "paper", "Subtype Witness Paper").await;
    assert_eq!(paper["kind"], "document");
    assert_eq!(paper["entity_type"], "paper");
    let persisted_paper = registry
        .dispatch("get", json!({"id": paper["id"]}))
        .await
        .expect("read persisted paper");
    assert_eq!(persisted_paper["kind"], "document");
    assert_eq!(persisted_paper["entity_type"], "paper");

    let profile = create(&registry, "brain_profile", "Subtype Witness Profile").await;
    assert_eq!(profile["kind"], "artifact");
    assert_eq!(profile["entity_type"], "brain_profile");

    let bulk = registry
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "paper", "name": "Bulk Paper"},
                    {"kind": "brain_profile", "name": "Bulk Profile"}
                ],
                "verbose": true,
            }),
        )
        .await
        .expect("bulk subtype create");
    assert_eq!(bulk["results"][0]["result"]["kind"], "document");
    assert_eq!(bulk["results"][0]["result"]["entity_type"], "paper");
    assert_eq!(bulk["results"][1]["result"]["kind"], "artifact");
    assert_eq!(bulk["results"][1]["result"]["entity_type"], "brain_profile");

    let report = registry
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "entity_type": "report",
                "name": "Subtype Witness Report",
                "skip_dedup_check": true,
            }),
        )
        .await
        .expect("create report");
    let listed = registry
        .dispatch("list", json!({"kind": "paper", "limit": 50}))
        .await
        .expect("list paper subtype");
    let ids: Vec<_> = listed["items"]
        .as_array()
        .expect("list items")
        .iter()
        .map(|item| item["id"].clone())
        .collect();
    assert!(ids.contains(&paper["id"]));
    assert!(ids.contains(&bulk["results"][0]["result"]["id"]));
    assert!(!ids.contains(&report["id"]));

    let searched = registry
        .dispatch(
            "search",
            json!({"kind": "paper", "query": "Subtype Witness", "source": "text"}),
        )
        .await
        .expect("search paper subtype");
    let hits = searched.as_array().expect("search hits");
    assert!(hits.iter().any(|hit| hit["id"] == paper["id"]));
    assert!(hits.iter().all(|hit| hit["id"] != report["id"]));
}

#[tokio::test]
async fn explicit_pairs_and_legacy_base_aliases_remain_distinct() {
    let registry = registry();
    for (kind, entity_type) in [
        ("artifact", "snapshot"),
        ("service", "api"),
        ("project", "tool"),
        ("project", "skill"),
    ] {
        let row = registry
            .dispatch(
                "create",
                json!({
                    "kind": kind,
                    "entity_type": entity_type,
                    "name": format!("Explicit {kind} {entity_type}"),
                    "skip_dedup_check": true,
                }),
            )
            .await
            .expect("explicit base kind and subtype");
        assert_eq!(row["kind"], kind);
        assert_eq!(row["entity_type"], entity_type);
    }
    for (alias, base) in [
        ("doc", "document"),
        ("art", "artifact"),
        ("svc", "service"),
        ("repo", "project"),
        ("resource", "resource"),
    ] {
        let row = create(&registry, alias, &format!("Legacy {alias}")).await;
        assert_eq!(row["kind"], base);
        assert!(row["entity_type"].is_null());
    }
    let bare = bare_registry();
    for (label, checked_registry) in [("bare", &bare), ("extra types", &registry)] {
        for alias in ["tool", "skill"] {
            let row = create(checked_registry, alias, &format!("{label} Legacy {alias}")).await;
            assert_eq!(row["kind"], "resource", "{label}: {alias}");
            assert!(row["entity_type"].is_null(), "{label}: {alias}");
        }
    }

    let wrong_base = registry
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "entity_type": "brain_profile",
                "name": "Wrong Base",
            }),
        )
        .await
        .expect_err("brain_profile is an Artifact subtype");
    assert!(matches!(wrong_base, RuntimeError::InvalidInput(_)));
}

#[tokio::test]
async fn subtype_tokens_reject_conflicting_fields_and_mutations() {
    let registry = registry();
    for args in [
        json!({"kind": "paper", "entity_type": "report", "name": "Contradiction"}),
        json!({"kind": "brain_profile", "entity_type": "snapshot", "name": "Contradiction"}),
    ] {
        let error = registry
            .dispatch("create", args)
            .await
            .expect_err("kind token and explicit subtype disagree");
        assert!(error.to_string().contains("contradicts"), "{error}");
    }
    let bulk_error = registry
        .dispatch(
            "create",
            json!({"items": [{"kind": "paper", "entity_type": "report", "name": "Bulk Conflict"}]}),
        )
        .await
        .expect_err("bulk item also checks the subtype");
    assert!(bulk_error.to_string().contains("contradicts"));

    let report = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "report", "name": "Report Guard"}),
        )
        .await
        .expect("create report");
    let id = report["id"].as_str().expect("report id");
    for (verb, args) in [
        (
            "update",
            json!({"id": id, "kind": "paper", "name": "Wrong Update"}),
        ),
        ("delete", json!({"id": id, "kind": "paper"})),
    ] {
        let error = registry
            .dispatch(verb, args)
            .await
            .expect_err("subtype hint cannot address a different subtype");
        assert!(error.to_string().contains("entity_type"), "{error}");
    }
    let paper = create(&registry, "paper", "Paper Guard").await;
    let error = registry
        .dispatch(
            "update",
            json!({"id": paper["id"], "kind": "paper", "entity_type": null}),
        )
        .await
        .expect_err("a subtype-qualified update cannot clear that subtype");
    assert!(error.to_string().contains("contradicts"), "{error}");

    let error = registry
        .dispatch(
            "merge",
            json!({
                "kind": "paper",
                "into_id": paper["id"],
                "from_id": report["id"],
                "dry_run": true,
            }),
        )
        .await
        .expect_err("a subtype-qualified merge must check both operands");
    assert!(error.to_string().contains("entity_type"), "{error}");
}

#[tokio::test]
async fn subtype_qualified_by_id_verbs_accept_legacy_null_entity_type() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    // Rows written before subtype persistence still carry the right base kind.
    let first = runtime
        .create_entity_with_embedding_report(
            &token,
            "document",
            None,
            "Legacy Paper",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("legacy document");
    let second = runtime
        .create_entity_with_embedding_report(
            &token,
            "document",
            None,
            "Legacy Paper Updated",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("second legacy document");

    registry
        .dispatch(
            "update",
            json!({"id": first.id, "kind": "paper", "name": "Legacy Paper Updated"}),
        )
        .await
        .expect("subtype-qualified update accepts legacy null subtype");
    let persisted = runtime
        .get_entity(&token, first.id)
        .await
        .expect("read updated legacy row");
    assert_eq!(persisted.kind, "document");
    assert_eq!(
        persisted.entity_type, None,
        "update does not invent a subtype"
    );

    registry
        .dispatch("delete", json!({"id": first.id, "kind": "paper"}))
        .await
        .expect("subtype-qualified delete accepts legacy null subtype");
    registry
        .dispatch("restore", json!({"id": first.id, "kind": "paper"}))
        .await
        .expect("subtype-qualified restore accepts legacy null subtype");
    let merge = registry
        .dispatch(
            "merge",
            json!({
                "kind": "paper",
                "into_id": first.id,
                "from_id": second.id,
                "dry_run": true,
            }),
        )
        .await
        .expect("subtype-qualified merge can inspect two legacy null rows");
    assert_eq!(merge["dry_run"], true);

    let wrong_base = runtime
        .create_entity_with_embedding_report(
            &token,
            "artifact",
            None,
            "Wrong Base",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("artifact without subtype");
    let error = registry
        .dispatch("update", json!({"id": wrong_base.id, "kind": "paper"}))
        .await
        .expect_err("a null subtype never bypasses the base-kind check");
    assert!(error.to_string().contains("kind mismatch"), "{error}");
}

#[tokio::test]
async fn resolve_subtype_filters_exact_name_candidates() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    let name = "Shared Subtype Name";
    let report = runtime
        .create_entity_with_embedding_report(
            &token,
            "document",
            Some("report"),
            name,
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("report");
    let paper = runtime
        .create_entity_with_embedding_report(
            &token,
            "document",
            Some("paper"),
            name,
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("paper");
    assert_ne!(report.id, paper.id);

    let resolved = registry
        .dispatch("resolve", json!({"kind": "paper", "refs": [name]}))
        .await
        .expect("resolve paper by exact name");
    assert_eq!(resolved["results"][0]["status"], "resolved");
    assert_eq!(resolved["results"][0]["id"], paper.id.to_string());
}
