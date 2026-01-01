use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use khive_db::stores::blob::FsBlobStore;
use khive_runtime::{EmbedderProvider, RuntimeConfig, VerbRegistryBuilder};
use khive_types::Namespace;

use super::*;

const TEST_NAME: &str =
    "handlers::embedding_warning_tests::ingest_discloses_bounded_text_and_persists_visual_embedding";
const CHILD_FLAG: &str = "KHIVE_TEST_MOODBOARD_EMBEDDING_WARNING_CHILD";

struct RecordingTextService(Arc<StdMutex<Vec<String>>>);

#[async_trait]
impl lattice_embed::EmbeddingService for RecordingTextService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0.lock().unwrap().extend_from_slice(texts);
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "moodboard-disclosure-fixture"
    }
}

struct RecordingTextProvider(Arc<StdMutex<Vec<String>>>);

#[async_trait]
impl EmbedderProvider for RecordingTextProvider {
    fn name(&self) -> &str {
        "moodboard-disclosure-fixture"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(Arc::new(RecordingTextService(Arc::clone(&self.0))))
    }
}

fn png(seed: u8) -> Vec<u8> {
    let image = RgbImage::from_pixel(32, 32, Rgb([seed, 100, 200]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode fixture raster");
    encoded.into_inner()
}

async fn assert_visual_embedding(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    descriptor: &DescriptorIdentity,
    response: &Value,
) -> Uuid {
    let asset_id = Uuid::parse_str(response["asset_id"].as_str().expect("asset id"))
        .expect("canonical asset UUID");
    assert_eq!(response["created"], true);
    assert_eq!(response["indexed"], true);
    assert_eq!(
        response["descriptor"],
        serde_json::to_value(descriptor).unwrap()
    );
    let expected: Vec<f32> =
        serde_json::from_value(response["embedding"].clone()).expect("response visual embedding");
    assert_eq!(expected.len(), 8);
    validate_embedding(&expected, descriptor).expect("finite normalized visual vector");
    let store = runtime
        .vectors_for_named_identity(token, &descriptor.vector_identity().unwrap())
        .await
        .expect("open actual descriptor store");
    let stored = store
        .get_vectors(&[asset_id], token.namespace().as_str(), VISUAL_FIELD)
        .await
        .expect("read persisted visual vector");
    assert_eq!(stored.len(), 1, "the verb must run index_embedding");
    assert_eq!(stored.get(&asset_id), Some(&expected));
    asset_id
}

#[tokio::test]
async fn ingest_discloses_bounded_text_and_persists_visual_embedding() {
    if std::env::var_os(CHILD_FLAG).is_none() {
        let checkpoint = tempfile::tempdir().expect("synthetic checkpoint directory");
        write_tiny_vlm_checkpoint(checkpoint.path());
        let output_dir = tempfile::tempdir().expect("child diagnostics directory");
        let stdout_path = output_dir.path().join("stdout");
        let stderr_path = output_dir.path().join("stderr");
        let mut child = Command::new(std::env::current_exe().expect("current test binary"))
            .args([TEST_NAME, "--exact", "--nocapture"])
            .env(CHILD_FLAG, "1")
            .env("KHIVE_MOODBOARD_MODEL_DIR", checkpoint.path())
            .env("KHIVE_MOODBOARD_MODEL_REVISION", "synthetic-disclosure-v1")
            .env("KHIVE_MOODBOARD_INFERENCE_CONCURRENCY", "1")
            .env_remove("KHIVE_MOODBOARD_CHECKPOINT_SHA256")
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .expect("spawn isolated fixture process");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().expect("read child status") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("moodboard fixture child exceeded its safety watchdog");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let stdout = std::fs::read_to_string(&stdout_path).unwrap();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap();
        assert!(status.success(), "child failed:\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "the exact child target must run once:\n{stdout}\n{stderr}"
        );
        return;
    }

    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.packs = vec!["kg".to_string(), "moodboard".to_string()];
    let runtime = KhiveRuntime::new(config).expect("memory runtime");
    let inputs = Arc::new(StdMutex::new(Vec::new()));
    runtime.register_embedder(RecordingTextProvider(Arc::clone(&inputs)));
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let blobs = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(FsBlobStore::new(blobs.path().to_path_buf(), 0).unwrap());
    runtime.install_blob_store(blob_store.clone()).unwrap();
    let pack = MoodboardPack::new(runtime.clone());
    let descriptor = pack
        .model_state()
        .describe()
        .await
        .expect("real descriptor");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(pack);
    let registry = builder.build().expect("real pack registry");

    let short = registry
        .dispatch(
            "moodboard.ingest",
            json!({
                "image_base64": BASE64.encode(png(25)),
                "media_type": "image/png",
                "name": "short asset",
                "caption": "short caption",
            }),
        )
        .await
        .expect("within-budget full verb succeeds");
    assert!(short.get("warnings").is_none());
    let short_id = assert_visual_embedding(&runtime, &token, &descriptor, &short).await;

    let name = "bounded-caption";
    let caption = "x".repeat(lattice_embed::MAX_TEXT_BYTES);
    assert!(name.len() <= 512);
    assert_eq!(caption.len(), 32 * 1024);
    assert!(name.len() + 1 + caption.len() > lattice_embed::MAX_TEXT_BYTES);
    let image = png(75);
    let expected_ref = ContentRef::from_digest_bytes(blake3::hash(&image).as_bytes());
    let response = registry
        .dispatch(
            "moodboard.ingest",
            json!({
                "image_base64": BASE64.encode(&image),
                "media_type": "image/png",
                "name": name,
                "caption": caption,
            }),
        )
        .await
        .expect("legal caption plus name must succeed despite bounded text input");
    assert_eq!(
        response["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "the ingest verb must disclose bounded text input"
    );
    let asset_id = assert_visual_embedding(&runtime, &token, &descriptor, &response).await;
    assert_ne!(asset_id, short_id);
    assert_eq!(response["content_ref"], expected_ref.to_string());
    assert_eq!(
        blob_store
            .get_bounded_verified(&expected_ref, u64::try_from(image.len()).unwrap())
            .await
            .unwrap(),
        image
    );
    let asset = runtime.get_entity(&token, asset_id).await.unwrap();
    assert_eq!(asset.name, name);
    assert_eq!(asset.description.as_deref(), Some(caption.as_str()));
    let recorded = inputs.lock().unwrap();
    assert_eq!(recorded.len(), 2, "each created asset embeds its text once");
    assert_eq!(recorded[0], "short asset short caption");
    assert_eq!(recorded[1].len(), lattice_embed::MAX_TEXT_BYTES);
    assert_eq!(
        recorded[1],
        format!("{name} {caption}")[..lattice_embed::MAX_TEXT_BYTES]
    );
}

fn tiny_vlm_checkpoint_shapes() -> Vec<(String, Vec<usize>)> {
    let hidden = 8usize;
    let mut shapes = vec![
        (
            "model.language_model.embed_tokens.weight".to_string(),
            vec![16, hidden],
        ),
        ("model.language_model.norm.weight".to_string(), vec![hidden]),
        (
            "model.language_model.layers.0.input_layernorm.weight".to_string(),
            vec![hidden],
        ),
        (
            "model.language_model.layers.0.post_attention_layernorm.weight".to_string(),
            vec![hidden],
        ),
        (
            "model.language_model.layers.0.mlp.gate_proj.weight".to_string(),
            vec![4, hidden],
        ),
        (
            "model.language_model.layers.0.mlp.up_proj.weight".to_string(),
            vec![4, hidden],
        ),
        (
            "model.language_model.layers.0.mlp.down_proj.weight".to_string(),
            vec![hidden, 4],
        ),
        (
            "model.language_model.layers.0.self_attn.q_proj.weight".to_string(),
            vec![16, hidden],
        ),
        (
            "model.language_model.layers.0.self_attn.k_proj.weight".to_string(),
            vec![hidden, hidden],
        ),
        (
            "model.language_model.layers.0.self_attn.v_proj.weight".to_string(),
            vec![hidden, hidden],
        ),
        (
            "model.language_model.layers.0.self_attn.o_proj.weight".to_string(),
            vec![hidden, hidden],
        ),
        (
            "model.language_model.layers.0.self_attn.q_norm.weight".to_string(),
            vec![hidden],
        ),
        (
            "model.language_model.layers.0.self_attn.k_norm.weight".to_string(),
            vec![hidden],
        ),
        (
            "model.visual.patch_embed.proj.weight".to_string(),
            vec![hidden, 3, 1, 16, 16],
        ),
        (
            "model.visual.patch_embed.proj.bias".to_string(),
            vec![hidden],
        ),
        (
            "model.visual.pos_embed.weight".to_string(),
            vec![16, hidden],
        ),
        (
            "model.visual.merger.linear_fc1.weight".to_string(),
            vec![32, 32],
        ),
        ("model.visual.merger.linear_fc1.bias".to_string(), vec![32]),
        (
            "model.visual.merger.linear_fc2.weight".to_string(),
            vec![hidden, 32],
        ),
        (
            "model.visual.merger.linear_fc2.bias".to_string(),
            vec![hidden],
        ),
        ("model.visual.merger.norm.weight".to_string(), vec![hidden]),
        ("model.visual.merger.norm.bias".to_string(), vec![hidden]),
    ];
    for (suffix, shape) in [
        ("attn.qkv.weight", vec![24, hidden]),
        ("attn.qkv.bias", vec![24]),
        ("attn.proj.weight", vec![hidden, hidden]),
        ("attn.proj.bias", vec![hidden]),
        ("mlp.linear_fc1.weight", vec![32, hidden]),
        ("mlp.linear_fc1.bias", vec![32]),
        ("mlp.linear_fc2.weight", vec![hidden, 32]),
        ("mlp.linear_fc2.bias", vec![hidden]),
        ("norm1.weight", vec![hidden]),
        ("norm1.bias", vec![hidden]),
        ("norm2.weight", vec![hidden]),
        ("norm2.bias", vec![hidden]),
    ] {
        shapes.push((format!("model.visual.blocks.0.{suffix}"), shape));
    }
    shapes
}

fn write_tiny_tokenizer_json(dir: &Path) {
    let tokenizer = r#"{
            "model": {
                "type": "BPE",
                "vocab": {
                    "a": 0, "b": 1, "c": 2, "d": 3,
                    "e": 4, "f": 5, "g": 6, "h": 7,
                    "i": 8, "j": 9, "k": 10, "l": 11,
                    "m": 12, "n": 13, "o": 14, "p": 15
                },
                "merges": []
            }
        }"#;
    std::fs::write(dir.join("tokenizer.json"), tokenizer).expect("write tokenizer.json");
}

fn write_tiny_vlm_checkpoint(dir: &Path) {
    let config = r#"{
            "text_config": {
                "hidden_size": 8,
                "num_hidden_layers": 1,
                "vocab_size": 16,
                "intermediate_size": 4,
                "rms_norm_eps": 0.000001,
                "num_attention_heads": 1,
                "num_key_value_heads": 1,
                "head_dim": 8,
                "rope_theta": 10000000.0,
                "partial_rotary_factor": 1.0,
                "rope_parameters": {
                    "rope_theta": 10000000.0,
                    "partial_rotary_factor": 1.0,
                    "mrope_section": [2, 1, 1],
                    "mrope_interleaved": true
                },
                "linear_num_key_heads": 2,
                "linear_num_value_heads": 2,
                "linear_key_head_dim": 32,
                "linear_value_head_dim": 32,
                "linear_conv_kernel_dim": 4,
                "tie_word_embeddings": true,
                "full_attention_interval": 1,
                "layer_types": ["full_attention"],
                "layer_mask": [true],
                "eos_token_id": 15,
                "max_position_embeddings": 512
            },
            "vision_config": {
                "depth": 1,
                "hidden_size": 8,
                "num_heads": 2,
                "patch_size": 16,
                "spatial_merge_size": 2,
                "out_hidden_size": 8,
                "temporal_patch_size": 1,
                "num_position_embeddings": 16,
                "in_channels": 3,
                "deepstack_visual_indexes": []
            },
            "image_token_id": 9,
            "vision_start_token_id": 10,
            "vision_end_token_id": 11,
            "tie_word_embeddings": true
        }"#;
    std::fs::write(dir.join("config.json"), config).expect("write config.json");
    write_tiny_tokenizer_json(dir);

    let shapes = tiny_vlm_checkpoint_shapes();
    let mut header = serde_json::Map::new();
    let mut data = Vec::new();
    for (index, (name, shape)) in shapes.iter().enumerate() {
        let start = data.len();
        let count: usize = shape.iter().product();
        for _ in 0..count {
            data.extend_from_slice(&((index + 1) as f32 / 100.0).to_le_bytes());
        }
        header.insert(
            name.clone(),
            json!({"dtype": "F32", "shape": shape, "data_offsets": [start, data.len()]}),
        );
    }
    let header = serde_json::to_vec(&header).unwrap();
    let mut bytes = Vec::with_capacity(8 + header.len() + data.len());
    bytes.extend_from_slice(&u64::try_from(header.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&data);
    std::fs::write(dir.join("model.safetensors"), bytes).expect("write checkpoint tensors");
}
