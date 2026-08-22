//! Authenticated preference-model artifact verification shared by serving and boot cutover.

use khive_runtime::{BlobHydrator, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::event::Event;
use khive_storage::types::{PageRequest, SqlRow, SqlStatement, SqlValue};
use khive_storage::SqlAccess;
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use uuid::Uuid;

use crate::preference::{
    deserialize_fann, sha256_hex, validate_loaded_bundle, ModelBundle, PreferenceScope,
    MODEL_BUNDLE_SCHEMA_VERSION,
};

/// Persistent ADR-149 namespace for immutable preference-model publication events.
const MODEL_EVENT_UUID_NAMESPACE: Uuid = Uuid::from_u128(0x1dc2_337e_b200_5bd1_824f_2653_1164_5c16);
const MAX_MODEL_BLOB_BYTES: u64 = 1024 * 1024;
const LEGACY_MODEL_PAGE_SIZE: u32 = 128;

pub(crate) fn model_event_id(model_id: Uuid, bundle_ref: &ContentRef) -> Uuid {
    let mut name = Vec::with_capacity(16 + 1 + 64);
    name.extend_from_slice(model_id.as_bytes());
    name.push(0);
    name.extend_from_slice(bundle_ref.as_str().as_bytes());
    Uuid::new_v5(&MODEL_EVENT_UUID_NAMESPACE, &name)
}

#[derive(Debug)]
pub(crate) struct VerifiedPreferenceBundle {
    pub bundle: ModelBundle,
    pub bundle_sha256: String,
    pub network_ref: ContentRef,
    pub network_sha256: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_model_event_evidence(
    event: &Event,
    entity_namespace: &str,
    model_id: Uuid,
    scope: &PreferenceScope,
    bundle_ref: &ContentRef,
    bundle_sha256: &str,
    network_ref: &ContentRef,
    network_sha256: &str,
) -> Result<(), RuntimeError> {
    let expected = serde_json::json!({
        "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
        "preference_model_id": model_id,
        "model_content_ref": bundle_ref,
        "model_fingerprint": bundle_sha256,
        "network_content_ref": network_ref,
        "network_sha256": network_sha256,
        "scope": scope,
    });
    let expected_actor = format!("{}:{}", scope.actor_kind, scope.actor_id);
    if event.id != model_event_id(model_id, bundle_ref)
        || event.namespace != entity_namespace
        || event.substrate != SubstrateKind::Entity
        || event.outcome != EventOutcome::Success
        || event.verb != "moodboard.model_record"
        || event.kind != EventKind::Audit
        || event.actor != expected_actor
        || event.target_id != Some(model_id)
        || event.aggregate_kind.as_deref() != Some("moodboard_model")
        || event.aggregate_id != Some(model_id)
        || event.payload_schema_version != 1
        || event.profile_state_version.is_some()
        || event.duration_us != 0
        || event.session_id.is_some()
        || event.payload != expected
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} lacks matching immutable pack provenance"
        )));
    }
    Ok(())
}

pub(crate) fn verify_preference_bundle_evidence(
    model_id: Uuid,
    entity_namespace: &str,
    bundle_ref: &ContentRef,
    bundle_bytes: &[u8],
    event: &Event,
) -> Result<VerifiedPreferenceBundle, RuntimeError> {
    let bundle_sha256 = sha256_hex(bundle_bytes);
    let bundle: ModelBundle = serde_json::from_slice(bundle_bytes).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} bundle is corrupt: {error}"
        ))
    })?;
    validate_loaded_bundle(&bundle)?;
    if bundle.scope.namespace != entity_namespace {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} bundle namespace {:?} does not match entity namespace {entity_namespace:?}",
            bundle.scope.namespace
        )));
    }
    let network_ref =
        ContentRef::from_hex(bundle.fann.network_content_ref.clone()).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard preference model {model_id} network_content_ref: {error}"
            ))
        })?;
    validate_model_event_evidence(
        event,
        entity_namespace,
        model_id,
        &bundle.scope,
        bundle_ref,
        &bundle_sha256,
        &network_ref,
        &bundle.fann.network_sha256,
    )?;
    let network_sha256 = bundle.fann.network_sha256.clone();
    Ok(VerifiedPreferenceBundle {
        bundle,
        bundle_sha256,
        network_ref,
        network_sha256,
    })
}

pub(crate) fn verify_preference_network(
    evidence: &VerifiedPreferenceBundle,
    network_bytes: &[u8],
) -> Result<lattice_fann::Network, RuntimeError> {
    if sha256_hex(network_bytes) != evidence.network_sha256 {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model FANN SHA-256 does not match its authenticated bundle"
                .to_string(),
        ));
    }
    deserialize_fann(network_bytes)
}

/// One authenticated network role for the attachment cutover coordinator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedModelNetworkAttachment {
    pub model_id: Uuid,
    pub network_content_ref: ContentRef,
    pub size_bytes: u64,
}

/// Count every legacy preference-model candidate, including soft-deleted rows.
pub async fn legacy_preference_model_count(sql: &dyn SqlAccess) -> Result<u64, RuntimeError> {
    let mut reader = sql.reader().await?;
    match reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM entities \
                  WHERE kind = 'artifact' AND entity_type = 'moodboard_model' \
                    AND content_ref IS NOT NULL"
                .to_string(),
            params: vec![],
            label: Some("moodboard_legacy_preference_model_count".to_string()),
        })
        .await?
    {
        Some(SqlValue::Integer(count)) if count >= 0 => Ok(count as u64),
        other => Err(RuntimeError::Internal(format!(
            "moodboard legacy preference model count returned invalid value {other:?}"
        ))),
    }
}

#[derive(Debug)]
struct LegacyPreferenceModelRow {
    model_id: Uuid,
    namespace: String,
    bundle_ref: ContentRef,
    fann_attachment_ref: Option<ContentRef>,
}

fn required_text(row: &SqlRow, name: &str, context: &str) -> Result<String, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn required_uuid(row: &SqlRow, name: &str, context: &str) -> Result<Uuid, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Uuid(value)) => Ok(*value),
        Some(SqlValue::Text(value)) => Uuid::parse_str(value).map_err(|error| {
            RuntimeError::Internal(format!(
                "{context} returned invalid {name} {value:?}: {error}"
            ))
        }),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn optional_text(row: &SqlRow, name: &str, context: &str) -> Result<Option<String>, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Null) | None => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn optional_uuid(row: &SqlRow, name: &str, context: &str) -> Result<Option<Uuid>, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Null) | None => Ok(None),
        Some(SqlValue::Uuid(value)) => Ok(Some(*value)),
        Some(SqlValue::Text(value)) => Uuid::parse_str(value).map(Some).map_err(|error| {
            RuntimeError::Internal(format!(
                "{context} returned invalid {name} {value:?}: {error}"
            ))
        }),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn required_i64(row: &SqlRow, name: &str, context: &str) -> Result<i64, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn optional_nonnegative_u64(
    row: &SqlRow,
    name: &str,
    context: &str,
) -> Result<Option<u64>, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Null) | None => Ok(None),
        Some(SqlValue::Integer(value)) if *value >= 0 => Ok(Some(*value as u64)),
        other => Err(RuntimeError::Internal(format!(
            "{context} returned invalid {name}: {other:?}"
        ))),
    }
}

fn parse_legacy_model_row(row: &SqlRow) -> Result<LegacyPreferenceModelRow, RuntimeError> {
    let context = "moodboard legacy preference model scan";
    let model_id = required_uuid(row, "id", context)?;
    let namespace = required_text(row, "namespace", context)?;
    let bundle_ref = ContentRef::from_hex(required_text(row, "legacy_content_ref", context)?)
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard legacy preference model {model_id} has invalid bundle content_ref: {error}"
            ))
        })?;
    let content_attachment_ref = optional_text(row, "content_attachment_ref", context)?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "moodboard legacy preference model {model_id} lacks its stage-1 content attachment"
            ))
        })?;
    let content_attachment_ref = ContentRef::from_hex(content_attachment_ref).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "moodboard legacy preference model {model_id} has invalid content attachment: {error}"
        ))
    })?;
    if content_attachment_ref != bundle_ref {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard legacy preference model {model_id} content attachment disagrees with its legacy bundle reference"
        )));
    }
    let fann_attachment_ref = optional_text(row, "fann_attachment_ref", context)?
        .map(ContentRef::from_hex)
        .transpose()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard legacy preference model {model_id} has invalid fann-network attachment: {error}"
            ))
        })?;
    Ok(LegacyPreferenceModelRow {
        model_id,
        namespace,
        bundle_ref,
        fann_attachment_ref,
    })
}

fn parse_event_row(row: &SqlRow, model_id: Uuid) -> Result<Event, RuntimeError> {
    let context = "moodboard legacy model event lookup";
    let payload = match row.get("payload") {
        Some(SqlValue::Json(value)) => value.clone(),
        Some(SqlValue::Text(value)) => serde_json::from_str(value).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard preference model {model_id} event payload is corrupt: {error}"
            ))
        })?,
        other => {
            return Err(RuntimeError::Internal(format!(
                "{context} returned invalid payload: {other:?}"
            )))
        }
    };
    let substrate_raw = required_text(row, "substrate", context)?;
    let substrate = substrate_raw.parse::<SubstrateKind>().map_err(|error| {
        RuntimeError::Internal(format!("{context} returned invalid substrate: {error}"))
    })?;
    let kind_raw = required_text(row, "kind", context)?;
    let kind = kind_raw.parse::<EventKind>().map_err(|error| {
        RuntimeError::Internal(format!("{context} returned invalid kind: {error}"))
    })?;
    let outcome = match required_text(row, "outcome", context)?.as_str() {
        "success" => EventOutcome::Success,
        "denied" => EventOutcome::Denied,
        "error" => EventOutcome::Error,
        other => {
            return Err(RuntimeError::Internal(format!(
                "{context} returned invalid outcome {other:?}"
            )))
        }
    };
    let payload_schema_version =
        u32::try_from(required_i64(row, "payload_schema_version", context)?).map_err(|_| {
            RuntimeError::Internal(format!("{context} returned invalid payload version"))
        })?;
    Ok(Event {
        id: required_uuid(row, "id", context)?,
        namespace: required_text(row, "namespace", context)?,
        verb: required_text(row, "verb", context)?,
        substrate,
        actor: required_text(row, "actor", context)?,
        kind,
        outcome,
        payload,
        payload_schema_version,
        profile_state_version: optional_nonnegative_u64(row, "profile_state_version", context)?,
        duration_us: required_i64(row, "duration_us", context)?,
        target_id: optional_uuid(row, "target_id", context)?,
        session_id: optional_uuid(row, "session_id", context)?,
        aggregate_kind: optional_text(row, "aggregate_kind", context)?,
        aggregate_id: optional_uuid(row, "aggregate_id", context)?,
        created_at: required_i64(row, "created_at", context)?,
    })
}

async fn load_exact_model_event(
    sql: &dyn SqlAccess,
    model: &LegacyPreferenceModelRow,
) -> Result<Event, RuntimeError> {
    let event_id = model_event_id(model.model_id, &model.bundle_ref);
    let mut reader = sql.reader().await?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id, namespace, verb, substrate, actor, kind, outcome, payload, \
                         payload_schema_version, profile_state_version, duration_us, target_id, \
                         session_id, aggregate_kind, aggregate_id, created_at \
                  FROM events WHERE namespace = ?1 AND id = ?2"
                .to_string(),
            params: vec![
                SqlValue::Text(model.namespace.clone()),
                SqlValue::Text(event_id.to_string()),
            ],
            label: Some("moodboard_legacy_model_event".to_string()),
        })
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "moodboard preference model {} has no exact immutable pack provenance event",
                model.model_id
            ))
        })?;
    parse_event_row(&row, model.model_id)
}

/// Authenticate legacy model bundles and networks without loading or selecting the pack.
///
/// This boot-only path performs read-only SQL through [`SqlAccess`] and all object reads
/// through the already shared [`BlobHydrator`]. It never constructs an `EventStore`, runtime
/// store, or SQL writer task.
pub async fn verify_legacy_preference_attachments(
    sql: &dyn SqlAccess,
    hydrator: &BlobHydrator,
) -> Result<Vec<VerifiedModelNetworkAttachment>, RuntimeError> {
    let mut verified_rows = Vec::new();
    let mut offset = 0_u64;
    loop {
        let mut reader = sql.reader().await?;
        let rows = reader
            .query_page(
                SqlStatement {
                    sql: "SELECT e.id, e.namespace, e.content_ref AS legacy_content_ref, \
                                 content.content_ref AS content_attachment_ref, \
                                 fann.content_ref AS fann_attachment_ref \
                          FROM entities e \
                          LEFT JOIN attachments content ON content.record_uuid = e.id \
                            AND content.substrate = 'entity' AND content.role = 'content' \
                          LEFT JOIN attachments fann ON fann.record_uuid = e.id \
                            AND fann.substrate = 'entity' AND fann.role = 'fann-network' \
                          WHERE e.kind = 'artifact' AND e.entity_type = 'moodboard_model' \
                            AND e.content_ref IS NOT NULL \
                          ORDER BY e.namespace, e.id"
                        .to_string(),
                    params: vec![],
                    label: Some("moodboard_legacy_preference_models".to_string()),
                },
                PageRequest {
                    offset,
                    limit: LEGACY_MODEL_PAGE_SIZE,
                },
            )
            .await?;
        drop(reader);
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        for row in rows {
            let model = parse_legacy_model_row(&row)?;
            let event = load_exact_model_event(sql, &model).await?;
            let bundle_blob = hydrator
                .hydrate_verified(&model.bundle_ref, MAX_MODEL_BLOB_BYTES)
                .await?;
            let evidence = verify_preference_bundle_evidence(
                model.model_id,
                &model.namespace,
                &model.bundle_ref,
                bundle_blob.bytes(),
                &event,
            )?;
            drop(bundle_blob);
            if model
                .fann_attachment_ref
                .as_ref()
                .is_some_and(|attached| attached != &evidence.network_ref)
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "moodboard preference model {} fann-network attachment disagrees with its authenticated bundle",
                    model.model_id
                )));
            }
            let network_blob = hydrator
                .hydrate_verified(&evidence.network_ref, MAX_MODEL_BLOB_BYTES)
                .await?;
            let size_bytes = u64::try_from(network_blob.bytes().len()).map_err(|_| {
                RuntimeError::Internal(format!(
                    "moodboard preference model {} network size exceeds u64",
                    model.model_id
                ))
            })?;
            let _network = verify_preference_network(&evidence, network_blob.bytes())?;
            drop(network_blob);
            verified_rows.push(VerifiedModelNetworkAttachment {
                model_id: model.model_id,
                network_content_ref: evidence.network_ref,
                size_bytes,
            });
        }
        offset = offset.checked_add(row_count as u64).ok_or_else(|| {
            RuntimeError::Internal("moodboard legacy model scan offset overflow".to_string())
        })?;
        if row_count < LEGACY_MODEL_PAGE_SIZE as usize {
            break;
        }
    }
    Ok(verified_rows)
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use base64::Engine as _;
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::BlobHydrator;
    use khive_storage::blob::ContentRef;
    use khive_storage::event::Event;
    use khive_storage::types::{
        PageRequest, SqlColumn, SqlRow, SqlStatement, SqlValue, StorageResult,
    };
    use khive_storage::{AtomicUnitOp, BlobStore, SqlAccess, SqlReader, SqlWriter};
    use khive_types::{EventKind, SubstrateKind};
    use uuid::Uuid;

    use crate::preference::{
        feature_schema_id, materialize_fann, sha256_hex, CalibrationProvenance, FannProvenance,
        ModelBundle, OptimizerProvenance, PreferenceScope, SplitCounts, TestMetrics,
        TrainingProvenance, FANN_CRATE_VERSION, FANN_FORMAT, FEATURE_COUNT,
        FEATURE_SCHEMA_CANONICAL_JSON, FEATURE_SCHEMA_VERSION, MODEL_BUNDLE_SCHEMA_VERSION,
        MODEL_FAMILY, OPTIMIZER_BACKTRACKING_IDENTITY, PAIR_SPLIT_REVISION, TIE_BAND_RULE_IDENTITY,
        TRAINING_REVISION,
    };

    use super::{
        legacy_preference_model_count, model_event_id, verify_legacy_preference_attachments,
        verify_preference_bundle_evidence, verify_preference_network,
    };

    struct ArtifactFixture {
        model_id: Uuid,
        bundle_ref: ContentRef,
        bundle_bytes: Vec<u8>,
        network_bytes: Vec<u8>,
        event: Event,
    }

    fn artifact_fixture() -> ArtifactFixture {
        let model_id = Uuid::from_u128(42);
        let scope = PreferenceScope {
            namespace: "local".to_string(),
            actor_kind: "actor".to_string(),
            actor_id: "alice".to_string(),
            board_entity_id: Uuid::from_u128(7),
            board_id: "a".repeat(64),
            model_key: "fixture_visual_model".to_string(),
            descriptor_fingerprint: "b".repeat(64),
            feature_schema_id: feature_schema_id().to_string(),
        };
        let (_, network_bytes) = materialize_fann(&[0.25; FEATURE_COUNT]).unwrap();
        let network_ref = ContentRef::from_digest_bytes(blake3::hash(&network_bytes).as_bytes());
        let split_counts = BTreeMap::from([
            (
                "train".to_string(),
                SplitCounts {
                    decisive_groups: 64,
                    decisive_judgments: 64,
                    left_labels: 32,
                    right_labels: 32,
                    ..Default::default()
                },
            ),
            (
                "calibration".to_string(),
                SplitCounts {
                    decisive_groups: 16,
                    decisive_judgments: 16,
                    left_labels: 8,
                    right_labels: 8,
                    tie_groups: 16,
                    tie_judgments: 16,
                    ..Default::default()
                },
            ),
            (
                "test".to_string(),
                SplitCounts {
                    decisive_groups: 16,
                    decisive_judgments: 16,
                    left_labels: 8,
                    right_labels: 8,
                    ..Default::default()
                },
            ),
        ]);
        let bundle = ModelBundle {
            schema_version: MODEL_BUNDLE_SCHEMA_VERSION.to_string(),
            model_family: MODEL_FAMILY.to_string(),
            scope: scope.clone(),
            feature_schema_version: FEATURE_SCHEMA_VERSION.to_string(),
            feature_schema_canonical_json_base64: base64::engine::general_purpose::STANDARD
                .encode(FEATURE_SCHEMA_CANONICAL_JSON),
            training: TrainingProvenance {
                snapshot_sha256: "c".repeat(64),
                snapshot_event_count: 112,
                excluded_probability_shown: 0,
                split_revision: PAIR_SPLIT_REVISION.to_string(),
                split_counts,
                optimizer: OptimizerProvenance {
                    revision: TRAINING_REVISION.to_string(),
                    loss: "weighted_binary_cross_entropy".to_string(),
                    precision: "float64".to_string(),
                    intercept: "fixed_zero".to_string(),
                    l2: 1.0e-2,
                    max_iterations: 2_048,
                    iterations: 12,
                    converged: true,
                    final_objective: 0.5,
                    gradient_infinity_norm: 1.0e-9,
                    seed: 0,
                    backtracking: OPTIMIZER_BACKTRACKING_IDENTITY.to_string(),
                },
            },
            calibration: CalibrationProvenance {
                calibrated: true,
                temperature: 1.25,
                log_temperature_bounds: [-4.0, 4.0],
                temperature_search_iterations: 128,
                tie_band_half_width: 0.05,
                tie_band_rule: TIE_BAND_RULE_IDENTITY.to_string(),
                tie_balanced_error: 0.25,
            },
            test_metrics: TestMetrics {
                decisive_groups: 16,
                decisive_judgments: 16,
                log_loss: 0.5,
                brier: 0.2,
                accuracy: 0.75,
                tie_groups: 0,
                tie_detection_rate: None,
            },
            fann: FannProvenance {
                crate_name: "lattice-fann".to_string(),
                crate_version: FANN_CRATE_VERSION.to_string(),
                format: FANN_FORMAT.to_string(),
                architecture: format!("{FEATURE_COUNT}->1 linear; zero intercept"),
                network_content_ref: network_ref.to_string(),
                network_sha256: sha256_hex(&network_bytes),
            },
        };
        let bundle_bytes = serde_json::to_vec(&bundle).unwrap();
        let bundle_ref = ContentRef::from_digest_bytes(blake3::hash(&bundle_bytes).as_bytes());
        let bundle_sha256 = sha256_hex(&bundle_bytes);
        let payload = serde_json::json!({
            "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
            "preference_model_id": model_id,
            "model_content_ref": bundle_ref,
            "model_fingerprint": bundle_sha256,
            "network_content_ref": network_ref,
            "network_sha256": bundle.fann.network_sha256,
            "scope": scope,
        });
        let mut event = Event::new(
            "local",
            "moodboard.model_record",
            EventKind::Audit,
            SubstrateKind::Entity,
            "actor:alice",
        )
        .with_target(model_id)
        .with_aggregate("moodboard_model", model_id)
        .with_payload(payload)
        .with_payload_schema_version(1);
        event.id = model_event_id(model_id, &bundle_ref);
        ArtifactFixture {
            model_id,
            bundle_ref,
            bundle_bytes,
            network_bytes,
            event,
        }
    }

    #[derive(Clone)]
    struct ReadOnlyLegacySql {
        model_row: SqlRow,
        event_row: SqlRow,
    }

    struct LegacyReader(ReadOnlyLegacySql);

    fn column(name: &str, value: SqlValue) -> SqlColumn {
        SqlColumn {
            name: name.to_string(),
            value,
        }
    }

    fn legacy_model_row(
        fixture: &ArtifactFixture,
        fann_attachment_ref: Option<&ContentRef>,
    ) -> SqlRow {
        SqlRow {
            columns: vec![
                column("id", SqlValue::Text(fixture.model_id.to_string())),
                column("namespace", SqlValue::Text("local".to_string())),
                column(
                    "legacy_content_ref",
                    SqlValue::Text(fixture.bundle_ref.to_string()),
                ),
                column(
                    "content_attachment_ref",
                    SqlValue::Text(fixture.bundle_ref.to_string()),
                ),
                column(
                    "fann_attachment_ref",
                    fann_attachment_ref
                        .map(|content_ref| SqlValue::Text(content_ref.to_string()))
                        .unwrap_or(SqlValue::Null),
                ),
            ],
        }
    }

    fn event_row(event: &Event) -> SqlRow {
        SqlRow {
            columns: vec![
                column("id", SqlValue::Text(event.id.to_string())),
                column("namespace", SqlValue::Text(event.namespace.clone())),
                column("verb", SqlValue::Text(event.verb.clone())),
                column("substrate", SqlValue::Text(event.substrate.to_string())),
                column("actor", SqlValue::Text(event.actor.clone())),
                column("kind", SqlValue::Text(event.kind.to_string())),
                column("outcome", SqlValue::Text(event.outcome.to_string())),
                column("payload", SqlValue::Json(event.payload.clone())),
                column(
                    "payload_schema_version",
                    SqlValue::Integer(i64::from(event.payload_schema_version)),
                ),
                column("profile_state_version", SqlValue::Null),
                column("duration_us", SqlValue::Integer(event.duration_us)),
                column(
                    "target_id",
                    event
                        .target_id
                        .map(SqlValue::Uuid)
                        .unwrap_or(SqlValue::Null),
                ),
                column("session_id", SqlValue::Null),
                column(
                    "aggregate_kind",
                    event
                        .aggregate_kind
                        .clone()
                        .map(SqlValue::Text)
                        .unwrap_or(SqlValue::Null),
                ),
                column(
                    "aggregate_id",
                    event
                        .aggregate_id
                        .map(SqlValue::Uuid)
                        .unwrap_or(SqlValue::Null),
                ),
                column("created_at", SqlValue::Integer(event.created_at)),
            ],
        }
    }

    #[async_trait]
    impl SqlReader for LegacyReader {
        async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
            assert_eq!(
                statement.label.as_deref(),
                Some("moodboard_legacy_model_event")
            );
            Ok(Some(self.0.event_row.clone()))
        }

        async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            assert_eq!(
                statement.label.as_deref(),
                Some("moodboard_legacy_preference_models")
            );
            assert!(
                !statement.sql.contains("deleted_at IS NULL"),
                "soft-deleted recoverable models must be scanned"
            );
            Ok(vec![self.0.model_row.clone()])
        }

        async fn query_page(
            &mut self,
            statement: SqlStatement,
            page: PageRequest,
        ) -> StorageResult<Vec<SqlRow>> {
            let rows = self.query_all(statement).await?;
            Ok(rows
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
                .collect())
        }

        async fn query_scalar(
            &mut self,
            statement: SqlStatement,
        ) -> StorageResult<Option<SqlValue>> {
            assert_eq!(
                statement.label.as_deref(),
                Some("moodboard_legacy_preference_model_count")
            );
            assert!(!statement.sql.contains("deleted_at IS NULL"));
            Ok(Some(SqlValue::Integer(1)))
        }

        async fn explain(&mut self, _statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl SqlAccess for ReadOnlyLegacySql {
        async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
            Ok(Box::new(LegacyReader(self.clone())))
        }

        async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
            panic!("boot verifier must never acquire a SQL writer")
        }

        async fn atomic_unit(&self, _op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
            panic!("boot verifier must never enter a SQL write transaction")
        }
    }

    #[test]
    fn model_event_identity_preserves_the_adr_149_golden() {
        let model_id = Uuid::parse_str("11111111-2222-4333-8444-555555555555").unwrap();
        let bundle_ref = ContentRef::from_hex("ab".repeat(32)).unwrap();

        assert_eq!(
            model_event_id(model_id, &bundle_ref),
            Uuid::parse_str("b5b8f223-6cab-5738-9b81-6488c2e3722b").unwrap()
        );
    }

    #[test]
    fn bundle_verifier_authenticates_event_and_derives_actor_from_scope() {
        let fixture = artifact_fixture();

        let verified = verify_preference_bundle_evidence(
            fixture.model_id,
            "local",
            &fixture.bundle_ref,
            &fixture.bundle_bytes,
            &fixture.event,
        )
        .expect("matching immutable evidence");

        assert_eq!(verified.bundle.scope.actor_id, "alice");
        assert_eq!(
            verified.network_ref.as_str(),
            verified.bundle.fann.network_content_ref
        );
    }

    #[test]
    fn bundle_verifier_rejects_a_current_actor_instead_of_the_recorded_scope_actor() {
        let mut fixture = artifact_fixture();
        fixture.event.actor = "actor:bob".to_string();

        let error = verify_preference_bundle_evidence(
            fixture.model_id,
            "local",
            &fixture.bundle_ref,
            &fixture.bundle_bytes,
            &fixture.event,
        )
        .expect_err("event actor must derive from immutable bundle scope");

        assert!(error.to_string().contains("immutable pack provenance"));
    }

    #[test]
    fn bundle_verifier_rejects_noncanonical_event_envelope_metadata() {
        let mut fixture = artifact_fixture();
        fixture.event.session_id = Some(Uuid::from_u128(99));

        let error = verify_preference_bundle_evidence(
            fixture.model_id,
            "local",
            &fixture.bundle_ref,
            &fixture.bundle_bytes,
            &fixture.event,
        )
        .expect_err("the immutable model event envelope must be exact");

        assert!(error.to_string().contains("immutable pack provenance"));
    }

    #[test]
    fn bundle_verifier_rejects_entity_namespace_mismatch() {
        let fixture = artifact_fixture();

        let error = verify_preference_bundle_evidence(
            fixture.model_id,
            "foreign",
            &fixture.bundle_ref,
            &fixture.bundle_bytes,
            &fixture.event,
        )
        .expect_err("bundle scope cannot cross entity namespaces");

        assert!(error.to_string().contains("namespace"));
    }

    #[test]
    fn network_verifier_rejects_corrupt_governed_bytes() {
        let fixture = artifact_fixture();
        let verified = verify_preference_bundle_evidence(
            fixture.model_id,
            "local",
            &fixture.bundle_ref,
            &fixture.bundle_bytes,
            &fixture.event,
        )
        .unwrap();
        let mut corrupt = fixture.network_bytes;
        corrupt[0] ^= 0xff;

        let error = verify_preference_network(&verified, &corrupt)
            .expect_err("governed network SHA must fail before FANN use");

        assert!(error.to_string().contains("SHA-256"));
    }

    #[tokio::test]
    async fn boot_verifier_is_pack_selection_independent_and_includes_soft_deleted_models() {
        let fixture = artifact_fixture();
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsBlobStore::new(temp.path().to_path_buf(), 0).unwrap());
        assert_eq!(
            store.put(fixture.bundle_bytes.clone()).await.unwrap(),
            fixture.bundle_ref
        );
        let network_ref = store.put(fixture.network_bytes.clone()).await.unwrap();
        let hydrator = BlobHydrator::new(store, 64 * 1024 * 1024).unwrap();
        let sql = ReadOnlyLegacySql {
            model_row: legacy_model_row(&fixture, None),
            event_row: event_row(&fixture.event),
        };

        assert_eq!(legacy_preference_model_count(&sql).await.unwrap(), 1);
        let verified = verify_legacy_preference_attachments(&sql, &hydrator)
            .await
            .unwrap();

        assert_eq!(verified.len(), 1);
        assert_eq!(verified[0].model_id, fixture.model_id);
        assert_eq!(verified[0].network_content_ref, network_ref);
        assert_eq!(verified[0].size_bytes, fixture.network_bytes.len() as u64);
    }

    #[tokio::test]
    async fn boot_verifier_rejects_a_preexisting_fann_role_disagreement() {
        let fixture = artifact_fixture();
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsBlobStore::new(temp.path().to_path_buf(), 0).unwrap());
        store.put(fixture.bundle_bytes.clone()).await.unwrap();
        store.put(fixture.network_bytes.clone()).await.unwrap();
        let hydrator = BlobHydrator::new(store, 64 * 1024 * 1024).unwrap();
        let wrong_ref = ContentRef::from_hex("f".repeat(64)).unwrap();
        let sql = ReadOnlyLegacySql {
            model_row: legacy_model_row(&fixture, Some(&wrong_ref)),
            event_row: event_row(&fixture.event),
        };

        let error = verify_legacy_preference_attachments(&sql, &hydrator)
            .await
            .expect_err("migration must never overwrite contradictory role evidence");

        assert!(error
            .to_string()
            .contains("fann-network attachment disagrees"));
    }

    #[tokio::test]
    async fn boot_verifier_rejects_corrupt_immutable_event_evidence() {
        let mut fixture = artifact_fixture();
        fixture.event.payload["network_sha256"] = serde_json::Value::String("0".repeat(64));
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsBlobStore::new(temp.path().to_path_buf(), 0).unwrap());
        store.put(fixture.bundle_bytes.clone()).await.unwrap();
        store.put(fixture.network_bytes.clone()).await.unwrap();
        let hydrator = BlobHydrator::new(store, 64 * 1024 * 1024).unwrap();
        let sql = ReadOnlyLegacySql {
            model_row: legacy_model_row(&fixture, None),
            event_row: event_row(&fixture.event),
        };

        let error = verify_legacy_preference_attachments(&sql, &hydrator)
            .await
            .expect_err("migration must authenticate the raw immutable event");

        assert!(error.to_string().contains("immutable pack provenance"));
    }
}
