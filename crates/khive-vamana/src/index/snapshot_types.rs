#[cfg(all(feature = "mmap", windows))]
use std::fs::File;
#[cfg(feature = "mmap")]
use std::{fs, path::Path};

use khive_quant::GsEncodedVector;
#[cfg(feature = "mmap")]
use khive_quant::GsSq8Codec;
use serde::{Deserialize, Serialize};

#[cfg(all(doc, feature = "mmap"))]
use super::read_commit_fingerprint;
use crate::{
    error::{Result, VamanaError},
    graph::CodesView,
};

#[cfg(feature = "mmap")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PublicationFileIdentity {
    pub(super) volume: u64,
    pub(super) file_index: u64,
    pub(super) len: u64,
    modified: Option<std::time::SystemTime>,
}

#[cfg(feature = "mmap")]
pub(super) fn is_read_only_lock_create_error(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EROFS)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(all(feature = "mmap", unix))]
pub(super) fn publication_file_identity(
    _: &Path,
    metadata: &fs::Metadata,
) -> Result<PublicationFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(PublicationFileIdentity {
        volume: metadata.dev(),
        file_index: metadata.ino(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(all(feature = "mmap", windows))]
pub(super) fn publication_file_identity(
    path: &Path,
    _: &fs::Metadata,
) -> Result<PublicationFileIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = File::open(path)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a live handle and `info` points to writable storage
    // for the documented output structure. Only a successful call initializes it.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    let metadata = file.metadata()?;
    Ok(PublicationFileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(all(feature = "mmap", not(any(unix, windows))))]
pub(super) fn publication_file_identity(
    _: &Path,
    _: &fs::Metadata,
) -> Result<PublicationFileIdentity> {
    Err(VamanaError::invalid_format(
        "file identity unavailable for unlocked load on this platform".into(),
    ))
}

#[cfg(feature = "mmap")]
pub(super) fn publication_file_snapshot(
    path: &Path,
) -> Result<Vec<Option<PublicationFileIdentity>>> {
    [
        "metadata.bin",
        "graph.bin",
        "vectors.bin",
        "lifecycle.bin",
        "codes.bin",
        ".checkpoint.lock",
    ]
    .iter()
    .map(|name| {
        let file_path = path.join(name);
        match fs::metadata(&file_path) {
            Ok(metadata) => publication_file_identity(&file_path, &metadata).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    })
    .collect()
}

/// Format identifier string stored in every `VamanaSnapshot`.
pub const VAMANA_SNAPSHOT_FORMAT: &str = "khive-vamana-index";
/// Snapshot format version; a mismatch causes `from_snapshot` to return an error.
pub const VAMANA_SNAPSHOT_VERSION: u32 = 1;

/// Corpus identity check stored inside a `VamanaSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusFingerprint {
    pub vector_count: u64,
    pub dimensions: u32,
}

/// Persisted commit fingerprint readable without loading the graph or vectors.
///
/// Returned by [`read_commit_fingerprint`] for warm-path classification by
/// callers that need to decide Hot/Stale/Cold without triggering a full graph
/// restore or rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedFingerprint {
    pub vector_count: u64,
    pub dimensions: u64,
    pub content_hash: [u8; 32],
}

/// Raw deserialization target for [`VamanaIndexSnapshot`].
#[derive(Deserialize)]
pub(super) struct VamanaIndexSnapshotRaw {
    pub(super) num_vectors: u64,
    pub(super) dimensions: u32,
    pub(super) max_degree: u32,
    pub(super) search_list_size: u32,
    pub(super) alpha: f64,
    pub(super) medoid: u32,
    pub(super) adjacency: Vec<Vec<u32>>,
    pub(super) vectors: Vec<f32>,
}

impl TryFrom<VamanaIndexSnapshotRaw> for VamanaIndexSnapshot {
    type Error = VamanaError;

    fn try_from(raw: VamanaIndexSnapshotRaw) -> std::result::Result<Self, VamanaError> {
        if raw.num_vectors == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: num_vectors must be > 0".into(),
            ));
        }
        if !raw.alpha.is_finite() || raw.alpha < 1.0 {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: alpha must be finite and >= 1.0, got {}",
                raw.alpha
            )));
        }
        if raw.dimensions == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: dimensions must be > 0".into(),
            ));
        }
        if raw.max_degree == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: max_degree must be > 0".into(),
            ));
        }
        if raw.search_list_size == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: search_list_size must be > 0".into(),
            ));
        }
        if raw.search_list_size < raw.max_degree {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: search_list_size ({}) must be >= max_degree ({})",
                raw.search_list_size, raw.max_degree
            )));
        }
        let num_vectors = usize::try_from(raw.num_vectors).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: num_vectors overflow".into())
        })?;
        let dimensions = usize::try_from(raw.dimensions).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: dimensions overflow".into())
        })?;
        let expected_floats = num_vectors.checked_mul(dimensions).ok_or_else(|| {
            VamanaError::invalid_format(
                "VamanaIndexSnapshot: num_vectors * dimensions overflow".into(),
            )
        })?;
        if raw.vectors.len() != expected_floats {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: vectors.len() ({}) != num_vectors * dimensions ({num_vectors} * {dimensions} = {expected_floats})",
                raw.vectors.len(),
            )));
        }
        if raw.adjacency.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: adjacency.len() ({}) != num_vectors ({num_vectors})",
                raw.adjacency.len(),
            )));
        }
        for (i, &v) in raw.vectors.iter().enumerate() {
            if !v.is_finite() {
                return Err(VamanaError::non_finite(
                    "VamanaIndexSnapshot.vectors",
                    format!("index {i}: {v}"),
                ));
            }
        }
        if raw.medoid as usize >= num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: medoid ({}) >= num_vectors ({num_vectors})",
                raw.medoid
            )));
        }
        let max_degree = usize::try_from(raw.max_degree).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: max_degree overflow".into())
        })?;
        for (node, neighbors) in raw.adjacency.iter().enumerate() {
            if neighbors.len() > max_degree {
                return Err(VamanaError::invalid_format(format!(
                    "VamanaIndexSnapshot: node {node} degree {} exceeds max_degree {max_degree}",
                    neighbors.len()
                )));
            }
            for &nb in neighbors {
                if nb as usize >= num_vectors {
                    return Err(VamanaError::invalid_format(format!(
                        "VamanaIndexSnapshot: neighbor {nb} >= num_vectors {num_vectors}"
                    )));
                }
                if nb as usize == node {
                    return Err(VamanaError::invalid_format(format!(
                        "VamanaIndexSnapshot: self-loop at node {node}"
                    )));
                }
            }
            let mut sorted = neighbors.clone();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            if sorted.len() != before {
                return Err(VamanaError::invalid_format(format!(
                    "VamanaIndexSnapshot: node {node} has duplicate neighbors"
                )));
            }
        }
        Ok(Self {
            num_vectors: raw.num_vectors,
            dimensions: raw.dimensions,
            max_degree: raw.max_degree,
            search_list_size: raw.search_list_size,
            alpha: raw.alpha,
            medoid: raw.medoid,
            adjacency: raw.adjacency,
            vectors: raw.vectors,
        })
    }
}

/// Serialisable graph payload stored inside `VamanaSnapshot`.
/// Deserialization validates that `alpha` is finite and >= 1.0, and that all
/// vector values are finite. Use `from_snapshot` to reconstruct a live index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "VamanaIndexSnapshotRaw")]
pub struct VamanaIndexSnapshot {
    /// Number of indexed vectors.
    pub num_vectors: u64,
    /// Vector dimensionality.
    pub dimensions: u32,
    /// Maximum out-degree used during build.
    pub max_degree: u32,
    /// Greedy-search candidate list size used during build.
    pub search_list_size: u32,
    /// Robust-prune alpha used during build.
    pub alpha: f64,
    /// Medoid node ID (start node for greedy search).
    pub medoid: u32,
    /// Adjacency lists; one `Vec<u32>` per node.
    pub adjacency: Vec<Vec<u32>>,
    /// Row-major flat vector data; `num_vectors × dimensions` `f32` values.
    pub vectors: Vec<f32>,
}

/// Raw deserialization target for [`VamanaSnapshot`].
#[derive(Deserialize)]
pub(super) struct VamanaSnapshotRaw {
    pub(super) format: String,
    pub(super) version: u32,
    pub(super) namespace: String,
    pub(super) model: String,
    pub(super) fingerprint: CorpusFingerprint,
    pub(super) index: VamanaIndexSnapshot,
    pub(super) external_ids: Vec<String>,
}

impl TryFrom<VamanaSnapshotRaw> for VamanaSnapshot {
    type Error = VamanaError;

    fn try_from(raw: VamanaSnapshotRaw) -> std::result::Result<Self, VamanaError> {
        let num_vectors = usize::try_from(raw.index.num_vectors).map_err(|_| {
            VamanaError::invalid_format("VamanaSnapshot: index.num_vectors overflow".into())
        })?;
        if raw.external_ids.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaSnapshot: external_ids.len() ({}) != num_vectors ({num_vectors})",
                raw.external_ids.len(),
            )));
        }
        Ok(Self {
            format: raw.format,
            version: raw.version,
            namespace: raw.namespace,
            model: raw.model,
            fingerprint: raw.fingerprint,
            index: raw.index,
            external_ids: raw.external_ids,
        })
    }
}

/// Self-validating snapshot of a `VamanaIndex`. Deserialization validates
/// vector finiteness and alpha range at the serde boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "VamanaSnapshotRaw")]
pub struct VamanaSnapshot {
    pub format: String,
    pub version: u32,
    pub namespace: String,
    pub model: String,
    pub fingerprint: CorpusFingerprint,
    pub index: VamanaIndexSnapshot,
    /// u32 node-id → external UUID string mapping preserved for `AnnBridge`.
    pub external_ids: Vec<String>,
}

#[derive(Clone)]
pub(super) enum VectorStorage {
    Owned(Vec<f32>),
    #[cfg(feature = "mmap")]
    Mmap {
        mmap: std::sync::Arc<memmap2::Mmap>,
        len_f32: usize,
    },
}

impl std::fmt::Debug for VectorStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(v) => write!(f, "Owned(len={})", v.len()),
            #[cfg(feature = "mmap")]
            Self::Mmap { len_f32, .. } => write!(f, "Mmap(len_f32={len_f32})"),
        }
    }
}

impl VectorStorage {
    pub(super) fn as_slice(&self) -> Result<&[f32]> {
        match self {
            Self::Owned(v) => Ok(v.as_slice()),
            #[cfg(feature = "mmap")]
            Self::Mmap { mmap, len_f32 } => {
                let floats: &[f32] = bytemuck::try_cast_slice(mmap.as_ref().as_ref())
                    .map_err(|_| VamanaError::invalid_format("vector mmap cast failed".into()))?;
                if floats.len() != *len_f32 {
                    return Err(VamanaError::invalid_format(format!(
                        "mmap f32 length {} != expected {}",
                        floats.len(),
                        len_f32
                    )));
                }
                Ok(floats)
            }
        }
    }
}

#[cfg(feature = "mmap")]
pub(super) const CODES_MAGIC: &[u8; 8] = b"KHVCODE1";
#[cfg(feature = "mmap")]
pub(super) const CODES_HEADER_LEN: usize = 8 + 8 + 8 + 4 + 4;

/// Storage for the per-node SQ8 code table: owned per-vector allocations
/// (build and mutation paths) or the flat, memory-mapped `codes.bin` segment
/// (v2 load path). Mirrors `VectorStorage`'s Owned/Mmap split.
#[derive(Clone)]
pub(super) enum CodeStore {
    Owned(Vec<GsEncodedVector>),
    #[cfg(feature = "mmap")]
    Mmap {
        mmap: std::sync::Arc<memmap2::Mmap>,
        dims: usize,
        len: usize,
    },
}

impl std::fmt::Debug for CodeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(v) => write!(f, "Owned(len={})", v.len()),
            #[cfg(feature = "mmap")]
            Self::Mmap { len, .. } => write!(f, "Mmap(len={len})"),
        }
    }
}

impl CodeStore {
    pub(super) fn view(&self) -> CodesView<'_> {
        match self {
            Self::Owned(v) => CodesView::Owned(v),
            #[cfg(feature = "mmap")]
            Self::Mmap { mmap, dims, len } => CodesView::Flat {
                bytes: &mmap.as_ref().as_ref()[CODES_HEADER_LEN + dims * 4..][..len * dims],
                dims: *dims,
            },
        }
    }

    pub(super) fn owned_mut(&mut self) -> Result<&mut Vec<GsEncodedVector>> {
        match self {
            Self::Owned(v) => Ok(v),
            #[cfg(feature = "mmap")]
            Self::Mmap { .. } => Err(VamanaError::invalid_format(
                "codes: unexpected Mmap after ensure_owned".into(),
            )),
        }
    }

    pub(super) fn ensure_owned(&mut self) {
        #[cfg(feature = "mmap")]
        if let Self::Mmap { len, .. } = self {
            let len = *len;
            let view = self.view();
            let owned: Vec<GsEncodedVector> = (0..len)
                .map(|i| GsEncodedVector {
                    codes: view.code(i).to_vec(),
                })
                .collect();
            *self = Self::Owned(owned);
        }
    }
}

/// Serialize the SQ8 codec parameters and per-node codes into the `codes.bin`
/// segment layout: magic, dims, count, gs, anisotropy_ratio, per-dimension
/// minima, then `count * dims` code bytes in ordinal order.
#[cfg(feature = "mmap")]
pub(super) fn encode_codes_bin(codec: &GsSq8Codec, codes: CodesView<'_>) -> Vec<u8> {
    let dims = codec.dims();
    let count = codes.len();
    let mut buf = Vec::with_capacity(CODES_HEADER_LEN + dims * 4 + count * dims);
    buf.extend_from_slice(CODES_MAGIC);
    buf.extend_from_slice(&(dims as u64).to_le_bytes());
    buf.extend_from_slice(&(count as u64).to_le_bytes());
    buf.extend_from_slice(&codec.gs.to_le_bytes());
    buf.extend_from_slice(&codec.anisotropy_ratio.to_le_bytes());
    for m in &codec.min {
        buf.extend_from_slice(&m.to_le_bytes());
    }
    for i in 0..count {
        buf.extend_from_slice(codes.code(i));
    }
    buf
}

/// Parse and validate a `codes.bin` header, returning the reconstructed codec.
/// The code bytes themselves stay in the caller's buffer/mapping at offset
/// `CODES_HEADER_LEN + dims * 4`.
#[cfg(feature = "mmap")]
pub(super) fn parse_codes_bin(
    data: &[u8],
    expected_dims: usize,
    expected_count: usize,
) -> Result<GsSq8Codec> {
    if data.len() < CODES_HEADER_LEN || &data[..8] != CODES_MAGIC {
        return Err(VamanaError::invalid_format(
            "codes.bin missing or bad magic".into(),
        ));
    }
    let dims = usize::try_from(u64::from_le_bytes(data[8..16].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("codes.bin dims overflow".into()))?;
    let count = usize::try_from(u64::from_le_bytes(data[16..24].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("codes.bin count overflow".into()))?;
    let gs = f32::from_le_bytes(data[24..28].try_into().unwrap());
    let anisotropy_ratio = f32::from_le_bytes(data[28..32].try_into().unwrap());
    if dims != expected_dims || count != expected_count {
        return Err(VamanaError::invalid_format(format!(
            "codes.bin shape {count}x{dims} != expected {expected_count}x{expected_dims}"
        )));
    }
    let min_header_bytes = dims.checked_mul(4).ok_or_else(|| {
        VamanaError::invalid_format("codes.bin dims byte length overflows".into())
    })?;
    let codes_bytes = count
        .checked_mul(dims)
        .ok_or_else(|| VamanaError::invalid_format("codes.bin count * dims overflows".into()))?;
    let expected_len = CODES_HEADER_LEN
        .checked_add(min_header_bytes)
        .and_then(|len| len.checked_add(codes_bytes))
        .ok_or_else(|| VamanaError::invalid_format("codes.bin expected length overflows".into()))?;
    if data.len() != expected_len {
        return Err(VamanaError::invalid_format(format!(
            "codes.bin length {} != expected {expected_len}",
            data.len()
        )));
    }
    if !gs.is_finite() || gs <= 0.0 {
        return Err(VamanaError::invalid_format(
            "codes.bin non-positive gs".into(),
        ));
    }
    let mut min = Vec::with_capacity(dims);
    for d in 0..dims {
        let off = CODES_HEADER_LEN + d * 4;
        let v = f32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if !v.is_finite() {
            return Err(VamanaError::invalid_format(
                "codes.bin non-finite min".into(),
            ));
        }
        min.push(v);
    }
    Ok(GsSq8Codec {
        min,
        gs,
        gs_sq: gs * gs,
        anisotropy_ratio,
    })
}
