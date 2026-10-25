use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::StoreSet;

pub const MAX_RECORD_BYTES: usize = 1_048_576;
pub const MAX_CONFIG_ID_BYTES: usize = 8192;
pub const MAX_PHASE_INTERVAL_MS: u64 = 86_400_000;

#[derive(Debug, thiserror::Error)]
pub enum RecordRefusal {
    #[error("maintenance I/O: {0}")]
    Io(#[from] io::Error),
    #[error("maintenance record decoding: {0}")]
    Wire(#[from] serde_json::Error),
    #[error("maintenance record refused: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Closing,
    Quiescent,
    Migrating,
    Ready,
    FailedClosed,
}

/// Exact executable identity; this value never authorizes executing a wire path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageIdentity {
    Inode {
        device: u64,
        inode: u64,
    },
    Modified {
        before_epoch: bool,
        seconds: u64,
        nanoseconds: u32,
    },
}

impl ImageIdentity {
    fn validate(&self) -> Result<(), RecordRefusal> {
        match self {
            Self::Inode { inode: 0, .. } => Err(invalid("zero executable inode")),
            Self::Modified {
                before_epoch,
                seconds,
                nanoseconds,
            } if *nanoseconds >= 1_000_000_000
                || (*before_epoch && *seconds == 0 && *nanoseconds == 0) =>
            {
                Err(invalid("noncanonical executable timestamp"))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetIdentity {
    start_path: PathBuf,
    image: ImageIdentity,
    config_id: String,
}

impl TargetIdentity {
    pub fn new(
        start_path: PathBuf,
        image: ImageIdentity,
        config_id: String,
    ) -> Result<Self, RecordRefusal> {
        validate_file_path(&start_path)?;
        image.validate()?;
        if config_id.len() > MAX_CONFIG_ID_BYTES {
            return Err(invalid("config identity too long"));
        }
        Ok(Self {
            start_path,
            image,
            config_id,
        })
    }
    pub fn start_path(&self) -> &Path {
        &self.start_path
    }
    pub fn image(&self) -> &ImageIdentity {
        &self.image
    }
    pub fn config_id(&self) -> &str {
        &self.config_id
    }
}

/// Static cooperative attribution, not cryptographic authentication or liveness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorIdentity {
    pub(super) uid: u32,
    pub(super) pid: u32,
    pub(super) boot_id: String,
    pub(super) start_ticks: u64,
    pub(super) image: ImageIdentity,
}

impl CoordinatorIdentity {
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }
    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
    pub fn image(&self) -> &ImageIdentity {
        &self.image
    }
    fn validate(&self, owner: u32) -> Result<(), RecordRefusal> {
        if self.uid != owner || self.pid == 0 || self.start_ticks == 0 {
            return Err(invalid("invalid coordinator attribution"));
        }
        validate_uuid(&self.boot_id)?;
        self.image.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub(super) epoch: String,
    pub(super) sequence: u64,
    pub(super) phase: Phase,
    pub(super) stores: StoreSet,
    pub(super) target: TargetIdentity,
    pub(super) coordinator: CoordinatorIdentity,
    pub(super) capability_digest: [u8; 32],
    pub(super) phase_started_unix_ms: u64,
    pub(super) phase_deadline_unix_ms: u64,
    pub(super) failure_code: Option<String>,
}

impl Declaration {
    /// Parse strict wire data without establishing trust or granting admission.
    ///
    /// `expected_owner_uid` is caller-supplied comparison data, not a filesystem
    /// metadata result or proof of provenance. This performs no filesystem or
    /// process checks and proves neither publication history nor actual readiness.
    /// A parsed `Ready` phase is data, never authorization to release stores.
    pub fn parse_untrusted(bytes: &[u8], expected_owner_uid: u32) -> Result<Self, RecordRefusal> {
        decode(bytes, expected_owner_uid)
    }

    /// Encode validated wire data without publishing or granting admission.
    ///
    /// `expected_owner_uid` is caller-supplied comparison data. This performs no
    /// filesystem or process checks, proves no ownership or publisher attribution,
    /// and does not capture a writer identity or authorize releasing stores.
    pub fn encode_untrusted(&self, expected_owner_uid: u32) -> Result<Vec<u8>, RecordRefusal> {
        encode(self, expected_owner_uid)
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn stores(&self) -> &StoreSet {
        &self.stores
    }
    pub fn target(&self) -> &TargetIdentity {
        &self.target
    }
    pub fn coordinator(&self) -> &CoordinatorIdentity {
        &self.coordinator
    }
    pub fn capability_digest(&self) -> &[u8; 32] {
        &self.capability_digest
    }
    pub fn phase_started_unix_ms(&self) -> u64 {
        self.phase_started_unix_ms
    }
    pub fn phase_deadline_unix_ms(&self) -> u64 {
        self.phase_deadline_unix_ms
    }
    pub fn failure_code(&self) -> Option<&str> {
        self.failure_code.as_deref()
    }

    pub(super) fn validate(&self, owner: u32) -> Result<(), RecordRefusal> {
        validate_uuid(&self.epoch)?;
        if self.sequence == 0 {
            return Err(invalid("zero sequence"));
        }
        self.coordinator.validate(owner)?;
        // Revalidate even internally constructed values at every publication boundary.
        TargetIdentity::new(
            self.target.start_path.clone(),
            self.target.image.clone(),
            self.target.config_id.clone(),
        )?;
        let interval = self
            .phase_deadline_unix_ms
            .checked_sub(self.phase_started_unix_ms)
            .ok_or_else(|| invalid("deadline precedes phase"))?;
        if interval == 0 || interval > MAX_PHASE_INTERVAL_MS {
            return Err(invalid("invalid phase deadline"));
        }
        match (self.phase, self.failure_code.as_deref()) {
            (Phase::FailedClosed, Some(code))
                if !code.is_empty()
                    && code.len() <= 64
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
            {
                Ok(())
            }
            (Phase::FailedClosed, _) => Err(invalid("invalid failure code")),
            (_, None) => Ok(()),
            _ => Err(invalid("failure code outside failed_closed")),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStores {
    main: Vec<u8>,
    paths: Vec<Vec<u8>>,
    digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTarget {
    start_path: Vec<u8>,
    image: ImageIdentity,
    config_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeclaration {
    version: u32,
    epoch: String,
    sequence: u64,
    phase: Phase,
    stores: WireStores,
    target: WireTarget,
    coordinator: CoordinatorIdentity,
    capability_digest: [u8; 32],
    phase_started_unix_ms: u64,
    phase_deadline_unix_ms: u64,
    // Option alone accepts an absent JSON field. Require its presence explicitly.
    #[serde(deserialize_with = "required_failure_code")]
    failure_code: Option<String>,
}

fn required_failure_code<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

pub(super) fn decode(bytes: &[u8], owner: u32) -> Result<Declaration, RecordRefusal> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid("record too large"));
    }
    let w: WireDeclaration = serde_json::from_slice(bytes)?;
    if w.version != 1 {
        return Err(invalid("unsupported record version"));
    }
    let stores = StoreSet::from_canonical(
        path(w.stores.main),
        w.stores.paths.into_iter().map(path).collect(),
    )?;
    if stores.digest() != &w.stores.digest {
        return Err(invalid("store digest mismatch"));
    }
    let record = Declaration {
        epoch: w.epoch,
        sequence: w.sequence,
        phase: w.phase,
        stores,
        target: TargetIdentity::new(
            path(w.target.start_path),
            w.target.image,
            w.target.config_id,
        )?,
        coordinator: w.coordinator,
        capability_digest: w.capability_digest,
        phase_started_unix_ms: w.phase_started_unix_ms,
        phase_deadline_unix_ms: w.phase_deadline_unix_ms,
        failure_code: w.failure_code,
    };
    record.validate(owner)?;
    Ok(record)
}

pub(super) fn encode(record: &Declaration, owner: u32) -> Result<Vec<u8>, RecordRefusal> {
    record.validate(owner)?;
    let w = WireDeclaration {
        version: 1,
        epoch: record.epoch.clone(),
        sequence: record.sequence,
        phase: record.phase,
        stores: WireStores {
            main: bytes(record.stores.main()),
            paths: record.stores.paths().iter().map(|p| bytes(p)).collect(),
            digest: *record.stores.digest(),
        },
        target: WireTarget {
            start_path: bytes(record.target.start_path()),
            image: record.target.image.clone(),
            config_id: record.target.config_id.clone(),
        },
        coordinator: record.coordinator.clone(),
        capability_digest: record.capability_digest,
        phase_started_unix_ms: record.phase_started_unix_ms,
        phase_deadline_unix_ms: record.phase_deadline_unix_ms,
        failure_code: record.failure_code.clone(),
    };
    let result = serde_json::to_vec(&w)?;
    if result.len() > MAX_RECORD_BYTES {
        return Err(invalid("record too large"));
    }
    Ok(result)
}

pub(super) fn invalid(message: &'static str) -> RecordRefusal {
    RecordRefusal::Invalid(message)
}
pub(super) fn path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}
fn bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}
pub(super) fn validate_file_path(path: &Path) -> Result<(), RecordRefusal> {
    StoreSet::from_canonical(path.to_path_buf(), vec![path.to_path_buf()])?;
    Ok(())
}
fn validate_uuid(value: &str) -> Result<(), RecordRefusal> {
    let id = uuid::Uuid::parse_str(value).map_err(|_| invalid("noncanonical UUID"))?;
    if id.hyphenated().to_string() != value {
        return Err(invalid("noncanonical UUID"));
    }
    Ok(())
}
