//! Moodboard verb handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::types::{
    SqlStatement, SqlValue, VectorIndexKind, VectorSearchHit, VectorSearchRequest,
};
use khive_storage::{BlobStore, Entity, VectorStore};
use khive_types::SubstrateKind;

use crate::model::{validate_embedding, DescriptorIdentity, LoadedVisionModel, VisionModelState};
use crate::preprocess::{prepare_raster, PreparedRaster};
use crate::MoodboardPack;

const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
const VISUAL_FIELD: &str = "visual.descriptor";
const DEFAULT_TOP_K: u32 = 20;
const MAX_TOP_K: u32 = 100;
const MAX_CANDIDATE_MULTIPLIER: u32 = 4;
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
    index_embedding(pack.runtime(), token, &descriptor, asset.id, &embedding).await?;

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
    let preprocessing_permit = pack.model_state().acquire_preprocessing_permit().await?;
    let original = read_bounded_source_blob(blob_store.as_ref(), &content_ref).await?;
    let prepared = prepare_raster(&original, None)?;
    drop(original);
    drop(preprocessing_permit);

    let model = pack.model_state().get().await?;
    let descriptor = model.descriptor().clone();
    let embedding = infer_prepared(
        pack.model_state(),
        model,
        prepared.inference_png,
        &descriptor,
    )
    .await?;
    let raw_hits = search_embedding(
        pack.runtime(),
        token,
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
    let mut hits = Vec::with_capacity(top_k as usize);
    let authorized_namespaces: BTreeSet<&str> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str())
        .collect();
    for hit in raw_hits {
        let score = validated_cosine_score(&hit)?;
        if hit.subject_id == query_asset_id || hits.len() == top_k as usize {
            continue;
        }
        let candidate = match runtime.get_entity(token, hit.subject_id).await {
            Ok(candidate) => candidate,
            Err(error) if is_stale_candidate_error(&error) => continue,
            Err(error) => return Err(error),
        };
        if !authorized_namespaces.contains(candidate.namespace.as_str())
            || candidate.kind != "artifact"
            || candidate.entity_type.as_deref() != Some("visual_asset")
        {
            continue;
        }
        let Some(candidate_ref) = candidate.content_ref else {
            continue;
        };
        let Ok(candidate_ref) = ContentRef::from_hex(candidate_ref) else {
            continue;
        };
        if !blob_store.exists(&candidate_ref).await? {
            continue;
        }
        hits.push(json!({
            "asset_id": candidate.id.to_string(),
            "score": score,
            "rank": hits.len() + 1,
            "name": candidate.name,
            "content_ref": candidate_ref.to_string(),
        }));
    }
    Ok(hits)
}

fn validated_cosine_score(hit: &VectorSearchHit) -> Result<f64, RuntimeError> {
    let score = hit.score.to_f64();
    if !score.is_finite() || !(-1.0..=1.0).contains(&score) {
        return Err(RuntimeError::Internal(format!(
            "moodboard vector backend returned invalid cosine score {score} for {} (expected finite [-1,1])",
            hit.subject_id
        )));
    }
    Ok(score)
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

async fn read_bounded_source_blob(
    blob_store: &dyn BlobStore,
    content_ref: &ContentRef,
) -> Result<Vec<u8>, RuntimeError> {
    let reported_size = blob_store
        .size(content_ref)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("moodboard source blob {content_ref}")))?;
    if reported_size > MAX_OBJECT_BYTES as u64 {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard source blob {content_ref} is {reported_size} bytes, exceeding the {MAX_OBJECT_BYTES}-byte maximum"
        )));
    }

    let bytes = blob_store.get(content_ref).await?;
    if bytes.len() > MAX_OBJECT_BYTES {
        return Err(RuntimeError::Internal(format!(
            "moodboard BlobStore returned {} bytes for {content_ref}, exceeding the preflighted {MAX_OBJECT_BYTES}-byte maximum",
            bytes.len()
        )));
    }
    if bytes.len() as u64 != reported_size {
        return Err(RuntimeError::Internal(format!(
            "moodboard BlobStore object {content_ref} changed size between preflight ({reported_size}) and read ({})",
            bytes.len()
        )));
    }
    verify_blob_digest(&bytes, content_ref)?;
    Ok(bytes)
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
            sql: "SELECT id FROM entities WHERE namespace = ?1 AND kind = 'artifact' \
                  AND entity_type = 'visual_asset' AND content_ref = ?2 \
                  AND deleted_at IS NULL ORDER BY created_at, id LIMIT 1"
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
    let asset = runtime
        .create_entity_with_content_ref(
            token,
            "artifact",
            Some("visual_asset"),
            name.unwrap_or(&default_name),
            caption,
            Some(asset_properties(prepared, original_len)),
            vec!["moodboard".to_string(), "visual_asset".to_string()],
            content_ref,
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

fn verify_blob_digest(bytes: &[u8], expected: &ContentRef) -> Result<(), RuntimeError> {
    let actual = ContentRef::from_digest_bytes(blake3::hash(bytes).as_bytes());
    if &actual != expected {
        return Err(RuntimeError::Internal(format!(
            "moodboard BlobStore object {expected} failed BLAKE3 verification (got {actual})"
        )));
    }
    Ok(())
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
    descriptor: &DescriptorIdentity,
) -> Result<Arc<dyn VectorStore>, RuntimeError> {
    let identity = descriptor.vector_identity()?;
    let store = runtime.vectors_for_named_identity(token, &identity).await?;
    let info = store.info().await?;
    if info.index_kind != VectorIndexKind::SqliteVec {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard v1 requires exact sqlite-vec retrieval, backend reported {:?}",
            info.index_kind
        )));
    }
    Ok(store)
}

async fn index_embedding(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    descriptor: &DescriptorIdentity,
    asset_id: Uuid,
    embedding: &[f32],
) -> Result<(), RuntimeError> {
    validate_embedding(embedding, descriptor)?;
    let store = exact_store(runtime, token, descriptor).await?;
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

async fn search_embedding(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    descriptor: &DescriptorIdentity,
    embedding: &[f32],
    top_k: u32,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    validate_embedding(embedding, descriptor)?;
    let store = exact_store(runtime, token, descriptor).await?;
    let namespaces: BTreeSet<String> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str().to_string())
        .collect();
    let mut merged = BTreeMap::<Uuid, VectorSearchHit>::new();
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
        for hit in store.search(request).await? {
            match merged.entry(hit.subject_id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(hit);
                }
                std::collections::btree_map::Entry::Occupied(mut entry)
                    if hit.score > entry.get().score =>
                {
                    entry.insert(hit);
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
    }

    let mut hits: Vec<_> = merged.into_values().collect();
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.subject_id.cmp(&right.subject_id))
    });
    hits.truncate(top_k as usize);
    for (index, hit) in hits.iter_mut().enumerate() {
        hit.rank = u32::try_from(index + 1).expect("top_k is bounded to u32");
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use async_trait::async_trait;
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::{BackendId, RuntimeConfig};
    use khive_types::Namespace;

    #[derive(Debug, Default)]
    struct OversizeBlobStore {
        get_calls: AtomicUsize,
    }

    #[async_trait]
    impl BlobStore for OversizeBlobStore {
        async fn put(&self, _bytes: Vec<u8>) -> khive_storage::types::StorageResult<ContentRef> {
            panic!("put is not used by the bounded read test")
        }

        async fn get(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::types::StorageResult<Vec<u8>> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn exists(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::types::StorageResult<bool> {
            Ok(true)
        }

        async fn size(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::types::StorageResult<Option<u64>> {
            Ok(Some(MAX_OBJECT_BYTES as u64 + 1))
        }

        async fn delete(
            &self,
            _content_ref: &ContentRef,
        ) -> khive_storage::types::StorageResult<bool> {
            Ok(false)
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

    #[tokio::test]
    async fn oversized_shared_blob_is_rejected_before_hydration() {
        let store = OversizeBlobStore::default();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(b"oversize").as_bytes());
        let error = read_bounded_source_blob(&store, &content_ref)
            .await
            .expect_err("oversized blob must fail at size preflight");
        assert!(error.to_string().contains("exceeding"));
        assert_eq!(store.get_calls.load(Ordering::SeqCst), 0);
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
        runtime.install_blob_store(blob_store.clone());
        let primary_ref = blob_store.put(b"primary".to_vec()).await.unwrap();
        let extra_ref = blob_store.put(b"extra".to_vec()).await.unwrap();
        let mut primary_entity =
            Entity::new(narrow.namespace().as_str(), "artifact", "primary candidate")
                .with_entity_type(Some("visual_asset"))
                .with_content_ref(primary_ref.to_string());
        primary_entity.id = primary_id;
        runtime
            .entities(&narrow)
            .unwrap()
            .upsert_entity(primary_entity)
            .await
            .unwrap();
        let mut extra_entity =
            Entity::new(extra.namespace().as_str(), "artifact", "extra candidate")
                .with_entity_type(Some("visual_asset"))
                .with_content_ref(extra_ref.to_string());
        extra_entity.id = extra_id;
        runtime
            .entities(&extra)
            .unwrap()
            .upsert_entity(extra_entity)
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
        pack.runtime().install_blob_store(blob_store.clone());
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
        runtime.install_blob_store(blob_store.clone());
        let live_ref = blob_store.put(b"live original".to_vec()).await.unwrap();
        let stale = Uuid::new_v4();
        let live = Entity::new(token.namespace().as_str(), "artifact", "live candidate")
            .with_entity_type(Some("visual_asset"))
            .with_content_ref(live_ref.to_string());
        let missing = Entity::new(token.namespace().as_str(), "artifact", "missing blob")
            .with_entity_type(Some("visual_asset"))
            .with_content_ref("b".repeat(64));
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
        runtime.install_blob_store(blob_store.clone());
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
