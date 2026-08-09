use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lattice_embed::vision::{PoolingStrategy, VisionEmbeddingModel};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

use khive_runtime::{NamedVectorIdentity, RuntimeError};

pub(crate) const MODEL_NAME: &str = "qwen3.5-vlm-pooled-visual";
pub(crate) const PROMPT: &str =
    "Represent the visual appearance of this graphic media asset for similarity retrieval.";
const PROMPT_REVISION: &str = "moodboard-style-retrieval-v1";
const SCHEMA_VERSION: &str = "moodboard.visual-descriptor.v1";
const PREPROCESSING_REVISION: &str = "moodboard-qwen35-srgb-pad32-max448-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InferenceIdentity {
    pub provider: &'static str,
    pub version: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreprocessingIdentity {
    pub revision: &'static str,
    pub max_side: u32,
    pub alignment: u32,
    pub matte_rgb: [u8; 3],
    pub resample: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptIdentity {
    pub revision: &'static str,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct DescriptorCore {
    schema_version: &'static str,
    model_name: &'static str,
    model_revision: String,
    checkpoint_sha256: String,
    inference: InferenceIdentity,
    preprocessing: PreprocessingIdentity,
    prompt: PromptIdentity,
    pooling: &'static str,
    dimensions: usize,
    normalization: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DescriptorIdentity {
    pub schema_version: &'static str,
    pub model_key: String,
    pub model_name: &'static str,
    pub model_revision: String,
    pub checkpoint_sha256: String,
    pub inference: InferenceIdentity,
    pub preprocessing: PreprocessingIdentity,
    pub prompt: PromptIdentity,
    pub pooling: &'static str,
    pub dimensions: usize,
    pub normalization: &'static str,
    pub fingerprint: String,
}

impl DescriptorIdentity {
    fn build(
        model_revision: String,
        checkpoint_sha256: String,
        dimensions: usize,
    ) -> Result<Self, RuntimeError> {
        if !(1..=8192).contains(&dimensions) {
            return Err(RuntimeError::Unconfigured(format!(
                "moodboard checkpoint dimensions must be in 1..=8192, got {dimensions}"
            )));
        }
        let core = DescriptorCore {
            schema_version: SCHEMA_VERSION,
            model_name: MODEL_NAME,
            model_revision,
            checkpoint_sha256,
            inference: InferenceIdentity {
                provider: "lattice-embed",
                version: "0.7.1",
            },
            preprocessing: PreprocessingIdentity {
                revision: PREPROCESSING_REVISION,
                max_side: 448,
                alignment: 32,
                matte_rgb: [128, 128, 128],
                resample: "lanczos3",
            },
            prompt: PromptIdentity {
                revision: PROMPT_REVISION,
                sha256: sha256_hex(PROMPT.as_bytes()),
            },
            pooling: "mean_visual_tokens",
            dimensions,
            normalization: "l2",
        };
        let canonical = canonical_json_bytes(&core)?;
        let fingerprint = sha256_hex(&canonical);
        let model_key = format!("moodboard_{fingerprint}_{dimensions}");
        Ok(Self {
            schema_version: core.schema_version,
            model_key,
            model_name: core.model_name,
            model_revision: core.model_revision,
            checkpoint_sha256: core.checkpoint_sha256,
            inference: core.inference,
            preprocessing: core.preprocessing,
            prompt: core.prompt,
            pooling: core.pooling,
            dimensions: core.dimensions,
            normalization: core.normalization,
            fingerprint,
        })
    }

    pub(crate) fn vector_identity(&self) -> Result<NamedVectorIdentity, RuntimeError> {
        NamedVectorIdentity::new(
            self.model_key.clone(),
            self.model_name.to_string(),
            self.dimensions,
        )
    }

    #[cfg(test)]
    pub(crate) fn fixture(dimensions: usize) -> Self {
        Self::build("fixture-revision".to_string(), "a".repeat(64), dimensions)
            .expect("fixture descriptor")
    }
}

pub(crate) struct LoadedVisionModel {
    model: VisionEmbeddingModel,
    descriptor: DescriptorIdentity,
}

impl LoadedVisionModel {
    pub(crate) fn descriptor(&self) -> &DescriptorIdentity {
        &self.descriptor
    }

    fn embed_with_prompt(&self, image_png: &[u8], prompt: &str) -> Result<Vec<f32>, RuntimeError> {
        self.model
            .embed_image(image_png, prompt, PoolingStrategy::MeanVisualTokens)
            .map_err(|error| {
                RuntimeError::Internal(format!("moodboard Lattice inference: {error}"))
            })
    }
}

pub(crate) struct VisionModelState {
    loaded: OnceCell<Arc<LoadedVisionModel>>,
    descriptor: OnceCell<DescriptorIdentity>,
    preprocessing_gate: Arc<Semaphore>,
    inference_gate: Result<Arc<Semaphore>, String>,
}

impl Default for VisionModelState {
    fn default() -> Self {
        let configured = std::env::var("KHIVE_MOODBOARD_INFERENCE_CONCURRENCY").ok();
        let inference_gate = parse_inference_concurrency(configured.as_deref())
            .map(|limit| Arc::new(Semaphore::new(limit)));
        Self {
            loaded: OnceCell::new(),
            descriptor: OnceCell::new(),
            preprocessing_gate: Arc::new(Semaphore::new(1)),
            inference_gate,
        }
    }
}

impl VisionModelState {
    pub(crate) async fn describe(&self) -> Result<DescriptorIdentity, RuntimeError> {
        if let Some(model) = self.loaded.get() {
            return Ok(model.descriptor().clone());
        }
        self.descriptor
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(discover_descriptor_from_environment)
                    .await
                    .map_err(|error| {
                        RuntimeError::Internal(format!(
                            "joining moodboard descriptor discovery: {error}"
                        ))
                    })?
            })
            .await
            .cloned()
    }

    pub(crate) async fn get(&self) -> Result<Arc<LoadedVisionModel>, RuntimeError> {
        let expected_descriptor = self.descriptor.get().cloned();
        let loaded = self
            .loaded
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(load_from_environment)
                    .await
                    .map_err(|error| {
                        RuntimeError::Internal(format!("joining moodboard model loader: {error}"))
                    })?
                    .and_then(|loaded| {
                        if let Some(expected) = &expected_descriptor {
                            if loaded.descriptor() != expected {
                                return Err(RuntimeError::Unconfigured(
                                    "moodboard model identity changed between descriptor discovery and checkpoint load"
                                        .to_string(),
                                ));
                            }
                        }
                        Ok(Arc::new(loaded))
                    })
            })
            .await?;
        let _ = self.descriptor.set(loaded.descriptor().clone());
        Ok(Arc::clone(loaded))
    }

    pub(crate) async fn infer(
        &self,
        model: Arc<LoadedVisionModel>,
        image_png: Vec<u8>,
    ) -> Result<Vec<f32>, RuntimeError> {
        self.infer_prompt(model, image_png, PROMPT.to_string())
            .await
    }

    pub(crate) async fn infer_prompt(
        &self,
        model: Arc<LoadedVisionModel>,
        image_png: Vec<u8>,
        prompt: String,
    ) -> Result<Vec<f32>, RuntimeError> {
        let _permit = self.acquire_inference_permit().await?;
        tokio::task::spawn_blocking(move || model.embed_with_prompt(&image_png, &prompt))
            .await
            .map_err(|error| {
                RuntimeError::Internal(format!("joining moodboard inference worker: {error}"))
            })?
    }

    async fn acquire_inference_permit(&self) -> Result<OwnedSemaphorePermit, RuntimeError> {
        let gate = self.inference_gate.as_ref().map_err(|message| {
            RuntimeError::Unconfigured(format!("KHIVE_MOODBOARD_INFERENCE_CONCURRENCY {message}"))
        })?;
        Arc::clone(gate)
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Internal("moodboard inference semaphore closed".to_string()))
    }

    pub(crate) async fn acquire_preprocessing_permit(
        &self,
    ) -> Result<OwnedSemaphorePermit, RuntimeError> {
        Arc::clone(&self.preprocessing_gate)
            .acquire_owned()
            .await
            .map_err(|_| {
                RuntimeError::Internal("moodboard preprocessing semaphore closed".to_string())
            })
    }
}

fn parse_inference_concurrency(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(1);
    };
    let parsed = value
        .parse::<usize>()
        .map_err(|_| "must be an integer in 1..=4".to_string())?;
    if !(1..=4).contains(&parsed) {
        return Err(format!("must be in 1..=4, got {parsed}"));
    }
    Ok(parsed)
}

pub(crate) fn validate_embedding(
    embedding: &[f32],
    descriptor: &DescriptorIdentity,
) -> Result<(), RuntimeError> {
    if embedding.len() != descriptor.dimensions {
        return Err(RuntimeError::Internal(format!(
            "moodboard Lattice embedding has {} dimensions, descriptor requires {}",
            embedding.len(),
            descriptor.dimensions
        )));
    }
    if let Some((index, value)) = embedding
        .iter()
        .copied()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(RuntimeError::Internal(format!(
            "moodboard Lattice embedding coordinate {index} is non-finite ({value})"
        )));
    }
    let norm = embedding
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || (norm - 1.0).abs() > 1e-3 {
        return Err(RuntimeError::Internal(format!(
            "moodboard Lattice embedding must be L2-normalized, got norm {norm}"
        )));
    }
    Ok(())
}

fn load_from_environment() -> Result<LoadedVisionModel, RuntimeError> {
    let discovered = discover_environment()?;
    let model = VisionEmbeddingModel::from_directory(&discovered.model_dir).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "loading moodboard Qwen3.5 checkpoint from {}: {error}",
            discovered.model_dir.display()
        ))
    })?;
    if model.dimensions() != discovered.descriptor.dimensions {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard checkpoint loaded with {} dimensions but config identity declared {}",
            model.dimensions(),
            discovered.descriptor.dimensions
        )));
    }
    verify_environment_unchanged_after_load(&discovered)?;
    Ok(LoadedVisionModel {
        model,
        descriptor: discovered.descriptor,
    })
}

fn verify_environment_unchanged_after_load(
    discovered: &DiscoveredEnvironment,
) -> Result<(), RuntimeError> {
    let config = read_checkpoint_config(&discovered.model_dir)?;
    validate_checkpoint_geometry(&config)?;
    let dimensions = checkpoint_dimensions(&config)?;
    if dimensions != discovered.descriptor.dimensions {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard checkpoint dimensions changed during load: discovered {}, post-load {dimensions}",
            discovered.descriptor.dimensions
        )));
    }
    let digest = canonical_checkpoint_sha256(&discovered.model_dir)?;
    if digest != discovered.descriptor.checkpoint_sha256 {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard checkpoint bytes changed during load: discovered {}, post-load {digest}",
            discovered.descriptor.checkpoint_sha256
        )));
    }
    Ok(())
}

struct DiscoveredEnvironment {
    model_dir: PathBuf,
    descriptor: DescriptorIdentity,
}

fn discover_descriptor_from_environment() -> Result<DescriptorIdentity, RuntimeError> {
    discover_environment().map(|discovered| discovered.descriptor)
}

fn discover_environment() -> Result<DiscoveredEnvironment, RuntimeError> {
    let model_dir = required_env_path("KHIVE_MOODBOARD_MODEL_DIR")?;
    let model_revision = required_env("KHIVE_MOODBOARD_MODEL_REVISION")?;
    let expected_checkpoint_sha256 = optional_checkpoint_attestation()?;
    discover_environment_from_inputs(
        model_dir,
        model_revision,
        expected_checkpoint_sha256.as_deref(),
    )
}

fn discover_environment_from_inputs(
    model_dir: PathBuf,
    model_revision: String,
    expected_checkpoint_sha256: Option<&str>,
) -> Result<DiscoveredEnvironment, RuntimeError> {
    if !model_dir.is_dir() {
        return Err(RuntimeError::Unconfigured(format!(
            "KHIVE_MOODBOARD_MODEL_DIR={} is not a directory",
            model_dir.display()
        )));
    }
    let config = read_checkpoint_config(&model_dir)?;
    validate_checkpoint_geometry(&config)?;
    let dimensions = checkpoint_dimensions(&config)?;
    let checkpoint_sha256 = canonical_checkpoint_sha256(&model_dir)?;
    verify_checkpoint_attestation(expected_checkpoint_sha256, &checkpoint_sha256)?;
    let descriptor = DescriptorIdentity::build(model_revision, checkpoint_sha256, dimensions)?;
    Ok(DiscoveredEnvironment {
        model_dir,
        descriptor,
    })
}

fn checkpoint_dimensions(config: &serde_json::Value) -> Result<usize, RuntimeError> {
    config
        .pointer("/text_config/hidden_size")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            RuntimeError::Unconfigured(
                "moodboard config.json requires integer text_config.hidden_size".to_string(),
            )
        })
}

fn read_checkpoint_config(model_dir: &Path) -> Result<serde_json::Value, RuntimeError> {
    let config_path = model_dir.join("config.json");
    let size = std::fs::metadata(&config_path)
        .map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "reading {} metadata: {error}",
                config_path.display()
            ))
        })?
        .len();
    if size > 8 * 1024 * 1024 {
        return Err(RuntimeError::Unconfigured(format!(
            "{} is {size} bytes, exceeding the 8 MiB configuration limit",
            config_path.display()
        )));
    }
    let bytes = std::fs::read(&config_path).map_err(|error| {
        RuntimeError::Unconfigured(format!("reading {}: {error}", config_path.display()))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeError::Unconfigured(format!("parsing {}: {error}", config_path.display()))
    })
}

fn validate_checkpoint_geometry(config: &serde_json::Value) -> Result<(), RuntimeError> {
    let patch_size = config
        .pointer("/vision_config/patch_size")
        .and_then(serde_json::Value::as_u64);
    let spatial_merge_size = config
        .pointer("/vision_config/spatial_merge_size")
        .and_then(serde_json::Value::as_u64);
    if (patch_size, spatial_merge_size) != (Some(16), Some(2)) {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard v1 requires Qwen3.5 vision patch_size=16 and spatial_merge_size=2, got patch_size={patch_size:?}, spatial_merge_size={spatial_merge_size:?}"
        )));
    }
    Ok(())
}

fn verify_checkpoint_attestation(expected: Option<&str>, actual: &str) -> Result<(), RuntimeError> {
    if let Some(expected) = expected.filter(|expected| *expected != actual) {
        return Err(RuntimeError::Unconfigured(format!(
            "KHIVE_MOODBOARD_CHECKPOINT_SHA256 does not match canonical checkpoint bytes: expected {expected}, computed {actual}"
        )));
    }
    Ok(())
}

fn canonical_checkpoint_sha256(model_dir: &Path) -> Result<String, RuntimeError> {
    let mut files = Vec::new();
    collect_checkpoint_files(model_dir, model_dir, &mut files)?;
    if files.len() > 100_000 {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard checkpoint contains {} files, exceeding the 100000-file digest limit",
            files.len()
        )));
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let mut digest = Sha256::new();
    digest.update(b"khive-moodboard-checkpoint-v1\0");
    digest.update((files.len() as u64).to_be_bytes());
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (relative, path) in files {
        let relative_bytes = relative.as_bytes();
        digest.update((relative_bytes.len() as u64).to_be_bytes());
        digest.update(relative_bytes);

        let mut file = std::fs::File::open(&path).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "opening moodboard checkpoint file {}: {error}",
                path.display()
            ))
        })?;
        let size = file
            .metadata()
            .map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "reading moodboard checkpoint metadata {}: {error}",
                    path.display()
                ))
            })?
            .len();
        digest.update(size.to_be_bytes());

        let mut read_size = 0_u64;
        loop {
            let count = file.read(&mut buffer).map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "hashing moodboard checkpoint file {}: {error}",
                    path.display()
                ))
            })?;
            if count == 0 {
                break;
            }
            read_size = read_size.saturating_add(count as u64);
            digest.update(&buffer[..count]);
        }
        if read_size != size {
            return Err(RuntimeError::Unconfigured(format!(
                "moodboard checkpoint file {} changed while hashing (metadata {size} bytes, read {read_size})",
                path.display()
            )));
        }
    }

    let bytes = digest.finalize();
    let mut out = String::with_capacity(64);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(out)
}

fn collect_checkpoint_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), RuntimeError> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "reading moodboard checkpoint directory {}: {error}",
            directory.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "reading entry in moodboard checkpoint directory {}: {error}",
                directory.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "reading moodboard checkpoint file type {}: {error}",
                path.display()
            ))
        })?;
        if file_type.is_dir() {
            collect_checkpoint_files(root, &path, files)?;
            continue;
        }
        if file_type.is_symlink() {
            let target = std::fs::metadata(&path).map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "resolving moodboard checkpoint symlink {}: {error}",
                    path.display()
                ))
            })?;
            if target.is_dir() {
                return Err(RuntimeError::Unconfigured(format!(
                    "moodboard checkpoint directory symlinks are unsupported: {}",
                    path.display()
                )));
            }
            if !target.is_file() {
                return Err(RuntimeError::Unconfigured(format!(
                    "moodboard checkpoint symlink does not resolve to a regular file: {}",
                    path.display()
                )));
            }
            let canonical_root = std::fs::canonicalize(root).map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "canonicalizing moodboard checkpoint root {}: {error}",
                    root.display()
                ))
            })?;
            let canonical_target = std::fs::canonicalize(&path).map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "canonicalizing moodboard checkpoint symlink {}: {error}",
                    path.display()
                ))
            })?;
            if !canonical_target.starts_with(&canonical_root) {
                return Err(RuntimeError::Unconfigured(format!(
                    "moodboard checkpoint file symlink escapes model directory: {} -> {}",
                    path.display(),
                    canonical_target.display()
                )));
            }
        } else if !file_type.is_file() {
            return Err(RuntimeError::Unconfigured(format!(
                "moodboard checkpoint contains a non-file entry: {}",
                path.display()
            )));
        }

        let relative = path.strip_prefix(root).map_err(|error| {
            RuntimeError::Internal(format!(
                "deriving moodboard checkpoint relative path {}: {error}",
                path.display()
            ))
        })?;
        let mut components = Vec::new();
        for component in relative.components() {
            let value = component.as_os_str().to_str().ok_or_else(|| {
                RuntimeError::Unconfigured(format!(
                    "moodboard checkpoint path is not valid UTF-8: {}",
                    path.display()
                ))
            })?;
            components.push(value);
        }
        let relative = components.join("/");
        if relative.is_empty() || relative.len() > 4096 {
            return Err(RuntimeError::Unconfigured(format!(
                "moodboard checkpoint relative path must be in 1..=4096 UTF-8 bytes: {}",
                path.display()
            )));
        }
        files.push((relative, path));
        if files.len() > 100_000 {
            return Err(RuntimeError::Unconfigured(
                "moodboard checkpoint exceeds the 100000-file digest limit".to_string(),
            ));
        }
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String, RuntimeError> {
    let value = std::env::var(name)
        .map_err(|_| RuntimeError::Unconfigured(format!("{name} must be set for moodboard")))?;
    if value.trim().is_empty() || value.trim() != value {
        return Err(RuntimeError::Unconfigured(format!(
            "{name} must be non-empty with no surrounding whitespace"
        )));
    }
    Ok(value)
}

fn required_env_path(name: &str) -> Result<PathBuf, RuntimeError> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| RuntimeError::Unconfigured(format!("{name} must be set for moodboard")))
}

fn optional_checkpoint_attestation() -> Result<Option<String>, RuntimeError> {
    let value = match std::env::var("KHIVE_MOODBOARD_CHECKPOINT_SHA256") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(RuntimeError::Unconfigured(
                "KHIVE_MOODBOARD_CHECKPOINT_SHA256 must be valid UTF-8".to_string(),
            ));
        }
    };
    if value.trim() != value {
        return Err(RuntimeError::Unconfigured(
            "KHIVE_MOODBOARD_CHECKPOINT_SHA256 must have no surrounding whitespace".to_string(),
        ));
    }
    validate_sha256("KHIVE_MOODBOARD_CHECKPOINT_SHA256", &value)?;
    Ok(Some(value))
}

fn validate_sha256(name: &str, value: &str) -> Result<(), RuntimeError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RuntimeError::Unconfigured(format!(
            "{name} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, RuntimeError> {
    let value = serde_json::to_value(value).map_err(|error| {
        RuntimeError::Internal(format!(
            "serializing moodboard descriptor identity: {error}"
        ))
    })?;
    let mut out = String::new();
    write_canonical_json(&value, &mut out)?;
    Ok(out.into_bytes())
}

fn write_canonical_json(value: &serde_json::Value, out: &mut String) -> Result<(), RuntimeError> {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => out.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            out.push_str(&serde_json::to_string(value).map_err(|error| {
                RuntimeError::Internal(format!(
                    "canonicalizing moodboard descriptor string: {error}"
                ))
            })?);
        }
        serde_json::Value::Array(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(']');
        }
        serde_json::Value::Object(values) => {
            out.push('{');
            let mut keys: Vec<&str> = values.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).map_err(|error| {
                    RuntimeError::Internal(format!(
                        "canonicalizing moodboard descriptor key: {error}"
                    ))
                })?);
                out.push(':');
                write_canonical_json(&values[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

    use super::*;

    #[test]
    fn descriptor_is_deterministic_and_closed() {
        let left = DescriptorIdentity::fixture(4);
        let right = DescriptorIdentity::fixture(4);
        assert_eq!(left, right);
        assert_eq!(left.fingerprint.len(), 64);
        assert_eq!(left.model_key, format!("moodboard_{}_4", left.fingerprint));

        let value = serde_json::to_value(left).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 12);
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["dimensions"], 4);
        assert_eq!(value["prompt"]["revision"], PROMPT_REVISION);
        assert_eq!(value["inference"]["version"], "0.7.1");
    }

    #[test]
    fn descriptor_fingerprint_matches_cross_language_golden() {
        let core = DescriptorCore {
            schema_version: SCHEMA_VERSION,
            model_name: MODEL_NAME,
            model_revision: "weights-r1".to_string(),
            checkpoint_sha256: "1".repeat(64),
            inference: InferenceIdentity {
                provider: "lattice-embed",
                version: "0.7.1",
            },
            preprocessing: PreprocessingIdentity {
                revision: PREPROCESSING_REVISION,
                max_side: 448,
                alignment: 32,
                matte_rgb: [128, 128, 128],
                resample: "lanczos3",
            },
            prompt: PromptIdentity {
                revision: PROMPT_REVISION,
                sha256: "2".repeat(64),
            },
            pooling: "mean_visual_tokens",
            dimensions: 4,
            normalization: "l2",
        };
        let fingerprint = sha256_hex(&canonical_json_bytes(&core).unwrap());
        assert_eq!(
            fingerprint,
            "88a9b26b399d878c77c3a4743dc38d2f538a951874b3c2fb6eb3d62d9cfbfd1c"
        );
        assert_eq!(
            format!("moodboard_{fingerprint}_4"),
            "moodboard_88a9b26b399d878c77c3a4743dc38d2f538a951874b3c2fb6eb3d62d9cfbfd1c_4"
        );

        let production_prompt =
            DescriptorIdentity::build("weights-r1".to_string(), "1".repeat(64), 4).unwrap();
        assert_eq!(
            production_prompt.prompt.sha256,
            "a67ae9b539c243f498c75f1ea9f19e7018860948087728d6f8e65b34eef6a66e"
        );
        assert_eq!(
            production_prompt.fingerprint,
            "59f1ababe9229fe1a2e871a92172d7f84461d28729172bbba5f7c55c4ccd0a53"
        );
        assert_eq!(
            production_prompt.model_key,
            "moodboard_59f1ababe9229fe1a2e871a92172d7f84461d28729172bbba5f7c55c4ccd0a53_4"
        );
    }

    #[test]
    fn embedding_validation_is_fail_closed() {
        let descriptor = DescriptorIdentity::fixture(4);
        validate_embedding(&[0.5, 0.5, 0.5, 0.5], &descriptor).unwrap();
        assert!(validate_embedding(&[1.0, 0.0], &descriptor).is_err());
        assert!(validate_embedding(&[f32::NAN, 0.0, 0.0, 0.0], &descriptor).is_err());
        assert!(validate_embedding(&[1.0, 1.0, 1.0, 1.0], &descriptor).is_err());
    }

    #[test]
    fn pinned_checkpoint_geometry_is_required() {
        let valid = tempfile::tempdir().unwrap();
        std::fs::write(
            valid.path().join("config.json"),
            br#"{"vision_config":{"patch_size":16,"spatial_merge_size":2}}"#,
        )
        .unwrap();
        let valid_config = read_checkpoint_config(valid.path()).unwrap();
        validate_checkpoint_geometry(&valid_config).unwrap();

        let invalid = tempfile::tempdir().unwrap();
        std::fs::write(
            invalid.path().join("config.json"),
            br#"{"vision_config":{"patch_size":14,"spatial_merge_size":2}}"#,
        )
        .unwrap();
        let invalid_config = read_checkpoint_config(invalid.path()).unwrap();
        let error = validate_checkpoint_geometry(&invalid_config).unwrap_err();
        assert!(error.to_string().contains("patch_size=16"));
        assert_eq!(
            checkpoint_dimensions(&serde_json::json!({"text_config":{"hidden_size":1024}}))
                .unwrap(),
            1024
        );
    }

    #[test]
    fn canonical_checkpoint_digest_rejects_one_byte_mutation() {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(checkpoint.path().join("config.json"), b"config-v1").unwrap();
        std::fs::write(checkpoint.path().join("tokenizer.json"), b"tokenizer-v1").unwrap();
        std::fs::write(checkpoint.path().join("model.safetensors"), b"weights-v1").unwrap();

        let expected = canonical_checkpoint_sha256(checkpoint.path()).unwrap();
        verify_checkpoint_attestation(Some(&expected), &expected).unwrap();
        verify_checkpoint_attestation(None, &expected).unwrap();

        std::fs::write(checkpoint.path().join("model.safetensors"), b"weights-v2").unwrap();
        let actual = canonical_checkpoint_sha256(checkpoint.path()).unwrap();
        let error = verify_checkpoint_attestation(Some(&expected), &actual).unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match canonical checkpoint bytes"));
    }

    #[test]
    fn descriptor_discovery_accepts_omitted_checkpoint_attestation() {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(
            checkpoint.path().join("config.json"),
            br#"{
                "vision_config":{"patch_size":16,"spatial_merge_size":2},
                "text_config":{"hidden_size":4}
            }"#,
        )
        .unwrap();
        std::fs::write(checkpoint.path().join("model.safetensors"), b"fixture").unwrap();

        let discovered = discover_environment_from_inputs(
            checkpoint.path().to_path_buf(),
            "fixture-r1".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(discovered.descriptor.dimensions, 4);
        assert_eq!(
            discovered.descriptor.checkpoint_sha256,
            canonical_checkpoint_sha256(checkpoint.path()).unwrap()
        );
    }

    #[test]
    fn post_load_verification_rejects_mutation_after_discovery() {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(
            checkpoint.path().join("config.json"),
            br#"{
                "vision_config":{"patch_size":16,"spatial_merge_size":2},
                "text_config":{"hidden_size":4}
            }"#,
        )
        .unwrap();
        std::fs::write(checkpoint.path().join("model.safetensors"), b"fixture-v1").unwrap();
        let discovered = discover_environment_from_inputs(
            checkpoint.path().to_path_buf(),
            "fixture-r1".to_string(),
            None,
        )
        .unwrap();

        std::fs::write(checkpoint.path().join("model.safetensors"), b"fixture-v2").unwrap();
        let error = verify_environment_unchanged_after_load(&discovered).unwrap_err();
        assert!(error.to_string().contains("changed during load"));
    }

    #[test]
    fn inference_concurrency_is_small_and_fail_closed() {
        assert_eq!(parse_inference_concurrency(None).unwrap(), 1);
        assert_eq!(parse_inference_concurrency(Some("4")).unwrap(), 4);
        assert!(parse_inference_concurrency(Some("0")).is_err());
        assert!(parse_inference_concurrency(Some("5")).is_err());
        assert!(parse_inference_concurrency(Some("many")).is_err());
    }

    #[tokio::test]
    async fn inference_gate_bounds_fake_peak_concurrency() {
        let state = Arc::new(VisionModelState {
            loaded: OnceCell::new(),
            descriptor: OnceCell::new(),
            preprocessing_gate: Arc::new(Semaphore::new(1)),
            inference_gate: Ok(Arc::new(Semaphore::new(1))),
        });
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            workers.push(tokio::spawn(async move {
                let _permit = state.acquire_inference_permit().await.unwrap();
                let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                current.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn preprocessing_gate_bounds_fake_peak_concurrency() {
        let state = Arc::new(VisionModelState {
            loaded: OnceCell::new(),
            descriptor: OnceCell::new(),
            preprocessing_gate: Arc::new(Semaphore::new(1)),
            inference_gate: Ok(Arc::new(Semaphore::new(1))),
        });
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            workers.push(tokio::spawn(async move {
                let _permit = state.acquire_preprocessing_permit().await.unwrap();
                let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                current.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[ignore = "requires KHIVE_MOODBOARD_* and a real Qwen3.5 checkpoint"]
    async fn real_checkpoint_descriptor_discovery_is_load_free() {
        let started = std::time::Instant::now();
        let descriptor = VisionModelState::default().describe().await.unwrap();
        assert_eq!(descriptor.schema_version, SCHEMA_VERSION);
        assert!((1..=8192).contains(&descriptor.dimensions));
        assert_eq!(descriptor.normalization, "l2");
        eprintln!(
            "moodboard real-checkpoint descriptor: {} discovery_ms={}",
            serde_json::to_string(&descriptor).unwrap(),
            started.elapsed().as_millis(),
        );
    }

    #[tokio::test]
    #[ignore = "requires KHIVE_MOODBOARD_* and a real Qwen3.5 checkpoint"]
    async fn mean_visual_tokens_is_prompt_invariant_under_current_causal_layout() {
        let image = RgbImage::from_pixel(64, 32, Rgb([25, 100, 200]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();

        let state = VisionModelState::default();
        let load_started = std::time::Instant::now();
        let model = state.get().await.unwrap();
        let load_elapsed = load_started.elapsed();
        let descriptor = model.descriptor().clone();

        let first_started = std::time::Instant::now();
        let first = state
            .infer_prompt(
                Arc::clone(&model),
                encoded.get_ref().clone(),
                PROMPT.to_string(),
            )
            .await
            .unwrap();
        let first_elapsed = first_started.elapsed();
        let repeated_started = std::time::Instant::now();
        let repeated = state
            .infer_prompt(
                Arc::clone(&model),
                encoded.get_ref().clone(),
                PROMPT.to_string(),
            )
            .await
            .unwrap();
        let repeated_elapsed = repeated_started.elapsed();
        let alternate_started = std::time::Instant::now();
        let alternate = state
            .infer_prompt(
                model,
                encoded.into_inner(),
                "A deliberately unrelated trailing prompt.".to_string(),
            )
            .await
            .unwrap();
        let alternate_elapsed = alternate_started.elapsed();

        assert_eq!(first.len(), descriptor.dimensions);
        assert_eq!(first.len(), repeated.len());
        assert_eq!(first.len(), alternate.len());
        validate_embedding(&first, &descriptor).unwrap();
        let norm = first
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        let repeat_max_delta = first
            .iter()
            .zip(repeated.iter())
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f32, f32::max);
        let prompt_max_delta = first
            .iter()
            .zip(alternate.iter())
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            repeat_max_delta <= 1e-6,
            "repeated inference unexpectedly changed: {repeat_max_delta}"
        );
        assert!(
            prompt_max_delta <= 1e-6,
            "image-pad pooling unexpectedly changed with trailing prompt: {prompt_max_delta}"
        );
        eprintln!(
            "moodboard real-checkpoint characterization: descriptor={} load_ms={} \
             inference_ms=[{},{},{}] norm={norm:.9} repeat_max_delta={repeat_max_delta:.9} \
             prompt_max_delta={prompt_max_delta:.9}",
            serde_json::to_string(&descriptor).unwrap(),
            load_elapsed.as_millis(),
            first_elapsed.as_millis(),
            repeated_elapsed.as_millis(),
            alternate_elapsed.as_millis(),
        );
    }
}
