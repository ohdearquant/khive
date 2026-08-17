//! Moodboard verb handlers.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use khive_fusion::union_fusion;
use khive_retrieval::{
    materialize_ranked_prefix, DropReason, MaterializationDecision, MaterializationError,
    MaterializationLimits, MaterializedItem, MaterializedPrefix, RankedCandidate,
};
use khive_runtime::{BlobHydrator, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::blob::ContentRef;
use khive_storage::types::{
    SqlStatement, SqlValue, VectorIndexKind, VectorSearchHit, VectorSearchRequest,
};
use khive_storage::{BlobStore, EmbeddingSpaceIdentity, Entity, NewAttachment, VectorStore};
use khive_types::SubstrateKind;

use crate::model::{validate_embedding, DescriptorIdentity, LoadedVisionModel, VisionModelState};
use crate::preprocess::{prepare_raster, PreparedRaster};
use crate::MoodboardPack;

const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
const VISUAL_FIELD: &str = "visual.descriptor";
const DEFAULT_TOP_K: u32 = 20;
const MAX_TOP_K: u32 = 100;
const MAX_CANDIDATE_MULTIPLIER: u32 = 4;
const MAX_MOODBOARD_CANDIDATES: usize = (MAX_TOP_K * MAX_CANDIDATE_MULTIPLIER + 1) as usize;
static INGEST_CONTENT_LOCKS: [Mutex<()>; 256] = [const { Mutex::const_new(()) }; 256];

pub(crate) async fn handle_model(
    pack: &MoodboardPack,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_fields(&params, "moodboard.model", &[])?;
    let descriptor = pack.model_state().describe().await?;
    Ok(identity_response(&descriptor))
}

pub(crate) async fn handle_ingest(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_fields(
        &params,
        "moodboard.ingest",
        &["image_base64", "name", "media_type", "caption"],
    )?;
    let name = optional_string(&params, "name", "moodboard.ingest", 512)?;
    let caption = optional_string(&params, "caption", "moodboard.ingest", 32 * 1024)?;
    let declared_media_type = optional_string(&params, "media_type", "moodboard.ingest", 64)?;
    let encoded_image = image_base64_input(&params)?;
    let core = pack.runtime().core();
    let blob_store = require_blob_store(&core)?;
    // Cold-load and verify the checkpoint before decoding large caller bytes:
    // this preserves the no-blob-side-effect identity fence without holding
    // the preprocessing memory permit across Qwen construction.
    let model = pack.model_state().get().await?;
    let embedding_identity = model.embedding_identity().clone();
    let descriptor = model.descriptor().clone();
    let preprocessing_permit = pack.model_state().acquire_preprocessing_permit().await?;
    let raw = decode_image_base64(encoded_image)?;
    let prepared = prepare_raster(&raw, declared_media_type.as_deref())?;
    let original_len = raw.len();

    let content_ref = blob_store.put(raw).await?;
    drop(preprocessing_permit);

    let (asset, created) = find_or_create_visual_asset(
        &core,
        token,
        &content_ref,
        name.as_deref(),
        caption.as_deref(),
        &prepared,
        original_len,
    )
    .await?;

    let embedding = infer_prepared(
        pack.model_state(),
        model,
        prepared.inference_png,
        &descriptor,
    )
    .await?;
    index_embedding_with_identity(
        pack.runtime(),
        token,
        &embedding_identity,
        &descriptor,
        asset.id,
        &embedding,
    )
    .await?;

    Ok(json!({
        "asset_id": asset.id.to_string(),
        "content_ref": content_ref.to_string(),
        "created": created,
        "indexed": true,
        "descriptor": descriptor,
        "experimental": true,
        "embedding": embedding,
    }))
}

pub(crate) async fn handle_search(
    pack: &MoodboardPack,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_fields(&params, "moodboard.search", &["asset_id", "top_k"])?;
    let asset_id = parse_asset_id(&params)?;
    let top_k = parse_top_k(&params)?;
    let core = pack.runtime().core();
    let asset = core.get_entity(token, asset_id).await?;
    validate_visual_asset(&asset)?;
    let content_ref = parse_entity_content_ref(&asset)?;

    let blob_store = require_blob_store(&core)?;
    let prepared = prepare_source_raster(pack, &core, &content_ref).await?;

    let model = pack.model_state().get().await?;
    let embedding_identity = model.embedding_identity().clone();
    let descriptor = model.descriptor().clone();
    let embedding = infer_prepared(
        pack.model_state(),
        model,
        prepared.inference_png,
        &descriptor,
    )
    .await?;
    let raw_hits = search_embedding_with_identity(
        pack.runtime(),
        token,
        &embedding_identity,
        &descriptor,
        &embedding,
        candidate_limit(top_k),
    )
    .await?;

    let hits =
        materialize_hits(&core, token, blob_store.as_ref(), asset_id, raw_hits, top_k).await?;

    Ok(json!({
        "query_asset_id": asset_id.to_string(),
        "descriptor": descriptor,
        "experimental": true,
        "hits": hits,
    }))
}

async fn materialize_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    raw_hits: Vec<VectorSearchHit>,
    top_k: u32,
) -> Result<Vec<Value>, RuntimeError> {
    let materialized = materialize_hits_with_diagnostics(
        runtime,
        token,
        blob_store,
        query_asset_id,
        raw_hits,
        top_k,
    )
    .await?;
    Ok(materialized
        .accepted
        .into_iter()
        .map(|item| item.output)
        .collect())
}

#[cfg(test)]
fn validated_cosine_score(hit: &VectorSearchHit) -> Result<f64, RuntimeError> {
    validated_cosine_score_value(hit.subject_id, hit.score)
}

fn validated_cosine_score_value(
    subject_id: Uuid,
    deterministic_score: DeterministicScore,
) -> Result<f64, RuntimeError> {
    let score = deterministic_score.to_f64();
    if !score.is_finite() || !(-1.0..=1.0).contains(&score) {
        return Err(RuntimeError::Internal(format!(
            "moodboard vector backend returned invalid cosine score {score} for {} (expected finite [-1,1])",
            subject_id
        )));
    }
    Ok(score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoodboardDropReason {
    SelfHit,
    StaleEntity,
    OutsideVisibleScope,
    WrongKind,
    WrongSubtype,
    MissingContentAttachment,
    MalformedContentRef,
    MissingBlob,
}

impl DropReason for MoodboardDropReason {
    const ALL: &'static [Self] = &[
        Self::SelfHit,
        Self::StaleEntity,
        Self::OutsideVisibleScope,
        Self::WrongKind,
        Self::WrongSubtype,
        Self::MissingContentAttachment,
        Self::MalformedContentRef,
        Self::MissingBlob,
    ];

    fn ordinal(self) -> usize {
        match self {
            Self::SelfHit => 0,
            Self::StaleEntity => 1,
            Self::OutsideVisibleScope => 2,
            Self::WrongKind => 3,
            Self::WrongSubtype => 4,
            Self::MissingContentAttachment => 5,
            Self::MalformedContentRef => 6,
            Self::MissingBlob => 7,
        }
    }
}

#[derive(Debug)]
enum MoodboardCandidateRow {
    Drop(MoodboardDropReason),
    Keep {
        asset_id: Uuid,
        name: String,
        content_ref: ContentRef,
    },
}

type MoodboardMaterializedHits =
    MaterializedPrefix<Uuid, DeterministicScore, Value, MoodboardDropReason>;

async fn materialize_hits_with_diagnostics(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    raw_hits: Vec<VectorSearchHit>,
    top_k: u32,
) -> Result<MoodboardMaterializedHits, RuntimeError> {
    let loader_batch_size = NonZeroUsize::new(1).expect("one is non-zero");
    let limits = MaterializationLimits::try_new(
        MAX_MOODBOARD_CANDIDATES,
        loader_batch_size,
        MAX_TOP_K as usize,
        MAX_MOODBOARD_CANDIDATES,
    )
    .map_err(|error| {
        RuntimeError::Internal(format!(
            "moodboard materialization limits violate the shared v1 envelope: {error}"
        ))
    })?;
    let authorized_namespaces: BTreeSet<String> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str().to_string())
        .collect();
    let candidates = raw_hits
        .into_iter()
        .map(|hit| RankedCandidate {
            key: hit.subject_id,
            score: hit.score,
        })
        .collect();

    let materialized = materialize_ranked_prefix(
        candidates,
        top_k as usize,
        loader_batch_size,
        limits,
        |candidate| (Reverse(candidate.score), candidate.key),
        |candidate| {
            validated_cosine_score_value(candidate.key, candidate.score)?;
            Ok(())
        },
        |keys| {
            load_moodboard_candidate_batch(
                runtime,
                token,
                blob_store,
                query_asset_id,
                &authorized_namespaces,
                keys,
            )
        },
        |_, row| match row {
            Some(MoodboardCandidateRow::Drop(reason)) => MaterializationDecision::Drop(reason),
            Some(MoodboardCandidateRow::Keep {
                asset_id,
                name,
                content_ref,
            }) => MaterializationDecision::Keep((asset_id, name, content_ref)),
            None => MaterializationDecision::Drop(MoodboardDropReason::StaleEntity),
        },
    )
    .await
    .map_err(map_moodboard_materialization_error)?;

    let accepted = materialized
        .accepted
        .into_iter()
        .map(|item| {
            let (asset_id, name, content_ref) = item.output;
            let output = json!({
                "asset_id": asset_id.to_string(),
                "score": item.candidate.score.to_f64(),
                "rank": item.rank,
                "name": name,
                "content_ref": content_ref.to_string(),
            });
            MaterializedItem {
                candidate: item.candidate,
                rank: item.rank,
                output,
            }
        })
        .collect();

    Ok(MaterializedPrefix {
        accepted,
        drop_counts: materialized.drop_counts,
        diagnostic_details: materialized.diagnostic_details,
        diagnostics_truncated: materialized.diagnostics_truncated,
    })
}

async fn load_moodboard_candidate_batch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    authorized_namespaces: &BTreeSet<String>,
    keys: Vec<Uuid>,
) -> Result<Vec<(Uuid, MoodboardCandidateRow)>, RuntimeError> {
    let [subject_id] = keys.as_slice() else {
        return Err(RuntimeError::Internal(format!(
            "moodboard materialization expected a one-row loader batch, got {}",
            keys.len()
        )));
    };
    let subject_id = *subject_id;
    if subject_id == query_asset_id {
        return Ok(vec![(
            subject_id,
            MoodboardCandidateRow::Drop(MoodboardDropReason::SelfHit),
        )]);
    }

    let candidate = match runtime.get_entity(token, subject_id).await {
        Ok(candidate) => candidate,
        Err(error) if is_stale_candidate_error(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let row = if !authorized_namespaces.contains(candidate.namespace.as_str()) {
        MoodboardCandidateRow::Drop(MoodboardDropReason::OutsideVisibleScope)
    } else if candidate.kind != "artifact" {
        MoodboardCandidateRow::Drop(MoodboardDropReason::WrongKind)
    } else if candidate.entity_type.as_deref() != Some("visual_asset") {
        MoodboardCandidateRow::Drop(MoodboardDropReason::WrongSubtype)
    } else if let Some(candidate_ref) = candidate.content_ref.as_deref() {
        match ContentRef::from_hex(candidate_ref) {
            Ok(candidate_ref) => {
                if blob_store.exists(&candidate_ref).await? {
                    MoodboardCandidateRow::Keep {
                        asset_id: candidate.id,
                        name: candidate.name,
                        content_ref: candidate_ref,
                    }
                } else {
                    MoodboardCandidateRow::Drop(MoodboardDropReason::MissingBlob)
                }
            }
            Err(_) => MoodboardCandidateRow::Drop(MoodboardDropReason::MalformedContentRef),
        }
    } else {
        MoodboardCandidateRow::Drop(MoodboardDropReason::MissingContentAttachment)
    };
    Ok(vec![(subject_id, row)])
}

fn map_moodboard_materialization_error(error: MaterializationError<RuntimeError>) -> RuntimeError {
    match error {
        MaterializationError::Caller(error) => error,
        structural => RuntimeError::Internal(format!(
            "moodboard ranked materialization invariant failed: {structural}"
        )),
    }
}

fn is_stale_candidate_error(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::NotFound(_) | RuntimeError::NamespaceMismatch { .. }
    )
}

fn candidate_limit(top_k: u32) -> u32 {
    top_k
        .saturating_mul(MAX_CANDIDATE_MULTIPLIER)
        .saturating_add(1)
}

fn identity_response(descriptor: &DescriptorIdentity) -> Value {
    json!({
        "descriptor": descriptor,
        "experimental": true,
    })
}

fn require_fields<'a>(
    params: &'a Value,
    verb: &str,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, RuntimeError> {
    let object = params.as_object().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("{verb} arguments must be a JSON object"))
    })?;
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: unknown argument {unknown:?}; allowed: {}",
            if allowed.is_empty() {
                "none".to_string()
            } else {
                allowed.join(", ")
            }
        )));
    }
    Ok(object)
}

fn optional_string(
    params: &Value,
    field: &str,
    verb: &str,
    max_len: usize,
) -> Result<Option<String>, RuntimeError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_str().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("{verb}: {field} must be a string when present"))
    })?;
    if value.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: {field} must be non-empty when present"
        )));
    }
    if value.len() > max_len {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: {field} is {} bytes, exceeding the {max_len}-byte limit",
            value.len()
        )));
    }
    Ok(Some(value.to_string()))
}

fn image_base64_input(params: &Value) -> Result<&str, RuntimeError> {
    let encoded = params
        .get("image_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "moodboard.ingest requires image_base64 (base64 string)".to_string(),
            )
        })?;
    let max_encoded = MAX_OBJECT_BYTES.saturating_mul(4) / 3 + 4;
    if encoded.len() > max_encoded {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.ingest image_base64 is {} characters, exceeding the {MAX_OBJECT_BYTES}-byte decoded ceiling",
            encoded.len()
        )));
    }
    Ok(encoded)
}

fn decode_image_base64(encoded: &str) -> Result<Vec<u8>, RuntimeError> {
    let raw = BASE64.decode(encoded).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "moodboard.ingest image_base64 is not valid base64: {error}"
        ))
    })?;
    if raw.len() > MAX_OBJECT_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.ingest image is {} bytes, exceeding the {MAX_OBJECT_BYTES}-byte maximum",
            raw.len()
        )));
    }
    Ok(raw)
}

fn parse_asset_id(params: &Value) -> Result<Uuid, RuntimeError> {
    let raw = params
        .get("asset_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "moodboard.search requires asset_id (bare canonical UUID)".to_string(),
            )
        })?;
    let id = Uuid::parse_str(raw).map_err(|error| {
        RuntimeError::InvalidInput(format!("moodboard.search asset_id is not a UUID: {error}"))
    })?;
    if id.to_string() != raw {
        return Err(RuntimeError::InvalidInput(
            "moodboard.search asset_id must be a bare lowercase hyphenated UUID".to_string(),
        ));
    }
    Ok(id)
}

fn parse_top_k(params: &Value) -> Result<u32, RuntimeError> {
    let Some(value) = params.get("top_k") else {
        return Ok(DEFAULT_TOP_K);
    };
    if value.is_null() {
        return Ok(DEFAULT_TOP_K);
    }
    let value = value.as_u64().ok_or_else(|| {
        RuntimeError::InvalidInput(
            "moodboard.search top_k must be a positive integer when present".to_string(),
        )
    })?;
    if !(1..=u64::from(MAX_TOP_K)).contains(&value) {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.search top_k must be in 1..={MAX_TOP_K}, got {value}"
        )));
    }
    Ok(value as u32)
}

fn require_blob_store(runtime: &KhiveRuntime) -> Result<Arc<dyn BlobStore>, RuntimeError> {
    runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "moodboard requires an installed BlobStore (configure [storage.blob] or KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}

fn require_blob_hydrator(runtime: &KhiveRuntime) -> Result<Arc<BlobHydrator>, RuntimeError> {
    runtime.blob_hydrator().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "moodboard requires an installed BlobStore (configure [storage.blob] or KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}

async fn prepare_source_raster(
    pack: &MoodboardPack,
    runtime: &KhiveRuntime,
    content_ref: &ContentRef,
) -> Result<PreparedRaster, RuntimeError> {
    let hydrator = require_blob_hydrator(runtime)?;
    let original = hydrator
        .hydrate_verified(content_ref, MAX_OBJECT_BYTES as u64)
        .await?;
    let preprocessing_permit = pack.model_state().acquire_preprocessing_permit().await?;
    let prepared = prepare_raster(original.bytes(), None)?;
    drop(original);
    drop(preprocessing_permit);
    Ok(prepared)
}

fn asset_properties(prepared: &PreparedRaster, original_bytes: usize) -> Value {
    json!({
        "schema_version": "moodboard.visual-asset.v1",
        "media_type": prepared.media_type,
        "original_bytes": original_bytes,
        "original_width": prepared.original_width,
        "original_height": prepared.original_height,
    })
}

async fn find_visual_asset(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    content_ref: &ContentRef,
) -> Result<Option<Entity>, RuntimeError> {
    let mut reader = runtime.sql().reader().await?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT e.id FROM entities e \
                  JOIN attachments a ON a.record_uuid = e.id \
                    AND a.substrate = 'entity' AND a.role = 'content' \
                  WHERE e.namespace = ?1 AND e.kind = 'artifact' \
                  AND e.entity_type = 'visual_asset' AND a.content_ref = ?2 \
                  AND e.deleted_at IS NULL ORDER BY e.created_at, e.id LIMIT 1"
                .to_string(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(content_ref.to_string()),
            ],
            label: Some("moodboard_find_visual_asset".to_string()),
        })
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = match row.get("id") {
        Some(SqlValue::Uuid(id)) => *id,
        Some(SqlValue::Text(id)) => Uuid::parse_str(id).map_err(|error| {
            RuntimeError::Internal(format!(
                "moodboard visual_asset row contains invalid UUID {id:?}: {error}"
            ))
        })?,
        other => {
            return Err(RuntimeError::Internal(format!(
                "moodboard visual_asset lookup returned invalid id: {other:?}"
            )))
        }
    };
    runtime.get_entity(token, id).await.map(Some)
}

#[allow(clippy::too_many_arguments)]
async fn find_or_create_visual_asset(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    content_ref: &ContentRef,
    name: Option<&str>,
    caption: Option<&str>,
    prepared: &PreparedRaster,
    original_len: usize,
) -> Result<(Entity, bool), RuntimeError> {
    let bytes = content_ref.as_str().as_bytes();
    let stripe = usize::from(hex_nibble(bytes[0])) * 16 + usize::from(hex_nibble(bytes[1]));
    let _guard = INGEST_CONTENT_LOCKS[stripe].lock().await;
    if let Some(asset) = find_visual_asset(runtime, token, content_ref).await? {
        return Ok((asset, false));
    }

    let default_name = format!("asset-{}", &content_ref.as_str()[..12]);
    let size_bytes = u64::try_from(original_len).map_err(|_| {
        RuntimeError::Internal("moodboard visual asset size exceeds u64".to_string())
    })?;
    let asset = runtime
        .create_entity_with_attachments(
            token,
            "artifact",
            Some("visual_asset"),
            name.unwrap_or(&default_name),
            caption,
            Some(asset_properties(prepared, original_len)),
            vec!["moodboard".to_string(), "visual_asset".to_string()],
            vec![NewAttachment {
                role: "content".to_string(),
                content_ref: content_ref.clone(),
                media_type: Some(prepared.media_type.to_string()),
                size_bytes: Some(size_bytes),
            }],
        )
        .await?;
    Ok((asset, true))
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("ContentRef guarantees lowercase hexadecimal bytes"),
    }
}

fn validate_visual_asset(entity: &Entity) -> Result<(), RuntimeError> {
    if entity.kind != "artifact" || entity.entity_type.as_deref() != Some("visual_asset") {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.search asset_id {} is not an artifact/visual_asset",
            entity.id
        )));
    }
    Ok(())
}

fn parse_entity_content_ref(entity: &Entity) -> Result<ContentRef, RuntimeError> {
    let raw = entity.content_ref.as_ref().ok_or_else(|| {
        RuntimeError::Internal(format!(
            "visual_asset {} has no attached content_ref",
            entity.id
        ))
    })?;
    ContentRef::from_hex(raw.clone()).map_err(|error| {
        RuntimeError::Internal(format!(
            "visual_asset {} has invalid content_ref: {error}",
            entity.id
        ))
    })
}

async fn infer_prepared(
    state: &VisionModelState,
    model: Arc<LoadedVisionModel>,
    inference_png: Vec<u8>,
    descriptor: &DescriptorIdentity,
) -> Result<Vec<f32>, RuntimeError> {
    let embedding = state.infer(model, inference_png).await?;
    validate_embedding(&embedding, descriptor)?;
    Ok(embedding)
}

async fn exact_store(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &EmbeddingSpaceIdentity,
) -> Result<Arc<dyn VectorStore>, RuntimeError> {
    let store = runtime.vectors_for_embedding_space(token, identity).await?;
    let info = store.info().await?;
    if info.index_kind != VectorIndexKind::SqliteVec {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard v1 requires exact sqlite-vec retrieval, backend reported {:?}",
            info.index_kind
        )));
    }
    Ok(store)
}

async fn index_embedding_with_identity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &EmbeddingSpaceIdentity,
    descriptor: &DescriptorIdentity,
    asset_id: Uuid,
    embedding: &[f32],
) -> Result<(), RuntimeError> {
    validate_embedding(embedding, descriptor)?;
    let store = exact_store(runtime, token, identity).await?;
    store
        .insert_exact_only(
            asset_id,
            SubstrateKind::Entity,
            token.namespace().as_str(),
            VISUAL_FIELD,
            vec![embedding.to_vec()],
        )
        .await?;
    Ok(())
}

async fn search_embedding_with_identity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &EmbeddingSpaceIdentity,
    descriptor: &DescriptorIdentity,
    embedding: &[f32],
    top_k: u32,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    validate_embedding(embedding, descriptor)?;
    let store = exact_store(runtime, token, identity).await?;
    let namespaces: BTreeSet<String> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str().to_string())
        .collect();
    let mut namespace_hits = Vec::with_capacity(namespaces.len());
    for namespace in namespaces {
        let request = VectorSearchRequest {
            query_vectors: vec![embedding.to_vec()],
            top_k,
            namespace: Some(namespace),
            kind: Some(SubstrateKind::Entity),
            embedding_model: Some(descriptor.model_name.to_string()),
            filter: None,
            backend_hints: None,
        };
        request
            .validate()
            .map_err(|error| RuntimeError::Internal(format!("moodboard vector query: {error}")))?;
        namespace_hits.push(store.search(request).await?);
    }
    Ok(merge_namespace_hits(namespace_hits, top_k))
}

#[cfg(test)]
async fn index_embedding(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    descriptor: &DescriptorIdentity,
    asset_id: Uuid,
    embedding: &[f32],
) -> Result<(), RuntimeError> {
    let identity = descriptor.vector_identity()?;
    index_embedding_with_identity(runtime, token, &identity, descriptor, asset_id, embedding).await
}

#[cfg(test)]
async fn search_embedding(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    descriptor: &DescriptorIdentity,
    embedding: &[f32],
    top_k: u32,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    let identity = descriptor.vector_identity()?;
    search_embedding_with_identity(runtime, token, &identity, descriptor, embedding, top_k).await
}

fn merge_namespace_hits(
    namespace_hits: Vec<Vec<VectorSearchHit>>,
    top_k: u32,
) -> Vec<VectorSearchHit> {
    union_fusion(
        namespace_hits
            .into_iter()
            .map(|hits| {
                hits.into_iter()
                    .map(|hit| (hit.subject_id, hit.score))
                    .collect()
            })
            .collect(),
    )
    .into_iter()
    .take(top_k as usize)
    .enumerate()
    .map(|(index, (subject_id, score))| VectorSearchHit {
        subject_id,
        score,
        rank: u32::try_from(index + 1).expect("top_k is bounded to u32"),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
        time::Duration,
    };

    use super::*;
    use async_trait::async_trait;
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::{BackendId, RuntimeConfig};
    use khive_storage::{Attachment, AttachmentSubstrate};
    use khive_types::Namespace;

    #[derive(Debug)]
    struct OrderedHydrationStore {
        bytes: Vec<u8>,
        content_ref: ContentRef,
        started: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl BlobStore for OrderedHydrationStore {
        async fn put(&self, _bytes: Vec<u8>) -> khive_storage::StorageResult<ContentRef> {
            panic!("put is not used by the search ordering test")
        }

        async fn get_bounded_verified(
            &self,
            content_ref: &ContentRef,
            max_bytes: u64,
        ) -> khive_storage::StorageResult<Vec<u8>> {
            assert_eq!(content_ref, &self.content_ref);
            assert_eq!(max_bytes, MAX_OBJECT_BYTES as u64);
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            Ok(self.bytes.clone())
        }

        async fn exists(&self, content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            Ok(content_ref == &self.content_ref)
        }

        async fn size(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::StorageResult<Option<u64>> {
            panic!("search source hydration must not compose size with a read")
        }

        async fn delete(&self, _content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            panic!("delete is not used by the search ordering test")
        }
    }

    #[derive(Debug)]
    struct CandidateProbeStore {
        present: BTreeSet<String>,
        fail_on: Option<String>,
        exists_calls: AtomicUsize,
    }

    impl CandidateProbeStore {
        fn new(present: impl IntoIterator<Item = ContentRef>) -> Self {
            Self {
                present: present
                    .into_iter()
                    .map(|content_ref| content_ref.to_string())
                    .collect(),
                fail_on: None,
                exists_calls: AtomicUsize::new(0),
            }
        }

        fn failing(content_ref: &ContentRef) -> Self {
            Self {
                present: BTreeSet::new(),
                fail_on: Some(content_ref.to_string()),
                exists_calls: AtomicUsize::new(0),
            }
        }

        fn exists_calls(&self) -> usize {
            self.exists_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BlobStore for CandidateProbeStore {
        async fn put(&self, _bytes: Vec<u8>) -> khive_storage::StorageResult<ContentRef> {
            panic!("put is not used by candidate materialization tests")
        }

        async fn get_bounded_verified(
            &self,
            _content_ref: &ContentRef,
            _max_bytes: u64,
        ) -> khive_storage::StorageResult<Vec<u8>> {
            panic!("candidate materialization must not hydrate blob bytes")
        }

        async fn exists(&self, content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            self.exists_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_on.as_deref() == Some(content_ref.as_str()) {
                return Err(khive_storage::StorageError::Timeout {
                    operation: "moodboard_candidate_exists_failure".into(),
                });
            }
            Ok(self.present.contains(content_ref.as_str()))
        }

        async fn size(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::StorageResult<Option<u64>> {
            panic!("candidate materialization must not preflight blob size")
        }

        async fn delete(&self, _content_ref: &ContentRef) -> khive_storage::StorageResult<bool> {
            panic!("delete is not used by candidate materialization tests")
        }
    }

    fn vector_hit(subject_id: Uuid, raw_score: i64, rank: u32) -> VectorSearchHit {
        serde_json::from_value(json!({
            "subject_id": subject_id,
            "score": raw_score,
            "rank": rank,
        }))
        .expect("valid vector hit fixture")
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_candidate_entity(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: Uuid,
        namespace: &str,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        content_ref: Option<ContentRef>,
    ) {
        let mut entity = Entity::new(namespace, kind, name)
            .with_entity_type(entity_type.map(ToString::to_string));
        entity.id = id;
        let created_at = entity.created_at;
        runtime
            .entities(token)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("candidate entity");
        if let Some(content_ref) = content_ref {
            runtime
                .attachments()
                .expect("attachment store")
                .upsert_attachment(Attachment {
                    record_uuid: id,
                    substrate: AttachmentSubstrate::Entity,
                    role: "content".to_string(),
                    content_ref,
                    media_type: Some("image/png".to_string()),
                    size_bytes: Some(1),
                    created_at,
                })
                .await
                .expect("candidate content attachment");
        }
    }

    fn main_and_secondary_runtimes() -> (KhiveRuntime, KhiveRuntime) {
        let make_backend = || {
            let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
            {
                let mut writer = backend.pool().try_writer().expect("writer");
                khive_db::run_migrations(writer.conn_mut()).expect("migrations");
            }
            Arc::new(backend)
        };
        let main_backend = make_backend();
        let secondary_backend = make_backend();

        let mut main_config = RuntimeConfig::no_embeddings();
        main_config.packs = vec!["kg".to_string()];
        main_config.backend_id = BackendId::new(BackendId::MAIN);
        let main = KhiveRuntime::from_backend(main_backend.clone(), main_config);

        let mut secondary_config = RuntimeConfig::no_embeddings();
        secondary_config.packs = vec!["kg".to_string()];
        secondary_config.backend_id = BackendId::new("moodboard");
        let secondary = KhiveRuntime::from_backend(secondary_backend, secondary_config)
            .with_core_backend(main_backend);
        (main, secondary)
    }

    #[tokio::test]
    async fn source_hydration_precedes_and_waits_through_preprocessing_admission() {
        let bytes = b"digest-valid but not a raster".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let store = Arc::new(OrderedHydrationStore {
            bytes,
            content_ref: content_ref.clone(),
            started: StdMutex::new(Some(started_tx)),
            calls: AtomicUsize::new(0),
        });
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.packs = vec!["kg".to_string()];
        config.blob_hydration_bytes = MAX_OBJECT_BYTES as u64;
        let runtime = KhiveRuntime::new(config).expect("memory runtime");
        runtime
            .install_blob_store(Arc::clone(&store) as Arc<dyn BlobStore>)
            .expect("install blob store");
        let pack = Arc::new(MoodboardPack::new(runtime.clone()));
        let held_preprocessing = pack
            .model_state()
            .acquire_preprocessing_permit()
            .await
            .expect("hold preprocessing admission");
        let core = runtime.core();
        let task_pack = Arc::clone(&pack);
        let task_ref = content_ref.clone();
        let task = tokio::spawn(async move {
            prepare_source_raster(task_pack.as_ref(), &core, &task_ref).await
        });

        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("backend hydration must start promptly")
            .expect("backend hydration must start before preprocessing admission is available");
        assert!(
            !task.is_finished(),
            "verified source and its lease must wait behind preprocessing admission"
        );

        let hydrator = runtime.blob_hydrator().expect("installed hydrator");
        let mut second_hydration =
            Box::pin(hydrator.hydrate_verified(&content_ref, MAX_OBJECT_BYTES as u64));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second_hydration.as_mut())
                .await
                .is_err(),
            "a second full-budget hydration must wait for the source lease"
        );
        assert_eq!(
            store.calls.load(Ordering::SeqCst),
            1,
            "the queued hydration must not reach the backend before lease release"
        );

        drop(held_preprocessing);
        task.await
            .expect("source preparation task joins")
            .expect_err("fixture bytes are not a raster");
        tokio::time::timeout(Duration::from_secs(1), second_hydration)
            .await
            .expect("queued hydration proceeds after source lease release")
            .expect("second hydration succeeds");
        assert_eq!(store.calls.load(Ordering::SeqCst), 2);
    }

    async fn moodboard_ann_delta_count(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        descriptor: &DescriptorIdentity,
    ) -> i64 {
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        match reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM ann_write_log \
                      WHERE namespace = ?1 AND embedding_model = ?2 AND field = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(token.namespace().as_str().to_string()),
                    SqlValue::Text(descriptor.model_name.to_string()),
                    SqlValue::Text(VISUAL_FIELD.to_string()),
                ],
                label: Some("moodboard_ann_delta_count".to_string()),
            })
            .await
            .expect("ann_write_log count")
        {
            Some(SqlValue::Integer(count)) => count,
            other => panic!("unexpected ann_write_log count: {other:?}"),
        }
    }

    #[test]
    fn input_parsing_is_strict_and_bounded() {
        assert!(parse_asset_id(&json!({"asset_id": Uuid::nil().to_string()})).is_ok());
        assert!(parse_asset_id(&json!({"asset_id": "not-a-uuid"})).is_err());
        assert_eq!(parse_top_k(&json!({})).unwrap(), DEFAULT_TOP_K);
        assert!(parse_top_k(&json!({"top_k": 0})).is_err());
        assert!(parse_top_k(&json!({"top_k": 101})).is_err());
        assert!(optional_string(&json!({"name": 3}), "name", "test", 10).is_err());
        assert!(require_fields(&json!({"future": true}), "moodboard.model", &[]).is_err());
        assert!(require_fields(
            &json!({"image_base64": "", "image_bas64": ""}),
            "moodboard.ingest",
            &["image_base64", "name", "media_type", "caption"]
        )
        .is_err());
        assert_eq!(candidate_limit(20), 81);
        assert_eq!(candidate_limit(100), 401);

        let properties = asset_properties(
            &PreparedRaster {
                inference_png: Vec::new(),
                media_type: "image/png",
                original_width: 10,
                original_height: 20,
            },
            123,
        );
        assert!(properties.get("inference_width").is_none());
        assert!(properties.get("inference_height").is_none());
        assert!(require_fields(
            &json!({"asset_id": Uuid::nil().to_string(), "topk": 3}),
            "moodboard.search",
            &["asset_id", "top_k"]
        )
        .is_err());

        let other_namespace_asset =
            Entity::new("remote-agent", "artifact", "cross-namespace by-id asset")
                .with_entity_type(Some("visual_asset"));
        assert!(validate_visual_asset(&other_namespace_asset).is_ok());

        let invalid_score: VectorSearchHit = serde_json::from_value(json!({
            "subject_id": Uuid::new_v4(),
            "score": 8_589_934_592_i64,
            "rank": 1
        }))
        .unwrap();
        assert!(validated_cosine_score(&invalid_score).is_err());
        let infinite_score: VectorSearchHit = serde_json::from_value(json!({
            "subject_id": Uuid::new_v4(),
            "score": i64::MAX,
            "rank": 1
        }))
        .unwrap();
        assert!(validated_cosine_score(&infinite_score).is_err());
        assert!(is_stale_candidate_error(&RuntimeError::NotFound(
            "gone".to_string()
        )));
        assert!(is_stale_candidate_error(&RuntimeError::NamespaceMismatch {
            id: Uuid::new_v4()
        }));
        assert!(!is_stale_candidate_error(&RuntimeError::Internal(
            "backend fault".to_string()
        )));
    }

    #[test]
    fn namespace_union_merge_keeps_max_score_canonical_ties_and_global_limit() {
        let duplicate = Uuid::from_u128(1);
        let lower_tie_id = Uuid::from_u128(2);
        let higher_tie_id = Uuid::from_u128(3);
        let truncated = Uuid::from_u128(4);

        let merged = merge_namespace_hits(
            vec![
                vec![
                    vector_hit(duplicate, 1_700_000_000, 1),
                    vector_hit(higher_tie_id, 3_400_000_000, 2),
                ],
                vec![
                    vector_hit(duplicate, 4_000_000_000, 1),
                    vector_hit(lower_tie_id, 3_400_000_000, 2),
                    vector_hit(truncated, 3_000_000_000, 3),
                ],
            ],
            3,
        );

        assert_eq!(
            merged.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
            vec![duplicate, lower_tie_id, higher_tie_id]
        );
        assert_eq!(merged[0].score.to_raw(), 4_000_000_000);
        assert_eq!(
            merged.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn ranked_materialization_self_hit_never_probes_blob() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let query_id = Uuid::from_u128(10);
        let live_id = Uuid::from_u128(11);
        let live_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
        insert_candidate_entity(
            &runtime,
            &token,
            live_id,
            token.namespace().as_str(),
            "artifact",
            Some("visual_asset"),
            "live",
            Some(live_ref.clone()),
        )
        .await;
        let blob_store = CandidateProbeStore::new([live_ref]);

        let result = materialize_hits_with_diagnostics(
            &runtime,
            &token,
            &blob_store,
            query_id,
            vec![
                vector_hit(query_id, 4_000_000_000, 1),
                vector_hit(live_id, 3_000_000_000, 2),
            ],
            1,
        )
        .await
        .expect("self exclusion followed by live hit");

        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.accepted[0].output["asset_id"], live_id.to_string());
        assert_eq!(
            result.drop_counts.count(MoodboardDropReason::SelfHit),
            Some(1)
        );
        assert_eq!(
            blob_store.exists_calls(),
            1,
            "self-hit must not consume a blob metadata probe"
        );
    }

    #[tokio::test]
    async fn ranked_materialization_validates_post_k_tail_without_blob_io() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let live_id = Uuid::from_u128(20);
        let tail_id = Uuid::from_u128(21);
        let live_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
        let tail_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
        insert_candidate_entity(
            &runtime,
            &token,
            live_id,
            token.namespace().as_str(),
            "artifact",
            Some("visual_asset"),
            "live",
            Some(live_ref.clone()),
        )
        .await;
        insert_candidate_entity(
            &runtime,
            &token,
            tail_id,
            token.namespace().as_str(),
            "artifact",
            Some("visual_asset"),
            "tail",
            Some(tail_ref.clone()),
        )
        .await;
        let blob_store = CandidateProbeStore::new([live_ref, tail_ref]);

        let error = materialize_hits_with_diagnostics(
            &runtime,
            &token,
            &blob_store,
            Uuid::from_u128(22),
            vec![
                vector_hit(live_id, 4_000_000_000, 1),
                vector_hit(tail_id, -8_589_934_592, 2),
            ],
            1,
        )
        .await
        .expect_err("invalid tail score remains fatal after K accepted hits");

        assert!(error
            .to_string()
            .contains("moodboard vector backend returned invalid cosine score"));
        assert_eq!(
            blob_store.exists_calls(),
            1,
            "post-K tail validation must perform no loader or blob I/O"
        );
    }

    #[tokio::test]
    async fn ranked_materialization_loader_error_precedes_later_invalid_score() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let failing_id = Uuid::from_u128(30);
        let tail_id = Uuid::from_u128(31);
        let failing_ref = ContentRef::from_hex("c".repeat(64)).unwrap();
        insert_candidate_entity(
            &runtime,
            &token,
            failing_id,
            token.namespace().as_str(),
            "artifact",
            Some("visual_asset"),
            "failing",
            Some(failing_ref.clone()),
        )
        .await;
        let blob_store = CandidateProbeStore::failing(&failing_ref);

        let error = materialize_hits_with_diagnostics(
            &runtime,
            &token,
            &blob_store,
            Uuid::from_u128(32),
            vec![
                vector_hit(failing_id, 4_000_000_000, 1),
                vector_hit(tail_id, -8_589_934_592, 2),
            ],
            1,
        )
        .await
        .expect_err("the earlier loader error must win");

        match error {
            RuntimeError::Storage(khive_storage::StorageError::Timeout { operation }) => {
                assert_eq!(operation.as_ref(), "moodboard_candidate_exists_failure");
            }
            other => panic!("expected the original typed storage error, got {other:?}"),
        }
        assert_eq!(blob_store.exists_calls(), 1);
    }

    #[tokio::test]
    async fn ranked_materialization_counts_typed_drops_and_honestly_underfills() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let query_id = Uuid::from_u128(40);
        let stale_id = Uuid::from_u128(41);
        let wrong_scope_id = Uuid::from_u128(42);
        let wrong_kind_id = Uuid::from_u128(43);
        let wrong_subtype_id = Uuid::from_u128(44);
        let missing_role_id = Uuid::from_u128(45);
        let missing_blob_id = Uuid::from_u128(46);
        let live_id = Uuid::from_u128(47);
        let deleted_id = Uuid::from_u128(48);
        let malformed_ref_id = Uuid::from_u128(49);
        let missing_blob_ref = ContentRef::from_hex("d".repeat(64)).unwrap();
        let live_ref = ContentRef::from_hex("e".repeat(64)).unwrap();
        let deleted_ref = ContentRef::from_hex("f".repeat(64)).unwrap();
        let malformed_placeholder_ref = ContentRef::from_hex("0".repeat(64)).unwrap();

        for (id, namespace, kind, entity_type, name, content_ref) in [
            (
                wrong_scope_id,
                "lambda:hidden",
                "artifact",
                Some("visual_asset"),
                "wrong scope",
                Some(ContentRef::from_hex("1".repeat(64)).unwrap()),
            ),
            (
                wrong_kind_id,
                token.namespace().as_str(),
                "document",
                Some("visual_asset"),
                "wrong kind",
                Some(ContentRef::from_hex("2".repeat(64)).unwrap()),
            ),
            (
                wrong_subtype_id,
                token.namespace().as_str(),
                "artifact",
                Some("moodboard"),
                "wrong subtype",
                Some(ContentRef::from_hex("3".repeat(64)).unwrap()),
            ),
            (
                missing_role_id,
                token.namespace().as_str(),
                "artifact",
                Some("visual_asset"),
                "missing role",
                None,
            ),
            (
                missing_blob_id,
                token.namespace().as_str(),
                "artifact",
                Some("visual_asset"),
                "missing blob",
                Some(missing_blob_ref.clone()),
            ),
            (
                live_id,
                token.namespace().as_str(),
                "artifact",
                Some("visual_asset"),
                "live",
                Some(live_ref.clone()),
            ),
            (
                deleted_id,
                token.namespace().as_str(),
                "artifact",
                Some("visual_asset"),
                "soft deleted",
                Some(deleted_ref),
            ),
            (
                malformed_ref_id,
                token.namespace().as_str(),
                "artifact",
                Some("visual_asset"),
                "malformed compatibility projection",
                Some(malformed_placeholder_ref),
            ),
        ] {
            insert_candidate_entity(
                &runtime,
                &token,
                id,
                namespace,
                kind,
                entity_type,
                name,
                content_ref,
            )
            .await;
        }
        assert!(runtime
            .delete_entity(&token, deleted_id, false)
            .await
            .expect("soft delete candidate"));
        let mut writer = runtime.sql().writer().await.expect("SQL writer");
        writer
            .execute_script(format!(
                "PRAGMA ignore_check_constraints = ON; \
                 UPDATE attachments SET content_ref = 'malformed' \
                 WHERE record_uuid = '{malformed_ref_id}' AND role = 'content'; \
                 PRAGMA ignore_check_constraints = OFF;"
            ))
            .await
            .expect("inject malformed compatibility projection");
        drop(writer);
        let blob_store = CandidateProbeStore::new([live_ref.clone()]);
        let live_score_raw = 1_000_000_000;

        let result = materialize_hits_with_diagnostics(
            &runtime,
            &token,
            &blob_store,
            query_id,
            vec![
                vector_hit(query_id, 4_000_000_000, 1),
                vector_hit(stale_id, 3_800_000_000, 2),
                vector_hit(deleted_id, 3_600_000_000, 3),
                vector_hit(wrong_scope_id, 3_400_000_000, 4),
                vector_hit(wrong_kind_id, 3_200_000_000, 5),
                vector_hit(wrong_subtype_id, 3_000_000_000, 6),
                vector_hit(missing_role_id, 2_800_000_000, 7),
                vector_hit(malformed_ref_id, 2_600_000_000, 8),
                vector_hit(missing_blob_id, 2_400_000_000, 9),
                vector_hit(live_id, live_score_raw, 10),
            ],
            2,
        )
        .await
        .expect("drops are non-fatal and candidate exhaustion honestly underfills");

        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.accepted[0].rank, 1);
        assert_eq!(
            result.accepted[0].output,
            json!({
                "asset_id": live_id.to_string(),
                "score": live_score_raw as f64 / 4_294_967_296.0,
                "rank": 1,
                "name": "live",
                "content_ref": live_ref.to_string(),
            })
        );
        assert_eq!(
            result.drop_counts.count(MoodboardDropReason::StaleEntity),
            Some(2)
        );
        for reason in [
            MoodboardDropReason::SelfHit,
            MoodboardDropReason::OutsideVisibleScope,
            MoodboardDropReason::WrongKind,
            MoodboardDropReason::WrongSubtype,
            MoodboardDropReason::MissingContentAttachment,
            MoodboardDropReason::MalformedContentRef,
            MoodboardDropReason::MissingBlob,
        ] {
            assert_eq!(result.drop_counts.count(reason), Some(1), "{reason:?}");
        }
        assert_eq!(result.drop_counts.total(), 9);
        assert_eq!(
            blob_store.exists_calls(),
            2,
            "only otherwise eligible candidates may probe blob metadata"
        );
    }

    #[tokio::test]
    async fn exact_visual_store_preserves_negative_cosine_endpoint() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let descriptor = DescriptorIdentity::fixture(4);
        let same = Uuid::new_v4();
        let opposite = Uuid::new_v4();
        index_embedding(&runtime, &token, &descriptor, same, &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            opposite,
            &[-1.0, 0.0, 0.0, 0.0],
        )
        .await
        .unwrap();
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            opposite,
            &[0.0, -1.0, 0.0, 0.0],
        )
        .await
        .unwrap();
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            opposite,
            &[-1.0, 0.0, 0.0, 0.0],
        )
        .await
        .unwrap();
        assert_eq!(
            moodboard_ann_delta_count(&runtime, &token, &descriptor).await,
            0,
            "permanently exact Moodboard inserts and replacements must not leak unconsumed ANN deltas"
        );

        let hits = search_embedding(&runtime, &token, &descriptor, &[1.0, 0.0, 0.0, 0.0], 2)
            .await
            .unwrap();
        assert_eq!(hits[0].subject_id, same);
        assert!((hits[0].score.to_f64() - 1.0).abs() < 1e-6);
        let opposite_hit = hits
            .iter()
            .find(|hit| hit.subject_id == opposite)
            .expect("opposite vector hit");
        assert!((opposite_hit.score.to_f64() + 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn visual_search_fans_out_over_visible_namespaces_but_narrow_token_does_not() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let primary_namespace = Namespace::local();
        let extra_namespace = Namespace::parse("lambda:moodboard-visible").unwrap();
        let narrow = runtime
            .authorize(primary_namespace.clone())
            .expect("narrow token");
        let extra = runtime
            .authorize(extra_namespace.clone())
            .expect("extra token");
        let wide = runtime
            .authorize_with_visibility(primary_namespace, vec![extra_namespace])
            .expect("wide token");
        let descriptor = DescriptorIdentity::fixture(4);
        let extra_id = Uuid::from_u128(1);
        let primary_id = Uuid::from_u128(2);

        index_embedding(
            &runtime,
            &narrow,
            &descriptor,
            primary_id,
            &[1.0, 0.0, 0.0, 0.0],
        )
        .await
        .unwrap();
        index_embedding(
            &runtime,
            &extra,
            &descriptor,
            extra_id,
            &[1.0, 0.0, 0.0, 0.0],
        )
        .await
        .unwrap();

        let wide_hits = search_embedding(&runtime, &wide, &descriptor, &[1.0, 0.0, 0.0, 0.0], 2)
            .await
            .unwrap();
        assert_eq!(
            wide_hits
                .iter()
                .map(|hit| hit.subject_id)
                .collect::<Vec<_>>(),
            vec![extra_id, primary_id],
            "equal scores use a stable UUID tie-break across namespace queries"
        );
        assert_eq!(
            wide_hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let narrow_hits =
            search_embedding(&runtime, &narrow, &descriptor, &[1.0, 0.0, 0.0, 0.0], 2)
                .await
                .unwrap();
        assert_eq!(
            narrow_hits
                .iter()
                .map(|hit| hit.subject_id)
                .collect::<Vec<_>>(),
            vec![primary_id],
            "an explicit namespace token stays narrow"
        );

        let root = tempfile::tempdir().unwrap();
        let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
        runtime
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let primary_ref = blob_store.put(b"primary".to_vec()).await.unwrap();
        let extra_ref = blob_store.put(b"extra".to_vec()).await.unwrap();
        let mut primary_entity =
            Entity::new(narrow.namespace().as_str(), "artifact", "primary candidate")
                .with_entity_type(Some("visual_asset"));
        primary_entity.id = primary_id;
        let primary_created_at = primary_entity.created_at;
        runtime
            .entities(&narrow)
            .unwrap()
            .upsert_entity(primary_entity)
            .await
            .unwrap();
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: primary_id,
                substrate: AttachmentSubstrate::Entity,
                role: "content".to_string(),
                content_ref: primary_ref,
                media_type: None,
                size_bytes: Some(7),
                created_at: primary_created_at,
            })
            .await
            .unwrap();
        let mut extra_entity =
            Entity::new(extra.namespace().as_str(), "artifact", "extra candidate")
                .with_entity_type(Some("visual_asset"));
        extra_entity.id = extra_id;
        let extra_created_at = extra_entity.created_at;
        runtime
            .entities(&extra)
            .unwrap()
            .upsert_entity(extra_entity)
            .await
            .unwrap();
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: extra_id,
                substrate: AttachmentSubstrate::Entity,
                role: "content".to_string(),
                content_ref: extra_ref,
                media_type: None,
                size_bytes: Some(5),
                created_at: extra_created_at,
            })
            .await
            .unwrap();

        let materialized = materialize_hits(
            &runtime,
            &wide,
            blob_store.as_ref(),
            Uuid::new_v4(),
            wide_hits.clone(),
            2,
        )
        .await
        .unwrap();
        assert_eq!(materialized.len(), 2);
        assert_eq!(materialized[0]["asset_id"], extra_id.to_string());

        let narrow_materialized = materialize_hits(
            &runtime,
            &narrow,
            blob_store.as_ref(),
            Uuid::new_v4(),
            wide_hits,
            2,
        )
        .await
        .unwrap();
        assert_eq!(narrow_materialized.len(), 1);
        assert_eq!(narrow_materialized[0]["asset_id"], primary_id.to_string());
    }

    #[tokio::test]
    async fn visual_entities_use_core_vectors_use_secondary_and_search_hydrates_core() {
        let (main, secondary) = main_and_secondary_runtimes();
        let pack = MoodboardPack::new(secondary);
        let token = pack
            .runtime()
            .authorize(Namespace::local())
            .expect("authorize");
        let root = tempfile::tempdir().unwrap();
        let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
        pack.runtime()
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let query_ref = blob_store
            .put(b"core query original".to_vec())
            .await
            .unwrap();
        let candidate_ref = blob_store
            .put(b"core candidate original".to_vec())
            .await
            .unwrap();
        let prepared = PreparedRaster {
            inference_png: Vec::new(),
            media_type: "image/png",
            original_width: 32,
            original_height: 32,
        };
        let core = pack.runtime().core();
        let (query, _) = find_or_create_visual_asset(
            &core,
            &token,
            &query_ref,
            Some("query"),
            None,
            &prepared,
            19,
        )
        .await
        .unwrap();
        let (candidate, _) = find_or_create_visual_asset(
            &core,
            &token,
            &candidate_ref,
            Some("candidate"),
            None,
            &prepared,
            23,
        )
        .await
        .unwrap();

        assert_eq!(
            main.get_entity(&token, query.id).await.unwrap().id,
            query.id
        );
        assert_eq!(
            main.get_entity(&token, candidate.id).await.unwrap().id,
            candidate.id
        );
        assert!(pack.runtime().get_entity(&token, query.id).await.is_err());
        assert!(
            find_visual_asset(pack.runtime(), &token, &query_ref)
                .await
                .unwrap()
                .is_none(),
            "visual_asset SQL must leave the secondary backend empty"
        );

        let descriptor = DescriptorIdentity::fixture(4);
        index_embedding(
            pack.runtime(),
            &token,
            &descriptor,
            query.id,
            &[1.0, 0.0, 0.0, 0.0],
        )
        .await
        .unwrap();
        index_embedding(
            pack.runtime(),
            &token,
            &descriptor,
            candidate.id,
            &[0.8, 0.6, 0.0, 0.0],
        )
        .await
        .unwrap();

        let raw_hits = search_embedding(
            pack.runtime(),
            &token,
            &descriptor,
            &[1.0, 0.0, 0.0, 0.0],
            2,
        )
        .await
        .unwrap();
        assert_eq!(raw_hits.len(), 2);
        let main_hits = search_embedding(&main, &token, &descriptor, &[1.0, 0.0, 0.0, 0.0], 2)
            .await
            .unwrap();
        assert!(
            main_hits.is_empty(),
            "visual vectors must not leak into main"
        );

        let hits = materialize_hits(&core, &token, blob_store.as_ref(), query.id, raw_hits, 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["asset_id"], candidate.id.to_string());
        assert_eq!(hits[0]["content_ref"], candidate_ref.to_string());
    }

    #[tokio::test]
    async fn stale_vector_overfetch_is_skipped_and_may_underfill() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let descriptor = DescriptorIdentity::fixture(4);
        let root = tempfile::tempdir().unwrap();
        let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
        runtime
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let live_ref = blob_store.put(b"live original".to_vec()).await.unwrap();
        let stale = Uuid::new_v4();
        let live = Entity::new(token.namespace().as_str(), "artifact", "live candidate")
            .with_entity_type(Some("visual_asset"));
        let missing = Entity::new(token.namespace().as_str(), "artifact", "missing blob")
            .with_entity_type(Some("visual_asset"));
        runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(live.clone())
            .await
            .unwrap();
        runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(missing.clone())
            .await
            .unwrap();
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: live.id,
                substrate: AttachmentSubstrate::Entity,
                role: "content".to_string(),
                content_ref: live_ref,
                media_type: None,
                size_bytes: Some(13),
                created_at: live.created_at,
            })
            .await
            .unwrap();
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: missing.id,
                substrate: AttachmentSubstrate::Entity,
                role: "content".to_string(),
                content_ref: ContentRef::from_hex("b".repeat(64)).unwrap(),
                media_type: None,
                size_bytes: None,
                created_at: missing.created_at,
            })
            .await
            .unwrap();
        index_embedding(&runtime, &token, &descriptor, stale, &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            missing.id,
            &[0.8, 0.6, 0.0, 0.0],
        )
        .await
        .unwrap();
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            live.id,
            &[0.0, 1.0, 0.0, 0.0],
        )
        .await
        .unwrap();

        let raw = search_embedding(&runtime, &token, &descriptor, &[1.0, 0.0, 0.0, 0.0], 4)
            .await
            .unwrap();
        let hits = materialize_hits(
            &runtime,
            &token,
            blob_store.as_ref(),
            Uuid::new_v4(),
            raw,
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "stale rows are skipped without score fabrication"
        );
        assert_eq!(hits[0]["asset_id"], live.id.to_string());
        assert_eq!(hits[0]["rank"], 1);
    }

    #[tokio::test]
    async fn concurrent_same_content_rechecks_under_content_lock() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let root = tempfile::tempdir().unwrap();
        let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
        runtime
            .install_blob_store(blob_store.clone())
            .expect("install blob store");
        let content_ref = blob_store
            .put(b"same original bytes".to_vec())
            .await
            .unwrap();
        let prepared = PreparedRaster {
            inference_png: Vec::new(),
            media_type: "image/png",
            original_width: 32,
            original_height: 32,
        };

        let first = find_or_create_visual_asset(
            &runtime,
            &token,
            &content_ref,
            Some("same"),
            None,
            &prepared,
            19,
        );
        let second = find_or_create_visual_asset(
            &runtime,
            &token,
            &content_ref,
            Some("same"),
            None,
            &prepared,
            19,
        );
        let (first, second) = tokio::join!(first, second);
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.0.id, second.0.id);
        assert_ne!(first.1, second.1, "exactly one caller creates the entity");
    }
}
