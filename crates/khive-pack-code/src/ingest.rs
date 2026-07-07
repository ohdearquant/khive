//! Pure `findings.json -> Vec<Entity>, Vec<Note>, Vec<Edge>` mapper.
//!
//! `ingest_findings_json` validates the entire input before constructing any
//! output record, so a malformed file never yields a partial batch. Record
//! identity is a content-derived UUIDv5 hash of the validated finding record
//! (`observed_at` excluded), so re-ingesting the same `findings.json` under
//! the same [`CodeIngestOptions`] reproduces the same entity, note, and edge
//! IDs, while a content change (severity, evidence, impact, ...) produces a
//! new finding id rather than colliding with the prior record.
//!
//! Only `severity` and `confidence` values, the evidence shape, and
//! `failure_scenario` presence are governed at ingest time — they reject
//! unknown/malformed values by design (ADR-085 D4 "fail closed; no silent
//! coercion"). Every other field (`categories`, `standard`, `refs`,
//! `priority`, raw audit `status`, `impact`, `recommendation`,
//! `verification`) is tolerated per ADR-085 Amendment 1 A1: ingest neither
//! rejects nor coerces them, it preserves whatever JSON value was provided
//! and omits the key when the field is absent. Raw `status` is preserved
//! verbatim under `properties.audit_status` — it has no agreed mapping to
//! the finding lifecycle (`kind_status`) yet.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use khive_storage::{Edge, Entity, LinkId, Note};
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::CodeIngestError;
use crate::vocab::{is_valid_confidence, is_valid_severity};

/// UUIDv5 namespace for all code-pack ingest identity, derived from
/// `Uuid::new_v5(Uuid::NAMESPACE_URL, b"https://github.com/ohdearquant/khive/adr/085/code-pack/v1")`.
pub const CODE_INGEST_NAMESPACE: Uuid = Uuid::from_u128(0x288fe3dc_ac69_5aef_ab1e_9b170fb07376);

/// Options controlling one `ingest_findings_json` call.
#[derive(Clone, Debug)]
pub struct CodeIngestOptions<'a> {
    /// KG namespace the produced records belong to.
    pub namespace: &'a str,
    /// Wall-clock timestamp stamped on produced records. Excluded from every
    /// identity tuple so re-ingesting the same sweep at a later time still
    /// reproduces the same IDs.
    pub observed_at: DateTime<Utc>,
    /// Stable sweep identity. When absent, derived as `audit.date:audit.commit`.
    pub source_run: Option<&'a str>,
}

/// Output of a successful ingest: deterministic entity/note/edge records
/// ready for the caller to persist through existing storage/runtime paths.
#[derive(Clone, Debug)]
pub struct CodeIngestBatch {
    pub entities: Vec<Entity>,
    pub notes: Vec<Note>,
    pub edges: Vec<Edge>,
}

fn uuid5_tuple<T: serde::Serialize>(parts: &T) -> Result<Uuid, CodeIngestError> {
    let bytes = serde_json::to_vec(parts)?;
    Ok(Uuid::new_v5(&CODE_INGEST_NAMESPACE, &bytes))
}

/// Recursively sort object keys so two JSON values that differ only in key
/// order serialize identically. Array element order is content and is left
/// untouched.
fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&str, Value> = map
                .iter()
                .map(|(k, v)| (k.as_str(), sort_json_keys(v)))
                .collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.to_string(), v);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

/// Render a tolerated (ungoverned) field for inclusion in note content text.
/// Strings pass through verbatim; any other JSON shape renders as its
/// canonical JSON text; absence renders as empty.
fn value_to_display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// A tolerated field is present when the key exists and its value is not
/// JSON `null` — an explicit `null` is treated the same as absence.
fn tolerated_field(obj: &Map<String, Value>, key: &str) -> Option<Value> {
    match obj.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.clone()),
    }
}

fn require_str<'a>(
    obj: &'a Map<String, Value>,
    key: &'static str,
    path: &'static str,
) -> Result<&'a str, CodeIngestError> {
    match obj.get(key) {
        None => Err(CodeIngestError::MissingField { path }),
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(_) => Err(CodeIngestError::InvalidType {
            path: path.to_string(),
            expected: "string",
        }),
    }
}

fn optional_str(
    obj: &Map<String, Value>,
    key: &'static str,
    path: &'static str,
) -> Result<Option<String>, CodeIngestError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(CodeIngestError::InvalidType {
            path: path.to_string(),
            expected: "string",
        }),
    }
}

fn normalize_title(title: &str) -> Result<String, CodeIngestError> {
    let normalized = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(CodeIngestError::InvalidType {
            path: "findings[].title".to_string(),
            expected: "non-empty string",
        });
    }
    Ok(normalized)
}

/// Canonicalize the tolerated `evidence` shapes (null, string, object, array
/// of strings/objects) into an array of `{description}`-or-richer objects.
/// Any other shape is rejected with the finding/evidence index named.
fn canonicalize_evidence(
    value: Option<&Value>,
    finding_index: usize,
) -> Result<Vec<Value>, CodeIngestError> {
    let items: Vec<Value> = match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Object(m)) => vec![Value::Object(m.clone())],
        Some(Value::Array(arr)) => arr.clone(),
        Some(_) => {
            return Err(CodeIngestError::InvalidEvidence {
                finding_index,
                evidence_index: 0,
            });
        }
    };

    let mut out = Vec::with_capacity(items.len());
    for (evidence_index, item) in items.into_iter().enumerate() {
        let canonical = match item {
            Value::String(s) => {
                if s.trim().is_empty() {
                    return Err(CodeIngestError::InvalidEvidence {
                        finding_index,
                        evidence_index,
                    });
                }
                json!({ "description": s })
            }
            Value::Object(m) => Value::Object(m),
            _ => {
                return Err(CodeIngestError::InvalidEvidence {
                    finding_index,
                    evidence_index,
                });
            }
        };
        out.push(canonical);
    }
    Ok(out)
}

struct ValidatedAudit {
    date: String,
    scope: String,
    repo: String,
    branch: String,
    commit: String,
    standards_file: String,
    extra: Map<String, Value>,
}

const KNOWN_AUDIT_KEYS: &[&str] = &[
    "date",
    "scope",
    "repo",
    "branch",
    "commit",
    "standards_file",
];

impl ValidatedAudit {
    fn parse(obj: &Map<String, Value>) -> Result<Self, CodeIngestError> {
        let mut extra = Map::new();
        for (k, v) in obj {
            if !KNOWN_AUDIT_KEYS.contains(&k.as_str()) {
                extra.insert(k.clone(), v.clone());
            }
        }
        Ok(Self {
            date: require_str(obj, "date", "audit.date")?.to_string(),
            scope: require_str(obj, "scope", "audit.scope")?.to_string(),
            repo: require_str(obj, "repo", "audit.repo")?.to_string(),
            branch: require_str(obj, "branch", "audit.branch")?.to_string(),
            commit: require_str(obj, "commit", "audit.commit")?.to_string(),
            standards_file: require_str(obj, "standards_file", "audit.standards_file")?.to_string(),
            extra,
        })
    }
}

struct ValidatedFinding {
    id: String,
    title: String,
    normalized_title: String,
    severity: String,
    confidence: String,
    categories: Option<Value>,
    standard: Option<Value>,
    evidence: Vec<Value>,
    impact: Option<Value>,
    recommendation: Option<Value>,
    verification: Option<Value>,
    failure_scenario: Option<String>,
    priority: Option<Value>,
    audit_status: Option<Value>,
    refs: Option<Value>,
    raw: Map<String, Value>,
}

const KNOWN_FINDING_KEYS: &[&str] = &[
    "id",
    "title",
    "severity",
    "confidence",
    "categories",
    "status",
    "standard",
    "evidence",
    "impact",
    "recommendation",
    "verification",
    "failure_scenario",
    "priority",
    "refs",
];

impl ValidatedFinding {
    fn parse(obj: &Map<String, Value>, finding_index: usize) -> Result<Self, CodeIngestError> {
        let id = require_str(obj, "id", "findings[].id")?.to_string();
        let title = require_str(obj, "title", "findings[].title")?.to_string();
        if title.trim().is_empty() {
            return Err(CodeIngestError::InvalidType {
                path: "findings[].title".to_string(),
                expected: "non-empty string",
            });
        }
        let normalized_title = normalize_title(&title)?;

        let severity = require_str(obj, "severity", "findings[].severity")?.to_string();
        if !is_valid_severity(&severity) {
            return Err(CodeIngestError::InvalidValue {
                field: "severity",
                value: severity,
                valid: "critical | high | medium | low | info",
            });
        }

        let confidence = require_str(obj, "confidence", "findings[].confidence")?.to_string();
        if !is_valid_confidence(&confidence) {
            return Err(CodeIngestError::InvalidValue {
                field: "confidence",
                value: confidence,
                valid: "high | medium | low",
            });
        }

        // `categories`, `standard`, `refs`, `priority`, `status`, `impact`,
        // `recommendation`, and `verification` are tolerated (ADR-085
        // Amendment 1 A1): ingest neither rejects nor coerces their shape,
        // it preserves whatever JSON value was provided or omits the key
        // when absent. Only `evidence` shape and `severity`/`confidence`/
        // `failure_scenario` presence remain fail-closed.
        let categories = tolerated_field(obj, "categories");
        let standard = tolerated_field(obj, "standard");
        let evidence = canonicalize_evidence(obj.get("evidence"), finding_index)?;
        let impact = tolerated_field(obj, "impact");
        let recommendation = tolerated_field(obj, "recommendation");
        let verification = tolerated_field(obj, "verification");
        let failure_scenario =
            optional_str(obj, "failure_scenario", "findings[].failure_scenario")?;
        let audit_status = tolerated_field(obj, "status");
        let priority = tolerated_field(obj, "priority");
        let refs = tolerated_field(obj, "refs");

        if matches!(severity.as_str(), "medium" | "high" | "critical") {
            let has_scenario = failure_scenario
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            if !has_scenario {
                return Err(CodeIngestError::MissingFailureScenario {
                    id: id.clone(),
                    severity: severity.clone(),
                });
            }
        }

        let mut raw = Map::new();
        for (k, v) in obj {
            if !KNOWN_FINDING_KEYS.contains(&k.as_str()) {
                raw.insert(k.clone(), v.clone());
            }
        }

        Ok(Self {
            id,
            title,
            normalized_title,
            severity,
            confidence,
            categories,
            standard,
            evidence,
            impact,
            recommendation,
            verification,
            failure_scenario,
            priority,
            audit_status,
            refs,
            raw,
        })
    }
}

/// Map a `findings.json` document into deterministic KG records.
///
/// Validates the entire document before constructing any output record —
/// malformed input never produces a partial batch. Output identity is
/// content-derived (see module docs), so calling this twice with the same
/// bytes and options reproduces identical entity/note/edge IDs.
pub fn ingest_findings_json(
    input: &[u8],
    options: CodeIngestOptions<'_>,
) -> Result<CodeIngestBatch, CodeIngestError> {
    let root: Value = serde_json::from_slice(input)?;
    let root_obj = root.as_object().ok_or(CodeIngestError::InvalidRoot)?;

    let audit_obj = root_obj
        .get("audit")
        .and_then(Value::as_object)
        .ok_or(CodeIngestError::InvalidRoot)?;
    let findings_arr = root_obj
        .get("findings")
        .and_then(Value::as_array)
        .ok_or(CodeIngestError::InvalidRoot)?;

    let audit = ValidatedAudit::parse(audit_obj)?;

    let source_run = match options.source_run.filter(|s| !s.trim().is_empty()) {
        Some(explicit) => explicit.to_string(),
        None => {
            if audit.date.trim().is_empty() || audit.commit.trim().is_empty() {
                return Err(CodeIngestError::MissingSourceRun);
            }
            format!("{}:{}", audit.date, audit.commit)
        }
    };

    let mut validated_findings = Vec::with_capacity(findings_arr.len());
    for (finding_index, raw) in findings_arr.iter().enumerate() {
        let obj = raw
            .as_object()
            .ok_or_else(|| CodeIngestError::InvalidType {
                path: format!("findings[{finding_index}]"),
                expected: "object",
            })?;
        validated_findings.push(ValidatedFinding::parse(obj, finding_index)?);
    }

    // All validation above must succeed before any record is constructed.

    let project_id = uuid5_tuple(&(
        "code-project",
        1u8,
        options.namespace,
        audit.repo.as_str(),
        audit.scope.as_str(),
    ))?;
    let project_external_id = serde_json::to_string(&(
        "code-project",
        1u8,
        options.namespace,
        audit.repo.as_str(),
        audit.scope.as_str(),
    ))?;

    let mut project_properties = Map::new();
    project_properties.insert("repo".into(), json!(audit.repo));
    project_properties.insert("branch".into(), json!(audit.branch));
    project_properties.insert("commit".into(), json!(audit.commit));
    project_properties.insert("date".into(), json!(audit.date));
    project_properties.insert("standards_file".into(), json!(audit.standards_file));
    project_properties.insert("source_run".into(), json!(source_run));
    project_properties.insert("external_id".into(), json!(project_external_id));
    if !audit.extra.is_empty() {
        project_properties.insert("audit_extra".into(), Value::Object(audit.extra));
    }

    let observed_micros = options.observed_at.timestamp_micros();

    let mut project_entity = Entity::new(options.namespace, "project", audit.scope.as_str());
    project_entity.id = project_id;
    project_entity.properties = Some(Value::Object(project_properties));
    project_entity.created_at = observed_micros;
    project_entity.updated_at = observed_micros;

    let mut notes = Vec::with_capacity(validated_findings.len());
    let mut edges = Vec::with_capacity(validated_findings.len());

    for finding in &validated_findings {
        // Identity is a canonical content hash of the validated finding,
        // `observed_at` excluded: same content re-ingested at a different
        // time reproduces the same id, and any content change (severity,
        // evidence, impact, ...) produces a different id rather than
        // silently overwriting the prior record under the old one.
        let identity_value = sort_json_keys(&json!({
            "kind": "code-finding",
            "schema_version": 1,
            "namespace": options.namespace,
            "source_run": source_run,
            "scope": audit.scope,
            "id": finding.id,
            "normalized_title": finding.normalized_title,
            "severity": finding.severity,
            "confidence": finding.confidence,
            "categories": finding.categories,
            "standard": finding.standard,
            "evidence": finding.evidence,
            "impact": finding.impact,
            "recommendation": finding.recommendation,
            "verification": finding.verification,
            "failure_scenario": finding.failure_scenario,
            "priority": finding.priority,
            "audit_status": finding.audit_status,
            "refs": finding.refs,
            "raw": finding.raw,
        }));
        let finding_id = uuid5_tuple(&identity_value)?;
        let finding_external_id = serde_json::to_string(&identity_value)?;

        let mut props = Map::new();
        props.insert("external_id".into(), json!(finding_external_id));
        props.insert("finding_id".into(), json!(finding.id));
        props.insert("severity".into(), json!(finding.severity));
        props.insert("confidence".into(), json!(finding.confidence));
        props.insert("source_run".into(), json!(source_run));
        props.insert("evidence".into(), Value::Array(finding.evidence.clone()));
        if let Some(categories) = &finding.categories {
            props.insert("categories".into(), categories.clone());
        }
        if let Some(standard) = &finding.standard {
            props.insert("standard".into(), standard.clone());
        }
        if let Some(refs) = &finding.refs {
            props.insert("refs".into(), refs.clone());
        }
        if let Some(priority) = &finding.priority {
            props.insert("priority".into(), priority.clone());
        }
        if let Some(status) = &finding.audit_status {
            props.insert("audit_status".into(), status.clone());
        }
        if let Some(scenario) = &finding.failure_scenario {
            props.insert("failure_scenario".into(), json!(scenario));
        }
        if let Some(impact) = &finding.impact {
            props.insert("impact".into(), impact.clone());
        }
        if let Some(recommendation) = &finding.recommendation {
            props.insert("recommendation".into(), recommendation.clone());
        }
        if let Some(verification) = &finding.verification {
            props.insert("verification".into(), verification.clone());
        }
        props.insert("kind_status".into(), json!("open"));
        if !finding.raw.is_empty() {
            props.insert("raw".into(), Value::Object(finding.raw.clone()));
        }

        let content = format!(
            "{}: {}",
            finding.title,
            value_to_display(finding.impact.as_ref())
        );
        let mut note = Note::new(options.namespace, "finding", content);
        note.id = finding_id;
        note.name = Some(finding.title.clone());
        note.properties = Some(Value::Object(props));
        note.created_at = observed_micros;
        note.updated_at = observed_micros;
        notes.push(note);

        let edge_id = uuid5_tuple(&(
            "code-edge",
            1u8,
            options.namespace,
            finding_id,
            project_id,
            EdgeRelation::Annotates.as_str(),
        ))?;
        edges.push(Edge {
            id: LinkId::from(edge_id),
            namespace: options.namespace.to_string(),
            source_id: finding_id,
            target_id: project_id,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: options.observed_at,
            updated_at: options.observed_at,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        });
    }

    Ok(CodeIngestBatch {
        entities: vec![project_entity],
        notes,
        edges,
    })
}
