//! Shared stored-record finalization boundary for ADR-115.
//!
//! Every admission-capable entity, note, or knowledge writer supplies the
//! final values it will persist here. The boundary takes one immutable
//! manifest snapshot, applies the unchanged detector to every runtime-owned
//! field scope, and is the only code that can synthesize the reserved posture
//! stamp and its target-bound audit event.

use std::sync::{Arc, LazyLock};

#[cfg(any(test, feature = "test-internals"))]
use std::collections::HashMap;
#[cfg(any(test, feature = "test-internals"))]
use std::sync::Mutex;

use khive_storage::event::Event;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{EventKind, SubstrateKind};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;
use crate::secret_gate::{self, RESERVED_SECRET_GATE_KEY, SECRET_GATE_EXEMPTION_STAMP};

use super::manifest::{digest_to_hex, scoped_digest, ManifestSnapshot, RuntimeFieldScope};

static PRODUCTION_MANIFEST: LazyLock<Arc<ManifestSnapshot>> =
    LazyLock::new(ManifestSnapshot::empty);

/// Public spelling of ADR-115's closed runtime-owned field scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretGateFieldScope {
    RecordContent,
    NameDescription,
    JsonProperties,
    Tags,
    CodeSource,
}

impl From<SecretGateFieldScope> for RuntimeFieldScope {
    fn from(value: SecretGateFieldScope) -> Self {
        match value {
            SecretGateFieldScope::RecordContent => Self::RecordContent,
            SecretGateFieldScope::NameDescription => Self::NameDescription,
            SecretGateFieldScope::JsonProperties => Self::JsonProperties,
            SecretGateFieldScope::Tags => Self::Tags,
            SecretGateFieldScope::CodeSource => Self::CodeSource,
        }
    }
}

/// Final stored values presented to the shared finalizer.
#[derive(Debug, Clone, Default)]
pub struct SecretGateCandidateFields {
    pub record_content: Vec<String>,
    pub name_description: Vec<String>,
    pub json_properties: Option<Value>,
    pub tags: Vec<String>,
    pub code_source: Vec<String>,
}

/// The identity family used by the finalizer's target-bound event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretGateTargetKind {
    Entity,
    Note,
    KnowledgeAtom,
    KnowledgeDomain,
}

impl SecretGateTargetKind {
    fn substrate(self) -> SubstrateKind {
        match self {
            Self::Entity => SubstrateKind::Entity,
            Self::Note => SubstrateKind::Note,
            Self::KnowledgeAtom | Self::KnowledgeDomain => SubstrateKind::Event,
        }
    }

    fn aggregate_kind(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Note => "note",
            Self::KnowledgeAtom => "knowledge_atom",
            Self::KnowledgeDomain => "knowledge_domain",
        }
    }
}

/// Result of finalizing one candidate. A present event means a fresh
/// exemption; its record statement and all event statements must be submitted
/// through one `SqlWriter::execute_batch` call.
#[derive(Debug, Clone)]
pub struct SecretGateFinalization {
    pub properties: Option<Value>,
    pub success_event: Option<Event>,
}

#[derive(Debug)]
struct Match {
    scope: RuntimeFieldScope,
    digest: [u8; 32],
    overridden_detector: String,
}

/// Finalize a new/full candidate through the one ADR-115 boundary.
#[allow(clippy::too_many_arguments)]
pub fn finalize_secret_gate_candidate(
    namespace: &str,
    actor: &str,
    target_id: Uuid,
    target_kind: SecretGateTargetKind,
    entry_point: &'static str,
    fields: &SecretGateCandidateFields,
    mut properties: Option<Value>,
) -> RuntimeResult<SecretGateFinalization> {
    secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
    let snapshot = snapshot_for_namespace(namespace);
    let matched = scan_fields(&snapshot, fields)?;

    let Some(matched) = matched else {
        return Ok(SecretGateFinalization {
            properties,
            success_event: None,
        });
    };

    let map = match properties {
        None => {
            properties = Some(Value::Object(Default::default()));
            properties
                .as_mut()
                .and_then(Value::as_object_mut)
                .expect("object was just constructed")
        }
        Some(Value::Object(ref mut map)) => map,
        Some(_) => {
            return Err(RuntimeError::InvalidInput(
                "a secret-gate exemption requires object-shaped properties so the runtime \
                 posture stamp can be persisted"
                    .into(),
            ));
        }
    };
    map.insert(
        RESERVED_SECRET_GATE_KEY.to_string(),
        Value::String(SECRET_GATE_EXEMPTION_STAMP.to_string()),
    );

    let digest_sha256 = digest_to_hex(&matched.digest);
    let event = Event::new(
        namespace,
        "secret_gate.finalize",
        EventKind::Audit,
        target_kind.substrate(),
        actor,
    )
    .with_target(target_id)
    .with_aggregate(target_kind.aggregate_kind(), target_id)
    .with_payload(serde_json::json!({
        "mechanism": "content-sha256-manifest-v1",
        "digest_sha256": digest_sha256,
        "field_scope": matched.scope.as_str(),
        "manifest_id": snapshot.manifest_id(),
        "overridden_detector": matched.overridden_detector,
        "canonical_verb": entry_point,
        "outcome": "exempted",
        "target_kind": target_kind.aggregate_kind(),
    }));

    Ok(SecretGateFinalization {
        properties,
        success_event: Some(event),
    })
}

/// Finalize an already-materialized update candidate that may carry a
/// runtime stamp preserved from its target. The stamp is carried only when a
/// target-bound success event proves the exact `(scope,digest)` still exists
/// in the final candidate. Otherwise it is removed and the candidate is
/// evaluated as a fresh write.
#[allow(clippy::too_many_arguments)]
pub async fn finalize_secret_gate_update_candidate(
    runtime: &KhiveRuntime,
    namespace: &str,
    actor: &str,
    target_id: Uuid,
    target_kind: SecretGateTargetKind,
    entry_point: &'static str,
    fields: &SecretGateCandidateFields,
    mut properties: Option<Value>,
) -> RuntimeResult<SecretGateFinalization> {
    let has_stamp = properties
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|map| map.get(RESERVED_SECRET_GATE_KEY))
        .is_some();
    if !has_stamp {
        return finalize_secret_gate_candidate(
            namespace,
            actor,
            target_id,
            target_kind,
            entry_point,
            fields,
            properties,
        );
    }

    let persisted_value = properties
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|map| map.get(RESERVED_SECRET_GATE_KEY))
        .and_then(Value::as_str);
    if persisted_value != Some(SECRET_GATE_EXEMPTION_STAMP) {
        return Err(RuntimeError::InvalidInput(format!(
            "persisted property key `{RESERVED_SECRET_GATE_KEY}` is not a runtime-issued stamp; \
             remove it through operator repair before updating this record"
        )));
    }

    let linkage = load_linkage(runtime, namespace, target_id, target_kind).await?;
    let values = scoped_values(fields);
    if let Some((scope, digest)) = linkage {
        let linkage_survives = values.iter().any(|(candidate_scope, value)| {
            *candidate_scope == scope && scoped_digest(scope, value) == digest
        });
        if linkage_survives {
            scan_fields_except_linkage(&snapshot_for_namespace(namespace), &values, scope, digest)?;
            return Ok(SecretGateFinalization {
                properties,
                success_event: None,
            });
        }
    } else {
        return Err(RuntimeError::InvalidInput(format!(
            "persisted property key `{RESERVED_SECRET_GATE_KEY}` has no target-bound finalizer audit; \
             it cannot be echoed or carried"
        )));
    }

    if let Some(map) = properties.as_mut().and_then(Value::as_object_mut) {
        map.remove(RESERVED_SECRET_GATE_KEY);
    }
    finalize_secret_gate_candidate(
        namespace,
        actor,
        target_id,
        target_kind,
        entry_point,
        fields,
        properties,
    )
}

/// Append the target-bound audit statements to a record statement so the
/// caller can submit one atomic SQL batch.
pub fn secret_gate_atomic_statements(
    record: SqlStatement,
    event: &Event,
) -> RuntimeResult<Vec<SqlStatement>> {
    let mut statements = vec![record];
    statements.extend(secret_gate_success_event_statements(event)?);
    Ok(statements)
}

/// Build the target-bound audit leg for a caller whose record mutation is
/// already expressed by transaction-local SQL rather than a [`SqlStatement`].
/// The caller must execute every returned statement in that same transaction
/// after the guarded record write.
pub(crate) fn secret_gate_success_event_statements(
    event: &Event,
) -> RuntimeResult<Vec<SqlStatement>> {
    let mut statements = Vec::new();
    #[cfg(any(test, feature = "test-internals"))]
    if consume_test_audit_failure(&event.namespace) {
        statements.push(SqlStatement {
            // Executes after the record statement inside the caller's one
            // batch and deterministically violates required event columns,
            // proving the record/stamp rollback boundary rather than merely
            // refusing before a transaction starts.
            sql: "INSERT INTO events (id) VALUES (?1)".into(),
            params: vec![SqlValue::Text(event.id.to_string())],
            label: Some("secret_gate.inject_success_audit_failure".into()),
        });
        return Ok(statements);
    }
    statements.extend(
        khive_db::stores::event::event_insert_statements(event).map_err(|error| {
            RuntimeError::Internal(format!("secret-gate audit statement: {error}"))
        })?,
    );
    Ok(statements)
}

fn scan_fields(
    snapshot: &ManifestSnapshot,
    fields: &SecretGateCandidateFields,
) -> RuntimeResult<Option<Match>> {
    let mut matched: Option<Match> = None;
    for value in &fields.record_content {
        scan_one(
            snapshot,
            RuntimeFieldScope::RecordContent,
            value,
            &mut matched,
        )?;
    }
    for value in &fields.name_description {
        scan_one(
            snapshot,
            RuntimeFieldScope::NameDescription,
            value,
            &mut matched,
        )?;
    }
    if let Some(properties) = fields.json_properties.as_ref() {
        let mut values = Vec::new();
        collect_json_strings(properties, true, &mut values);
        for value in values {
            scan_one(
                snapshot,
                RuntimeFieldScope::JsonProperties,
                value,
                &mut matched,
            )?;
        }
    }
    for value in &fields.tags {
        scan_one(snapshot, RuntimeFieldScope::Tags, value, &mut matched)?;
    }
    for value in &fields.code_source {
        scan_one(snapshot, RuntimeFieldScope::CodeSource, value, &mut matched)?;
    }
    Ok(matched)
}

fn scoped_values(fields: &SecretGateCandidateFields) -> Vec<(RuntimeFieldScope, &str)> {
    let mut values = Vec::new();
    values.extend(
        fields
            .record_content
            .iter()
            .map(|value| (RuntimeFieldScope::RecordContent, value.as_str())),
    );
    values.extend(
        fields
            .name_description
            .iter()
            .map(|value| (RuntimeFieldScope::NameDescription, value.as_str())),
    );
    if let Some(properties) = fields.json_properties.as_ref() {
        let mut property_values = Vec::new();
        collect_json_strings(properties, true, &mut property_values);
        values.extend(
            property_values
                .into_iter()
                .map(|value| (RuntimeFieldScope::JsonProperties, value)),
        );
    }
    values.extend(
        fields
            .tags
            .iter()
            .map(|value| (RuntimeFieldScope::Tags, value.as_str())),
    );
    values.extend(
        fields
            .code_source
            .iter()
            .map(|value| (RuntimeFieldScope::CodeSource, value.as_str())),
    );
    values
}

fn scan_fields_except_linkage(
    snapshot: &ManifestSnapshot,
    values: &[(RuntimeFieldScope, &str)],
    linked_scope: RuntimeFieldScope,
    linked_digest: [u8; 32],
) -> RuntimeResult<()> {
    let mut matched = None;
    for &(scope, value) in values {
        if scope == linked_scope && scoped_digest(scope, value) == linked_digest {
            continue;
        }
        scan_one(snapshot, scope, value, &mut matched)?;
    }
    if matched.is_some() {
        return Err(RuntimeError::InvalidInput(
            "an already-exempted update also matched a distinct fresh manifest entry".into(),
        ));
    }
    Ok(())
}

async fn load_linkage(
    runtime: &KhiveRuntime,
    namespace: &str,
    target_id: Uuid,
    target_kind: SecretGateTargetKind,
) -> RuntimeResult<Option<(RuntimeFieldScope, [u8; 32])>> {
    let mut reader = runtime.sql().reader().await.map_err(RuntimeError::from)?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT payload FROM events WHERE namespace=?1 AND target_id=?2 \
                  AND aggregate_kind=?3 AND aggregate_id=?2 AND verb='secret_gate.finalize' \
                  AND kind='audit' AND outcome='success' ORDER BY created_at DESC, id DESC LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(target_id.to_string()),
                SqlValue::Text(target_kind.aggregate_kind().to_string()),
            ],
            label: Some("secret_gate.load_target_linkage".into()),
        })
        .await
        .map_err(RuntimeError::from)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let Some(SqlValue::Text(payload)) = row.get("payload") else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(payload)
        .map_err(|_| RuntimeError::Internal("malformed secret-gate audit payload".into()))?;
    let scope = match payload.get("field_scope").and_then(Value::as_str) {
        Some("record-content") => RuntimeFieldScope::RecordContent,
        Some("name-description") => RuntimeFieldScope::NameDescription,
        Some("json-properties") => RuntimeFieldScope::JsonProperties,
        Some("tags") => RuntimeFieldScope::Tags,
        Some("code-source") => RuntimeFieldScope::CodeSource,
        _ => return Ok(None),
    };
    let Some(hex) = payload.get("digest_sha256").and_then(Value::as_str) else {
        return Ok(None);
    };
    if hex.len() != 64 {
        return Ok(None);
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let Some(pair) = hex.get(index * 2..index * 2 + 2) else {
            return Ok(None);
        };
        let Ok(parsed) = u8::from_str_radix(pair, 16) else {
            return Ok(None);
        };
        *byte = parsed;
    }
    Ok(Some((scope, digest)))
}

fn scan_one(
    snapshot: &ManifestSnapshot,
    scope: RuntimeFieldScope,
    value: &str,
    matched: &mut Option<Match>,
) -> RuntimeResult<()> {
    let Some(detected) = secret_gate::scan(value) else {
        return Ok(());
    };
    let Some(meta) = snapshot.lookup(scope, value) else {
        return Err(RuntimeError::SecretDetected(detected));
    };
    let digest = scoped_digest(scope, value);
    match matched {
        None => {
            *matched = Some(Match {
                scope,
                digest,
                overridden_detector: meta.overridden_detector.clone(),
            });
        }
        Some(previous) if previous.scope == scope && previous.digest == digest => {}
        Some(_) => {
            return Err(RuntimeError::InvalidInput(
                "secret-gate manifest matched more than one distinct candidate field".into(),
            ));
        }
    }
    Ok(())
}

fn collect_json_strings<'a>(value: &'a Value, top_level: bool, output: &mut Vec<&'a str>) {
    match value {
        Value::String(value) => output.push(value),
        Value::Array(values) => {
            for value in values {
                collect_json_strings(value, false, output);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if top_level && key == RESERVED_SECRET_GATE_KEY {
                    continue;
                }
                output.push(key);
                collect_json_strings(value, false, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(any(test, feature = "test-internals"))]
type FixtureArmSet = Mutex<HashMap<String, (Arc<()>, Arc<ManifestSnapshot>)>>;

#[cfg(any(test, feature = "test-internals"))]
static TEST_FIXTURE_ARMS: LazyLock<FixtureArmSet> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(any(test, feature = "test-internals"))]
static TEST_AUDIT_FAILURE_ARMS: LazyLock<Mutex<HashMap<String, Arc<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Scoped, one-shot, namespace-specific non-deployable fixture arm.
#[cfg(any(test, feature = "test-internals"))]
#[must_use = "dropping the guard before finalization disarms the fixture"]
pub struct SecretGateTestArm {
    namespace: String,
    token: Arc<()>,
}

/// Scoped one-shot success-audit failure arm for an atomicity regression.
#[cfg(any(test, feature = "test-internals"))]
#[must_use = "dropping the guard before finalization disarms the failure"]
pub struct SecretGateAuditFailureArm {
    namespace: String,
    token: Arc<()>,
}

#[cfg(any(test, feature = "test-internals"))]
impl Drop for SecretGateAuditFailureArm {
    fn drop(&mut self) {
        let mut arms = TEST_AUDIT_FAILURE_ARMS
            .lock()
            .expect("audit failure arm mutex poisoned");
        if arms
            .get(&self.namespace)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            arms.remove(&self.namespace);
        }
    }
}

#[cfg(any(test, feature = "test-internals"))]
impl Drop for SecretGateTestArm {
    fn drop(&mut self) {
        let mut arms = TEST_FIXTURE_ARMS
            .lock()
            .expect("fixture arm mutex poisoned");
        if arms
            .get(&self.namespace)
            .is_some_and(|(token, _)| Arc::ptr_eq(token, &self.token))
        {
            arms.remove(&self.namespace);
        }
    }
}

/// Arm exactly one finalization in `namespace` with a non-deployable manifest
/// fixture. This fixture is not evidence of operator adjudication.
#[cfg(any(test, feature = "test-internals"))]
pub fn arm_secret_gate_test_exemption(
    namespace: &str,
    scope: SecretGateFieldScope,
    exact_value: &str,
) -> SecretGateTestArm {
    let token = Arc::new(());
    let fixture = super::manifest::fixture::TestOnlyManifestFixture::for_exact_value(
        scope.into(),
        exact_value,
    );
    let mut arms = TEST_FIXTURE_ARMS
        .lock()
        .expect("fixture arm mutex poisoned");
    assert!(
        !arms.contains_key(namespace),
        "secret-gate fixture namespace is already armed"
    );
    arms.insert(
        namespace.to_string(),
        (Arc::clone(&token), fixture.snapshot()),
    );
    SecretGateTestArm {
        namespace: namespace.to_string(),
        token,
    }
}

/// Make the next fresh exemption's success-event statement fail after its
/// record statement, within the same SQL batch.
#[cfg(any(test, feature = "test-internals"))]
pub fn arm_secret_gate_test_audit_failure(namespace: &str) -> SecretGateAuditFailureArm {
    let token = Arc::new(());
    let mut arms = TEST_AUDIT_FAILURE_ARMS
        .lock()
        .expect("audit failure arm mutex poisoned");
    assert!(
        arms.insert(namespace.to_string(), Arc::clone(&token))
            .is_none(),
        "secret-gate audit failure namespace is already armed"
    );
    SecretGateAuditFailureArm {
        namespace: namespace.to_string(),
        token,
    }
}

#[cfg(any(test, feature = "test-internals"))]
fn consume_test_audit_failure(namespace: &str) -> bool {
    TEST_AUDIT_FAILURE_ARMS
        .lock()
        .expect("audit failure arm mutex poisoned")
        .remove(namespace)
        .is_some()
}

fn snapshot_for_namespace(namespace: &str) -> Arc<ManifestSnapshot> {
    #[cfg(any(test, feature = "test-internals"))]
    if let Some((_, snapshot)) = TEST_FIXTURE_ARMS
        .lock()
        .expect("fixture arm mutex poisoned")
        .remove(namespace)
    {
        return snapshot;
    }
    let _ = namespace;
    Arc::clone(&PRODUCTION_MANIFEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_arm_is_one_shot_and_namespace_scoped() {
        let value = ["AKIA", "TESTFIXTURE", "0000000000"].concat();
        let _arm = arm_secret_gate_test_exemption(
            "finalizer-fixture-a",
            SecretGateFieldScope::RecordContent,
            &value,
        );
        let fields = SecretGateCandidateFields {
            record_content: vec![value.clone()],
            ..Default::default()
        };

        let wrong_namespace = finalize_secret_gate_candidate(
            "finalizer-fixture-b",
            "test",
            Uuid::new_v4(),
            SecretGateTargetKind::Entity,
            "entity.create",
            &fields,
            None,
        );
        assert!(matches!(
            wrong_namespace,
            Err(RuntimeError::SecretDetected(_))
        ));

        let admitted = finalize_secret_gate_candidate(
            "finalizer-fixture-a",
            "test",
            Uuid::new_v4(),
            SecretGateTargetKind::Entity,
            "entity.create",
            &fields,
            None,
        )
        .expect("matching namespace consumes fixture");
        assert!(admitted.success_event.is_some());

        let consumed = finalize_secret_gate_candidate(
            "finalizer-fixture-a",
            "test",
            Uuid::new_v4(),
            SecretGateTargetKind::Entity,
            "entity.create",
            &fields,
            None,
        );
        assert!(matches!(consumed, Err(RuntimeError::SecretDetected(_))));
    }

    #[test]
    fn success_event_is_target_bound_and_never_contains_submitted_content() {
        let value = ["AKIA", "EVENTFIXTURE", "00000000"].concat();
        let target = Uuid::new_v4();
        let _arm = arm_secret_gate_test_exemption(
            "finalizer-event-redaction",
            SecretGateFieldScope::RecordContent,
            &value,
        );
        let finalization = finalize_secret_gate_candidate(
            "finalizer-event-redaction",
            "lambda:test",
            target,
            SecretGateTargetKind::KnowledgeAtom,
            "knowledge.atom",
            &SecretGateCandidateFields {
                record_content: vec![value.clone()],
                ..Default::default()
            },
            None,
        )
        .expect("fixture match");
        let event = finalization.success_event.expect("fresh success event");
        assert_eq!(event.target_id, Some(target));
        assert_eq!(event.aggregate_id, Some(target));
        assert_eq!(event.aggregate_kind.as_deref(), Some("knowledge_atom"));
        assert!(!event.payload.to_string().contains(&value));
    }
}
