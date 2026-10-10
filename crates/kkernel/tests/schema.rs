use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRegistry;
use khive_runtime::{
    base_entity_endpoint_rules, classify_operation, present_with_policy, render_format,
    AllowAllGate, CallerEnrollmentGate, Gate, GateDecision, GateError, GateRef, GateRequest,
    KhiveRuntime, Namespace, NamespaceToken, OperationAccess, OutputFormat, PackRuntime,
    PresentationMode, RuntimeConfig, RuntimeError, StorageBackend, VerbRegistry,
    VerbRegistryBuilder, OPERATION_CLASSIFIER_VERSION,
};
use khive_storage::{EntityFilter, EntityStore, GraphStore, NoteStore};
use khive_types::{
    canonical_json_bytes, EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Hash32, Pack,
    VerbCategory, VerbPresentationPolicy, Visibility,
};
use kkernel as _;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

fn isolated_parent() -> bool {
    let private = tempfile::tempdir().expect("private child home");
    khive_storage::test_support::run_exact_test_in_child("SCHEMA_TEST_CHILD", false, |command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") || key == "LATTICE_MODEL_CACHE" {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", private.path())
            .current_dir(private.path());
    })
}

fn setup(
    names: &[String],
    gate: GateRef,
    actor: &str,
    namespace: &str,
) -> (KhiveRuntime, Arc<StorageBackend>, VerbRegistryBuilder) {
    let backend = Arc::new(StorageBackend::memory().expect("private backend"));
    backend.prepare_core_schema().expect("private core schema");
    let config = RuntimeConfig {
        packs: names.to_vec(),
        brain_profile: None,
        actor_id: Some(actor.into()),
        events_split: None,
        mounts: vec![],
        gate: gate.clone(),
        default_namespace: Namespace::parse(namespace).unwrap(),
        ..RuntimeConfig::no_embeddings().for_metadata_registry()
    };
    let runtime = KhiveRuntime::from_backend(backend.clone(), config);
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_actor_id(Some(actor.into()))
        .with_default_namespace(namespace)
        .with_gate(gate);
    PackRegistry::register_packs(names, runtime.clone(), &mut builder).expect("real packs");
    (runtime, backend, builder)
}

fn default_registry() -> (Arc<StorageBackend>, VerbRegistry) {
    let (_, backend, builder) = setup(
        &RuntimeConfig::built_in_packs(),
        Arc::new(AllowAllGate),
        "schema-test",
        "local",
    );
    (backend, builder.build().expect("default serving registry"))
}

async fn domain_counts(backend: &StorageBackend) -> (u64, u64, u64) {
    (
        backend
            .count_entities("local", EntityFilter::default())
            .await
            .unwrap(),
        backend.count_notes("local", None).await.unwrap(),
        backend
            .count_edges(khive_storage::types::EdgeFilter::default())
            .await
            .unwrap(),
    )
}

fn assert_hash_and_counts(value: &Value) {
    let hash = value["contract_version"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    assert!(hash
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    for key in [
        "entity_kinds",
        "note_kinds",
        "edge_relations",
        "endpoint_rules",
        "packs_loaded",
    ] {
        assert_eq!(
            value["counts"][key].as_u64().unwrap(),
            value[key].as_array().unwrap().len() as u64,
            "{key}"
        );
    }
    let mut payload = value.clone();
    payload.as_object_mut().unwrap().remove("contract_version");
    assert_eq!(
        hash,
        Hash32::from_blake3(&canonical_json_bytes(&payload).unwrap()).to_string()
    );
    payload["counts"]["endpoint_rules"] =
        json!(value["counts"]["endpoint_rules"].as_u64().unwrap() + 1);
    assert_ne!(
        hash,
        Hash32::from_blake3(&canonical_json_bytes(&payload).unwrap()).to_string(),
        "counts are hashed"
    );
}

#[tokio::test]
async fn default_schema_is_complete_attributed_and_does_not_write_domain_rows() {
    if isolated_parent() {
        return;
    }
    let (backend, registry) = default_registry();
    let before = domain_counts(&backend).await;
    assert_eq!(before, (0, 0, 0));
    let value = registry.dispatch("schema", json!({})).await.unwrap();
    assert_hash_and_counts(&value);
    assert_eq!(domain_counts(&backend).await, before);
    assert!(value["entity_kinds"]
        .as_array()
        .unwrap()
        .contains(&json!("resource")));
    for note in ["message", "scheduled_event", "task"] {
        assert!(
            value["note_kinds"]
                .as_array()
                .unwrap()
                .contains(&json!(note)),
            "{note}"
        );
    }
    let expected_relations = [
        "annotates",
        "competes_with",
        "composed_with",
        "contains",
        "depends_on",
        "derived_from",
        "enables",
        "extends",
        "implements",
        "instance_of",
        "introduced_by",
        "links_to",
        "located_in",
        "owns",
        "part_of",
        "precedes",
        "refutes",
        "supersedes",
        "supports",
        "variant_of",
    ];
    assert_eq!(value["edge_relations"], json!(expected_relations));
    assert_eq!(expected_relations.len(), EdgeRelation::ALL.len());
    let rows = value["endpoint_rules"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        base_entity_endpoint_rules().len() + registry.all_edge_rules_with_packs().len()
    );
    assert!(rows.contains(&json!({"source":"*", "source_substrate":"entity", "relation":"instance_of", "target":"concept", "target_substrate":"entity", "origin":"base"})));
    assert!(rows.contains(&json!({"source":"task", "source_substrate":"note", "relation":"depends_on", "target":"task", "target_substrate":"note", "origin":"pack:gtd"})));
    assert!(rows.iter().any(|row| row["origin"] == "pack:code"
        && (row["source_entity_type"] == "function" || row["target_entity_type"] == "function")));
    assert!(rows
        .iter()
        .filter(|row| row["origin"] == "base")
        .all(|row| row.get("source_entity_type").is_none()
            && row.get("target_entity_type").is_none()));
    for key in [
        "entity_kinds",
        "note_kinds",
        "edge_relations",
        "packs_loaded",
    ] {
        let strings: Vec<_> = value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(strings.windows(2).all(|pair| pair[0] < pair[1]), "{key}");
    }
    let mut packs = RuntimeConfig::built_in_packs();
    packs.sort();
    assert_eq!(value["packs_loaded"], json!(packs));
    assert_eq!(registry.dispatch("schema", json!({})).await.unwrap(), value);
    assert_eq!(
        registry
            .clone()
            .dispatch("schema", json!({}))
            .await
            .unwrap(),
        value
    );
}

const DUPLICATE: EdgeEndpointRule = EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::EntityOfKind("schema_kind"),
    target: EndpointKind::NoteOfKind("schema_kind"),
};
struct Extra<const ALTERNATE: bool>;
impl<const ALTERNATE: bool> Pack for Extra<ALTERNATE> {
    const NAME: &'static str = "schema_fixture";
    const NOTE_KINDS: &'static [&'static str] = &["schema_kind"];
    const ENTITY_KINDS: &'static [&'static str] = &["schema_kind"];
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[
        DUPLICATE,
        DUPLICATE,
        EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: if ALTERNATE { "datatype" } else { "function" },
            },
            target: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "interface",
            },
        },
    ];
}
struct Other;
impl Pack for Other {
    const NAME: &'static str = "other_schema_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[DUPLICATE];
}
#[async_trait]
impl<const ALTERNATE: bool> PackRuntime for Extra<ALTERNATE> {
    khive_runtime::pack_runtime_metadata!();
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        unreachable!("metadata-only fixture has no handlers")
    }
}
#[async_trait]
impl PackRuntime for Other {
    khive_runtime::pack_runtime_metadata!();
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        unreachable!("metadata-only fixture has no handlers")
    }
}

fn with_extras<const ALTERNATE: bool>(reverse: bool, actor: &str, namespace: &str) -> VerbRegistry {
    let mut names = RuntimeConfig::built_in_packs();
    if reverse {
        names.reverse();
    }
    let (_, _, mut builder) = setup(&names, Arc::new(AllowAllGate), actor, namespace);
    if reverse {
        builder.register(Other).register(Extra::<ALTERNATE>);
    } else {
        builder.register(Extra::<ALTERNATE>).register(Other);
    }
    builder.build().unwrap()
}

#[tokio::test]
async fn schema_hash_tracks_declarations_not_registration_order_or_identity() {
    if isolated_parent() {
        return;
    }
    let (_, plain) = default_registry();
    let base = plain.dispatch("schema", json!({})).await.unwrap();
    let a = with_extras::<false>(false, "first", "local")
        .dispatch("schema", json!({}))
        .await
        .unwrap();
    let reordered = with_extras::<false>(true, "second", "another")
        .dispatch("schema", json!({}))
        .await
        .unwrap();
    assert_eq!(a, reordered);
    assert_ne!(a["contract_version"], base["contract_version"]);
    assert_eq!(
        a["counts"]["endpoint_rules"].as_u64().unwrap(),
        base["counts"]["endpoint_rules"].as_u64().unwrap() + 4
    );
    assert_eq!(
        a["counts"]["packs_loaded"].as_u64().unwrap(),
        base["counts"]["packs_loaded"].as_u64().unwrap() + 2
    );
    let changed = with_extras::<true>(false, "first", "local")
        .dispatch("schema", json!({}))
        .await
        .unwrap();
    assert_eq!(a["packs_loaded"], changed["packs_loaded"]);
    assert_eq!(a["entity_kinds"], changed["entity_kinds"]);
    assert_eq!(a["note_kinds"], changed["note_kinds"]);
    assert_eq!(a["counts"], changed["counts"]);
    assert_ne!(
        a["contract_version"], changed["contract_version"],
        "subtypes must affect the hash"
    );
    assert_hash_and_counts(&a);
    assert_hash_and_counts(&changed);
    let rows = a["endpoint_rules"].as_array().unwrap();
    let duplicate = json!({"source":"schema_kind", "source_substrate":"entity", "relation":"depends_on", "target":"schema_kind", "target_substrate":"note", "origin":"pack:schema_fixture"});
    assert_eq!(rows.iter().filter(|row| **row == duplicate).count(), 2);
    let mut other = duplicate;
    other["origin"] = json!("pack:other_schema_fixture");
    assert_eq!(rows.iter().filter(|row| **row == other).count(), 1);
    assert!(rows.contains(&json!({"source":"concept", "source_substrate":"entity", "source_entity_type":"function", "relation":"depends_on", "target":"concept", "target_substrate":"entity", "target_entity_type":"interface", "origin":"pack:schema_fixture"})));
    let keys: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row["origin"].as_str().unwrap(),
                row["source_substrate"].as_str().unwrap(),
                row["source"].as_str().unwrap(),
                row.get("source_entity_type").map(|v| v.as_str().unwrap()),
                row["relation"].as_str().unwrap(),
                row["target_substrate"].as_str().unwrap(),
                row["target"].as_str().unwrap(),
                row.get("target_entity_type").map(|v| v.as_str().unwrap()),
            )
        })
        .collect();
    assert!(keys.windows(2).all(|pair| pair[0] <= pair[1]));
}

fn assert_rendered(registry: &VerbRegistry, value: &Value) {
    let policy = registry.presentation_policy_for("schema");
    assert_eq!(policy, VerbPresentationPolicy::AlwaysVerbose);
    let presented = present_with_policy(value.clone(), PresentationMode::Agent, 0, policy);
    assert_eq!(
        &presented, value,
        "schema policy preserves the complete hashed payload"
    );
    // The MCP boundary resolves AlwaysVerbose before its final format pass.
    let json_text = render_format(
        presented.clone(),
        OutputFormat::Json,
        PresentationMode::Verbose,
    );
    let parsed: Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(parsed, *value);
    assert_hash_and_counts(&parsed);
    let table = render_format(presented, OutputFormat::Table, PresentationMode::Verbose);
    let lines: Vec<_> = table.lines().filter(|line| line.starts_with('|')).collect();
    let columns: Vec<_> = lines[0]
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect();
    for column in [
        "source",
        "target",
        "relation",
        "origin",
        "source_substrate",
        "target_substrate",
    ] {
        assert!(columns.contains(&column));
    }
    let rows = value["endpoint_rules"].as_array().unwrap();
    assert_eq!(lines.len(), rows.len() + 2);
    for (line, row) in lines[2..].iter().zip(rows) {
        let cells: Vec<_> = line.trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(cells.len(), columns.len());
        for (column, cell) in columns.iter().zip(cells) {
            assert_eq!(
                cell,
                row.get(*column).and_then(Value::as_str).unwrap_or(""),
                "{column}"
            );
        }
    }
    for key in [
        "entity_kinds",
        "note_kinds",
        "edge_relations",
        "packs_loaded",
        "counts",
    ] {
        let prefix = format!("{key}: ");
        let sibling = table
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(sibling).unwrap(),
            value[key],
            "{key}"
        );
    }
    assert!(table.lines().any(|line| line
        == format!(
            "contract_version: {}",
            value["contract_version"].as_str().unwrap()
        )));
    assert!(rows.iter().any(|row| row["source"] == "*"));
}

#[tokio::test]
async fn actual_schema_renders_every_row_and_full_siblings_for_agent_requests() {
    if isolated_parent() {
        return;
    }
    let registry = with_extras::<false>(false, "schema-test", "local");
    let value = registry.dispatch("schema", json!({})).await.unwrap();
    assert_rendered(&registry, &value);
    let table = render_format(value, OutputFormat::Table, PresentationMode::Verbose);
    assert!(table.lines().next().unwrap().contains("source_entity_type"));
    assert!(table.lines().next().unwrap().contains("target_entity_type"));
}

#[tokio::test]
async fn sparse_actual_kg_projection_keeps_empty_arrays_and_the_same_hash_on_the_wire() {
    if isolated_parent() {
        return;
    }
    let (runtime, _, builder) = setup(
        &["kg".into()],
        Arc::new(AllowAllGate),
        "schema-test",
        "local",
    );
    let registry = builder.build().unwrap();
    let empty = VerbRegistryBuilder::new().build().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let value = KgPack::new(runtime)
        .dispatch("schema", json!({}), &empty, &token)
        .await
        .unwrap();
    for key in ["entity_kinds", "note_kinds", "packs_loaded"] {
        assert_eq!(value[key], json!([]));
        assert_eq!(value["counts"][key], 0);
    }
    assert_eq!(
        value["endpoint_rules"].as_array().unwrap().len(),
        base_entity_endpoint_rules().len()
    );
    assert_hash_and_counts(&value);
    assert_rendered(&registry, &value);
    let lookalike = HandlerDef {
        name: "schema.lookalike",
        description: "",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    };
    assert_eq!(
        lookalike.presentation_policy(),
        VerbPresentationPolicy::Standard
    );
    let standard = present_with_policy(
        value,
        PresentationMode::Agent,
        0,
        lookalike.presentation_policy(),
    );
    assert!(
        standard.get("entity_kinds").is_none(),
        "negative control exercises actual empty elision"
    );
}

#[tokio::test]
async fn schema_is_discoverable_strict_and_namespace_independent() {
    if isolated_parent() {
        return;
    }
    let (_, registry) = default_registry();
    let help = registry
        .dispatch("schema", json!({"help":true}))
        .await
        .unwrap();
    assert_eq!(help["verb"], "schema");
    assert_eq!(help["category"], "Assertive");
    assert_eq!(help["params"], json!([]));
    for phrase in [
        "*",
        "any entity kind",
        "no filters",
        "origin",
        "hash",
        "counts",
    ] {
        assert!(
            help["description"].as_str().unwrap().contains(phrase),
            "{phrase}"
        );
    }
    let verbs = registry
        .dispatch("verbs", json!({"pack":"kg"}))
        .await
        .unwrap();
    let advertised: Vec<_> = verbs["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["verb"] == "schema")
        .collect();
    assert_eq!(advertised.len(), 1);
    assert_eq!(advertised[0]["category"], "Assertive");
    assert!(registry
        .all_verbs_with_names()
        .iter()
        .any(|(owner, handler)| *owner == "kg"
            && handler.name == "schema"
            && handler.visibility == Visibility::Verb));
    for key in ["relation", "kind", "filter", "pack"] {
        let error = registry
            .dispatch("schema", json!({(key):"concept"}))
            .await
            .unwrap_err();
        assert!(matches!(&error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(error.to_string().contains(key), "{error}");
    }
    let plain = registry.dispatch("schema", json!({})).await.unwrap();
    assert_eq!(
        registry
            .dispatch("schema", json!({"namespace":"other"}))
            .await
            .unwrap(),
        plain
    );
    assert!(registry
        .dispatch("schema", json!({"namespace":42}))
        .await
        .is_err());
    assert!(matches!(
        registry
            .dispatch("schema.lookalike", json!({}))
            .await
            .unwrap_err(),
        RuntimeError::UnknownVerb(_)
    ));
}

#[derive(Debug)]
struct RecordingGate {
    deny: bool,
    requests: Arc<Mutex<Vec<GateRequest>>>,
}
impl Gate for RecordingGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(if self.deny {
            GateDecision::deny("schema fixture refusal")
        } else {
            GateDecision::allow()
        })
    }
}

#[tokio::test]
async fn schema_obeys_the_actual_gate_before_parameter_validation() {
    if isolated_parent() {
        return;
    }
    for deny in [false, true] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(RecordingGate {
            deny,
            requests: requests.clone(),
        });
        let (_, _, builder) = setup(&["kg".into()], gate, "schema-test", "local");
        let registry = builder.build().unwrap();
        let result = registry
            .dispatch("schema", json!({"namespace":"target"}))
            .await;
        if deny {
            assert!(
                matches!(result.unwrap_err(), RuntimeError::PermissionDenied { verb, reason, .. } if verb == "schema" && reason == "schema fixture refusal")
            );
            assert!(matches!(
                registry
                    .dispatch("schema", json!({"filter":"invalid"}))
                    .await
                    .unwrap_err(),
                RuntimeError::PermissionDenied { .. }
            ));
        } else {
            assert_hash_and_counts(&result.unwrap());
        }
        let seen = requests.lock().unwrap();
        assert_eq!(seen[0].verb, "schema");
        assert_eq!(seen[0].actor.id, "schema-test");
        assert_eq!(seen[0].namespace.as_str(), "target");
        assert_eq!(seen[0].args, json!({"namespace":"target"}));
        assert_eq!(seen.len(), if deny { 2 } else { 1 });
    }
}

#[tokio::test]
async fn schema_is_an_explicit_read_without_weakening_enrollment_or_unknown_denials() {
    if isolated_parent() {
        return;
    }
    assert_eq!(classify_operation("schema"), Some(OperationAccess::Read));
    assert_eq!(classify_operation("schema.lookalike"), None);
    assert_eq!(OPERATION_CLASSIFIER_VERSION, "domain-effects-v13");
    let gate = Arc::new(CallerEnrollmentGate::with_write_denials(
        vec!["schema-test".into()],
        false,
        vec!["schema-*".into()],
    ));
    for actor in ["schema-test", "unlisted"] {
        let (_, _, builder) = setup(&["kg".into()], gate.clone(), actor, "local");
        let registry = builder.build().unwrap();
        let schema = registry.dispatch("schema", json!({})).await;
        if actor == "schema-test" {
            assert_hash_and_counts(&schema.unwrap());
        } else {
            assert!(matches!(
                schema.unwrap_err(),
                RuntimeError::PermissionDenied { .. }
            ));
        }
        assert!(matches!(
            registry
                .dispatch("create", json!({"kind":"concept","name":"refused"}))
                .await
                .unwrap_err(),
            RuntimeError::PermissionDenied { .. }
        ));
    }
    let unknown = GateRequest::new(
        khive_runtime::ActorRef::new("agent", "schema-test"),
        Namespace::local(),
        "schema.lookalike",
        json!({}),
    );
    assert!(!gate.check(&unknown).unwrap().is_allow());
    let reads: BTreeSet<_> = khive_runtime::CLASSIFIED_OPERATIONS
        .iter()
        .filter(|(_, access)| *access == OperationAccess::Read)
        .map(|(verb, _)| *verb)
        .collect();
    assert!(reads.contains("schema") && reads.contains("get"));
}
