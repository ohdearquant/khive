//! End-to-end tests for the code pack against an in-memory runtime.
//!
//! FILE SIZE JUSTIFICATION: mirrors `khive-pack-gtd/tests/integration.rs` — a
//! single shared `Fixture` wires `KgPack` + `CodePack` against an in-memory
//! runtime; splitting would duplicate that setup across files.
//!
//! Covers the ADR-085 integration matrix: vocabulary registration, edge
//! endpoint acceptance and rejection, finding-hook defaulting and validation,
//! ingest idempotency, and malformed-input rejection.

use chrono::{DateTime, Utc};
use khive_pack_code::{ingest_findings_json, CodeIngestError, CodeIngestOptions, CodePack};
use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_types::{EdgeRelation, EndpointKind};
use serde_json::{json, Value};

#[allow(dead_code)]
fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

#[allow(dead_code)]
fn registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(CodePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

#[allow(dead_code)]
fn kg_only_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

#[allow(dead_code)]
async fn dispatch(registry: &VerbRegistry, verb: &str, args: Value) -> Result<Value, RuntimeError> {
    registry.dispatch(verb, args).await
}

#[allow(dead_code)]
fn sample_findings_json() -> Value {
    json!({
        "audit": {
            "date": "2026-07-07",
            "scope": "khive-pack-code",
            "repo": "khive",
            "branch": "feature/code-pack",
            "commit": "abc123",
            "standards_file": "audit-guidelines.md",
        },
        "findings": [
            {
                "id": "khive-pack-code-001",
                "title": "  Missing   bounds check  ",
                "severity": "high",
                "confidence": "high",
                "categories": ["security"],
                "status": "open",
                "standard": "audit-guidelines.md#security",
                "evidence": [{"path": "src/ingest.rs", "line": 42, "description": "unchecked index"}],
                "impact": "panics on malformed input",
                "recommendation": "validate before indexing",
                "verification": "add regression test",
                "failure_scenario": "attacker-controlled index panics the process",
                "priority": "P1",
            }
        ]
    })
}

#[allow(dead_code)]
fn ingest_options(source_run: Option<&str>) -> CodeIngestOptions<'_> {
    CodeIngestOptions {
        namespace: "local",
        observed_at: Utc::now(),
        source_run,
    }
}

#[tokio::test]
async fn code_pack_declares_adr085_metadata() {
    let registry = registry(rt());
    assert!(registry.all_note_kinds().contains(&"finding"));
}

/// End-to-end (not merely unit-level) proof that the four code concept
/// subtypes and their aliases registered in `khive-pack-kg`'s `BUILTIN_DEFS`
/// are reachable through the full `create` verb dispatch path, canonicalizing
/// aliases to their token before persistence. The registry-level resolution
/// itself is covered by `khive-pack-kg`'s own
/// `entity_type_registry_accepts_code_tokens_and_aliases` unit test; this
/// proves the wiring holds through dispatch, not just the raw registry call.
#[tokio::test]
async fn entity_type_registry_accepts_code_tokens_and_aliases() {
    let reg = registry(rt());
    for (alias, canonical) in [
        ("mod", "module"),
        ("namespace", "module"),
        ("fn", "function"),
        ("func", "function"),
        ("method", "function"),
        ("enum", "datatype"),
        ("record", "datatype"),
        ("type_alias", "datatype"),
        ("trait", "interface"),
        ("protocol", "interface"),
    ] {
        let created = dispatch(
            &reg,
            "create",
            json!({"kind": "entity", "name": format!("Node-{alias}"), "entity_kind": "concept", "entity_type": alias}),
        )
        .await
        .unwrap_or_else(|e| panic!("alias {alias:?} must be creatable: {e}"));
        assert_eq!(
            created["entity_type"], canonical,
            "alias {alias:?} must canonicalize to {canonical:?}"
        );
    }
}

/// `struct`/`class` must remain owned by formal's `structure` token even when
/// the code pack is registered — the code pack must not shadow them with
/// `datatype`. See also `khive-pack-kg`'s unit-level
/// `entity_type_registry_does_not_claim_struct_or_class_for_code`.
#[tokio::test]
async fn entity_type_registry_does_not_claim_struct_or_class_for_code() {
    let reg = registry(rt());
    for alias in ["struct", "class"] {
        let created = dispatch(
            &reg,
            "create",
            json!({"kind": "entity", "name": format!("Node-{alias}"), "entity_kind": "concept", "entity_type": alias}),
        )
        .await
        .unwrap_or_else(|e| panic!("alias {alias:?} must be creatable: {e}"));
        assert_eq!(
            created["entity_type"], "structure",
            "alias {alias:?} must remain owned by formal's structure, not datatype"
        );
    }
}

#[test]
fn code_edge_rules_match_adr085_triples() {
    let pack = CodePack::new(rt());
    let rules = pack.edge_rules();
    assert_eq!(
        rules.len(),
        22,
        "ADR-085 declares exactly 22 additive edge rules"
    );

    fn concept(entity_type: &'static str) -> EndpointKind {
        EndpointKind::EntityOfType {
            kind: "concept",
            entity_type,
        }
    }

    let expected: Vec<(EdgeRelation, EndpointKind, EndpointKind)> = vec![
        (
            EdgeRelation::DependsOn,
            concept("function"),
            concept("function"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("function"),
            concept("datatype"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("function"),
            concept("interface"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("datatype"),
            concept("datatype"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("datatype"),
            concept("interface"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("interface"),
            concept("interface"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("interface"),
            concept("datatype"),
        ),
        (
            EdgeRelation::DependsOn,
            concept("module"),
            concept("module"),
        ),
        (
            EdgeRelation::Contains,
            EndpointKind::EntityOfKind("project"),
            concept("module"),
        ),
        (
            EdgeRelation::Contains,
            EndpointKind::EntityOfKind("project"),
            concept("function"),
        ),
        (
            EdgeRelation::Contains,
            EndpointKind::EntityOfKind("project"),
            concept("datatype"),
        ),
        (
            EdgeRelation::Contains,
            EndpointKind::EntityOfKind("project"),
            concept("interface"),
        ),
        (
            EdgeRelation::Implements,
            concept("datatype"),
            concept("interface"),
        ),
        (
            EdgeRelation::Implements,
            concept("function"),
            EndpointKind::EntityOfKind("concept"),
        ),
        (
            EdgeRelation::Implements,
            concept("datatype"),
            EndpointKind::EntityOfKind("concept"),
        ),
        (
            EdgeRelation::Implements,
            concept("module"),
            EndpointKind::EntityOfKind("concept"),
        ),
        (EdgeRelation::Contains, concept("module"), concept("module")),
        (
            EdgeRelation::Contains,
            concept("module"),
            concept("function"),
        ),
        (
            EdgeRelation::Contains,
            concept("module"),
            concept("datatype"),
        ),
        (
            EdgeRelation::Contains,
            concept("module"),
            concept("interface"),
        ),
        (
            EdgeRelation::Extends,
            concept("interface"),
            concept("interface"),
        ),
        (
            EdgeRelation::Extends,
            concept("datatype"),
            concept("datatype"),
        ),
    ];
    assert_eq!(
        expected.len(),
        22,
        "expected fixture itself must list all 22 ADR-085 triples"
    );

    for (relation, source, target) in &expected {
        assert!(
            rules
                .iter()
                .any(|r| r.relation == *relation && r.source == *source && r.target == *target),
            "missing ADR-085 rule {relation:?} {source:?} -> {target:?}"
        );
    }
}

async fn create_concept(registry: &VerbRegistry, name: &str, entity_type: &str) -> String {
    let created = dispatch(
        registry,
        "create",
        json!({"kind": "entity", "name": name, "entity_kind": "concept", "entity_type": entity_type}),
    )
    .await
    .unwrap_or_else(|e| panic!("create {entity_type} entity must succeed: {e}"));
    created["id"]
        .as_str()
        .expect("created entity has id")
        .to_string()
}

async fn create_project(registry: &VerbRegistry, name: &str) -> String {
    let created = dispatch(
        registry,
        "create",
        json!({"kind": "entity", "name": name, "entity_kind": "project"}),
    )
    .await
    .unwrap_or_else(|e| panic!("create project entity must succeed: {e}"));
    created["id"]
        .as_str()
        .expect("created entity has id")
        .to_string()
}

#[tokio::test]
async fn link_accepts_function_depends_on_datatype_with_code_pack() {
    let reg = registry(rt());
    let src = create_concept(&reg, "DoTheThing", "function").await;
    let tgt = create_concept(&reg, "ThingRecord", "datatype").await;
    let result = dispatch(
        &reg,
        "link",
        json!({"source_id": src, "target_id": tgt, "relation": "depends_on", "weight": 1.0}),
    )
    .await;
    assert!(
        result.is_ok(),
        "function depends_on datatype must be accepted with kg,code registered: {result:?}"
    );
}

#[tokio::test]
async fn link_accepts_project_contains_module_with_code_pack() {
    let reg = registry(rt());
    let src = create_project(&reg, "khive-pack-code").await;
    let tgt = create_concept(&reg, "ingest", "module").await;
    let result = dispatch(
        &reg,
        "link",
        json!({"source_id": src, "target_id": tgt, "relation": "contains", "weight": 1.0}),
    )
    .await;
    assert!(
        result.is_ok(),
        "project contains module must be accepted with kg,code registered: {result:?}"
    );
}

#[tokio::test]
async fn link_rejects_function_depends_on_datatype_without_code_pack() {
    let reg = kg_only_registry(rt());
    let src = create_concept(&reg, "DoTheThing", "function").await;
    let tgt = create_concept(&reg, "ThingRecord", "datatype").await;
    let err = dispatch(
        &reg,
        "link",
        json!({"source_id": src, "target_id": tgt, "relation": "depends_on", "weight": 1.0}),
    )
    .await
    .expect_err("function depends_on datatype must be rejected without the code pack");
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn link_rejects_function_depends_on_project_with_code_pack() {
    let reg = registry(rt());
    let src = create_concept(&reg, "DoTheThing", "function").await;
    let tgt = create_project(&reg, "khive-pack-code").await;
    let err = dispatch(
        &reg,
        "link",
        json!({"source_id": src, "target_id": tgt, "relation": "depends_on", "weight": 1.0}),
    )
    .await
    .expect_err("function depends_on project has no ADR-085 rule and must remain rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn create_finding_defaults_kind_status_open() {
    let reg = registry(rt());
    let resp = dispatch(
        &reg,
        "create",
        json!({
            "kind": "finding",
            "title": "Missing bounds check",
            "properties": {"severity": "high", "confidence": "high"},
        }),
    )
    .await
    .expect("create(kind=finding) with valid severity/confidence must succeed");
    assert_eq!(resp["properties"]["kind_status"], "open");
}

#[tokio::test]
async fn create_finding_rejects_invalid_severity_with_valid_values() {
    let reg = registry(rt());
    let err = dispatch(
        &reg,
        "create",
        json!({
            "kind": "finding",
            "title": "Missing bounds check",
            "properties": {"severity": "catastrophic", "confidence": "high"},
        }),
    )
    .await
    .expect_err("invalid severity must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("critical, high, medium, low, info"),
        "error must name valid severities, got: {msg}"
    );
}

#[tokio::test]
async fn create_finding_rejects_invalid_confidence_with_valid_values() {
    let reg = registry(rt());
    let err = dispatch(
        &reg,
        "create",
        json!({
            "kind": "finding",
            "title": "Missing bounds check",
            "properties": {"severity": "high", "confidence": "sorta"},
        }),
    )
    .await
    .expect_err("invalid confidence must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("high, medium, low"),
        "error must name valid confidences, got: {msg}"
    );
}

#[tokio::test]
async fn create_finding_rejects_non_object_properties() {
    let reg = registry(rt());
    let err = dispatch(
        &reg,
        "create",
        json!({
            "kind": "finding",
            "title": "Missing bounds check",
            "properties": "high",
        }),
    )
    .await
    .expect_err("a present non-object properties must be rejected, not coerced");
    let msg = err.to_string();
    assert!(
        msg.contains("properties must be an object"),
        "error must name the properties-shape requirement, got: {msg}"
    );
}

fn valid_findings_bytes() -> Vec<u8> {
    serde_json::to_vec(&sample_findings_json()).expect("sample findings.json serializes")
}

#[test]
fn ingest_findings_json_same_input_same_ids() {
    let bytes = valid_findings_bytes();
    let opts_a = ingest_options(Some("fixed-run"));
    let opts_b = ingest_options(Some("fixed-run"));

    let batch_a = ingest_findings_json(&bytes, opts_a).expect("first ingest must succeed");
    let batch_b = ingest_findings_json(&bytes, opts_b).expect("second ingest must succeed");

    assert_eq!(batch_a.entities.len(), 1);
    assert_eq!(batch_b.entities.len(), 1);
    assert_eq!(batch_a.entities[0].id, batch_b.entities[0].id);

    assert_eq!(batch_a.notes.len(), batch_b.notes.len());
    for (a, b) in batch_a.notes.iter().zip(batch_b.notes.iter()) {
        assert_eq!(
            a.id, b.id,
            "re-ingesting the same findings.json must reproduce the same note id"
        );
    }

    assert_eq!(batch_a.edges.len(), batch_b.edges.len());
    for (a, b) in batch_a.edges.iter().zip(batch_b.edges.iter()) {
        assert_eq!(
            a.id, b.id,
            "re-ingesting the same findings.json must reproduce the same edge id"
        );
    }
}

#[test]
fn ingest_findings_json_same_ids_across_different_observed_at() {
    let bytes = valid_findings_bytes();
    let early: DateTime<Utc> = "2020-01-01T00:00:00Z".parse().expect("valid rfc3339");
    let late: DateTime<Utc> = "2030-06-15T12:30:00Z".parse().expect("valid rfc3339");

    let batch_early = ingest_findings_json(
        &bytes,
        CodeIngestOptions {
            namespace: "local",
            observed_at: early,
            source_run: Some("fixed-run"),
        },
    )
    .expect("ingest at early timestamp must succeed");
    let batch_late = ingest_findings_json(
        &bytes,
        CodeIngestOptions {
            namespace: "local",
            observed_at: late,
            source_run: Some("fixed-run"),
        },
    )
    .expect("ingest at late timestamp must succeed");

    assert_eq!(
        batch_early.entities[0].id, batch_late.entities[0].id,
        "observed_at must be excluded from identity"
    );
    assert_eq!(batch_early.notes[0].id, batch_late.notes[0].id);
    assert_eq!(batch_early.edges[0].id, batch_late.edges[0].id);
}

#[test]
fn ingest_different_content_produces_different_finding_id() {
    // Same external id/title/evidence path as `sample_findings_json`, but a
    // changed content field (severity). The finding id must diverge — a
    // content change must never collide with the prior record's id.
    let bytes_a = valid_findings_bytes();
    let mut doc_b = sample_findings_json();
    doc_b["findings"][0]["severity"] = json!("critical");
    let bytes_b = serde_json::to_vec(&doc_b).expect("serializes");

    let batch_a = ingest_findings_json(&bytes_a, ingest_options(Some("fixed-run")))
        .expect("first ingest must succeed");
    let batch_b = ingest_findings_json(&bytes_b, ingest_options(Some("fixed-run")))
        .expect("second ingest must succeed");

    assert_ne!(
        batch_a.notes[0].id, batch_b.notes[0].id,
        "changing finding content (severity) must produce a different finding id"
    );
    assert_ne!(
        batch_a.edges[0].id, batch_b.edges[0].id,
        "the finding's annotate-edge id derives from the finding id and must also diverge"
    );
    // The unrelated project entity is scoped by repo/scope only, so it is
    // unaffected by a finding-level content change — the old record's
    // entity is left untouched, as the amendment requires.
    assert_eq!(batch_a.entities[0].id, batch_b.entities[0].id);
}

#[test]
fn ingest_rejects_missing_findings_array() {
    let malformed = json!({
        "audit": {
            "date": "2026-07-07",
            "scope": "khive-pack-code",
            "repo": "khive",
            "branch": "feature/code-pack",
            "commit": "abc123",
            "standards_file": "audit-guidelines.md",
        },
    });
    let bytes = serde_json::to_vec(&malformed).expect("serializes");
    let err = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect_err("missing findings array must be rejected");
    assert!(
        matches!(err, CodeIngestError::InvalidRoot),
        "expected InvalidRoot, got: {err:?}"
    );
}

#[test]
fn ingest_rejects_invalid_confidence_before_records() {
    let mut doc = sample_findings_json();
    doc["findings"][0]["confidence"] = json!("sorta");
    let bytes = serde_json::to_vec(&doc).expect("serializes");
    let err = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect_err("invalid confidence must be rejected before any record is built");
    match err {
        CodeIngestError::InvalidValue { field, valid, .. } => {
            assert_eq!(field, "confidence");
            assert_eq!(valid, "high | medium | low");
        }
        other => panic!("expected InvalidValue{{field: confidence}}, got: {other:?}"),
    }
}

#[test]
fn ingest_rejects_invalid_severity_before_records() {
    let mut doc = sample_findings_json();
    doc["findings"][0]["severity"] = json!("catastrophic");
    let bytes = serde_json::to_vec(&doc).expect("serializes");
    let err = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect_err("invalid severity must be rejected before any record is built");
    match err {
        CodeIngestError::InvalidValue { field, valid, .. } => {
            assert_eq!(field, "severity");
            assert_eq!(valid, "critical | high | medium | low | info");
        }
        other => panic!("expected InvalidValue{{field: severity}}, got: {other:?}"),
    }
}

#[test]
fn ingest_tolerates_out_of_vocab_priority() {
    // ADR-085 Amendment 1 A1: `priority` is ungoverned — ingest neither
    // rejects nor coerces it, it preserves whatever value was provided.
    let mut doc = sample_findings_json();
    doc["findings"][0]["priority"] = json!("P9");
    let bytes = serde_json::to_vec(&doc).expect("serializes");
    let batch = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect("out-of-vocab priority must be tolerated, not rejected");
    assert_eq!(
        batch.notes[0].properties.as_ref().expect("has properties")["priority"],
        json!("P9"),
        "tolerated priority value must be preserved as-is"
    );
}

#[test]
fn ingest_rejects_invalid_evidence_shape() {
    let mut doc = sample_findings_json();
    doc["findings"][0]["evidence"] = json!([42]);
    let bytes = serde_json::to_vec(&doc).expect("serializes");
    let err = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect_err("evidence entries that are neither string nor object must be rejected");
    assert!(
        matches!(err, CodeIngestError::InvalidEvidence { .. }),
        "expected InvalidEvidence, got: {err:?}"
    );
}

#[test]
fn ingest_requires_failure_scenario_for_medium_or_higher() {
    let mut doc = sample_findings_json();
    doc["findings"][0]["severity"] = json!("medium");
    doc["findings"][0]
        .as_object_mut()
        .expect("finding is an object")
        .remove("failure_scenario");
    let bytes = serde_json::to_vec(&doc).expect("serializes");
    let err = ingest_findings_json(&bytes, ingest_options(Some("run")))
        .expect_err("medium severity without failure_scenario must be rejected");
    match err {
        CodeIngestError::MissingFailureScenario { severity, .. } => {
            assert_eq!(severity, "medium");
        }
        other => panic!("expected MissingFailureScenario, got: {other:?}"),
    }
}
