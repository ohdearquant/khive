use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lattice_embed::vision::{PoolingStrategy, VisionEmbeddingModel};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

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
                version: "0.9.0",
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
    // Lattice may retain memory maps into the checkpoint. Keep the private,
    // attested snapshot alive until the model itself is dropped.
    _checkpoint: Arc<PreparedCheckpoint>,
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

#[derive(Clone, Copy)]
enum CachedRuntimeErrorKind {
    InvalidInput,
    Unconfigured,
    Internal,
}

#[derive(Clone)]
struct CachedRuntimeError {
    kind: CachedRuntimeErrorKind,
    message: String,
}

impl CachedRuntimeError {
    fn from_runtime(error: RuntimeError) -> Self {
        match error {
            RuntimeError::InvalidInput(message) => Self {
                kind: CachedRuntimeErrorKind::InvalidInput,
                message,
            },
            RuntimeError::Unconfigured(message) => Self {
                kind: CachedRuntimeErrorKind::Unconfigured,
                message,
            },
            RuntimeError::Internal(message) => Self {
                kind: CachedRuntimeErrorKind::Internal,
                message,
            },
            other => Self {
                kind: CachedRuntimeErrorKind::Internal,
                message: other.to_string(),
            },
        }
    }

    fn to_runtime(&self) -> RuntimeError {
        match self.kind {
            CachedRuntimeErrorKind::InvalidInput => {
                RuntimeError::InvalidInput(self.message.clone())
            }
            CachedRuntimeErrorKind::Unconfigured => {
                RuntimeError::Unconfigured(self.message.clone())
            }
            CachedRuntimeErrorKind::Internal => RuntimeError::Internal(self.message.clone()),
        }
    }
}

enum BlockingStageState<T> {
    Idle,
    Running,
    Ready(Result<Arc<T>, CachedRuntimeError>),
}

/// A cancellation-independent, single-flight blocking computation.
///
/// The caller only awaits a watch notification. The owned Tokio task and its
/// `spawn_blocking` child outlive any cancelled waiter and publish exactly one
/// terminal result for all current and future callers.
struct BlockingStage<T> {
    state: Mutex<BlockingStageState<T>>,
    changed: watch::Sender<u64>,
}

impl<T> Default for BlockingStage<T> {
    fn default() -> Self {
        let (changed, _receiver) = watch::channel(0);
        Self {
            state: Mutex::new(BlockingStageState::Idle),
            changed,
        }
    }
}

impl<T> BlockingStage<T>
where
    T: Send + Sync + 'static,
{
    async fn get_or_start<F>(
        self: &Arc<Self>,
        operation: &'static str,
        job: F,
    ) -> Result<Arc<T>, RuntimeError>
    where
        F: FnOnce() -> Result<T, RuntimeError> + Send + 'static,
    {
        let mut changed = self.changed.subscribe();
        let mut job = Some(job);
        loop {
            let should_start = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*state {
                    BlockingStageState::Ready(Ok(value)) => return Ok(Arc::clone(value)),
                    BlockingStageState::Ready(Err(error)) => return Err(error.to_runtime()),
                    BlockingStageState::Running => false,
                    BlockingStageState::Idle => {
                        *state = BlockingStageState::Running;
                        true
                    }
                }
            };

            if should_start {
                let stage = Arc::clone(self);
                let job = job.take().expect("idle stage owns one starter job");
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(job)
                        .await
                        .map_err(|error| {
                            RuntimeError::Internal(format!("joining {operation}: {error}"))
                        })
                        .and_then(|result| result)
                        .map(Arc::new)
                        .map_err(CachedRuntimeError::from_runtime);
                    {
                        let mut state = stage
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *state = BlockingStageState::Ready(result);
                    }
                    let next = (*stage.changed.borrow()).wrapping_add(1);
                    stage.changed.send_replace(next);
                });
            }

            changed.changed().await.map_err(|_| {
                RuntimeError::Internal(format!("{operation} result channel closed"))
            })?;
        }
    }
}

pub(crate) struct VisionModelState {
    prepared: Arc<BlockingStage<PreparedCheckpoint>>,
    loaded: Arc<BlockingStage<LoadedVisionModel>>,
    preprocessing_gate: Arc<Semaphore>,
    inference_gate: Result<Arc<Semaphore>, String>,
}

impl Default for VisionModelState {
    fn default() -> Self {
        let configured = std::env::var("KHIVE_MOODBOARD_INFERENCE_CONCURRENCY").ok();
        let inference_gate = parse_inference_concurrency(configured.as_deref())
            .map(|limit| Arc::new(Semaphore::new(limit)));
        Self {
            prepared: Arc::new(BlockingStage::default()),
            loaded: Arc::new(BlockingStage::default()),
            preprocessing_gate: Arc::new(Semaphore::new(1)),
            inference_gate,
        }
    }
}

impl VisionModelState {
    pub(crate) async fn describe(&self) -> Result<DescriptorIdentity, RuntimeError> {
        let prepared = self
            .prepared
            .get_or_start(
                "moodboard checkpoint preparation",
                prepare_checkpoint_from_environment,
            )
            .await?;
        Ok(prepared.descriptor.clone())
    }

    pub(crate) async fn get(&self) -> Result<Arc<LoadedVisionModel>, RuntimeError> {
        let prepared = self
            .prepared
            .get_or_start(
                "moodboard checkpoint preparation",
                prepare_checkpoint_from_environment,
            )
            .await?;
        let expected_descriptor = prepared.descriptor.clone();
        let load_checkpoint = Arc::clone(&prepared);
        let loaded = self
            .loaded
            .get_or_start("moodboard model loader", move || {
                load_prepared_checkpoint(load_checkpoint)
            })
            .await?;
        verify_descriptor_commit(&expected_descriptor, loaded.descriptor())?;
        Ok(loaded)
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
        let permit = self.acquire_inference_permit().await?;
        spawn_blocking_with_permit(permit, "moodboard inference worker", move || {
            model.embed_with_prompt(&image_png, &prompt)
        })
        .await
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

async fn spawn_blocking_with_permit<T, F>(
    permit: OwnedSemaphorePermit,
    operation: &'static str,
    job: F,
) -> Result<T, RuntimeError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RuntimeError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        job()
    })
    .await
    .map_err(|error| RuntimeError::Internal(format!("joining {operation}: {error}")))?
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

fn load_prepared_checkpoint(
    checkpoint: Arc<PreparedCheckpoint>,
) -> Result<LoadedVisionModel, RuntimeError> {
    let model = load_from_prepared_snapshot(&checkpoint, |model_dir| {
        let model = VisionEmbeddingModel::from_directory(model_dir).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "loading moodboard Qwen3.5 checkpoint from {}: {error}",
                model_dir.display()
            ))
        })?;
        if model.dimensions() != checkpoint.descriptor.dimensions {
            return Err(RuntimeError::Unconfigured(format!(
                "moodboard checkpoint loaded with {} dimensions but config identity declared {}",
                model.dimensions(),
                checkpoint.descriptor.dimensions
            )));
        }
        Ok(model)
    })?;
    Ok(LoadedVisionModel {
        model,
        descriptor: checkpoint.descriptor.clone(),
        _checkpoint: checkpoint,
    })
}

fn load_from_prepared_snapshot<T, F>(
    checkpoint: &PreparedCheckpoint,
    loader: F,
) -> Result<T, RuntimeError>
where
    F: FnOnce(&Path) -> Result<T, RuntimeError>,
{
    let loaded = loader(checkpoint.model_dir())?;
    let committed_descriptor = descriptor_for_checkpoint_directory(
        checkpoint.model_dir(),
        checkpoint.descriptor.model_revision.clone(),
        Some(&checkpoint.descriptor.checkpoint_sha256),
    )?;
    verify_descriptor_commit(&checkpoint.descriptor, &committed_descriptor)?;
    Ok(loaded)
}

fn verify_descriptor_commit(
    expected: &DescriptorIdentity,
    committed: &DescriptorIdentity,
) -> Result<(), RuntimeError> {
    if committed != expected {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard model identity changed between checkpoint preparation and model publication: expected {}, committed {}",
            expected.fingerprint, committed.fingerprint
        )));
    }
    Ok(())
}

struct PreparedCheckpoint {
    snapshot: tempfile::TempDir,
    descriptor: DescriptorIdentity,
}

impl PreparedCheckpoint {
    fn model_dir(&self) -> &Path {
        self.snapshot.path()
    }
}

impl Drop for PreparedCheckpoint {
    fn drop(&mut self) {
        // TempDir cannot remove read-only files on every platform. Restore
        // owner write permission only for this private tree immediately before
        // its normal recursive cleanup.
        let _ = set_snapshot_tree_readonly(self.snapshot.path(), false);
    }
}

fn prepare_checkpoint_from_environment() -> Result<PreparedCheckpoint, RuntimeError> {
    let model_dir = required_env_path("KHIVE_MOODBOARD_MODEL_DIR")?;
    let model_revision = required_env("KHIVE_MOODBOARD_MODEL_REVISION")?;
    let expected_checkpoint_sha256 = optional_checkpoint_attestation()?;
    prepare_checkpoint_from_inputs(
        model_dir,
        model_revision,
        expected_checkpoint_sha256.as_deref(),
    )
}

fn prepare_checkpoint_from_inputs(
    model_dir: PathBuf,
    model_revision: String,
    expected_checkpoint_sha256: Option<&str>,
) -> Result<PreparedCheckpoint, RuntimeError> {
    let root_metadata = std::fs::symlink_metadata(&model_dir).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "reading KHIVE_MOODBOARD_MODEL_DIR={} metadata: {error}",
            model_dir.display()
        ))
    })?;
    if metadata_is_checkpoint_link(&root_metadata) || !root_metadata.is_dir() {
        return Err(RuntimeError::Unconfigured(format!(
            "KHIVE_MOODBOARD_MODEL_DIR={} must be a non-symlink directory",
            model_dir.display()
        )));
    }

    let snapshot = tempfile::Builder::new()
        .prefix("khive-moodboard-checkpoint-")
        .tempdir()
        .map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "creating private moodboard checkpoint snapshot: {error}"
            ))
        })?;
    copy_checkpoint_tree(&model_dir, snapshot.path())?;
    let descriptor = descriptor_for_checkpoint_directory(
        snapshot.path(),
        model_revision,
        expected_checkpoint_sha256,
    )?;
    let prepared = PreparedCheckpoint {
        snapshot,
        descriptor,
    };
    set_snapshot_tree_readonly(prepared.model_dir(), true)?;
    Ok(prepared)
}

fn descriptor_for_checkpoint_directory(
    model_dir: &Path,
    model_revision: String,
    expected_checkpoint_sha256: Option<&str>,
) -> Result<DescriptorIdentity, RuntimeError> {
    let config = read_checkpoint_config(model_dir)?;
    validate_checkpoint_geometry(&config)?;
    let dimensions = checkpoint_dimensions(&config)?;
    let checkpoint_sha256 = canonical_checkpoint_sha256(model_dir)?;
    verify_checkpoint_attestation(expected_checkpoint_sha256, &checkpoint_sha256)?;
    DescriptorIdentity::build(model_revision, checkpoint_sha256, dimensions)
}

fn copy_checkpoint_tree(source_root: &Path, destination_root: &Path) -> Result<(), RuntimeError> {
    copy_checkpoint_tree_with(source_root, destination_root, |_| Ok(()))
}

fn copy_checkpoint_tree_with<F>(
    source_root: &Path,
    destination_root: &Path,
    mut before_open: F,
) -> Result<(), RuntimeError>
where
    F: FnMut(&Path) -> Result<(), RuntimeError>,
{
    let canonical_root = std::fs::canonicalize(source_root).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "canonicalizing moodboard checkpoint root {}: {error}",
            source_root.display()
        ))
    })?;
    let mut files = Vec::new();
    collect_checkpoint_files(source_root, source_root, &mut files)?;
    if files.len() > 100_000 {
        return Err(RuntimeError::Unconfigured(format!(
            "moodboard checkpoint contains {} files, exceeding the 100000-file snapshot limit",
            files.len()
        )));
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    for (relative, source_path) in files {
        let destination_path = destination_root.join(&relative);
        let parent = destination_path.parent().ok_or_else(|| {
            RuntimeError::Internal(format!(
                "deriving private checkpoint parent for {}",
                destination_path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "creating private checkpoint directory {}: {error}",
                parent.display()
            ))
        })?;

        before_open(&source_path)?;
        // The opened handle, rather than the earlier directory entry, is the
        // security boundary. Resolve that handle after open and reject it
        // unless the actual file object is still rooted in the canonical
        // model directory. A symlink/directory swap between collection and
        // open therefore fails closed; later renames cannot change the bytes
        // read through this handle.
        let mut source = open_checkpoint_source(&canonical_root, &source_path)?;
        let source_size = source
            .metadata()
            .map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "reading checkpoint source metadata {}: {error}",
                    source_path.display()
                ))
            })?
            .len();
        let mut destination = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination_path)
            .map_err(|error| {
                RuntimeError::Unconfigured(format!(
                    "creating private checkpoint file {}: {error}",
                    destination_path.display()
                ))
            })?;
        let copied = std::io::copy(&mut source, &mut destination).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "copying checkpoint source {} into private snapshot: {error}",
                source_path.display()
            ))
        })?;
        if copied != source_size {
            return Err(RuntimeError::Unconfigured(format!(
                "checkpoint source {} changed while snapshotting (metadata {source_size} bytes, copied {copied})",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn open_checkpoint_source(
    canonical_root: &Path,
    source_path: &Path,
) -> Result<std::fs::File, RuntimeError> {
    let source = std::fs::File::open(source_path).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "opening checkpoint source {} for private snapshot: {error}",
            source_path.display()
        ))
    })?;
    let metadata = source.metadata().map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "reading opened checkpoint source metadata {}: {error}",
            source_path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(RuntimeError::Unconfigured(format!(
            "opened checkpoint source is not a regular file: {}",
            source_path.display()
        )));
    }
    let opened_path = opened_file_path(&source).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "resolving opened checkpoint source {}: {error}",
            source_path.display()
        ))
    })?;
    if !opened_path.starts_with(canonical_root) {
        return Err(RuntimeError::Unconfigured(format!(
            "opened checkpoint source escapes model directory: {} -> {}",
            source_path.display(),
            opened_path.display()
        )));
    }
    Ok(source)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn opened_file_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn opened_file_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let mut bytes = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `file` owns a live descriptor and `bytes` is a writable
    // PATH_MAX-sized buffer for F_GETPATH. fcntl writes a NUL-terminated path
    // on success and does not retain the pointer.
    let result = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            bytes.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let length = bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "F_GETPATH returned no NUL terminator",
        )
    })?;
    Ok(PathBuf::from(OsStr::from_bytes(&bytes[..length])))
}

#[cfg(windows)]
fn opened_file_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let mut path = vec![0_u16; 260];
    loop {
        // SAFETY: `file` owns a live handle and `path` exposes the writable
        // buffer and length passed to the Win32 API for this call only.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let length = length as usize;
        if length < path.len() {
            path.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&path)));
        }
        path.resize(length.saturating_add(1), 0);
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn opened_file_path(_file: &std::fs::File) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure opened-file path resolution is unsupported on this Unix target",
    ))
}

fn set_snapshot_tree_readonly(root: &Path, readonly: bool) -> Result<(), RuntimeError> {
    fn collect_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
        paths.push(directory.to_path_buf());
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                collect_paths(&path, paths)?;
            } else {
                paths.push(path);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    collect_paths(root, &mut paths).map_err(|error| {
        RuntimeError::Unconfigured(format!(
            "walking private checkpoint snapshot {}: {error}",
            root.display()
        ))
    })?;
    // Seal children before parents. During cleanup, restore parent traversal
    // and mutation permission before touching children.
    paths.sort_by_key(|path| path.components().count());
    if readonly {
        paths.reverse();
    }
    for path in paths {
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "reading private checkpoint permissions {}: {error}",
                path.display()
            ))
        })?;
        let mut permissions = metadata.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let owner_write = if readonly { 0 } else { 0o200 };
            permissions.set_mode((permissions.mode() & !0o222) | owner_write);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(readonly);
        std::fs::set_permissions(&path, permissions).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "{} private checkpoint path {}: {error}",
                if readonly { "sealing" } else { "unsealing" },
                path.display()
            ))
        })?;
    }
    Ok(())
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
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            RuntimeError::Unconfigured(format!(
                "reading moodboard checkpoint file type {}: {error}",
                path.display()
            ))
        })?;
        let file_type = metadata.file_type();
        let is_link = metadata_is_checkpoint_link(&metadata);
        if file_type.is_dir() && !is_link {
            collect_checkpoint_files(root, &path, files)?;
            continue;
        }
        if is_link {
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

fn metadata_is_checkpoint_link(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
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
        assert_eq!(value["inference"]["version"], "0.9.0");
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
                version: "0.9.0",
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
            "b57fb3cf43da387cde12425e6d7d442af269ba37ecabfbe4c975cb80abdf56e5"
        );
        assert_eq!(
            format!("moodboard_{fingerprint}_4"),
            "moodboard_b57fb3cf43da387cde12425e6d7d442af269ba37ecabfbe4c975cb80abdf56e5_4"
        );

        let production_prompt =
            DescriptorIdentity::build("weights-r1".to_string(), "1".repeat(64), 4).unwrap();
        assert_eq!(
            production_prompt.prompt.sha256,
            "a67ae9b539c243f498c75f1ea9f19e7018860948087728d6f8e65b34eef6a66e"
        );
        assert_eq!(
            production_prompt.fingerprint,
            "5d62815b1b662fa926c58aaaf58553e3d842b615cd90f431fe6e7c1bd782ea0b"
        );
        assert_eq!(
            production_prompt.model_key,
            "moodboard_5d62815b1b662fa926c58aaaf58553e3d842b615cd90f431fe6e7c1bd782ea0b_4"
        );

        let indexed_qwen_09 = DescriptorIdentity::build(
            "hf-Qwen-Qwen3.5-0.8B-2fc06364715b967f1860aea9cf38778875588b17".to_string(),
            "6dca0d0e661696b36985cbce8f89e1a91377822065de31eac94e90a0e45d43d3".to_string(),
            1024,
        )
        .unwrap();
        assert_eq!(
            indexed_qwen_09.fingerprint,
            "bd91f5bf961eb429a6f57b6c16bafde9eeea249d799b1ff0d31e32cf05e5bc8f"
        );
        assert_eq!(
            indexed_qwen_09.model_key,
            "moodboard_bd91f5bf961eb429a6f57b6c16bafde9eeea249d799b1ff0d31e32cf05e5bc8f_1024"
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
    fn canonical_checkpoint_digest_matches_independent_nested_golden() {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::create_dir(checkpoint.path().join("weights")).unwrap();
        std::fs::write(checkpoint.path().join("config.json"), b"config\n").unwrap();
        std::fs::write(checkpoint.path().join("tokenizer.json"), b"tokenizer").unwrap();
        std::fs::write(
            checkpoint.path().join("weights/model.bin"),
            b"\x00\x01weights\n",
        )
        .unwrap();

        assert_eq!(
            canonical_checkpoint_sha256(checkpoint.path()).unwrap(),
            "6dfb8c119af6525c58247ef3021e9143ded6de7019be0daed595acd477a41f8e"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_symlink_policy_accepts_internal_files_and_rejects_boundaries() {
        use std::os::unix::fs::symlink;

        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(checkpoint.path().join("weights.bin"), b"internal").unwrap();
        symlink("weights.bin", checkpoint.path().join("alias.bin")).unwrap();
        canonical_checkpoint_sha256(checkpoint.path()).expect("internal file symlink is accepted");
        let snapshot = tempfile::tempdir().unwrap();
        copy_checkpoint_tree(checkpoint.path(), snapshot.path())
            .expect("internal file symlink is materialized into the snapshot");
        assert_eq!(
            std::fs::read(snapshot.path().join("alias.bin")).unwrap(),
            b"internal"
        );

        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.bin"), b"outside").unwrap();
        symlink(
            outside.path().join("outside.bin"),
            checkpoint.path().join("escape.bin"),
        )
        .unwrap();
        let error = canonical_checkpoint_sha256(checkpoint.path()).unwrap_err();
        assert!(error.to_string().contains("escapes model directory"));
        std::fs::remove_file(checkpoint.path().join("escape.bin")).unwrap();

        std::fs::create_dir(checkpoint.path().join("nested")).unwrap();
        symlink("nested", checkpoint.path().join("nested-link")).unwrap();
        let error = canonical_checkpoint_sha256(checkpoint.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("directory symlinks are unsupported"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_copy_rejects_escape_swapped_in_after_collection() {
        use std::os::unix::fs::symlink;

        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(checkpoint.path().join("config.json"), b"config").unwrap();
        let victim = checkpoint.path().join("weights.bin");
        std::fs::write(&victim, b"trusted").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        let snapshot = tempfile::tempdir().unwrap();
        let mut swapped = false;

        let error = copy_checkpoint_tree_with(checkpoint.path(), snapshot.path(), |source_path| {
            if source_path == victim && !swapped {
                std::fs::remove_file(source_path).unwrap();
                symlink(&outside_file, source_path).unwrap();
                swapped = true;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(swapped, "test must swap the source after collection");
        assert!(error
            .to_string()
            .contains("opened checkpoint source escapes model directory"));
    }

    #[test]
    fn checkpoint_preparation_accepts_omitted_attestation() {
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

        let prepared = prepare_checkpoint_from_inputs(
            checkpoint.path().to_path_buf(),
            "fixture-r1".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(prepared.descriptor.dimensions, 4);
        assert_eq!(
            prepared.descriptor.checkpoint_sha256,
            canonical_checkpoint_sha256(checkpoint.path()).unwrap()
        );
        assert_ne!(prepared.model_dir(), checkpoint.path());
    }

    #[test]
    fn private_snapshot_binds_identity_across_atomic_source_replacement() {
        let parent = tempfile::tempdir().unwrap();
        let checkpoint = parent.path().join("checkpoint");
        std::fs::create_dir(&checkpoint).unwrap();
        std::fs::write(
            checkpoint.join("config.json"),
            br#"{
                "vision_config":{"patch_size":16,"spatial_merge_size":2},
                "text_config":{"hidden_size":4}
            }"#,
        )
        .unwrap();
        std::fs::write(checkpoint.join("model.safetensors"), b"trusted-v1").unwrap();
        let prepared =
            prepare_checkpoint_from_inputs(checkpoint.clone(), "fixture-r1".to_string(), None)
                .unwrap();

        let retired = parent.path().join("checkpoint-retired");
        std::fs::rename(&checkpoint, &retired).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        std::fs::write(
            checkpoint.join("config.json"),
            br#"{
                "vision_config":{"patch_size":16,"spatial_merge_size":2},
                "text_config":{"hidden_size":4}
            }"#,
        )
        .unwrap();
        std::fs::write(checkpoint.join("model.safetensors"), b"untrusted-v2").unwrap();

        let loaded_bytes = load_from_prepared_snapshot(&prepared, |model_dir| {
            std::fs::read(model_dir.join("model.safetensors"))
                .map_err(|error| RuntimeError::Internal(format!("fake snapshot loader: {error}")))
        })
        .unwrap();
        assert_eq!(loaded_bytes, b"trusted-v1");
        assert_eq!(
            canonical_checkpoint_sha256(prepared.model_dir()).unwrap(),
            prepared.descriptor.checkpoint_sha256
        );
        assert_ne!(
            canonical_checkpoint_sha256(&checkpoint).unwrap(),
            prepared.descriptor.checkpoint_sha256
        );
    }

    #[test]
    fn descriptor_commit_rejects_mismatched_publication() {
        let expected = DescriptorIdentity::fixture(4);
        let committed =
            DescriptorIdentity::build("other-revision".to_string(), "a".repeat(64), 4).unwrap();
        let error = verify_descriptor_commit(&expected, &committed).unwrap_err();
        assert!(error.to_string().contains("identity changed"));
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
    async fn blocking_stage_survives_first_waiter_cancellation_without_duplicate_work() {
        let stage = Arc::new(BlockingStage::<usize>::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let first_stage = Arc::clone(&stage);
        let first_calls = Arc::clone(&calls);
        let first_entered = Arc::clone(&entered);
        let first_release = Arc::clone(&release);
        let first = tokio::spawn(async move {
            first_stage
                .get_or_start("fake single-flight job", move || {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    first_entered.store(true, Ordering::SeqCst);
                    while !first_release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    Ok(7)
                })
                .await
        });
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        first.abort();
        let _ = first.await;

        release.store(true, Ordering::SeqCst);
        let second_calls = Arc::clone(&calls);
        let value = stage
            .get_or_start("fake single-flight job", move || {
                second_calls.fetch_add(1, Ordering::SeqCst);
                Ok(9)
            })
            .await
            .unwrap();
        assert_eq!(*value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn inference_gate_bounds_fake_peak_concurrency() {
        let state = Arc::new(VisionModelState {
            prepared: Arc::new(BlockingStage::default()),
            loaded: Arc::new(BlockingStage::default()),
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
    async fn cancelled_inference_waiter_does_not_release_native_work_permit() {
        let state = Arc::new(VisionModelState {
            prepared: Arc::new(BlockingStage::default()),
            loaded: Arc::new(BlockingStage::default()),
            preprocessing_gate: Arc::new(Semaphore::new(1)),
            inference_gate: Ok(Arc::new(Semaphore::new(1))),
        });
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = tokio::spawn(async move {
            let permit = worker_state.acquire_inference_permit().await.unwrap();
            spawn_blocking_with_permit(permit, "fake inference", move || {
                worker_entered.store(true, Ordering::SeqCst);
                while !worker_release.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                Ok(())
            })
            .await
        });
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state.inference_gate.as_ref().unwrap().available_permits(),
            0
        );
        worker.abort();
        let _ = worker.await;
        tokio::task::yield_now().await;
        assert_eq!(
            state.inference_gate.as_ref().unwrap().available_permits(),
            0,
            "caller cancellation must not release a permit still owned by native work"
        );

        release.store(true, Ordering::SeqCst);
        let permit = state.acquire_inference_permit().await.unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn preprocessing_gate_bounds_fake_peak_concurrency() {
        let state = Arc::new(VisionModelState {
            prepared: Arc::new(BlockingStage::default()),
            loaded: Arc::new(BlockingStage::default()),
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
    async fn real_checkpoint_preparation_is_model_load_free() {
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
