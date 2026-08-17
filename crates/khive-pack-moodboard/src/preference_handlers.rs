//! Public preference-learning verb handlers and durable Khive provenance.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use uuid::{Uuid, Version};

use khive_runtime::{
    actor_is_unattributed, BlobHydrator, KhiveRuntime, NamespaceToken, RuntimeError,
};
use khive_storage::blob::ContentRef;
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::{BlobStore, Entity};
use khive_types::{EdgeRelation, EventKind, EventOutcome, SubstrateKind};

use crate::preference::{
    deserialize_fann, feature_schema_id, feature_schema_response, is_lower_hex_64, predict,
    prepare_training_data, sha256_hex, train_model, validate_features, validate_loaded_bundle,
    validate_reason_code, JudgmentChoice, JudgmentRecord, ModelBundle, PreferenceScope,
    PresentationProvenance, RandomizationProvenance, ReasonCode, ResultOccurrence,
    SelectionProvenance, ServeRecord, TrainedModel, FEATURE_COUNT, FEATURE_SCHEMA_VERSION,
    JUDGMENT_SCHEMA_VERSION, MAX_TRAINING_EVENTS, MODEL_BUNDLE_SCHEMA_VERSION,
    PREFERENCE_RESPONSE_SCHEMA_VERSION, RANDOMIZATION_REVISION, SERVE_SCHEMA_VERSION,
};
use crate::MoodboardPack;

const SERVE_RECORD_VERB: &str = "moodboard.serve_record";
const JUDGMENT_RECORD_VERB: &str = "moodboard.judgment_record";
const MODEL_RECORD_VERB: &str = "moodboard.model_record";
const MAX_MODEL_BLOB_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_MS: u64 = 3_600_000;
// These UUIDv5 namespaces and their name framing are persistent wire identity.
// ADR-149 freezes both values; golden tests below prevent accidental drift.
const JUDGMENT_UUID_NAMESPACE: Uuid = Uuid::from_u128(0x8fc4_55de_533c_5d1d_9228_09b8_1ef1_8e33);
const MODEL_EVENT_UUID_NAMESPACE: Uuid = Uuid::from_u128(0x1dc2_337e_b200_5bd1_824f_2653_1164_5c16);
static JUDGMENT_LOCKS: [Mutex<()>; 256] = [const { Mutex::const_new(()) }; 256];
static MODEL_LOCKS: [Mutex<()>; 256] = [const { Mutex::const_new(()) }; 256];
const TRAIN_CONCURRENCY: usize = 1;
static TRAIN_GATE: Semaphore = Semaphore::const_new(TRAIN_CONCURRENCY);

/// Admission control for training, taken BEFORE the judgment snapshot is
/// loaded. `try_acquire` (not `acquire().await`) keeps the gate's wait queue
/// empty by construction: a refused caller holds no snapshot and no queued
/// future, so concurrent callers cannot each retain up to
/// `MAX_TRAINING_EVENTS` records while waiting on the one running fit.
pub(crate) fn acquire_train_permit() -> Result<SemaphorePermit<'static>, RuntimeError> {
    TRAIN_GATE.try_acquire().map_err(|_| {
        RuntimeError::InvalidInput(
            "moodboard.train_preference: another training run is in progress; retry after it completes"
                .to_string(),
        )
    })
}

/// Full-batch preference fitting is CPU-bound for up to `MAX_TRAINING_EVENTS`
/// events; running it inline on the async executor lets concurrent attributed
/// callers monopolize worker threads. The caller obtains `permit` from
/// [`acquire_train_permit`] before loading its snapshot; the permit is moved
/// INTO the blocking task so the training slot is released only when the fit
/// itself finishes. Held by this future instead, cancellation at the join
/// point would free the slot while the blocking fit kept running, admitting a
/// second concurrent fit.
pub(crate) async fn fit_preference_bounded(
    permit: SemaphorePermit<'static>,
    records: Vec<(i64, JudgmentRecord)>,
    scope: PreferenceScope,
) -> Result<TrainedModel, RuntimeError> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let data = prepare_training_data(&records, &scope)?;
        train_model(&data, scope)
    })
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!("joining moodboard training worker: {error}"))
    })?
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorInput {
    model_key: String,
    descriptor_fingerprint: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CandidateState {
    Scored,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateInput {
    state: CandidateState,
    asset_id: String,
    content_ref: String,
    #[serde(default)]
    source_rank: Option<u32>,
    features: [f32; FEATURE_COUNT],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionInput {
    policy_revision: String,
    #[serde(default)]
    pair_propensity: Option<f64>,
    #[serde(default)]
    candidate_pool_sha256: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExposureInput {
    #[serde(default)]
    preference_probability_shown: bool,
    #[serde(default)]
    source_rank_shown: bool,
    #[serde(default)]
    served_preference_model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeInput {
    board_entity_id: String,
    board_id: String,
    descriptor: DescriptorInput,
    #[serde(default)]
    feature_schema_id: Option<String>,
    source_report_sha256: String,
    candidates: [CandidateInput; 2],
    selection: SelectionInput,
    #[serde(default)]
    exposure: ExposureInput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeInput {
    serve_id: String,
    left_result_occurrence_id: String,
    right_result_occurrence_id: String,
    choice: JudgmentChoice,
    #[serde(default)]
    reason_code: Option<ReasonCode>,
    #[serde(default)]
    response_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrainInput {
    board_entity_id: String,
    board_id: String,
    descriptor: DescriptorInput,
    #[serde(default)]
    feature_schema_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceInput {
    preference_model_id: String,
    board_entity_id: String,
    board_id: String,
    descriptor: DescriptorInput,
    feature_schema_id: String,
    source_report_sha256: String,
    left: CandidateInput,
    right: CandidateInput,
}

#[derive(Debug)]
struct LoadedPreferenceModel {
    entity: Entity,
    bundle_content_ref: ContentRef,
    bundle_sha256: String,
    bundle: ModelBundle,
    network: lattice_fann::Network,
}

pub(crate) async fn handle_serve(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_attributed_actor(token, "moodboard.serve")?;
    let core = pack.runtime().core();
    let input: ServeInput = parse_input(params, "moodboard.serve")?;
    validate_board_id(&input.board_id, "moodboard.serve board_id")?;
    validate_descriptor(&input.descriptor, "moodboard.serve")?;
    validate_feature_schema_fence(input.feature_schema_id.as_deref(), "moodboard.serve")?;
    validate_sha256(
        &input.source_report_sha256,
        "moodboard.serve source_report_sha256",
    )?;
    validate_selection(&input.selection)?;
    // The immutable serve record must be able to reconstruct what was shown:
    // a persisted source_rank_shown=true with absent ranks would record an
    // exposure the record cannot reproduce. Checked before any lookup or
    // hydration — it is pure input validation.
    if input.exposure.source_rank_shown
        && input
            .candidates
            .iter()
            .any(|candidate| candidate.source_rank.is_none())
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard.serve exposure.source_rank_shown=true requires source_rank on both \
             candidates"
                .to_string(),
        ));
    }

    let board_entity_id =
        parse_canonical_uuid(&input.board_entity_id, "moodboard.serve board_entity_id")?;
    validate_board(&core, token, board_entity_id, &input.board_id).await?;
    let scope = scope_from_input(
        token,
        board_entity_id,
        input.board_id.clone(),
        &input.descriptor,
    );

    let mut validated = Vec::with_capacity(2);
    for (index, candidate) in input.candidates.into_iter().enumerate() {
        debug_assert_eq!(candidate.state, CandidateState::Scored);
        validate_features(
            &candidate.features,
            &format!("moodboard.serve candidates[{index}]"),
        )?;
        let asset_id = parse_canonical_uuid(
            &candidate.asset_id,
            &format!("moodboard.serve candidates[{index}].asset_id"),
        )?;
        let content_ref = parse_content_ref(
            &candidate.content_ref,
            &format!("moodboard.serve candidates[{index}].content_ref"),
        )?;
        validate_asset(&core, token, asset_id, &content_ref).await?;
        validated.push(ResultOccurrence {
            result_occurrence_id: Uuid::new_v4(),
            source_candidate_index: index as u8,
            asset_id,
            content_ref: content_ref.to_string(),
            source_rank: candidate.source_rank,
            features: candidate.features,
        });
    }
    if validated[0].asset_id == validated[1].asset_id
        || validated[0].content_ref == validated[1].content_ref
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard.serve candidates must identify two distinct assets and content refs"
                .to_string(),
        ));
    }

    let presentation = validate_exposure(&core, token, &scope, input.exposure).await?;
    let serve_id = Uuid::new_v4();
    let randomization = side_randomization(serve_id);
    if randomization.swap_applied {
        validated.swap(0, 1);
    }
    let record = ServeRecord {
        schema_version: SERVE_SCHEMA_VERSION.to_string(),
        serve_id,
        scope: scope.clone(),
        source_report_sha256: input.source_report_sha256,
        left: validated.remove(0),
        right: validated.remove(0),
        selection: SelectionProvenance {
            policy_revision: input.selection.policy_revision,
            pair_propensity: input.selection.pair_propensity,
            candidate_pool_sha256: input.selection.candidate_pool_sha256,
        },
        presentation,
        randomization,
    };
    append_serve_event(&core, token, &record).await?;

    Ok(json!({
        "schema_version": SERVE_SCHEMA_VERSION,
        "serve_id": record.serve_id,
        "scope": record.scope,
        "feature_schema": feature_schema_response(),
        "left": occurrence_response(&record.left),
        "right": occurrence_response(&record.right),
        "randomization": record.randomization,
        "experimental": true,
    }))
}

pub(crate) async fn handle_judge(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_attributed_actor(token, "moodboard.judge")?;
    let core = pack.runtime().core();
    let input: JudgeInput = parse_input(params, "moodboard.judge")?;
    validate_reason_code(input.choice, input.reason_code)?;
    if input
        .response_ms
        .is_some_and(|value| value > MAX_RESPONSE_MS)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.judge response_ms must be at most {MAX_RESPONSE_MS}"
        )));
    }
    let serve_id = parse_canonical_uuid(&input.serve_id, "moodboard.judge serve_id")?;
    let left_occurrence_id = parse_canonical_uuid(
        &input.left_result_occurrence_id,
        "moodboard.judge left_result_occurrence_id",
    )?;
    let right_occurrence_id = parse_canonical_uuid(
        &input.right_result_occurrence_id,
        "moodboard.judge right_result_occurrence_id",
    )?;
    let serve = load_serve_record(&core, token, serve_id).await?;
    if left_occurrence_id != serve.left.result_occurrence_id
        || right_occurrence_id != serve.right.result_occurrence_id
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard.judge occurrence IDs do not exactly match the served left/right presentation"
                .to_string(),
        ));
    }

    let judgment_id = judgment_id_for(serve_id);
    let record = JudgmentRecord {
        schema_version: JUDGMENT_SCHEMA_VERSION.to_string(),
        judgment_id,
        serve_id,
        scope: serve.scope,
        source_report_sha256: serve.source_report_sha256,
        left: serve.left,
        right: serve.right,
        selection: serve.selection,
        presentation: serve.presentation,
        randomization: serve.randomization,
        choice: input.choice,
        reason_code: input.reason_code,
        response_ms: input.response_ms,
    };
    let created = append_judgment_idempotent(&core, token, &record).await?;
    Ok(json!({
        "schema_version": JUDGMENT_SCHEMA_VERSION,
        "judgment_id": judgment_id,
        "serve_id": serve_id,
        "choice": input.choice,
        "reason_code": input.reason_code,
        "created": created,
        "experimental": true,
    }))
}

pub(crate) async fn handle_train_preference(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_attributed_actor(token, "moodboard.train_preference")?;
    let core = pack.runtime().core();
    let input: TrainInput = parse_input(params, "moodboard.train_preference")?;
    validate_board_id(&input.board_id, "moodboard.train_preference board_id")?;
    validate_descriptor(&input.descriptor, "moodboard.train_preference")?;
    validate_feature_schema_fence(
        input.feature_schema_id.as_deref(),
        "moodboard.train_preference",
    )?;
    let board_entity_id = parse_canonical_uuid(
        &input.board_entity_id,
        "moodboard.train_preference board_entity_id",
    )?;
    validate_board(&core, token, board_entity_id, &input.board_id).await?;
    let scope = scope_from_input(token, board_entity_id, input.board_id, &input.descriptor);
    // Admission precedes the snapshot load so a refused caller never
    // materializes the up-to-MAX_TRAINING_EVENTS record vector.
    let train_permit = acquire_train_permit()?;
    let records = load_judgment_snapshot(&core, token).await?;
    let mut trained = fit_preference_bounded(train_permit, records, scope.clone()).await?;

    let blob_store = require_blob_store(&core)?;
    let network_content_ref = blob_store.put(trained.network_bytes.clone()).await?;
    trained.bundle.fann.network_content_ref = network_content_ref.to_string();
    validate_loaded_bundle(&trained.bundle)?;
    let bundle_bytes = serde_json::to_vec(&trained.bundle).map_err(|error| {
        RuntimeError::Internal(format!(
            "serialize moodboard preference model bundle: {error}"
        ))
    })?;
    let bundle_sha256 = sha256_hex(&bundle_bytes);
    let bundle_content_ref = blob_store.put(bundle_bytes).await?;

    let stripe = content_ref_stripe(&bundle_content_ref);
    let _guard = MODEL_LOCKS[stripe].lock().await;
    let (model, created) = find_or_create_model(
        &core,
        token,
        &scope,
        &trained.bundle,
        &bundle_content_ref,
        &bundle_sha256,
    )
    .await?;
    core.link(
        token,
        model.id,
        board_entity_id,
        EdgeRelation::DerivedFrom,
        1.0,
        Some(json!({
            "feature_schema_id": feature_schema_id(),
            "training_snapshot_sha256": trained.bundle.training.snapshot_sha256,
        })),
    )
    .await?;
    ensure_model_event(
        &core,
        token,
        &model,
        &scope,
        &bundle_content_ref,
        &bundle_sha256,
        &trained.bundle.fann.network_content_ref,
        &trained.bundle.fann.network_sha256,
    )
    .await?;

    // Run the exact just-persisted FANN head before acknowledging publication.
    let round_trip = load_preference_model(&core, token, model.id, &scope).await?;
    let zero = [0.0; FEATURE_COUNT];
    let (_, neutral_probability) = predict(
        &round_trip.network,
        round_trip.bundle.calibration.temperature,
        &zero,
        &zero,
    )?;
    if neutral_probability != 0.5 {
        return Err(RuntimeError::Internal(
            "moodboard persisted zero-intercept FANN head failed neutral round-trip".to_string(),
        ));
    }

    Ok(json!({
        "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
        "preference_model_id": model.id,
        "content_ref": bundle_content_ref,
        "model_fingerprint": bundle_sha256,
        "network_content_ref": network_content_ref,
        "network_sha256": trained.bundle.fann.network_sha256,
        "created": created,
        "scope": scope,
        "training": trained.bundle.training,
        "calibration": trained.bundle.calibration,
        "test_metrics": trained.bundle.test_metrics,
        "fann_inference_verified": true,
        "experimental": true,
    }))
}

pub(crate) async fn handle_preference(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_attributed_actor(token, "moodboard.preference")?;
    let core = pack.runtime().core();
    let input: PreferenceInput = parse_input(params, "moodboard.preference")?;
    validate_board_id(&input.board_id, "moodboard.preference board_id")?;
    validate_descriptor(&input.descriptor, "moodboard.preference")?;
    validate_feature_schema_fence(Some(&input.feature_schema_id), "moodboard.preference")?;
    validate_sha256(
        &input.source_report_sha256,
        "moodboard.preference source_report_sha256",
    )?;
    let model_id = parse_canonical_uuid(
        &input.preference_model_id,
        "moodboard.preference preference_model_id",
    )?;
    let board_entity_id = parse_canonical_uuid(
        &input.board_entity_id,
        "moodboard.preference board_entity_id",
    )?;
    validate_board(&core, token, board_entity_id, &input.board_id).await?;
    let scope = scope_from_input(token, board_entity_id, input.board_id, &input.descriptor);

    let (left_asset_id, left_content_ref) =
        validate_inference_candidate(&core, token, &input.left, "left").await?;
    let (right_asset_id, right_content_ref) =
        validate_inference_candidate(&core, token, &input.right, "right").await?;
    if left_asset_id == right_asset_id || left_content_ref == right_content_ref {
        return Err(RuntimeError::InvalidInput(
            "moodboard.preference requires two distinct asset identities".to_string(),
        ));
    }
    let loaded = load_preference_model(&core, token, model_id, &scope).await?;
    let (logit, probability_left) = predict(
        &loaded.network,
        loaded.bundle.calibration.temperature,
        &input.left.features,
        &input.right.features,
    )?;
    let margin = (probability_left - 0.5).abs();
    let inside_band = margin <= loaded.bundle.calibration.tie_band_half_width;

    Ok(json!({
        "schema_version": PREFERENCE_RESPONSE_SCHEMA_VERSION,
        "prediction_kind": "learned_pairwise_preference",
        "conditional_on": "decisive_judgment",
        "probability_left_given_decisive": probability_left,
        "probability_right_given_decisive": 1.0 - probability_left,
        "raw_fann_logit": logit,
        "calibrated_temperature": loaded.bundle.calibration.temperature,
        "indifference": {
            "state": if inside_band { "inside_calibrated_band" } else { "outside_calibrated_band" },
            "probability_margin_from_half": margin,
            "calibrated_half_width": loaded.bundle.calibration.tie_band_half_width,
        },
        "conformal_evidence": {
            "state": "not_computed_by_this_verb",
            "note": "learned preference is not a conformal p-value or coherence statistic",
        },
        "preference_model_id": loaded.entity.id,
        "model_content_ref": loaded.bundle_content_ref,
        "model_fingerprint": loaded.bundle_sha256,
        "source_report_sha256": input.source_report_sha256,
        "scope": scope,
        "left": { "asset_id": left_asset_id, "content_ref": left_content_ref },
        "right": { "asset_id": right_asset_id, "content_ref": right_content_ref },
        "experimental": true,
    }))
}

fn parse_input<T: for<'de> Deserialize<'de>>(params: Value, verb: &str) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|error| RuntimeError::InvalidInput(format!("{verb} arguments: {error}")))
}

fn require_attributed_actor(token: &NamespaceToken, verb: &str) -> Result<(), RuntimeError> {
    if actor_is_unattributed(token.actor()) {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb} requires an explicitly configured actor; anonymous/local attribution is not eligible for preference learning"
        )));
    }
    Ok(())
}

fn actor_label(token: &NamespaceToken) -> String {
    format!("{}:{}", token.actor().kind, token.actor().id)
}

fn has_success_entity_envelope(event: &Event, token: &NamespaceToken) -> bool {
    event.namespace == token.namespace().as_str()
        && event.substrate == SubstrateKind::Entity
        && event.outcome == EventOutcome::Success
}

fn parse_canonical_uuid(raw: &str, context: &str) -> Result<Uuid, RuntimeError> {
    let parsed = Uuid::parse_str(raw)
        .map_err(|error| RuntimeError::InvalidInput(format!("{context} is not a UUID: {error}")))?;
    if parsed.to_string() != raw {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} must be a bare lowercase hyphenated UUID"
        )));
    }
    Ok(parsed)
}

fn parse_content_ref(raw: &str, context: &str) -> Result<ContentRef, RuntimeError> {
    if !is_lower_hex_64(raw) {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} must be a 64-character lowercase hexadecimal BlobStore content ref"
        )));
    }
    ContentRef::from_hex(raw.to_string())
        .map_err(|error| RuntimeError::InvalidInput(format!("{context}: {error}")))
}

fn validate_sha256(raw: &str, context: &str) -> Result<(), RuntimeError> {
    if !is_lower_hex_64(raw) {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} must be 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn validate_board_id(raw: &str, context: &str) -> Result<(), RuntimeError> {
    validate_sha256(raw, context)
}

fn validate_descriptor(input: &DescriptorInput, verb: &str) -> Result<(), RuntimeError> {
    validate_model_key(&input.model_key, &format!("{verb} descriptor.model_key"))?;
    validate_sha256(
        &input.descriptor_fingerprint,
        &format!("{verb} descriptor.descriptor_fingerprint"),
    )
}

fn validate_model_key(raw: &str, context: &str) -> Result<(), RuntimeError> {
    if raw.is_empty()
        || raw.len() > 128
        || !raw.is_ascii()
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} must be 1..=128 ASCII [A-Za-z0-9_.-] characters"
        )));
    }
    Ok(())
}

fn validate_feature_schema_fence(supplied: Option<&str>, verb: &str) -> Result<(), RuntimeError> {
    if let Some(supplied) = supplied {
        validate_sha256(supplied, &format!("{verb} feature_schema_id"))?;
        if supplied != feature_schema_id() {
            return Err(RuntimeError::InvalidInput(format!(
                "{verb} feature_schema_id does not match the installed {} contract",
                FEATURE_SCHEMA_VERSION
            )));
        }
    }
    Ok(())
}

fn validate_selection(input: &SelectionInput) -> Result<(), RuntimeError> {
    validate_selection_values(
        &input.policy_revision,
        input.pair_propensity,
        input.candidate_pool_sha256.as_deref(),
        "moodboard.serve selection",
    )
}

fn validate_selection_values(
    policy_revision: &str,
    pair_propensity: Option<f64>,
    candidate_pool_sha256: Option<&str>,
    context: &str,
) -> Result<(), RuntimeError> {
    if policy_revision.trim().is_empty()
        || policy_revision.len() > 128
        || policy_revision.trim() != policy_revision
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context}.policy_revision must be a non-empty trimmed string of at most 128 bytes"
        )));
    }
    if pair_propensity
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value) || value == 0.0)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context}.pair_propensity must be finite and in (0,1]"
        )));
    }
    if let Some(digest) = candidate_pool_sha256 {
        validate_sha256(digest, &format!("{context}.candidate_pool_sha256"))?;
    }
    Ok(())
}

fn scope_from_input(
    token: &NamespaceToken,
    board_entity_id: Uuid,
    board_id: String,
    descriptor: &DescriptorInput,
) -> PreferenceScope {
    PreferenceScope {
        namespace: token.namespace().as_str().to_string(),
        actor_kind: token.actor().kind.clone(),
        actor_id: token.actor().id.clone(),
        board_entity_id,
        board_id,
        model_key: descriptor.model_key.clone(),
        descriptor_fingerprint: descriptor.descriptor_fingerprint.clone(),
        feature_schema_id: feature_schema_id().to_string(),
    }
}

async fn validate_board(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    board_id: Uuid,
    expected_board_fingerprint: &str,
) -> Result<Entity, RuntimeError> {
    let entity = runtime.get_entity(token, board_id).await?;
    if entity.kind != "artifact" || entity.entity_type.as_deref() != Some("moodboard") {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard board_entity_id {board_id} must be a live artifact/moodboard"
        )));
    }
    let stored = entity
        .properties
        .as_ref()
        .and_then(|properties| properties.get("board_id"))
        .and_then(Value::as_str);
    if stored != Some(expected_board_fingerprint) {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard board_entity_id {board_id} does not carry the requested immutable board_id"
        )));
    }
    Ok(entity)
}

async fn validate_asset(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    asset_id: Uuid,
    expected_ref: &ContentRef,
) -> Result<Entity, RuntimeError> {
    let entity = runtime.get_entity(token, asset_id).await?;
    if entity.kind != "artifact" || entity.entity_type.as_deref() != Some("visual_asset") {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard asset {asset_id} must be a live artifact/visual_asset"
        )));
    }
    if entity.content_ref.as_deref() != Some(expected_ref.as_str()) {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard asset {asset_id} content_ref does not match the occurrence provenance"
        )));
    }
    let blob_store = require_blob_store(runtime)?;
    if !blob_store.exists(expected_ref).await? {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard asset {asset_id} references a missing BlobStore object"
        )));
    }
    Ok(entity)
}

async fn validate_exposure(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    scope: &PreferenceScope,
    input: ExposureInput,
) -> Result<PresentationProvenance, RuntimeError> {
    match (
        input.preference_probability_shown,
        input.served_preference_model_id.as_deref(),
    ) {
        (true, None) => {
            return Err(RuntimeError::InvalidInput(
                "moodboard.serve exposure.served_preference_model_id is required when a preference probability was shown"
                    .to_string(),
            ));
        }
        (false, Some(_)) => {
            return Err(RuntimeError::InvalidInput(
                "moodboard.serve exposure.served_preference_model_id is only valid when preference_probability_shown=true"
                    .to_string(),
            ));
        }
        _ => {}
    }
    let served_preference_model_id = if let Some(raw) = input.served_preference_model_id {
        let model_id =
            parse_canonical_uuid(&raw, "moodboard.serve exposure.served_preference_model_id")?;
        // Full bundle/provenance validation prevents a forged model id from
        // becoming trusted exposure metadata.
        let _ = load_preference_model(runtime, token, model_id, scope).await?;
        Some(model_id)
    } else {
        None
    };
    Ok(PresentationProvenance {
        preference_probability_shown: input.preference_probability_shown,
        source_rank_shown: input.source_rank_shown,
        served_preference_model_id,
    })
}

fn side_randomization(serve_id: Uuid) -> RandomizationProvenance {
    let mut hasher = Sha256::new();
    hasher.update(RANDOMIZATION_REVISION.as_bytes());
    hasher.update([0]);
    hasher.update(serve_id.as_bytes());
    let digest = hasher.finalize();
    RandomizationProvenance {
        revision: RANDOMIZATION_REVISION.to_string(),
        sha256: sha256_hex(
            &[RANDOMIZATION_REVISION.as_bytes(), &[0], serve_id.as_bytes()].concat(),
        ),
        swap_applied: digest[0] & 1 == 1,
    }
}

fn validate_scope_intrinsic(scope: &PreferenceScope, context: &str) -> Result<(), RuntimeError> {
    if scope.namespace.is_empty()
        || scope.actor_kind.is_empty()
        || scope.actor_id.is_empty()
        || scope.board_entity_id.is_nil()
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} has an incomplete immutable scope"
        )));
    }
    validate_board_id(&scope.board_id, &format!("{context} scope.board_id"))?;
    validate_model_key(&scope.model_key, &format!("{context} scope.model_key"))?;
    validate_sha256(
        &scope.descriptor_fingerprint,
        &format!("{context} scope.descriptor_fingerprint"),
    )?;
    if scope.feature_schema_id != feature_schema_id() {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} has the wrong immutable feature-schema identity"
        )));
    }
    Ok(())
}

fn validate_occurrence_intrinsic(
    occurrence: &ResultOccurrence,
    context: &str,
) -> Result<(), RuntimeError> {
    if occurrence.result_occurrence_id.get_version() != Some(Version::Random)
        || occurrence.asset_id.is_nil()
        || occurrence.source_candidate_index > 1
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} has an invalid occurrence, asset, or source-candidate identity"
        )));
    }
    parse_content_ref(&occurrence.content_ref, &format!("{context}.content_ref"))?;
    validate_features(&occurrence.features, context)
}

fn validate_presentation_intrinsic(
    presentation: &PresentationProvenance,
    context: &str,
) -> Result<(), RuntimeError> {
    let valid_model_identity = presentation
        .served_preference_model_id
        .is_none_or(|model_id| !model_id.is_nil());
    if !valid_model_identity
        || presentation.preference_probability_shown
            != presentation.served_preference_model_id.is_some()
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} has inconsistent probability-exposure provenance"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_pair_provenance_intrinsic(
    serve_id: Uuid,
    scope: &PreferenceScope,
    source_report_sha256: &str,
    left: &ResultOccurrence,
    right: &ResultOccurrence,
    selection: &SelectionProvenance,
    presentation: &PresentationProvenance,
    randomization: &RandomizationProvenance,
    context: &str,
) -> Result<(), RuntimeError> {
    if serve_id.get_version() != Some(Version::Random) {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} serve_id must retain its generated UUIDv4 identity"
        )));
    }
    validate_scope_intrinsic(scope, context)?;
    validate_sha256(
        source_report_sha256,
        &format!("{context} source_report_sha256"),
    )?;
    validate_occurrence_intrinsic(left, &format!("{context} left"))?;
    validate_occurrence_intrinsic(right, &format!("{context} right"))?;
    if left.result_occurrence_id == right.result_occurrence_id
        || left.asset_id == right.asset_id
        || left.content_ref == right.content_ref
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} must retain two distinct occurrence, asset, and content identities"
        )));
    }
    let expected_indices = if randomization.swap_applied {
        (1, 0)
    } else {
        (0, 1)
    };
    if (left.source_candidate_index, right.source_candidate_index) != expected_indices
        || randomization != &side_randomization(serve_id)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{context} has invalid side-randomization provenance"
        )));
    }
    validate_selection_values(
        &selection.policy_revision,
        selection.pair_propensity,
        selection.candidate_pool_sha256.as_deref(),
        &format!("{context} selection"),
    )?;
    validate_presentation_intrinsic(presentation, &format!("{context} presentation"))
}

fn validate_serve_record_intrinsic(record: &ServeRecord) -> Result<(), RuntimeError> {
    if record.schema_version != SERVE_SCHEMA_VERSION {
        return Err(RuntimeError::InvalidInput(
            "moodboard stored serve has the wrong record schema".to_string(),
        ));
    }
    validate_pair_provenance_intrinsic(
        record.serve_id,
        &record.scope,
        &record.source_report_sha256,
        &record.left,
        &record.right,
        &record.selection,
        &record.presentation,
        &record.randomization,
        "moodboard stored serve",
    )
}

fn validate_judgment_record_intrinsic(record: &JudgmentRecord) -> Result<(), RuntimeError> {
    if record.schema_version != JUDGMENT_SCHEMA_VERSION
        || record.judgment_id != judgment_id_for(record.serve_id)
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard stored judgment has the wrong record identity".to_string(),
        ));
    }
    validate_pair_provenance_intrinsic(
        record.serve_id,
        &record.scope,
        &record.source_report_sha256,
        &record.left,
        &record.right,
        &record.selection,
        &record.presentation,
        &record.randomization,
        "moodboard stored judgment",
    )?;
    validate_reason_code(record.choice, record.reason_code)?;
    if record
        .response_ms
        .is_some_and(|response_ms| response_ms > MAX_RESPONSE_MS)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard stored judgment response_ms exceeds {MAX_RESPONSE_MS}"
        )));
    }
    Ok(())
}

fn occurrence_response(occurrence: &ResultOccurrence) -> Value {
    json!({
        "result_occurrence_id": occurrence.result_occurrence_id,
        "asset_id": occurrence.asset_id,
        "content_ref": occurrence.content_ref,
        "source_rank": occurrence.source_rank,
    })
}

async fn append_serve_event(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    record: &ServeRecord,
) -> Result<(), RuntimeError> {
    validate_serve_record_intrinsic(record)?;
    let payload = serde_json::to_value(record)
        .map_err(|error| RuntimeError::Internal(format!("serialize serve record: {error}")))?;
    let mut event = Event::new(
        token.namespace().as_str(),
        SERVE_RECORD_VERB,
        EventKind::Audit,
        SubstrateKind::Entity,
        actor_label(token),
    )
    .with_target(record.scope.board_entity_id)
    .with_aggregate("moodboard_serve", record.serve_id)
    .with_payload(payload)
    .with_payload_schema_version(1);
    event.id = record.serve_id;
    runtime.events(token)?.append_event(event).await?;
    Ok(())
}

async fn load_serve_record(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    serve_id: Uuid,
) -> Result<ServeRecord, RuntimeError> {
    let event = runtime
        .events(token)?
        .get_event(serve_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("moodboard serve {serve_id}")))?;
    if !has_success_entity_envelope(&event, token)
        || event.verb != SERVE_RECORD_VERB
        || event.kind != EventKind::Audit
        || event.actor != actor_label(token)
        || event.aggregate_kind.as_deref() != Some("moodboard_serve")
        || event.aggregate_id != Some(serve_id)
        || event.payload_schema_version != 1
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard serve {serve_id} has the wrong immutable identity or actor"
        )));
    }
    let record: ServeRecord = serde_json::from_value(event.payload).map_err(|error| {
        RuntimeError::Internal(format!(
            "moodboard serve {serve_id} payload is corrupt: {error}"
        ))
    })?;
    if record.serve_id != serve_id
        || record.scope.namespace != token.namespace().as_str()
        || record.scope.actor_kind != token.actor().kind
        || record.scope.actor_id != token.actor().id
        || record.scope.feature_schema_id != feature_schema_id()
        || event.target_id != Some(record.scope.board_entity_id)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard serve {serve_id} provenance failed validation"
        )));
    }
    validate_serve_record_intrinsic(&record)?;
    Ok(record)
}

fn judgment_id_for(serve_id: Uuid) -> Uuid {
    Uuid::new_v5(&JUDGMENT_UUID_NAMESPACE, serve_id.as_bytes())
}

async fn append_judgment_idempotent(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    record: &JudgmentRecord,
) -> Result<bool, RuntimeError> {
    validate_judgment_record_intrinsic(record)?;
    let stripe = usize::from(record.serve_id.as_bytes()[0]);
    let _guard = JUDGMENT_LOCKS[stripe].lock().await;
    let store = runtime.events(token)?;
    if let Some(existing) = store.get_event(record.judgment_id).await? {
        return compare_existing_judgment(existing, token, record).map(|()| false);
    }
    let payload = serde_json::to_value(record)
        .map_err(|error| RuntimeError::Internal(format!("serialize judgment record: {error}")))?;
    let mut event = Event::new(
        token.namespace().as_str(),
        JUDGMENT_RECORD_VERB,
        EventKind::FeedbackExplicit,
        SubstrateKind::Entity,
        actor_label(token),
    )
    .with_target(record.scope.board_entity_id)
    .with_aggregate("moodboard_judgment", record.serve_id)
    .with_payload(payload)
    .with_payload_schema_version(1);
    event.id = record.judgment_id;
    match store.append_event(event).await {
        Ok(()) => Ok(true),
        Err(first_error) => match store.get_event(record.judgment_id).await? {
            Some(existing) => compare_existing_judgment(existing, token, record).map(|()| false),
            None => Err(first_error.into()),
        },
    }
}

fn compare_existing_judgment(
    existing: Event,
    token: &NamespaceToken,
    requested: &JudgmentRecord,
) -> Result<(), RuntimeError> {
    if existing.id != requested.judgment_id
        || !has_success_entity_envelope(&existing, token)
        || existing.verb != JUDGMENT_RECORD_VERB
        || existing.kind != EventKind::FeedbackExplicit
        || existing.actor != actor_label(token)
        || existing.target_id != Some(requested.scope.board_entity_id)
        || existing.aggregate_kind.as_deref() != Some("moodboard_judgment")
        || existing.aggregate_id != Some(requested.serve_id)
        || existing.payload_schema_version != 1
        || requested.judgment_id != judgment_id_for(requested.serve_id)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard judgment id {} is occupied by incompatible provenance",
            requested.judgment_id
        )));
    }
    let existing_record: JudgmentRecord =
        serde_json::from_value(existing.payload).map_err(|error| {
            RuntimeError::Internal(format!(
                "moodboard existing judgment {} is corrupt: {error}",
                requested.judgment_id
            ))
        })?;
    validate_judgment_record_intrinsic(&existing_record)?;
    if &existing_record != requested {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard serve {} already has a conflicting immutable judgment",
            requested.serve_id
        )));
    }
    Ok(())
}

async fn load_judgment_snapshot(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Vec<(i64, JudgmentRecord)>, RuntimeError> {
    let store = runtime.events(token)?;
    // One storage query is one stable SQLite read snapshot. Offset paging would
    // permit a newly appended event to shift later pages and cause duplication
    // or omission, so the hard ceiling is also the single-query limit.
    let page = store
        .query_events(
            EventFilter {
                verbs: vec![JUDGMENT_RECORD_VERB.to_string()],
                actors: vec![actor_label(token)],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: (MAX_TRAINING_EVENTS + 1) as u32,
            },
        )
        .await?;
    if page
        .total
        .is_some_and(|total| total > MAX_TRAINING_EVENTS as u64)
        || page.items.len() > MAX_TRAINING_EVENTS
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.train_preference actor judgment snapshot exceeds {MAX_TRAINING_EVENTS} records"
        )));
    }
    let mut records = Vec::new();
    for event in page.items {
        let valid_envelope = has_success_entity_envelope(&event, token);
        let record: JudgmentRecord = serde_json::from_value(event.payload).map_err(|error| {
            RuntimeError::Internal(format!(
                "moodboard judgment event {} payload is corrupt: {error}",
                event.id
            ))
        })?;
        if !valid_envelope
            || event.verb != JUDGMENT_RECORD_VERB
            || event.kind != EventKind::FeedbackExplicit
            || event.actor != actor_label(token)
            || event.target_id != Some(record.scope.board_entity_id)
            || event.aggregate_kind.as_deref() != Some("moodboard_judgment")
            || event.aggregate_id != Some(record.serve_id)
            || event.payload_schema_version != 1
            || record.schema_version != JUDGMENT_SCHEMA_VERSION
            || record.judgment_id != event.id
            || record.judgment_id != judgment_id_for(record.serve_id)
            || record.scope.namespace != token.namespace().as_str()
            || record.scope.actor_kind != token.actor().kind
            || record.scope.actor_id != token.actor().id
        {
            return Err(RuntimeError::Internal(format!(
                "moodboard judgment event {} failed immutable identity validation",
                event.id
            )));
        }
        validate_judgment_record_intrinsic(&record)?;
        records.push((event.created_at, record));
    }
    records.sort_by_key(|(_, record)| record.judgment_id);
    Ok(records)
}

fn require_blob_store(runtime: &KhiveRuntime) -> Result<Arc<dyn BlobStore>, RuntimeError> {
    runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "moodboard preference learning requires an installed BlobStore".to_string(),
        )
    })
}

fn require_blob_hydrator(runtime: &KhiveRuntime) -> Result<Arc<BlobHydrator>, RuntimeError> {
    runtime.blob_hydrator().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "moodboard preference learning requires an installed BlobStore".to_string(),
        )
    })
}

fn content_ref_stripe(content_ref: &ContentRef) -> usize {
    let bytes = content_ref.as_str().as_bytes();
    usize::from(hex_nibble(bytes[0])) * 16 + usize::from(hex_nibble(bytes[1]))
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("ContentRef is lowercase hexadecimal"),
    }
}

async fn find_model_by_content_ref(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    content_ref: &ContentRef,
) -> Result<Option<Entity>, RuntimeError> {
    let mut reader = runtime.sql().reader().await?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE namespace = ?1 AND kind = 'artifact' \
                  AND entity_type = 'moodboard_model' AND content_ref = ?2 \
                  AND deleted_at IS NULL ORDER BY created_at, id LIMIT 1"
                .to_string(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(content_ref.to_string()),
            ],
            label: Some("moodboard_find_preference_model".to_string()),
        })
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = match row.get("id") {
        Some(SqlValue::Uuid(id)) => *id,
        Some(SqlValue::Text(raw)) => Uuid::parse_str(raw).map_err(|error| {
            RuntimeError::Internal(format!(
                "moodboard model lookup returned invalid UUID {raw:?}: {error}"
            ))
        })?,
        other => {
            return Err(RuntimeError::Internal(format!(
                "moodboard model lookup returned invalid id {other:?}"
            )));
        }
    };
    runtime.get_entity(token, id).await.map(Some)
}

async fn find_or_create_model(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    scope: &PreferenceScope,
    bundle: &ModelBundle,
    bundle_content_ref: &ContentRef,
    bundle_sha256: &str,
) -> Result<(Entity, bool), RuntimeError> {
    if let Some(existing) = find_model_by_content_ref(runtime, token, bundle_content_ref).await? {
        return Ok((existing, false));
    }
    let properties = json!({
        "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
        "model_family": bundle.model_family,
        "model_fingerprint": bundle_sha256,
        "scope": scope,
        "descriptor_fingerprint": scope.descriptor_fingerprint,
        "feature_schema_id": scope.feature_schema_id,
        "training_snapshot_sha256": bundle.training.snapshot_sha256,
        "seed": bundle.training.optimizer.seed,
        "temperature": bundle.calibration.temperature,
        "tie_band_half_width": bundle.calibration.tie_band_half_width,
        "test_metrics": bundle.test_metrics,
        "network_content_ref": bundle.fann.network_content_ref,
        "network_sha256": bundle.fann.network_sha256,
    });
    let model = runtime
        .create_entity_with_content_ref(
            token,
            "artifact",
            Some("moodboard_model"),
            &format!("preference-{}", &bundle_sha256[..16]),
            Some("Experimental calibrated pairwise-preference FANN head; not a conformal coherence score."),
            Some(properties),
            vec![
                "moodboard".to_string(),
                "preference_model".to_string(),
                "experimental".to_string(),
            ],
            bundle_content_ref,
        )
        .await?;
    Ok((model, true))
}

fn model_event_id(model_id: Uuid, bundle_content_ref: &ContentRef) -> Uuid {
    let mut name = Vec::with_capacity(16 + 1 + 64);
    name.extend_from_slice(model_id.as_bytes());
    name.push(0);
    name.extend_from_slice(bundle_content_ref.as_str().as_bytes());
    Uuid::new_v5(&MODEL_EVENT_UUID_NAMESPACE, &name)
}

#[allow(clippy::too_many_arguments)]
async fn ensure_model_event(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    model: &Entity,
    scope: &PreferenceScope,
    bundle_content_ref: &ContentRef,
    bundle_sha256: &str,
    network_content_ref: &str,
    network_sha256: &str,
) -> Result<(), RuntimeError> {
    let event_id = model_event_id(model.id, bundle_content_ref);
    let payload = json!({
        "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
        "preference_model_id": model.id,
        "model_content_ref": bundle_content_ref,
        "model_fingerprint": bundle_sha256,
        "network_content_ref": network_content_ref,
        "network_sha256": network_sha256,
        "scope": scope,
    });
    let store = runtime.events(token)?;
    if let Some(existing) = store.get_event(event_id).await? {
        return validate_model_event(
            &existing,
            token,
            model.id,
            scope,
            bundle_content_ref,
            bundle_sha256,
            network_content_ref,
            network_sha256,
        );
    }
    let mut event = Event::new(
        token.namespace().as_str(),
        MODEL_RECORD_VERB,
        EventKind::Audit,
        SubstrateKind::Entity,
        actor_label(token),
    )
    .with_target(model.id)
    .with_aggregate("moodboard_model", model.id)
    .with_payload(payload)
    .with_payload_schema_version(1);
    event.id = event_id;
    match store.append_event(event).await {
        Ok(()) => Ok(()),
        Err(first_error) => match store.get_event(event_id).await? {
            Some(existing) => validate_model_event(
                &existing,
                token,
                model.id,
                scope,
                bundle_content_ref,
                bundle_sha256,
                network_content_ref,
                network_sha256,
            ),
            None => Err(first_error.into()),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_model_event(
    event: &Event,
    token: &NamespaceToken,
    model_id: Uuid,
    scope: &PreferenceScope,
    bundle_content_ref: &ContentRef,
    bundle_sha256: &str,
    network_content_ref: &str,
    network_sha256: &str,
) -> Result<(), RuntimeError> {
    let expected = json!({
        "schema_version": MODEL_BUNDLE_SCHEMA_VERSION,
        "preference_model_id": model_id,
        "model_content_ref": bundle_content_ref,
        "model_fingerprint": bundle_sha256,
        "network_content_ref": network_content_ref,
        "network_sha256": network_sha256,
        "scope": scope,
    });
    if event.id != model_event_id(model_id, bundle_content_ref)
        || !has_success_entity_envelope(event, token)
        || event.verb != MODEL_RECORD_VERB
        || event.kind != EventKind::Audit
        || event.actor != actor_label(token)
        || event.target_id != Some(model_id)
        || event.aggregate_kind.as_deref() != Some("moodboard_model")
        || event.aggregate_id != Some(model_id)
        || event.payload_schema_version != 1
        || event.payload != expected
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} lacks matching immutable pack provenance"
        )));
    }
    Ok(())
}

async fn load_preference_model(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    model_id: Uuid,
    expected_scope: &PreferenceScope,
) -> Result<LoadedPreferenceModel, RuntimeError> {
    let entity = runtime.get_entity(token, model_id).await?;
    if entity.kind != "artifact" || entity.entity_type.as_deref() != Some("moodboard_model") {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference_model_id {model_id} must be a live artifact/moodboard_model"
        )));
    }
    let raw_ref = entity.content_ref.as_deref().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} has no bundle content_ref"
        ))
    })?;
    let bundle_content_ref = parse_content_ref(raw_ref, "moodboard model content_ref")?;
    let hydrator = require_blob_hydrator(runtime)?;
    let bundle_blob = hydrator
        .hydrate_verified(&bundle_content_ref, MAX_MODEL_BLOB_BYTES)
        .await?;
    let bundle_sha256 = sha256_hex(bundle_blob.bytes());
    let bundle: ModelBundle = serde_json::from_slice(bundle_blob.bytes()).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} bundle is corrupt: {error}"
        ))
    })?;
    validate_loaded_bundle(&bundle)?;
    if &bundle.scope != expected_scope {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} has the wrong board, actor, descriptor, or feature-schema scope"
        )));
    }
    validate_entity_model_properties(&entity, &bundle, &bundle_sha256)?;
    let network_content_ref = parse_content_ref(
        &bundle.fann.network_content_ref,
        "moodboard model network_content_ref",
    )?;
    drop(bundle_blob);
    let network_blob = hydrator
        .hydrate_verified(&network_content_ref, MAX_MODEL_BLOB_BYTES)
        .await?;
    if sha256_hex(network_blob.bytes()) != bundle.fann.network_sha256 {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {model_id} FANN SHA-256 does not match its bundle"
        )));
    }
    let network = deserialize_fann(network_blob.bytes())?;
    drop(network_blob);
    let event_id = model_event_id(model_id, &bundle_content_ref);
    let event = runtime
        .events(token)?
        .get_event(event_id)
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "moodboard preference model {model_id} has no immutable pack provenance event"
            ))
        })?;
    validate_model_event(
        &event,
        token,
        model_id,
        &bundle.scope,
        &bundle_content_ref,
        &bundle_sha256,
        &bundle.fann.network_content_ref,
        &bundle.fann.network_sha256,
    )?;
    Ok(LoadedPreferenceModel {
        entity,
        bundle_content_ref,
        bundle_sha256,
        bundle,
        network,
    })
}

fn validate_entity_model_properties(
    entity: &Entity,
    bundle: &ModelBundle,
    bundle_sha256: &str,
) -> Result<(), RuntimeError> {
    let Some(properties) = entity.properties.as_ref() else {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {} has no identity properties",
            entity.id
        )));
    };
    let property_scope: PreferenceScope =
        serde_json::from_value(properties.get("scope").cloned().ok_or_else(|| {
            RuntimeError::InvalidInput("model scope property missing".to_string())
        })?)
        .map_err(|error| RuntimeError::InvalidInput(format!("model scope property: {error}")))?;
    if properties.get("schema_version").and_then(Value::as_str) != Some(MODEL_BUNDLE_SCHEMA_VERSION)
        || properties.get("model_family").and_then(Value::as_str)
            != Some(bundle.model_family.as_str())
        || properties.get("model_fingerprint").and_then(Value::as_str) != Some(bundle_sha256)
        || properties
            .get("network_content_ref")
            .and_then(Value::as_str)
            != Some(bundle.fann.network_content_ref.as_str())
        || properties.get("network_sha256").and_then(Value::as_str)
            != Some(bundle.fann.network_sha256.as_str())
        || property_scope != bundle.scope
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model {} entity/bundle identity mismatch",
            entity.id
        )));
    }
    Ok(())
}

async fn validate_inference_candidate(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    input: &CandidateInput,
    side: &str,
) -> Result<(Uuid, ContentRef), RuntimeError> {
    debug_assert_eq!(input.state, CandidateState::Scored);
    validate_features(&input.features, &format!("moodboard.preference {side}"))?;
    let asset_id = parse_canonical_uuid(
        &input.asset_id,
        &format!("moodboard.preference {side}.asset_id"),
    )?;
    let content_ref = parse_content_ref(
        &input.content_ref,
        &format!("moodboard.preference {side}.content_ref"),
    )?;
    validate_asset(runtime, token, asset_id, &content_ref).await?;
    Ok((asset_id, content_ref))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::{BackendId, Namespace, RuntimeConfig};

    use super::*;
    use crate::preference::{
        materialize_fann, pair_split, CalibrationProvenance, FannProvenance, OptimizerProvenance,
        PairKey, SplitCounts, TestMetrics, TrainingProvenance, FANN_CRATE_VERSION, FANN_FORMAT,
        FEATURE_SCHEMA_CANONICAL_JSON, MODEL_FAMILY, OPTIMIZER_BACKTRACKING_IDENTITY,
        PAIR_SPLIT_REVISION, TIE_BAND_RULE_IDENTITY, TRAINING_REVISION,
    };

    #[derive(Debug)]
    struct RecordingBoundedStore {
        inner: Arc<FsBlobStore>,
        calls: Mutex<Vec<(ContentRef, u64)>>,
    }

    #[async_trait::async_trait]
    impl BlobStore for RecordingBoundedStore {
        async fn put(&self, bytes: Vec<u8>) -> khive_storage::StorageResult<ContentRef> {
            self.inner.put(bytes).await
        }

        async fn get_bounded_verified(
            &self,
            content_ref: &ContentRef,
            max_bytes: u64,
        ) -> khive_storage::StorageResult<Vec<u8>> {
            self.calls
                .lock()
                .unwrap()
                .push((content_ref.clone(), max_bytes));
            self.inner
                .get_bounded_verified(content_ref, max_bytes)
                .await
        }

        async fn exists(&self, content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            self.inner.exists(content_ref).await
        }

        async fn size(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::StorageResult<Option<u64>> {
            panic!("preference hydration must not compose size with a read")
        }

        async fn delete(&self, content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            self.inner.delete(content_ref).await
        }
    }

    fn persistent_runtime(db_path: &Path, actor_id: &str) -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(db_path.to_path_buf()),
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string(), "moodboard".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some(actor_id.to_string()),
        })
        .expect("persistent runtime")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn train_admission_refuses_concurrent_callers_and_releases_the_gate() {
        assert_eq!(
            TRAIN_GATE.available_permits(),
            TRAIN_CONCURRENCY,
            "training gate must start at its declared bound"
        );
        let permit = acquire_train_permit().expect("first caller must be admitted");
        let refused = acquire_train_permit()
            .expect_err("a second caller must be refused while a fit is admitted");
        assert!(
            refused
                .to_string()
                .contains("another training run is in progress"),
            "refusal must name the running fit: {refused}"
        );
        fit_preference_bounded(
            permit,
            crate::preference::tests::sufficient_records(false),
            crate::preference::tests::fixture_scope(),
        )
        .await
        .expect("admitted training must succeed");
        assert_eq!(
            TRAIN_GATE.available_permits(),
            TRAIN_CONCURRENCY,
            "the completed fit must return its permit"
        );
        drop(acquire_train_permit().expect("gate must re-admit after release"));
        assert_eq!(
            TRAIN_GATE.available_permits(),
            TRAIN_CONCURRENCY,
            "a dropped permit must return to the gate"
        );
    }

    #[tokio::test]
    async fn validate_asset_keeps_candidate_checks_metadata_only() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.db");
        let blob_root = temp.path().join("blobs");
        let runtime = persistent_runtime(&db_path, "alice");
        let blob_store = Arc::new(FsBlobStore::new(blob_root.clone(), 0).unwrap());
        runtime
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let token = runtime.authorize(Namespace::local()).unwrap();
        let content_ref = blob_store
            .put(b"moodboard candidate raster bytes".to_vec())
            .await
            .unwrap();
        let asset = runtime
            .create_entity_with_content_ref(
                &token,
                "artifact",
                Some("visual_asset"),
                "corrupt-candidate-fixture",
                None,
                None,
                vec![],
                &content_ref,
            )
            .await
            .unwrap();
        validate_asset(&runtime, &token, asset.id, &content_ref)
            .await
            .expect("present candidate must validate");
        let hex = content_ref.as_str();
        let object_path = blob_root.join(&hex[0..2]).join(&hex[2..4]).join(hex);
        std::fs::write(&object_path, b"corrupted candidate bytes").unwrap();
        validate_asset(&runtime, &token, asset.id, &content_ref)
            .await
            .expect("candidate eligibility must use metadata-only existence, not hydrate bytes");
    }

    fn valid_serve_record() -> ServeRecord {
        let serve_id = Uuid::parse_str("2bb33a1d-e35f-4436-90bc-d22cc1959e0a").unwrap();
        let randomization = side_randomization(serve_id);
        let mut occurrences = [
            ResultOccurrence {
                result_occurrence_id: Uuid::parse_str("00000000-0000-4000-8000-000000000001")
                    .unwrap(),
                source_candidate_index: 0,
                asset_id: Uuid::parse_str("00000000-0000-4000-8000-000000000101").unwrap(),
                content_ref: "1".repeat(64),
                source_rank: Some(1),
                features: [0.25; FEATURE_COUNT],
            },
            ResultOccurrence {
                result_occurrence_id: Uuid::parse_str("00000000-0000-4000-8000-000000000002")
                    .unwrap(),
                source_candidate_index: 1,
                asset_id: Uuid::parse_str("00000000-0000-4000-8000-000000000102").unwrap(),
                content_ref: "2".repeat(64),
                source_rank: Some(2),
                features: [0.75; FEATURE_COUNT],
            },
        ];
        if randomization.swap_applied {
            occurrences.swap(0, 1);
        }
        ServeRecord {
            schema_version: SERVE_SCHEMA_VERSION.to_string(),
            serve_id,
            scope: PreferenceScope {
                namespace: "local".to_string(),
                actor_kind: "actor".to_string(),
                actor_id: "alice".to_string(),
                board_entity_id: Uuid::parse_str("00000000-0000-4000-8000-000000000201").unwrap(),
                board_id: "a".repeat(64),
                model_key: "fixture_visual_model".to_string(),
                descriptor_fingerprint: "b".repeat(64),
                feature_schema_id: feature_schema_id().to_string(),
            },
            source_report_sha256: "c".repeat(64),
            left: occurrences[0].clone(),
            right: occurrences[1].clone(),
            selection: SelectionProvenance {
                policy_revision: "fixture-policy-v1".to_string(),
                pair_propensity: Some(0.5),
                candidate_pool_sha256: Some("d".repeat(64)),
            },
            presentation: PresentationProvenance {
                preference_probability_shown: false,
                source_rank_shown: true,
                served_preference_model_id: None,
            },
            randomization,
        }
    }

    #[test]
    fn randomized_side_provenance_is_recomputable() {
        let serve_id = Uuid::parse_str("2bb33a1d-e35f-4436-90bc-d22cc1959e0a").unwrap();
        let first = side_randomization(serve_id);
        let second = side_randomization(serve_id);
        assert_eq!(first, second);
        assert_eq!(first.revision, RANDOMIZATION_REVISION);
        assert!(is_lower_hex_64(&first.sha256));
    }

    #[test]
    fn durable_uuid_namespaces_and_name_framing_are_golden() {
        let serve_id = Uuid::parse_str("2bb33a1d-e35f-4436-90bc-d22cc1959e0a").unwrap();
        assert_eq!(
            judgment_id_for(serve_id),
            Uuid::parse_str("b04e8b7c-b57d-55c2-b55e-8eac15aafdff").unwrap()
        );

        let model_id = Uuid::parse_str("11111111-2222-4333-8444-555555555555").unwrap();
        let bundle_content_ref = ContentRef::from_hex("ab".repeat(32)).unwrap();
        assert_eq!(
            model_event_id(model_id, &bundle_content_ref),
            Uuid::parse_str("b5b8f223-6cab-5738-9b81-6488c2e3722b").unwrap()
        );
    }

    #[test]
    fn immutable_event_envelope_rejects_namespace_substrate_and_outcome_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = persistent_runtime(&temp.path().join("state.db"), "alice");
        let token = runtime.authorize(Namespace::local()).unwrap();
        let event = Event::new(
            "local",
            SERVE_RECORD_VERB,
            EventKind::Audit,
            SubstrateKind::Entity,
            actor_label(&token),
        );
        assert!(has_success_entity_envelope(&event, &token));

        let mut wrong_namespace = event.clone();
        wrong_namespace.namespace = "foreign".to_string();
        assert!(!has_success_entity_envelope(&wrong_namespace, &token));

        let mut wrong_substrate = event.clone();
        wrong_substrate.substrate = SubstrateKind::Note;
        assert!(!has_success_entity_envelope(&wrong_substrate, &token));

        let mut wrong_outcome = event;
        wrong_outcome.outcome = EventOutcome::Error;
        assert!(!has_success_entity_envelope(&wrong_outcome, &token));
    }

    #[test]
    fn intrinsic_record_validation_rejects_semantic_provenance_mutations() {
        let serve = valid_serve_record();
        validate_serve_record_intrinsic(&serve).unwrap();

        let mut wrong_randomization = serve.clone();
        wrong_randomization.randomization.sha256 = "e".repeat(64);
        assert!(validate_serve_record_intrinsic(&wrong_randomization).is_err());

        let mut inconsistent_exposure = serve.clone();
        inconsistent_exposure
            .presentation
            .served_preference_model_id =
            Some(Uuid::parse_str("00000000-0000-4000-8000-000000000301").unwrap());
        assert!(validate_serve_record_intrinsic(&inconsistent_exposure).is_err());

        let mut judgment = JudgmentRecord {
            schema_version: JUDGMENT_SCHEMA_VERSION.to_string(),
            judgment_id: judgment_id_for(serve.serve_id),
            serve_id: serve.serve_id,
            scope: serve.scope,
            source_report_sha256: serve.source_report_sha256,
            left: serve.left,
            right: serve.right,
            selection: serve.selection,
            presentation: serve.presentation,
            randomization: serve.randomization,
            choice: JudgmentChoice::Left,
            reason_code: Some(ReasonCode::Style),
            response_ms: Some(500),
        };
        validate_judgment_record_intrinsic(&judgment).unwrap();

        judgment.reason_code = Some(ReasonCode::EquallyGood);
        assert!(validate_judgment_record_intrinsic(&judgment).is_err());
        judgment.reason_code = Some(ReasonCode::Style);
        judgment.response_ms = Some(MAX_RESPONSE_MS + 1);
        assert!(validate_judgment_record_intrinsic(&judgment).is_err());
    }

    #[test]
    fn unordered_pair_split_cannot_leak_when_sides_reverse() {
        let scope = PreferenceScope {
            namespace: "local".to_string(),
            actor_kind: "actor".to_string(),
            actor_id: "alice".to_string(),
            board_entity_id: Uuid::nil(),
            board_id: "a".repeat(64),
            model_key: "model".to_string(),
            descriptor_fingerprint: "b".repeat(64),
            feature_schema_id: feature_schema_id().to_string(),
        };
        let forward = PairKey::new(&"1".repeat(64), &"2".repeat(64));
        let reverse = PairKey::new(&"2".repeat(64), &"1".repeat(64));
        assert_eq!(forward, reverse);
        assert_eq!(pair_split(&scope, &forward), pair_split(&scope, &reverse));
    }

    #[test]
    fn abstain_requires_reason_and_never_becomes_decisive() {
        assert!(validate_reason_code(JudgmentChoice::Abstain, None).is_err());
        assert!(validate_reason_code(
            JudgmentChoice::Abstain,
            Some(ReasonCode::InsufficientContext)
        )
        .is_ok());
        assert_eq!(JudgmentChoice::Abstain.decisive_label(), None);
        assert_eq!(JudgmentChoice::Tie.decisive_label(), None);
    }

    #[tokio::test]
    async fn model_blob_artifact_and_fann_load_survive_runtime_restart() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.db");
        let blob_root = temp.path().join("blobs");
        let runtime = persistent_runtime(&db_path, "alice");
        let blob_store = Arc::new(FsBlobStore::new(blob_root.clone(), 0).unwrap());
        runtime
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let token = runtime.authorize(Namespace::local()).unwrap();
        let scope = PreferenceScope {
            namespace: "local".to_string(),
            actor_kind: "actor".to_string(),
            actor_id: "alice".to_string(),
            board_entity_id: Uuid::from_u128(42),
            board_id: "a".repeat(64),
            model_key: "fixture_visual_model".to_string(),
            descriptor_fingerprint: "b".repeat(64),
            feature_schema_id: feature_schema_id().to_string(),
        };
        let (_, network_bytes) = materialize_fann(&[0.25; FEATURE_COUNT]).unwrap();
        let network_content_ref = blob_store.put(network_bytes.clone()).await.unwrap();
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
            feature_schema_canonical_json_base64: BASE64.encode(FEATURE_SCHEMA_CANONICAL_JSON),
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
                network_content_ref: network_content_ref.to_string(),
                network_sha256: sha256_hex(&network_bytes),
            },
        };
        validate_loaded_bundle(&bundle).unwrap();
        let bundle_bytes = serde_json::to_vec(&bundle).unwrap();
        let bundle_sha256 = sha256_hex(&bundle_bytes);
        let bundle_content_ref = blob_store.put(bundle_bytes).await.unwrap();
        let (entity, created) = find_or_create_model(
            &runtime,
            &token,
            &scope,
            &bundle,
            &bundle_content_ref,
            &bundle_sha256,
        )
        .await
        .unwrap();
        assert!(created);
        ensure_model_event(
            &runtime,
            &token,
            &entity,
            &scope,
            &bundle_content_ref,
            &bundle_sha256,
            &bundle.fann.network_content_ref,
            &bundle.fann.network_sha256,
        )
        .await
        .unwrap();
        let model_id = entity.id;
        drop(token);
        drop(runtime);
        drop(blob_store);

        let restarted = persistent_runtime(&db_path, "alice");
        let recording_store = Arc::new(RecordingBoundedStore {
            inner: Arc::new(FsBlobStore::new(blob_root, 0).unwrap()),
            calls: Mutex::new(Vec::new()),
        });
        restarted
            .install_blob_store(Arc::clone(&recording_store) as Arc<dyn BlobStore>)
            .expect("install blob store");
        let restarted_token = restarted.authorize(Namespace::local()).unwrap();
        let loaded = load_preference_model(&restarted, &restarted_token, model_id, &scope)
            .await
            .unwrap();
        let (_, probability) = predict(
            &loaded.network,
            loaded.bundle.calibration.temperature,
            &[0.9; FEATURE_COUNT],
            &[0.1; FEATURE_COUNT],
        )
        .unwrap();
        assert!(probability > 0.5);
        assert_eq!(
            *recording_store.calls.lock().unwrap(),
            vec![
                (bundle_content_ref.clone(), MAX_MODEL_BLOB_BYTES),
                (network_content_ref.clone(), MAX_MODEL_BLOB_BYTES),
            ],
            "bundle and network must each enter shared bounded hydration at the 1 MiB pack cap"
        );

        let mut wrong_scope = scope;
        wrong_scope.board_id = "f".repeat(64);
        let error = load_preference_model(&restarted, &restarted_token, model_id, &wrong_scope)
            .await
            .expect_err("wrong identity must fail closed");
        assert!(error.to_string().contains("wrong board"));
    }
}
