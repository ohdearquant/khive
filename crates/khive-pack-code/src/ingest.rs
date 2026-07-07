//! Pure `findings.json -> Vec<Entity>, Vec<Note>, Vec<Edge>` mapper.
//!
//! `ingest_findings_json` validates the entire input before constructing any
//! output record, so a malformed file never yields a partial batch. Record
//! identity is content-derived (UUIDv5 over JSON tuples), so re-ingesting the
//! same `findings.json` under the same [`CodeIngestOptions`] reproduces the
//! same entity, note, and edge IDs.
//!
//! Only `severity`, `confidence`, `priority`, and the evidence shape are
//! governed at ingest time — they reject unknown/malformed values by design
//! (ADR-085 D4 "fail closed; no silent coercion"). Raw audit `status` has no
//! agreed mapping to the finding lifecycle (`kind_status`) yet, so it is
//! preserved verbatim under `properties.audit_status` rather than validated
//! or coerced.

use chrono::{DateTime, Utc};
use khive_storage::{Edge, Entity, LinkId, Note};
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::CodeIngestError;
use crate::vocab::{is_valid_confidence, is_valid_priority, is_valid_severity};

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

fn first_evidence_path(evidence: &[Value]) -> Option<String> {
    evidence
        .iter()
        .find_map(|e| e.get("path").and_then(Value::as_str).map(str::to_string))
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
    categories: Vec<String>,
    standard: String,
    evidence: Vec<Value>,
    impact: String,
    recommendation: String,
    verification: String,
    failure_scenario: Option<String>,
    priority: Option<String>,
    audit_status: Option<String>,
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

        let categories = match obj.get("categories") {
            None => Vec::new(),
            Some(Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    match item {
                        Value::String(s) => out.push(s.clone()),
                        _ => {
                            return Err(CodeIngestError::InvalidType {
                                path: "findings[].categories".to_string(),
                                expected: "array of strings",
                            });
                        }
                    }
                }
                out
            }
            Some(_) => {
                return Err(CodeIngestError::InvalidType {
                    path: "findings[].categories".to_string(),
                    expected: "array of strings",
                });
            }
        };

        let standard =
            optional_str(obj, "standard", "findings[].standard")?.unwrap_or_else(String::new);
        let evidence = canonicalize_evidence(obj.get("evidence"), finding_index)?;
        let impact = optional_str(obj, "impact", "findings[].impact")?.unwrap_or_else(String::new);
        let recommendation = optional_str(obj, "recommendation", "findings[].recommendation")?
            .unwrap_or_else(String::new);
        let verification = optional_str(obj, "verification", "findings[].verification")?
            .unwrap_or_else(String::new);
        let failure_scenario =
            optional_str(obj, "failure_scenario", "findings[].failure_scenario")?;
        let audit_status = optional_str(obj, "status", "findings[].status")?;

        let priority = optional_str(obj, "priority", "findings[].priority")?;
        if let Some(p) = &priority {
            if !is_valid_priority(p) {
                return Err(CodeIngestError::InvalidValue {
                    field: "priority",
                    value: p.clone(),
                    valid: "P0 | P1 | P2 | P3",
                });
            }
        }

        let refs = match obj.get("refs") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some(Value::Object(m.clone())),
            Some(_) => {
                return Err(CodeIngestError::InvalidType {
                    path: "findings[].refs".to_string(),
                    expected: "object",
                });
            }
        };

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
        let first_path = first_evidence_path(&finding.evidence);
        let finding_key = (
            "code-finding",
            1u8,
            options.namespace,
            source_run.as_str(),
            audit.scope.as_str(),
            finding.id.as_str(),
            finding.normalized_title.as_str(),
            first_path.as_deref().unwrap_or(""),
        );
        let finding_id = uuid5_tuple(&finding_key)?;
        let finding_external_id = serde_json::to_string(&finding_key)?;

        let mut props = Map::new();
        props.insert("external_id".into(), json!(finding_external_id));
        props.insert("finding_id".into(), json!(finding.id));
        props.insert("severity".into(), json!(finding.severity));
        props.insert("confidence".into(), json!(finding.confidence));
        props.insert("categories".into(), json!(finding.categories));
        props.insert("source_run".into(), json!(source_run));
        props.insert("standard".into(), json!(finding.standard));
        props.insert("evidence".into(), Value::Array(finding.evidence.clone()));
        if let Some(refs) = &finding.refs {
            props.insert("refs".into(), refs.clone());
        }
        if let Some(priority) = &finding.priority {
            props.insert("priority".into(), json!(priority));
        }
        if let Some(status) = &finding.audit_status {
            props.insert("audit_status".into(), json!(status));
        }
        if let Some(scenario) = &finding.failure_scenario {
            props.insert("failure_scenario".into(), json!(scenario));
        }
        props.insert("impact".into(), json!(finding.impact));
        props.insert("recommendation".into(), json!(finding.recommendation));
        props.insert("verification".into(), json!(finding.verification));
        props.insert("kind_status".into(), json!("open"));
        if !finding.raw.is_empty() {
            props.insert("raw".into(), Value::Object(finding.raw.clone()));
        }

        let content = format!("{}: {}", finding.title, finding.impact);
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
