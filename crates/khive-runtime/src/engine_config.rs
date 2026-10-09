//! TOML-based embedding engine configuration for khive.
//!
//! Loads `.khive/config.toml` (or `--config` / `KHIVE_CONFIG`) and exposes an
//! `[[engines]]` array for arbitrary-N embedding engine registration. Falls back
//! to `KHIVE_EMBEDDING_MODEL` env vars when no config file is present.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use khive_types::{namespace::Namespace, SubstrateKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{config::BackendId, presentation::OutputFormat};

#[path = "engine_config_backend_disk_guard.rs"]
mod backend_disk_guard;

#[path = "engine_config_peers.rs"]
mod peers;
pub use peers::EngineConfig;
pub(crate) use peers::{canonical_engine_name, validate_peer_engines};

// ---- Error type ----

/// Errors produced while loading or validating a `KhiveConfig`.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Credential(#[from] crate::credentials::CredentialError),

    #[error("mount configuration: {reason}")]
    InvalidMountConfig { reason: String },

    #[error("config file I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("config TOML parse error in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("exactly one engine must be marked `default = true`; found {found}")]
    DefaultCount { found: usize },

    #[error("duplicate engine name: {name:?}")]
    DuplicateName { name: String },

    #[error(
        "engine {name:?}: model {model:?} is not a recognized lattice_embed::EmbeddingModel name"
    )]
    UnknownModel { name: String, model: String },

    #[error("engine {name:?}: fusion_weight must be > 0, got {value}")]
    InvalidFusionWeight { name: String, value: f64 },

    #[error(
        "engine {name:?}: fusion_weight is not applied by current retrieval; \
         remove it until weighted multi-engine fusion is wired"
    )]
    UnsupportedFusionWeight { name: String },

    #[error("engine name must not be empty: {name:?}")]
    InvalidEngineName { name: String },

    #[error("engine {name:?} collides with another engine after alias canonicalization to {canonical:?}; remove the duplicate declaration")]
    AliasCollision { name: String, canonical: String },

    #[error("engine {name:?}: weight must be finite and strictly positive, got {value}")]
    InvalidEngineWeight { name: String, value: f64 },

    #[error("engine {name:?}: non-unit weight is refused until configured weights reach every activated retrieval path")]
    UnsupportedEngineWeight { name: String },

    #[error("engine {name:?}: dims must be positive and at most 4294967295, got {value}")]
    InvalidEngineDimensions { name: String, value: i64 },

    #[error(
        "engine {name:?}: configured dims {expected} do not match provider dimensions {actual}"
    )]
    EngineDimensionMismatch {
        name: String,
        expected: i64,
        actual: usize,
    },

    #[error("engine {name:?}: cannot mix legacy model/default/fusion_weight entries with canonical name/weight entries")]
    EngineKeyConflict { name: String },

    #[error("engines {name:?} and {other:?} share the storage key {key:?}; distinct engine identities require distinct bindings")]
    EngineKeyCollision {
        name: String,
        other: String,
        key: String,
    },

    #[error("actor.id {id:?} is not a valid namespace: {reason}")]
    InvalidActorId { id: String, reason: String },

    #[error("[actor].mailbox_readers: {reason}")]
    InvalidMailboxReaders { reason: String },

    #[error("[gate].granted_actors entry {id:?} is not a valid actor id: {reason}")]
    InvalidGrantedActorId { id: String, reason: String },
    #[error("[gate].deny_writes_for is invalid: {reason}")]
    InvalidWriteDenyPatterns { reason: String },

    #[error("duplicate backend name: {name:?}")]
    DuplicateBackendName { name: String },

    #[error("invalid backend name {name:?}: {reason}")]
    InvalidBackendName { name: String, reason: String },

    #[error("backend {name:?}: `served_kinds` must not be empty when declared")]
    EmptyBackendServedKinds { name: String },

    #[error("backend {name:?}: invalid disk guard configuration: {reason}")]
    InvalidBackendDiskGuard { name: String, reason: String },

    #[error(
        "backends {first_backend:?} and {second_backend:?} name the same database but resolve \
         different disk reserve/deadline policies"
    )]
    DiskGuardAliasConflict {
        first_backend: String,
        second_backend: String,
    },

    #[error("KHIVE_SQLITE_WAL_CEILING_BYTES must be an unsigned decimal byte count")]
    InvalidWalCeilingEnvironment { value: String },

    #[error(
        "backend {name:?}: wal_ceiling_bytes {value} exceeds supported SQLite offset arithmetic"
    )]
    WalCeilingOffsetOverflow { name: String, value: u64 },

    #[error(
        "backend {name:?}: nonzero wal_ceiling_bytes {value} requires a file-backed SQLite backend"
    )]
    WalCeilingMemoryBackend { name: String, value: u64 },

    #[error("backend {name:?}: nonzero wal_ceiling_bytes {value} requires SQLite WAL mode")]
    WalCeilingNonWalBackend { name: String, value: u64 },

    #[error(
        "backends {first_backend:?} and {second_backend:?} name the same database at {} \
         but resolve different WAL ceilings ({first_bytes} and {second_bytes} bytes)",
        crate::secret_gate::bounded_masked_log_text(&path.to_string_lossy())
    )]
    WalCeilingAliasConflict {
        first_backend: String,
        second_backend: String,
        path: PathBuf,
        first_bytes: u64,
        second_bytes: u64,
    },

    #[error(
        "backend configuration leaves searchable substrate kinds {kinds:?} unserved; \
         defined backends: {defined}"
    )]
    MissingBackendSearchKinds {
        kinds: Vec<SubstrateKind>,
        defined: String,
    },

    #[error(
        "[packs.{pack}].backend = {backend:?} references an unknown backend; \
         defined backends: {defined}"
    )]
    UnknownPackBackend {
        pack: String,
        backend: String,
        defined: String,
    },

    #[error(
        "[[backends]] entry {name:?}: field `{field}` is not yet supported; \
         remove it from the config or wait for a future release that implements it"
    )]
    UnsupportedBackendField { name: String, field: &'static str },

    #[error(
        "top-level `db = {value:?}` is not a supported config-file key; \
         use `--db` / `KHIVE_DB` to select a single-file database, or \
         `[[backends]].path` to declare storage backend topology"
    )]
    UnsupportedTopLevelDb { value: String },

    #[error("[[git_write.allowed]] entry {repo:?}: {reason}")]
    InvalidGitWriteEntry { repo: String, reason: String },

    #[error("[git_write] {key}: {reason}")]
    InvalidGitWriteConfig { key: String, reason: String },

    #[error("[exec] {key}: {reason}")]
    InvalidExecConfig { key: String, reason: String },

    #[error("{entry}: {reason}")]
    InvalidTelemetryConfig { entry: String, reason: String },

    #[error("[web] {key}: {reason}")]
    InvalidWebConfig { key: String, reason: String },

    #[error(
        "[runtime] blob_hydration_bytes must be between {min} and {max} bytes inclusive; got {value}"
    )]
    InvalidBlobHydrationBytes { value: u64, min: u64, max: u64 },

    #[error("the explicitly selected config file does not exist: {path}")]
    ExplicitConfigMissing { path: PathBuf },

    /// Retained for source compatibility with callers that matched the
    /// fail-loud behavior of older builds. Supported `[gate]` sections no
    /// longer produce this error.
    #[error("[gate] configuration is not supported by this build")]
    UnsupportedGateSection,

    #[error(
        "[display] timezone {timezone:?} is not a recognized IANA zone name (e.g. \"America/New_York\", \"UTC\")"
    )]
    InvalidDisplayTimezone { timezone: String },

    /// Loader-context wrapper attaching the config file the error came from.
    ///
    /// Added as a wrapping variant, rather than reshaping the existing
    /// variants, so every existing constructor, field access, and the
    /// `From<std::io::Error>` conversion survive unchanged. The enum is not
    /// `#[non_exhaustive]`, so an exhaustive `match` on `ConfigError` must
    /// still add an arm for this variant — either matching `InFile` and
    /// recursing into `source`, or a wildcard. `Parse` already carries its
    /// path and is never wrapped.
    #[error("{source} (config file: {})", path.display())]
    InFile {
        path: PathBuf,
        #[source]
        source: Box<ConfigError>,
    },
}

impl ConfigError {
    /// Attach the loading config file's path unless the error already names
    /// one (`Parse`, `ExplicitConfigMissing`) or is already wrapped.
    fn in_file(self, path: &Path) -> Self {
        match self {
            already @ (ConfigError::Parse { .. }
            | ConfigError::ExplicitConfigMissing { .. }
            | ConfigError::InFile { .. }) => already,
            other => ConfigError::InFile {
                path: path.to_path_buf(),
                source: Box::new(other),
            },
        }
    }
}

// ---- Config structs ----

/// Actor configuration — the default namespace / identity for this khive instance.
///
/// Corresponds to the `[actor]` TOML section. `id` is used as the
/// `default_namespace` for gate/attribution policy input. OSS dispatch pins
/// writes to the shared `local` namespace regardless of this value (ADR-007
/// Rev 4 Rule 0); cloud deployments derive the namespace from an authenticated
/// `NamespaceToken` instead.
///
/// ```toml
/// [actor]
/// id = "lambda:leo"                          # attribution identity (required)
/// display_name = "example actor"   # human label (optional)
/// visible_namespaces = ["lambda:khive", "local"]  # widens default read scope (ADR-007 Rev 4 Rule 3b)
/// ```
///
/// `visible_namespaces` is consumed by OSS dispatch to widen the DEFAULT
/// multi-record read scope to `['local'] ∪ visible_namespaces` (ADR-007 Rev 4
/// Rule 3b). Writes remain pinned to `'local'`. An explicit `namespace=` request
/// param is a precise single-namespace escape and is not widened. A cloud gate
/// may also consult this list as policy input at its own layer.
///
/// The table is closed. Both keys above are authorization input, so a misspelled
/// key fails startup instead of silently applying its default, and a `[gate]` key
/// written here is reported rather than discarded.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActorConfig {
    /// Namespace identifier used as the default actor for all operations.
    ///
    /// Must be a valid `Namespace` string (e.g. `"local"`, `"lambda:khive"`).
    /// Defaults to `"local"` when absent — backward-compatible with pre-actor
    /// deployments.
    #[serde(default)]
    pub id: Option<String>,

    /// Optional human-readable label for this actor. Not used by the runtime;
    /// surfaced in introspection and log output only.
    #[serde(default)]
    pub display_name: Option<String>,

    /// Exact actor labels permitted to inspect this explicit actor's mailbox.
    ///
    /// A nonempty list requires an explicit non-local `id`. Labels are bounded
    /// to 255 bytes, nonblank, and contain no control characters; `local` is
    /// forbidden. At most 256 entries are accepted before deduplication. This
    /// is trusted serving-host policy, never inferred from environment identity
    /// or supplied by a request. Changes take effect in a new server epoch.
    #[serde(default)]
    pub mailbox_readers: Vec<String>,

    /// Additional namespaces that widen the DEFAULT multi-record read scope
    /// to `['local'] ∪ visible_namespaces` (ADR-007 Rev 4 Rule 3b). Each string
    /// must be a valid `Namespace`. Writes remain pinned to `'local'`. An
    /// explicit `namespace=` request param is a precise escape and is not widened
    /// by this list. A cloud gate may also consult it as policy input.
    #[serde(default)]
    pub visible_namespaces: Option<Vec<String>>,

    /// Namespaces this actor's comm.send/reply may deliver messages INTO
    /// (outbound, sender-side). Empty by default — cross-namespace delivery
    /// denied unless explicitly declared. The comm handler uses an ordinary
    /// `NamespaceToken` (minted via `with_namespace`) in an append-only manner;
    /// the token itself is NOT type-enforced write-only. The recipient-side
    /// `allowed_inbound_namespaces` (bilateral mutual opt-in) is reserved for
    /// a future cloud-path authorization ADR (not yet written).
    ///
    /// Each entry must be a valid `Namespace` string; validated at
    /// config-load time. An empty list preserves the prior deny-all behavior
    /// for any actor that does not add this field.
    #[serde(default)]
    pub allowed_outbound_namespaces: Vec<String>,
}

/// Built-in caller-enrollment policy configured by `[gate]`.
///
/// The table is intentionally closed: misspelled or future keys fail startup
/// instead of being silently ignored at an authorization boundary. Presence
/// installs [`khive_gate::CallerEnrollmentGate`]; absence preserves the gate
/// already supplied in the base [`crate::RuntimeConfig`].
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GateSectionConfig {
    /// Exact resolved actor ids permitted to dispatch requests.
    #[serde(default)]
    pub granted_actors: Vec<String>,

    /// Whether the implicit anonymous/local caller is admitted.
    #[serde(default)]
    pub grant_unattributed: bool,

    /// Whole actor-ID patterns denying all but explicitly reviewed reads.
    /// Case-sensitive; only `*` is a wildcard. Does not enroll a caller.
    #[serde(default)]
    pub deny_writes_for: Vec<String>,
}

// ---- Per-pack backend config (ADR-028) ----

/// Storage backend kind.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// SQLite file-backed database (default).
    #[default]
    Sqlite,
    /// In-memory database — for testing only; state is lost on restart.
    Memory,
}

/// The configured WAL ceiling and the writer policy actually enforced by a backend.
///
/// A read-only SQLite backend retains its configured value for reporting but
/// has no writer policy, so `effective_bytes` is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedWalCeiling {
    /// Selected field, environment, or default value before read-only handling.
    pub configured_bytes: u64,
    /// Writer-enforced value; zero for a read-only backend.
    pub effective_bytes: u64,
    /// Origin of `configured_bytes`.
    pub source: khive_db::WalCeilingSource,
}

/// Resolve the ceiling with backend-field precedence over the environment.
///
/// `env_value` is a construction-time snapshot supplied by the host. This
/// function never reads process environment, so forwarding and backend opening
/// can validate the same policy even when the environment later changes.
pub fn resolve_wal_ceiling(
    backend_field: Option<u64>,
    env_value: Option<&str>,
    backend_name: &str,
    kind: BackendKind,
    wal_mode: bool,
    read_only: bool,
) -> Result<ResolvedWalCeiling, ConfigError> {
    if kind == BackendKind::Memory && backend_field.is_none() {
        return Ok(ResolvedWalCeiling {
            configured_bytes: 0,
            effective_bytes: 0,
            source: khive_db::WalCeilingSource::Default,
        });
    }
    let (configured_bytes, source) = if let Some(bytes) = backend_field {
        (bytes, khive_db::WalCeilingSource::BackendField)
    } else if let Some(raw) = env_value {
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ConfigError::InvalidWalCeilingEnvironment {
                value: raw.to_owned(),
            });
        }
        let bytes = raw
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidWalCeilingEnvironment {
                value: raw.to_owned(),
            })?;
        (bytes, khive_db::WalCeilingSource::Environment)
    } else {
        (0, khive_db::WalCeilingSource::Default)
    };

    if configured_bytes != 0 {
        if i64::try_from(configured_bytes).is_err() {
            return Err(ConfigError::WalCeilingOffsetOverflow {
                name: backend_name.to_owned(),
                value: configured_bytes,
            });
        }
        if kind == BackendKind::Memory {
            return Err(ConfigError::WalCeilingMemoryBackend {
                name: backend_name.to_owned(),
                value: configured_bytes,
            });
        }
        if !wal_mode {
            return Err(ConfigError::WalCeilingNonWalBackend {
                name: backend_name.to_owned(),
                value: configured_bytes,
            });
        }
    }

    Ok(ResolvedWalCeiling {
        configured_bytes,
        effective_bytes: if read_only { 0 } else { configured_bytes },
        source,
    })
}

/// Configuration for a named storage backend.
///
/// Corresponds to a `[[backends]]` entry in `khive.toml`.
/// When no `[[backends]]` section is present, a single implicit `main` backend
/// is synthesised from the existing `--db` / `KHIVE_DB` / default-path resolution.
/// All packs fall back to `main` when their name is absent from `[packs]`.
/// `cache_mb` and `journal_mode` are parsed but rejected during validation
/// because per-backend tuning is not implemented.
///
/// ```toml
/// [[backends]]
/// name = "main"
/// kind = "sqlite"
/// path = "~/.khive/khive.db"
/// read_only = false
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// Unique backend name. Referenced by `[packs.<name>].backend`.
    pub name: String,
    /// Storage backend kind. Defaults to `sqlite`.
    #[serde(default)]
    pub kind: BackendKind,
    /// Filesystem path for `sqlite` kind. Tilde is expanded to `$HOME`.
    /// `None` for `memory` kind (path is ignored when present).
    pub path: Option<std::path::PathBuf>,
    /// SQLite page-cache size in MiB. Parsed but rejected as unsupported.
    pub cache_mb: Option<u32>,
    /// SQLite journal mode (e.g. `"wal"`). Parsed but rejected as unsupported.
    pub journal_mode: Option<String>,
    /// Substrate kinds this backend serves.
    ///
    /// Omission preserves conservative fan-out to this backend. An explicit
    /// declaration is closed over [`SubstrateKind`] and must not be empty.
    /// The backend set must cover both `note` and `entity` search.
    #[serde(default)]
    pub served_kinds: Option<BTreeSet<SubstrateKind>>,
    /// Open the backend read-only. Defaults to `false`.
    #[serde(default)]
    pub read_only: bool,
    /// WAL extent ceiling in bytes. `None` inherits the environment setting;
    /// zero disables the ceiling. Read-only backends retain the configured
    /// value for reporting but enforce no writer policy.
    pub wal_ceiling_bytes: Option<u64>,
    /// SQLite disk reserve, in bytes. Zero explicitly disables the floor.
    #[serde(default)]
    pub disk_reserve_bytes: Option<u64>,
    /// Volume-guard acquisition deadline, in milliseconds (100..=10000).
    #[serde(default)]
    pub disk_guard_deadline_ms: Option<u64>,
}

/// Per-pack backend assignment.
///
/// Corresponds to a `[packs.<pack-name>]` entry in `khive.toml`.
/// Packs whose name is absent from `[packs]` fall back to the `main` backend.
///
/// ```toml
/// [packs.knowledge]
/// backend = "knowledge"
///
/// [packs.comm]
/// backend = "comm"
/// no_embed = true
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PackConfig {
    /// Backend name this pack is assigned to. Must match a `[[backends]].name`,
    /// or `main` when no backends are declared.
    #[serde(default = "default_pack_backend")]
    pub backend: String,
    /// Public handlers disabled by the operator. Names must belong to this loaded pack.
    #[serde(default)]
    pub verbs_disabled: Vec<String>,
    /// Disable vector embedding for this pack's runtime: rows it writes get
    /// FTS and metadata only, no `vec_*` rows and no ANN participation. The
    /// opt-out covers pack-owned writes on the pack's own backend; it does
    /// NOT cover `core()`-routed concept writes, which embed with the MAIN
    /// runtime's embedders (the boot path wires them in via
    /// `with_core_embedders_from`) so the shared graph stays uniformly
    /// searchable. Fits packs whose own rows are structural rather than
    /// retrieval targets (e.g. comm). Effective in multi-backend boot, where
    /// each pack gets its own runtime. Defaults to `false`.
    #[serde(default)]
    pub no_embed: bool,
}

fn default_pack_backend() -> String {
    BackendId::MAIN.to_owned()
}

// ---- Blob store config (ADR-111 Amendment 2) ----

/// `[storage.blob]` section: a closed `backend = "fs" | "s3"` selector.
///
/// Internally tagged on `backend` with `deny_unknown_fields`: an unknown
/// top-level key, a field that belongs to the other backend variant (e.g.
/// `bucket` under `backend = "fs"`), or an S3 credential field (never
/// accepted in TOML -- ADR-111 Amendment 2 reads credentials from the
/// process environment only) are all rejected at config-load time by the
/// same mechanism, since each variant only declares its own fields.
///
/// ```toml
/// [storage.blob]
/// backend = "fs"
/// root = "/var/lib/khive/blobs"
/// floor_bytes = 100000000000
/// ```
///
/// ```toml
/// [storage.blob]
/// backend = "s3"
/// bucket = "khive-blobs"
/// region = "us-east-1"
/// endpoint = "https://objects.example.invalid"
/// prefix = "blobs"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum BlobConfig {
    /// Filesystem-backed blob storage (`FsBlobStore`). Root resolution is
    /// unchanged from khive#292: `KHIVE_BLOB_ROOT` env var, then this
    /// `root`, then `<db_dir>/blobs`.
    Fs {
        #[serde(default)]
        root: Option<String>,
        #[serde(default)]
        floor_bytes: Option<u64>,
    },
    /// S3-compatible blob storage (`S3BlobStore`). `KHIVE_BLOB_ROOT` has no
    /// effect for this backend. Credentials always come from
    /// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN` in the
    /// process environment, never from this section.
    S3 {
        bucket: String,
        region: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        prefix: Option<String>,
        #[serde(default)]
        allow_http: Option<bool>,
    },
}

/// `[blob]` pack policy, separate from the `[storage.blob]` backend selector.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BlobSectionConfig {
    /// Permit server-local file transfers. Absent means disabled.
    #[serde(default)]
    pub file_transfers: bool,
}

/// `[storage]` section in `khive.toml`. Holds storage-layer config not
/// already covered by `[[backends]]` (ADR-028). Unknown fields are rejected
/// so an unsupported database selector cannot silently select the default store.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StorageSectionConfig {
    /// Blob store backend selector (ADR-111 Amendment 2). Absent means
    /// `FsBlobStore` at the existing root-resolution precedence, unchanged
    /// from khive#292 -- existing configurations keep behaving exactly as
    /// they did before this section existed.
    #[serde(default)]
    pub blob: Option<BlobConfig>,
}

/// `[brain]` read policy resolved by the serving process.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BrainSectionConfig {
    /// Actor ids permitted to request fleet-wide `brain.event_counts` reads.
    #[serde(default)]
    pub fleet_readers: Vec<String>,
}

// ---- git-write policy (ADR-108 Amendment) ----

/// One `[[git_write.allowed]]` entry: a repo this operator has declared
/// trusted for khive-mediated git writes, plus the branches on it a write
/// verb (`git.commit`/`git.branch`/`git.update_ref`/`git.push`) may target.
///
/// ```toml
/// [[git_write.allowed]]
/// repo = "/abs/path/repo"
/// branches = ["feat/*", "fix/*"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitWriteEntryConfig {
    /// Absolute local path to the allowlisted repository.
    pub repo: String,
    /// Non-empty list of exact branch names or single-`*`-wildcard globs
    /// this repo entry permits writes against.
    pub branches: Vec<String>,
}

/// `[git_write]` section — the closed repo/branch allowlist consulted by
/// `khive-pack-git`'s write verbs at the handler level (ADR-108 Amendment),
/// independent of Gate policy. Absent or empty `allowed` is the fail-closed
/// default: the write verbs report themselves unavailable rather than
/// defaulting open.
///
/// ```toml
/// [[git_write.allowed]]
/// repo = "/abs/path/repo"
/// branches = ["feat/*", "fix/*"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitWriteSectionConfig {
    /// Absolute git executable override; absent preserves PATH resolution.
    #[serde(default)]
    pub program: Option<PathBuf>,
    #[serde(default)]
    pub allowed: Vec<GitWriteEntryConfig>,
    #[serde(default)]
    pub actors: BTreeMap<String, GitWriteActorConfig>,
    #[serde(default)]
    pub repositories: BTreeMap<String, GitWriteRepositoryConfig>,
    #[serde(default = "default_git_credential_resolver")]
    pub credential_resolver: Vec<String>,
    #[serde(default)]
    pub contract_faults: bool,
    #[serde(default)]
    pub fault: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitWriteRepositoryConfig {
    /// HTTPS platform remote, or an absolute path / file:/// URL with an empty slug.
    pub remote: String,
    /// owner/name for HTTPS; empty explicitly opts into credential-free local pushes.
    pub slug: String,
    pub visibility: String,
    /// Merge dispatch refusals for this repository (ADR-182 Amendment 7):
    /// `opener` refuses a `git.pr_merge` dispatched by the account or actor
    /// that opened the pull request; `last_pusher` refuses one dispatched by
    /// the login on the newest push receipt for `expected_head`. Empty (the
    /// default) refuses neither.
    #[serde(default)]
    pub merge_refusals: Vec<String>,
}

impl GitWriteRepositoryConfig {
    pub const MERGE_REFUSALS: [&'static str; 2] = ["opener", "last_pusher"];

    /// Whether this repository row lists the named merge refusal.
    pub fn refuses_merge_by(&self, entry: &str) -> bool {
        self.merge_refusals.iter().any(|listed| listed == entry)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitWriteActorConfig {
    pub name: String,
    pub email: String,
    pub credential_ref: String,
    pub platform_identity: String,
}

fn default_git_credential_resolver() -> Vec<String> {
    [
        "/usr/bin/security",
        "find-generic-password",
        "-w",
        "-s",
        "{ref}",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

impl Default for GitWriteSectionConfig {
    fn default() -> Self {
        Self {
            program: None,
            allowed: Vec::new(),
            actors: BTreeMap::new(),
            repositories: BTreeMap::new(),
            credential_resolver: default_git_credential_resolver(),
            contract_faults: false,
            fault: None,
        }
    }
}

impl GitWriteSectionConfig {
    pub fn git_program(&self) -> &Path {
        self.program.as_deref().unwrap_or_else(|| Path::new("git"))
    }

    pub fn validate_dev_loop(&self) -> Result<(), ConfigError> {
        let invalid = |key: &str, reason: &str| ConfigError::InvalidGitWriteConfig {
            key: key.to_string(),
            reason: reason.to_string(),
        };
        if let Some(program) = &self.program {
            if !program.is_absolute() {
                return Err(invalid("git_write.program", "must be absolute"));
            }
            let metadata = std::fs::metadata(program).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    invalid("git_write.program", "does not exist")
                } else {
                    invalid("git_write.program", &format!("is not executable: {error}"))
                }
            })?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(windows)]
            let executable = program
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
                });
            #[cfg(not(any(unix, windows)))]
            let executable = false;
            if !metadata.is_file() || !executable {
                return Err(invalid("git_write.program", "is not executable"));
            }
        }
        if self.contract_faults && !cfg!(feature = "contract-faults") {
            tracing::error!(
                target: "khive.boot",
                "[git_write] contract_faults requires the test-only contract-faults build feature"
            );
            return Err(invalid(
                "contract_faults",
                "requires the test-only contract-faults build feature",
            ));
        }
        if let Some(fault) = &self.fault {
            if !self.contract_faults {
                return Err(invalid("fault", "requires contract_faults = true"));
            }
            let valid = fault.split_once(':').is_some_and(|(verb, point)| {
                matches!(verb, "git.push" | "git.pr_merge")
                    && matches!(
                        point,
                        "reply-lost-after-effect" | "audit-fails-after-effect"
                    )
            });
            if !valid {
                return Err(invalid("fault", "unsupported contract fault selector"));
            }
        }
        for (path, repository) in &self.repositories {
            let key = format!("repositories.{path}.merge_refusals");
            let mut seen: Vec<&str> = Vec::new();
            for entry in &repository.merge_refusals {
                if !GitWriteRepositoryConfig::MERGE_REFUSALS.contains(&entry.as_str()) {
                    return Err(invalid(&key, "entries must be opener or last_pusher"));
                }
                if seen.contains(&entry.as_str()) {
                    return Err(invalid(&key, "entries must not repeat"));
                }
                seen.push(entry);
            }
        }
        // The default keychain program is Unix-only. Legacy configurations with
        // no actor mappings cannot invoke it, so they remain loadable elsewhere.
        if !cfg!(unix)
            && self.actors.is_empty()
            && self.credential_resolver == default_git_credential_resolver()
        {
            return Ok(());
        }
        let argv = &self.credential_resolver;
        let Some(program) = argv.first() else {
            return Err(invalid("credential_resolver", "argv must not be empty"));
        };
        let program_path = Path::new(program);
        if !program_path.is_absolute() {
            return Err(invalid(
                "credential_resolver",
                "argv[0] must be an absolute path",
            ));
        }
        let program_name = program_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            program_name.trim_end_matches(".exe"),
            "sh" | "bash"
                | "dash"
                | "zsh"
                | "ksh"
                | "fish"
                | "csh"
                | "tcsh"
                | "cmd"
                | "powershell"
                | "pwsh"
                | "env"
        ) {
            return Err(invalid(
                "credential_resolver",
                "shell or env launcher is not allowed",
            ));
        }
        if argv.iter().any(|arg| arg.chars().any(char::is_control)) {
            return Err(invalid(
                "credential_resolver",
                "argv must not contain control characters",
            ));
        }
        if program.contains(['{', '}'])
            || argv[1..]
                .iter()
                .any(|arg| arg != "{ref}" && arg.contains(['{', '}']))
        {
            return Err(invalid(
                "credential_resolver",
                "{ref} must be a complete argument and is the only allowed template",
            ));
        }
        if !argv[1..].iter().any(|arg| arg == "{ref}") {
            return Err(invalid(
                "credential_resolver",
                "argv must contain a {ref} argument",
            ));
        }
        for (actor, identity) in &self.actors {
            if actor.trim().is_empty() || actor.chars().any(char::is_control) {
                return Err(invalid(
                    "actors",
                    "actor labels must be nonempty and contain no control characters",
                ));
            }
            for (field, value) in [
                ("name", &identity.name),
                ("email", &identity.email),
                ("credential_ref", &identity.credential_ref),
                ("platform_identity", &identity.platform_identity),
            ] {
                if value.trim().is_empty() || value.chars().any(char::is_control) {
                    return Err(invalid(
                        &format!("actors.{actor}.{field}"),
                        "must be nonempty and contain no control characters",
                    ));
                }
            }
            if identity.name.contains(['<', '>']) || identity.email.contains(['<', '>']) {
                return Err(invalid(
                    &format!("actors.{actor}"),
                    "name and email must not contain Git identity delimiters",
                ));
            }
        }
        Ok(())
    }
}

// ---- exec sandbox (ADR-181) ----

/// `[exec.limits]`: per-run resource limits applied to the sandboxed child
/// and inherited by its descendants (`setrlimit` before exec).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExecLimitsConfig {
    #[serde(default)]
    pub cpu_seconds: Option<u64>,
    #[serde(default)]
    pub address_space: Option<u64>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub nproc: Option<u64>,
}

/// `[exec]` section (ADR-181): where runs materialize, what they may read,
/// which caller environment keys pass through, which executable paths the
/// `never` list matches, and the output caps, wall-clock defaults and resource
/// limits. `never` matches paths, not a program's capabilities (ADR-181 A9).
///
/// ```toml
/// [exec]
/// root = "/var/lib/khive/exec"
/// read_roots = ["/opt/toolchains/python3.11"]
/// env = ["SOURCE_DATE_EPOCH"]
/// # The never list matches resolved executable paths, not renamed copies.
/// never = ["/usr/bin/curl"]
/// max_output_bytes = 1048576
/// timeout_default_s = 30
/// timeout_max_s = 600
/// binary_digest_timeout_s = 10 # 1..=60; independent of run timeout
/// keep = false
///
/// [exec.limits]
/// cpu_seconds = 60
/// file_size = 104857600
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecSectionConfig {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub read_roots: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub never: Vec<String>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
    #[serde(default)]
    pub timeout_default_s: Option<f64>,
    #[serde(default)]
    pub timeout_max_s: Option<f64>,
    /// Stalled-mount guard for hashing an authorized binary (1..=60 seconds).
    #[serde(default)]
    pub binary_digest_timeout_s: Option<u64>,
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub limits: ExecLimitsConfig,
}

pub const DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S: u64 = 10;
pub const MAX_EXEC_BINARY_DIGEST_TIMEOUT_S: u64 = 60;

// ---- web fetch/search policy (ADR-175 Amendment 1, carried into ADR-191 D3) ----

/// One `[[web.allowlist]]` entry: an exclusive host the operator has opted
/// into reachability for `web.fetch`/`web.search`. Presence of ANY entry
/// makes the allowlist exclusive (ADR-175 A1.2.3); absence leaves the public
/// internet reachable subject to the other egress rules. Matched by exact,
/// normalized (lowercase, trailing-dot-stripped) host equality only — no
/// suffix wildcarding, unlike `[[web.credentials]].hosts` (A1.2.6).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAllowlistEntry {
    pub host: String,
}

/// One `[[web.credentials]]` entry: a named secret, read from the process
/// environment at request time (never accepted as a verb argument), bound to
/// the set of hosts it may be presented to. Each `hosts` entry is either an
/// exact IP-literal address (matched exactly, never as a suffix) or a
/// hostname suffix (`example.com` matches `example.com` and any
/// `*.example.com` at a DNS label boundary) — ADR-175 A1.2.6.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebCredentialConfig {
    /// Name the caller passes as `web.fetch`'s `credential` argument.
    pub name: String,
    /// Process environment variable holding the secret value.
    pub env_var: String,
    /// Non-empty set of hosts (exact IP literals or hostname suffixes) this
    /// credential may be presented to.
    pub hosts: Vec<String>,
}

/// One canned result inside a `kind = "fixture"` `[[web.search_providers]]`
/// entry — deterministic, non-networked search results (demos, offline
/// corpora, and the fixture arm of `web.search`'s own test suite).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebFixtureResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// One `[[web.search_providers]]` entry (ADR-175 A1.3). The provider is
/// operator configuration; `web.search`'s `provider` argument only selects
/// among entries declared here by `name`. Closed, tagged on `kind`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum WebSearchProviderConfig {
    /// Deterministic canned results — no outbound request.
    Fixture {
        name: String,
        #[serde(default)]
        default: bool,
        results: Vec<WebFixtureResult>,
    },
    /// A real HTTP GET search backend. `url_template` must contain the
    /// literal substring `{query}`, replaced with the percent-encoded query
    /// at request time; `{limit}` is replaced with the effective limit when
    /// present. The response body is JSON: an array of `{title, url,
    /// snippet}` objects. `api_key_env`, when set, is a process environment
    /// variable sent as `Authorization: Bearer <value>`.
    Http {
        name: String,
        #[serde(default)]
        default: bool,
        url_template: String,
        #[serde(default)]
        api_key_env: Option<String>,
        /// Hosts (exact IP literals or hostname suffixes) `api_key_env`'s
        /// value may be presented to, modeled on
        /// `[[web.credentials]].hosts`. Required non-empty whenever
        /// `api_key_env` is set (`validate` enforces this) — an unscoped key
        /// would ride along to whatever host `url_template` resolves to,
        /// which defeats the point of scoping it at all.
        #[serde(default)]
        hosts: Vec<String>,
    },
}

impl WebSearchProviderConfig {
    pub fn name(&self) -> &str {
        match self {
            WebSearchProviderConfig::Fixture { name, .. } => name,
            WebSearchProviderConfig::Http { name, .. } => name,
        }
    }

    pub fn is_default(&self) -> bool {
        match self {
            WebSearchProviderConfig::Fixture { default, .. } => *default,
            WebSearchProviderConfig::Http { default, .. } => *default,
        }
    }
}

/// `[web]` section (ADR-175 Amendment 1, ADR-191 D3): operator policy for
/// `web.fetch` and `web.search` — ceilings, the address allowlist, credential
/// host-set bindings, and configured search providers.
///
/// No generic per-pack settings map exists in this file today (`PackConfig`
/// carries only `backend`/`no_embed`, both storage-routing concerns) so this
/// follows the established precedent for a pack needing rich operator policy:
/// a dedicated top-level section threaded through `RuntimeConfig`, the same
/// shape as `[exec]` and `[git_write]`.
///
/// ```toml
/// [web]
/// timeout_default_s = 30
/// timeout_max_s = 120
/// max_bytes_default = 5242880
/// max_bytes_max = 52428800
/// search_limit_default = 10
/// search_limit_max = 50
///
/// [[web.allowlist]]
/// host = "example.com"
///
/// [[web.credentials]]
/// name = "example-token"
/// env_var = "EXAMPLE_API_TOKEN"
/// hosts = ["example.com"]
///
/// [[web.search_providers]]
/// kind = "fixture"
/// name = "demo"
/// default = true
/// results = [{ title = "Example", url = "https://example.com", snippet = "..." }]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WebSectionConfig {
    #[serde(default)]
    pub timeout_default_s: Option<u64>,
    #[serde(default)]
    pub timeout_max_s: Option<u64>,
    #[serde(default)]
    pub max_bytes_default: Option<u64>,
    #[serde(default)]
    pub max_bytes_max: Option<u64>,
    #[serde(default)]
    pub search_limit_default: Option<u32>,
    #[serde(default)]
    pub search_limit_max: Option<u32>,
    #[serde(default)]
    pub allowlist: Vec<WebAllowlistEntry>,
    #[serde(default)]
    pub credentials: Vec<WebCredentialConfig>,
    #[serde(default)]
    pub search_providers: Vec<WebSearchProviderConfig>,
    /// Directories `web.ingest`'s disk mode may read from, modeled on
    /// `[exec] read_roots`. Absent or empty fails closed: disk ingest is
    /// refused entirely until the operator names at least one root.
    #[serde(default)]
    pub read_roots: Vec<String>,
}

/// Effective operator bounds shared by file validation and programmatic web dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebCeilings {
    pub timeout_default_s: u64,
    pub timeout_max_s: u64,
    pub max_bytes_default: u64,
    pub max_bytes_max: u64,
    pub search_limit_default: u32,
    pub search_limit_max: u32,
}

impl Default for WebCeilings {
    fn default() -> Self {
        Self {
            timeout_default_s: 30,
            timeout_max_s: 120,
            max_bytes_default: 5 * 1024 * 1024,
            max_bytes_max: 50 * 1024 * 1024,
            search_limit_default: 10,
            search_limit_max: 50,
        }
    }
}

impl WebSectionConfig {
    pub fn resolved_ceilings(&self) -> Result<WebCeilings, ConfigError> {
        let defaults = WebCeilings::default();
        let bounds = WebCeilings {
            timeout_default_s: self.timeout_default_s.unwrap_or(defaults.timeout_default_s),
            timeout_max_s: self.timeout_max_s.unwrap_or(defaults.timeout_max_s),
            max_bytes_default: self.max_bytes_default.unwrap_or(defaults.max_bytes_default),
            max_bytes_max: self.max_bytes_max.unwrap_or(defaults.max_bytes_max),
            search_limit_default: self
                .search_limit_default
                .unwrap_or(defaults.search_limit_default),
            search_limit_max: self.search_limit_max.unwrap_or(defaults.search_limit_max),
        };
        for (key, default, maximum, maximum_key) in [
            (
                "timeout_default_s",
                bounds.timeout_default_s,
                bounds.timeout_max_s,
                "timeout_max_s",
            ),
            (
                "max_bytes_default",
                bounds.max_bytes_default,
                bounds.max_bytes_max,
                "max_bytes_max",
            ),
            (
                "search_limit_default",
                u64::from(bounds.search_limit_default),
                u64::from(bounds.search_limit_max),
                "search_limit_max",
            ),
        ] {
            if default == 0 || default > maximum {
                return Err(ConfigError::InvalidWebConfig {
                    key: key.into(),
                    reason: format!(
                        "resolved default must be positive and not exceed {maximum_key}={maximum}"
                    ),
                });
            }
        }
        if std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(bounds.timeout_max_s))
            .is_none()
        {
            return Err(ConfigError::InvalidWebConfig {
                key: "timeout_max_s".into(),
                reason: "cannot be represented as a request deadline".into(),
            });
        }
        Ok(bounds)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.resolved_ceilings()?;
        let invalid = |key: &str, reason: &str| ConfigError::InvalidWebConfig {
            key: key.to_string(),
            reason: reason.to_string(),
        };
        let mut seen_hosts = std::collections::HashSet::new();
        for entry in &self.allowlist {
            let normalized = entry.host.trim().trim_end_matches('.').to_ascii_lowercase();
            if normalized.is_empty() {
                return Err(invalid("allowlist.host", "must not be empty"));
            }
            if !seen_hosts.insert(normalized) {
                return Err(invalid("allowlist.host", "duplicate host entry"));
            }
        }
        let mut seen_credentials = std::collections::HashSet::new();
        for credential in &self.credentials {
            if credential.name.trim().is_empty() {
                return Err(invalid("credentials.name", "must not be empty"));
            }
            if !seen_credentials.insert(credential.name.clone()) {
                return Err(invalid("credentials.name", "duplicate credential name"));
            }
            if credential.env_var.trim().is_empty() {
                return Err(invalid("credentials.env_var", "must not be empty"));
            }
            if credential.hosts.is_empty() {
                return Err(invalid(
                    "credentials.hosts",
                    "must name at least one host or suffix",
                ));
            }
        }
        let mut seen_providers = std::collections::HashSet::new();
        let mut default_count = 0;
        for provider in &self.search_providers {
            let name = provider.name();
            if name.trim().is_empty() {
                return Err(invalid("search_providers.name", "must not be empty"));
            }
            if !seen_providers.insert(name.to_string()) {
                return Err(invalid("search_providers.name", "duplicate provider name"));
            }
            if provider.is_default() {
                default_count += 1;
            }
            if let WebSearchProviderConfig::Http {
                url_template,
                api_key_env,
                hosts,
                ..
            } = provider
            {
                if !url_template.contains("{query}") {
                    return Err(invalid(
                        "search_providers.url_template",
                        "must contain the literal substring {query}",
                    ));
                }
                if api_key_env.is_some() && hosts.is_empty() {
                    return Err(invalid(
                        "search_providers.hosts",
                        "an api_key_env-bearing provider must name at least one host or suffix",
                    ));
                }
            }
        }
        if default_count > 1 {
            return Err(invalid(
                "search_providers",
                "at most one provider may set default = true",
            ));
        }
        Ok(())
    }
}

/// Top-level khive configuration loaded from `khive.toml` or `config.toml`.
///
/// Sections consumed today:
/// - `[[engines]]`: embedding engine declarations
/// - `[actor]`: default namespace / identity (OSS actor model)
/// - `[gate]`: built-in caller enrollment
/// - `[runtime]`: runtime knobs (pack selection, brain profile, output format)
/// - `[brain]`: actor read policy
/// - `[telemetry]`: stream and channel carrier policy
/// - `[[backends]]`: storage backend declarations (ADR-028)
/// - `[packs.<name>]`: per-pack backend assignments (ADR-028)
/// - `[display]`: rendering timezone (ADR-169)
/// - `[blob]`: server-local file-transfer opt-in
///
/// Unknown top-level keys are silently ignored by serde for forward
/// compatibility. The `[actor]`, `[gate]`, `[brain]`, `[blob]`, and `[telemetry]` tables are closed
/// with `deny_unknown_fields` so a misspelled policy key always fails startup.
/// `[storage]`, `[[backends]]` entries and `[exec]` are also closed so unknown
/// destination keys cannot be silently dropped, as are `[web]` and `[[mounts]]` entries.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KhiveConfig {
    /// Read by `credentials::read_tables` at load, never by this derive.
    #[serde(skip)]
    pub credentials: Vec<crate::credentials::CredentialConfig>,

    #[serde(skip)]
    pub visibility_receipts: Option<crate::credentials::VisibilityReceiptConfig>,

    #[serde(default)]
    pub mounts: Vec<crate::mount_config::MountConfig>,

    /// Typed only so a top-level `db` key can be rejected loudly by
    /// [`KhiveConfig::validate`] instead of being silently ignored as an
    /// unknown key. Not a supported config-file storage selector: single-file
    /// database selection is `--db`/`KHIVE_DB`, and storage topology is
    /// `[[backends]].path`.
    #[serde(default)]
    pub db: Option<String>,

    /// Embedding engine declarations.
    #[serde(default)]
    pub engines: Vec<EngineConfig>,

    /// Distinguishes a file's explicit `engines = []` from an omitted engine list.
    /// Use [`Self::load`] for file input; direct deserialization does not track
    /// presence or perform legacy engine conversion.
    #[serde(skip)]
    pub engines_declared: bool,

    /// Default actor identity for this khive instance.
    ///
    /// When present, `actor.id` feeds configuration identity and gate/attribution
    /// policy input.  A non-`'local'` `actor.id` is folded into the default READ
    /// visible-set at config load (ADR-007 Rev 4 Rule 3b) — it widens what default
    /// multi-record reads return, but never routes writes or sets `default_namespace`.
    /// Cloud model derives actor identity from an authenticated token.
    #[serde(default)]
    pub actor: ActorConfig,

    /// Optional caller-enrollment policy. A present, even empty, table is an
    /// explicit fail-closed policy; an absent table preserves the runtime's
    /// existing gate.
    #[serde(default)]
    pub gate: Option<GateSectionConfig>,

    /// Runtime knobs: namespace overrides, brain profile, etc.
    #[serde(default)]
    pub runtime: RuntimeSectionConfig,

    /// Named storage backends (ADR-028).
    ///
    /// When absent or empty, a single implicit `main` backend is used and all
    /// packs share it — identical to pre-ADR-028 behavior.
    #[serde(default)]
    pub backends: Vec<BackendConfig>,

    /// Per-pack backend assignments (ADR-028).
    ///
    /// Maps pack name to backend name. Packs absent from this map fall back to
    /// the `main` backend. Validated at load time: every referenced backend name
    /// must appear in `backends`.
    #[serde(default)]
    pub packs: std::collections::HashMap<String, PackConfig>,

    /// Actor read policy. An absent or empty list grants no fleet-wide reads.
    #[serde(default)]
    pub brain: BrainSectionConfig,

    /// Git-write policy allowlist (ADR-108 Amendment). Absent or empty
    /// `allowed` fails closed — `khive-pack-git`'s write verbs are
    /// unavailable until this section is populated.
    #[serde(default)]
    pub git_write: GitWriteSectionConfig,

    /// Server-local file-transfer opt-in. Other blob verbs are unaffected.
    #[serde(default)]
    pub blob: BlobSectionConfig,

    /// Storage-layer config not covered by `[[backends]]` (ADR-111
    /// Amendment 2: `[storage.blob]`'s `fs`/`s3` selector).
    #[serde(default)]
    pub storage: StorageSectionConfig,

    /// Exec sandbox section (ADR-181). Absent means no runs: the exec pack
    /// refuses every `exec.run` until `[exec] read_roots` names a toolchain.
    #[serde(default)]
    pub exec: ExecSectionConfig,

    /// Stream and channel carrier policy. Unclassified kinds default to ephemeral.
    #[serde(default)]
    pub telemetry: crate::telemetry_config::TelemetryConfig,

    /// Rendering timezone configuration (ADR-169). Absent `timezone` resolves
    /// to the host's local zone at [`RuntimeConfig`](crate::RuntimeConfig)
    /// construction time.
    #[serde(default)]
    pub display: DisplaySectionConfig,

    /// `web.fetch`/`web.search` operator policy (ADR-175 Amendment 1).
    /// Absent is the fail-closed default for search (no provider configured)
    /// and the permissive-subject-to-address-rules default for fetch (no
    /// allowlist configured).
    #[serde(default)]
    pub web: WebSectionConfig,
}

/// `[runtime]` section in `khive.toml`.
///
/// Carries runtime knobs resolved during process construction. Most mirror a
/// CLI flag / environment tier; field documentation calls out exceptions.
/// All fields are optional and preserve their already-resolved base value when
/// absent.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeSectionConfig {
    /// Packs to load when neither `--pack` nor `KHIVE_PACKS` selects them.
    /// An absent or empty list preserves the built-in production default.
    #[serde(default)]
    pub packs: Option<Vec<String>>,

    /// Brain profile ID to use for `memory.feedback` / `knowledge.feedback`
    /// and recall-time score boosting (ADR-035 §Brain profile configuration).
    ///
    /// Mirrors `--brain-profile` / `KHIVE_BRAIN_PROFILE`. When absent, the
    /// namespace-bound profile (via `brain.resolve`) is tried, then the
    /// global tuning prior is used as the final fallback.
    #[serde(default)]
    pub brain_profile: Option<String>,

    /// Default output serialization format (ADR-078).
    ///
    /// Mirrors `--output-format` / `KHIVE_OUTPUT_FORMAT`. Precedence (highest to lowest):
    /// per-request `format` field → `KHIVE_OUTPUT_FORMAT` → this field → builtin `json`.
    ///
    /// Accepted values: `"json"` (default), `"auto"`, `"table"`.
    #[serde(default)]
    pub default_output_format: Option<OutputFormat>,

    /// Aggregate process-local admission budget for digest-verified blob
    /// hydration (ADR-160 D3), in raw bytes.
    ///
    /// This knob has no environment-variable counterpart. When absent, the
    /// resolved runtime keeps its built-in 256 MiB default (or a value supplied
    /// directly through `RuntimeConfig`).
    #[serde(default)]
    pub blob_hydration_bytes: Option<u64>,
}

/// `[display]` section in `khive.toml` — the timezone khive anchors date-only
/// input to (ADR-169 Implementation step 1).
///
/// ```toml
/// [display]
/// timezone = "America/New_York"
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DisplaySectionConfig {
    /// IANA zone name (e.g. `"America/New_York"`, `"Asia/Tokyo"`, `"UTC"`).
    /// Validated at load time against `chrono_tz::Tz`'s zone table — an
    /// unrecognized name is a startup error, not a silent fallback. Absent →
    /// the host's local zone, resolved once via `iana-time-zone` and falling
    /// back to UTC when the host zone cannot be determined.
    #[serde(default)]
    pub timezone: Option<String>,
}

impl KhiveConfig {
    /// Load and validate a `KhiveConfig` from an explicit path.
    ///
    /// Search order:
    /// 1. `path` argument (explicit override — e.g. from `--config` / `KHIVE_CONFIG`)
    /// 2. `./.khive/config.toml` (project-local config, relative to the MCP server cwd)
    ///
    /// The project-local default collocates config with the `khive-test.db` that already
    /// lives under `.khive/` in each project directory. `~/.khive/config.toml` is searched
    /// by [`KhiveConfig::load_with_home_fallback`] when the project-local file is absent.
    ///
    /// If the resolved file does **not exist**, returns `Ok(None)`.
    /// A missing config is not an error — callers fall back to the env-var path.
    ///
    /// If the file exists but cannot be parsed, returns a `ConfigError`.
    /// After parsing, `validate()` runs and any logical errors are returned.
    pub fn load(path: Option<&Path>) -> Result<Option<Self>, ConfigError> {
        let resolved = match path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(".khive/config.toml"),
        };

        if !resolved.exists() {
            return Ok(None);
        }

        // Diagnostics name the canonical path so an error is actionable from
        // any cwd; resolution keeps using `resolved` as given.
        let diagnostic_path = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        let raw = std::fs::read_to_string(&resolved)
            .map_err(|source| ConfigError::from(source).in_file(&diagnostic_path))?;
        let mut document: toml::Value =
            toml::from_str(&raw).map_err(|source| ConfigError::Parse {
                path: diagnostic_path.clone(),
                source,
            })?;
        let engines_declared = peers::normalize_engine_input(&mut document, &diagnostic_path)
            .map_err(|error| error.in_file(&diagnostic_path))?;
        let mut cfg: KhiveConfig = document.try_into().map_err(|source| ConfigError::Parse {
            path: diagnostic_path.clone(),
            source,
        })?;
        cfg.engines_declared = engines_declared;
        crate::credentials::read_tables(&raw, &mut cfg)
            .map_err(|error| ConfigError::from(error).in_file(&diagnostic_path))?;
        cfg.validate()
            .map_err(|error| error.in_file(&diagnostic_path))?;
        for engine in &mut cfg.engines {
            engine.name = canonical_engine_name(&engine.name);
        }
        Ok(Some(cfg))
    }

    /// Load config with the full resolution order:
    ///
    /// 1. Explicit `path` (from `--config` / `KHIVE_CONFIG`)
    /// 2. `./khive.toml` (project-local, project root)
    /// 3. `<db-dir>/config.toml` (project-local, anchored to the resolved database's
    ///    own directory — see `project_config_anchor_dir`)
    /// 4. `~/.khive/config.toml` (user-global)
    ///
    /// Returns the first file found, or `Ok(None)` when none exist.
    /// Parse errors are propagated immediately — a malformed config is always
    /// an error regardless of which tier it came from.
    ///
    /// The explicit tier (1) is stricter than the discovery tiers: a `path`
    /// that names a file which does not exist returns
    /// [`ConfigError::ExplicitConfigMissing`] instead of falling through to
    /// tiers 2-4 — an operator-selected config that is missing is the same
    /// class of mistake as one that is malformed, and silently discovering a
    /// different file would boot against a config the operator did not select
    /// (ADR-035).
    ///
    /// `db_path` should be the same database path the caller is about to open
    /// (or has already resolved). Passing it makes tier 3 resolve identically
    /// for any two processes that target the same database, regardless of
    /// their process working directory — this is what lets a thin client and
    /// a warm daemon serving the same database agree on one config file. Pass
    /// `None` when no database path is known yet; tier 3 then falls back to
    /// the process cwd, matching the pre-existing behavior.
    pub fn load_with_home_fallback(
        path: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<Self>, ConfigError> {
        Ok(Self::load_with_home_fallback_and_source(path, db_path)?.map(|(config, _)| config))
    }

    /// Load config with the full resolution order and retain the exact file
    /// that supplied it.
    ///
    /// This is the diagnostic-preserving form of
    /// [`KhiveConfig::load_with_home_fallback`]. Runtime callers that need to
    /// tell an operator which selected file must be edited should use this
    /// method instead of reconstructing the discovery order independently.
    pub fn load_with_home_fallback_and_source(
        path: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<(Self, PathBuf)>, ConfigError> {
        // Tier 1: explicit path (highest priority). An explicit selection
        // naming a MISSING file fails loud here — once, at the loader, for
        // every entry point — instead of silently falling through to the
        // discovery tiers: a mistyped path would otherwise boot against a
        // config the operator did not select (ADR-035: an entry point must
        // not document an explicit tier while silently falling back to
        // discovery). Tiers 2-4 keep their tolerant contract.
        if let Some(p) = path {
            if !p.exists() {
                return Err(ConfigError::ExplicitConfigMissing {
                    path: p.to_path_buf(),
                });
            }
            return Ok(Self::load(Some(p))?.map(|config| (config, Self::diagnostic_config_path(p))));
        }

        // Tiers 2-4: search project root, db-anchored hidden dir, user-global.
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home_root = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_roots_and_source(&project_root, home_root.as_deref(), db_path)
    }

    /// Testable inner search: tiers 2-4, given explicit roots instead of
    /// reading `cwd` and `HOME` from process state.
    ///
    /// - Tier 2: `<project_root>/khive.toml` (still cwd-anchored — unchanged)
    /// - Tier 3: `<db_dir>/config.toml`, anchored to `db_path` rather than
    ///   `project_root` (see `project_config_anchor_dir`); falls back to
    ///   `<project_root>/.khive/config.toml` when `db_path` is `None`
    /// - Tier 4: `<home_root>/.khive/config.toml` (skipped when `None`)
    #[cfg(test)]
    pub(crate) fn load_with_roots(
        project_root: &Path,
        home_root: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<Self>, ConfigError> {
        Ok(
            Self::load_with_roots_and_source(project_root, home_root, db_path)?
                .map(|(config, _)| config),
        )
    }

    fn load_with_roots_and_source(
        project_root: &Path,
        home_root: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<(Self, PathBuf)>, ConfigError> {
        // Tier 2: project root khive.toml.
        let tier2 = project_root.join("khive.toml");
        if tier2.exists() {
            return Ok(Self::load(Some(&tier2))?
                .map(|config| (config, Self::diagnostic_config_path(&tier2))));
        }

        // Tier 3: project-local hidden dir, anchored to the resolved database's
        // own directory instead of the process cwd.
        let tier3 = Self::project_config_anchor_dir(db_path, project_root).join("config.toml");
        if tier3.exists() {
            return Ok(Self::load(Some(&tier3))?
                .map(|config| (config, Self::diagnostic_config_path(&tier3))));
        }

        // Tier 4: user-global ~/.khive/config.toml.
        if let Some(home) = home_root {
            let tier4 = home.join(".khive/config.toml");
            if tier4.exists() {
                return Ok(Self::load(Some(&tier4))?
                    .map(|config| (config, Self::diagnostic_config_path(&tier4))));
            }
        }

        Ok(None)
    }

    fn diagnostic_config_path(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Resolve the directory searched for the tier-3 project-local config file.
    ///
    /// Anchored to the directory containing the resolved database file, not the
    /// process cwd: two processes at different working directories that open the
    /// same database agree on this directory, which is what keeps their
    /// `config_id` fingerprints in sync (a client and a warm daemon serving the
    /// same database must resolve identical config so the daemon accepts the
    /// client's forwarded requests instead of rejecting them on a config
    /// mismatch).
    ///
    /// `db_path` is canonicalized first so symlinks/relative components collapse
    /// to the same absolute directory regardless of caller cwd. The database file
    /// may not exist yet (first run before anything has been written) — in that
    /// case canonicalization fails and the path is absolutized against
    /// `project_root` instead (or used as-is if already absolute); this must
    /// never panic, it is the expected cold-start case.
    ///
    /// If `db_dir` (the resolved database's parent directory) is itself named
    /// `.khive`, the config lives directly inside it (`<db_dir>/config.toml`) —
    /// this is the common case where the database is `<root>/.khive/khive.db`.
    /// Otherwise the config lives in a `.khive` subdirectory of `db_dir`.
    ///
    /// `db_path == None` (e.g. an in-memory database, or no database path known
    /// yet) falls back to `<project_root>/.khive`, preserving the pre-existing
    /// cwd-anchored behavior for callers with no database to anchor on.
    fn project_config_anchor_dir(db_path: Option<&Path>, project_root: &Path) -> PathBuf {
        let Some(db_path) = db_path else {
            return project_root.join(".khive");
        };

        let absolute = std::fs::canonicalize(db_path).unwrap_or_else(|_| {
            if db_path.is_absolute() {
                db_path.to_path_buf()
            } else {
                project_root.join(db_path)
            }
        });

        let db_dir = absolute.parent().map(Path::to_path_buf).unwrap_or(absolute);

        if db_dir.file_name().is_some_and(|name| name == ".khive") {
            db_dir
        } else {
            db_dir.join(".khive")
        }
    }

    /// Validate the parsed config for logical consistency.
    ///
    /// Checks:
    /// Engine names and storage keys are unique; dimensions and weights are
    /// positive. Non-unit weights remain refused until retrieval applies them.
    pub fn validate(&self) -> Result<(), ConfigError> {
        crate::mount_config::validate_mounts(&self.mounts)?;
        self.git_write.validate_dev_loop()?;
        self.telemetry.validate()?;
        self.web.validate()?;
        crate::credentials::CredentialConfig::validate_all(&self.credentials)?;
        if let Some(receipts) = &self.visibility_receipts {
            receipts.validate(&self.credentials)?;
        }

        // Reject a top-level `db` key loudly instead of letting serde's
        // forward-compatible unknown-key tolerance silently swallow it: a
        // config author expecting `db=` to select the database would
        // otherwise get silent divergence from `--db`/`KHIVE_DB`.
        if let Some(value) = self.db.as_deref() {
            if !value.is_empty() {
                return Err(ConfigError::UnsupportedTopLevelDb {
                    value: value.to_string(),
                });
            }
        }

        // ADR-181 resource limits: macOS returns EINVAL for RLIMIT_AS and
        // RLIMIT_DATA, and RLIMIT_NPROC counts every process of the uid, so
        // neither can bound one run. Refuse loudly instead of pretending.
        if cfg!(target_os = "macos") {
            if self.exec.limits.address_space.is_some() {
                return Err(ConfigError::InvalidExecConfig {
                    key: "limits.address_space".to_string(),
                    reason: "unsupported_on_platform: macOS does not enforce an address-space rlimit per process".to_string(),
                });
            }
            if self.exec.limits.nproc.is_some() {
                return Err(ConfigError::InvalidExecConfig {
                    key: "limits.nproc".to_string(),
                    reason: "unsupported_on_platform: RLIMIT_NPROC counts every process of the uid, not one run".to_string(),
                });
            }
        }
        // These defaults must agree with the exec pack's resolved defaults.
        // Validate the effective pair, including one-sided overrides, before
        // Duration conversion or a deadline can panic in exec.run.
        let default_timeout = self.exec.timeout_default_s.unwrap_or(30.0);
        let maximum_timeout = self.exec.timeout_max_s.unwrap_or(600.0);
        for (key, value) in [
            ("timeout_default_s", default_timeout),
            ("timeout_max_s", maximum_timeout),
        ] {
            let duration = value
                .is_finite()
                .then(|| std::time::Duration::try_from_secs_f64(value).ok())
                .flatten()
                .filter(|duration| !duration.is_zero());
            if duration
                .is_none_or(|duration| std::time::Instant::now().checked_add(duration).is_none())
            {
                return Err(ConfigError::InvalidExecConfig {
                    key: key.to_string(),
                    reason: "must be positive, finite, and representable as a deadline".to_string(),
                });
            }
        }
        if default_timeout > maximum_timeout {
            return Err(ConfigError::InvalidExecConfig {
                key: "timeout_default_s".to_string(),
                reason: format!(
                    "default {default_timeout} exceeds timeout_max_s {maximum_timeout}"
                ),
            });
        }
        let binary_digest_timeout = self
            .exec
            .binary_digest_timeout_s
            .unwrap_or(DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S);
        if !(1..=MAX_EXEC_BINARY_DIGEST_TIMEOUT_S).contains(&binary_digest_timeout) {
            return Err(ConfigError::InvalidExecConfig {
                key: "binary_digest_timeout_s".to_string(),
                reason: format!("must be between 1 and {MAX_EXEC_BINARY_DIGEST_TIMEOUT_S} seconds"),
            });
        }

        if let Some(value) = self.runtime.blob_hydration_bytes {
            let min = khive_storage::MAX_BLOB_WHOLE_BYTES;
            let max = tokio::sync::Semaphore::MAX_PERMITS as u64;
            if value < min || value > max {
                return Err(ConfigError::InvalidBlobHydrationBytes { value, min, max });
            }
        }

        // Validate actor.id when present — an invalid namespace is a startup error,
        // not a silent fallback.
        if let Some(id) = self.actor.id.as_deref() {
            if id.is_empty() {
                return Err(ConfigError::InvalidActorId {
                    id: id.to_string(),
                    reason: "actor.id must not be empty; remove the key or provide a value"
                        .to_string(),
                });
            }
            Namespace::parse(id).map_err(|e| ConfigError::InvalidActorId {
                id: id.to_string(),
                reason: e.to_string(),
            })?;
        }

        self.actor
            .mailbox_gate(std::sync::Arc::new(khive_gate::AllowAllGate))
            .map_err(|error| ConfigError::InvalidMailboxReaders {
                reason: error.to_string(),
            })?;

        if let Some(ref vis) = self.actor.visible_namespaces {
            for ns_str in vis {
                if ns_str.is_empty() {
                    return Err(ConfigError::InvalidActorId {
                        id: ns_str.clone(),
                        reason: "visible_namespaces entries must not be empty".to_string(),
                    });
                }
                Namespace::parse(ns_str).map_err(|e| ConfigError::InvalidActorId {
                    id: ns_str.clone(),
                    reason: format!("invalid visible namespace: {e}"),
                })?;
            }
        }

        if let Some(gate) = &self.gate {
            khive_gate::CallerEnrollmentGate::validate_write_denials(&gate.deny_writes_for)
                .map_err(|error| ConfigError::InvalidWriteDenyPatterns {
                    reason: error.to_string(),
                })?;
            for id in &gate.granted_actors {
                if id.is_empty() {
                    return Err(ConfigError::InvalidGrantedActorId {
                        id: id.clone(),
                        reason: "actor ids must not be empty".to_string(),
                    });
                }
                Namespace::parse(id).map_err(|error| ConfigError::InvalidGrantedActorId {
                    id: id.clone(),
                    reason: error.to_string(),
                })?;
            }
        }

        // Validate actor.allowed_outbound_namespaces (fail-closed at startup on malformed entry).
        for ns_str in &self.actor.allowed_outbound_namespaces {
            if ns_str.is_empty() {
                return Err(ConfigError::InvalidActorId {
                    id: ns_str.clone(),
                    reason: "allowed_outbound_namespaces entries must not be empty".to_string(),
                });
            }
            Namespace::parse(ns_str).map_err(|e| ConfigError::InvalidActorId {
                id: ns_str.clone(),
                reason: format!("invalid allowed_outbound_namespaces entry: {e}"),
            })?;
        }

        // Backend names must be unique.
        if !self.backends.is_empty() {
            let mut seen_backends = std::collections::HashSet::new();
            for backend in &self.backends {
                BackendId::parse(&backend.name).map_err(|error| {
                    ConfigError::InvalidBackendName {
                        name: backend.name.clone(),
                        reason: error.to_string(),
                    }
                })?;
                if backend
                    .served_kinds
                    .as_ref()
                    .is_some_and(BTreeSet::is_empty)
                {
                    return Err(ConfigError::EmptyBackendServedKinds {
                        name: backend.name.clone(),
                    });
                }
                if !seen_backends.insert(backend.name.clone()) {
                    return Err(ConfigError::DuplicateBackendName {
                        name: backend.name.clone(),
                    });
                }

                // The field's static invariants are checked when the config
                // file loads. The host resolves the environment fallback once
                // for every effective backend before forwarding or opening it.
                if backend.wal_ceiling_bytes.is_some() {
                    resolve_wal_ceiling(
                        backend.wal_ceiling_bytes,
                        None,
                        &backend.name,
                        backend.kind.clone(),
                        backend
                            .journal_mode
                            .as_deref()
                            .is_none_or(|mode| mode.eq_ignore_ascii_case("wal")),
                        backend.read_only,
                    )?;
                }

                backend.resolve_disk_guard(&khive_db::DiskGuardEnvironment::default())?;

                // Reject fields that are parsed but not yet implemented: silently
                // accepting them would let misconfiguration slip past startup.
                if backend.cache_mb.is_some() {
                    return Err(ConfigError::UnsupportedBackendField {
                        name: backend.name.clone(),
                        field: "cache_mb",
                    });
                }
                if backend.journal_mode.is_some() {
                    return Err(ConfigError::UnsupportedBackendField {
                        name: backend.name.clone(),
                        field: "journal_mode",
                    });
                }
            }
        }

        let defined: Vec<&str> = if self.backends.is_empty() {
            vec![BackendId::MAIN]
        } else {
            self.backends.iter().map(|b| b.name.as_str()).collect()
        };
        for (pack_name, pack_cfg) in &self.packs {
            if !defined.contains(&pack_cfg.backend.as_str()) {
                return Err(ConfigError::UnknownPackBackend {
                    pack: pack_name.clone(),
                    backend: pack_cfg.backend.clone(),
                    defined: defined.join(", "),
                });
            }
        }

        if !self.backends.is_empty() {
            let missing: Vec<_> = [SubstrateKind::Note, SubstrateKind::Entity]
                .into_iter()
                .filter(|kind| {
                    !self.backends.iter().any(|backend| {
                        backend
                            .served_kinds
                            .as_ref()
                            .is_none_or(|served| served.contains(kind))
                    })
                })
                .collect();
            if !missing.is_empty() {
                return Err(ConfigError::MissingBackendSearchKinds {
                    kinds: missing,
                    defined: defined.join(", "),
                });
            }
        }

        // Validate [display] timezone (ADR-169): an unrecognized IANA zone
        // name is a startup error, not a silent fallback to the host zone.
        if let Some(tz) = self.display.timezone.as_deref() {
            if tz.trim().is_empty() || tz.parse::<chrono_tz::Tz>().is_err() {
                return Err(ConfigError::InvalidDisplayTimezone {
                    timezone: tz.to_string(),
                });
            }
        }

        // Validate [[git_write.allowed]] entries (ADR-108 Amendment): each
        // repo must be a non-empty absolute path, and each entry must carry
        // at least one branch pattern — an entry with an empty `branches`
        // list would silently allowlist a repo for no branch at all, which
        // reads as "configured" while behaving identically to "not
        // allowlisted"; reject it loudly instead of leaving that trap.
        for entry in &self.git_write.allowed {
            if entry.repo.trim().is_empty() {
                return Err(ConfigError::InvalidGitWriteEntry {
                    repo: entry.repo.clone(),
                    reason: "repo must not be empty".to_string(),
                });
            }
            if !Path::new(&entry.repo).is_absolute() {
                return Err(ConfigError::InvalidGitWriteEntry {
                    repo: entry.repo.clone(),
                    reason: "repo must be an absolute path".to_string(),
                });
            }
            if entry.branches.is_empty() {
                return Err(ConfigError::InvalidGitWriteEntry {
                    repo: entry.repo.clone(),
                    reason: "branches must not be empty".to_string(),
                });
            }
            if entry.branches.iter().any(|b| b.trim().is_empty()) {
                return Err(ConfigError::InvalidGitWriteEntry {
                    repo: entry.repo.clone(),
                    reason: "branches entries must not be empty".to_string(),
                });
            }
            // ADR-108 specifies exact name or a SINGLE-star wildcard per
            // branch pattern -- a pattern with two or more `*` (e.g. `**`,
            // `rel-*-*-final`) is a wider grammar than the ADR authorizes
            // and must be rejected at config load, not silently accepted.
            if let Some(bad) = entry.branches.iter().find(|b| b.matches('*').count() > 1) {
                return Err(ConfigError::InvalidGitWriteEntry {
                    repo: entry.repo.clone(),
                    reason: format!(
                        "branch pattern {bad:?} must contain at most one '*' wildcard (ADR-108)"
                    ),
                });
            }
        }

        validate_peer_engines(&self.engines)
    }

    /// First peer used by single-engine compatibility APIs, if any.
    pub fn default_engine(&self) -> Option<&EngineConfig> {
        self.engines.first()
    }
}

// ---- Env-var fallback ----

/// Build an in-memory `KhiveConfig` from the legacy env-var path.
///
/// Used when no config file is present. Emits `tracing::info!` directing
/// operators to migrate to `~/.khive/config.toml`.
///
/// The primary model (`KHIVE_EMBEDDING_MODEL`) becomes the first peer;
/// additional models follow in their existing order. When only
/// `KHIVE_ADDITIONAL_EMBEDDING_MODELS` is set, the built-in default model is
/// synthesized as the primary — the additional list is additive, never a
/// replacement for the primary (khive#1221; matches `RuntimeConfig::default()`,
/// which resolves an unset `KHIVE_EMBEDDING_MODEL` to the built-in default).
pub fn config_from_env() -> KhiveConfig {
    let primary_model = std::env::var("KHIVE_EMBEDDING_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let additional_raw = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
        .ok()
        .unwrap_or_default();
    let additional: Vec<String> = crate::runtime::parse_pack_list(&additional_raw)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    if primary_model.is_none() && additional.is_empty() {
        return KhiveConfig::default();
    }

    tracing::info!(
        "using env-var embedding config; consider migrating to .khive/config.toml in your project root"
    );

    config_from_env_parts(primary_model, additional)
}

/// Pure core of [`config_from_env`], separated so the engine-list derivation
/// is testable without mutating process-global environment variables.
fn config_from_env_parts(primary_model: Option<String>, additional: Vec<String>) -> KhiveConfig {
    let mut engines = Vec::new();

    let primary =
        primary_model.unwrap_or_else(|| lattice_embed::EmbeddingModel::AllMiniLmL6V2.to_string());
    for model in std::iter::once(primary).chain(additional) {
        let name = canonical_engine_name(&model);
        if engines
            .iter()
            .any(|engine: &EngineConfig| engine.name == name)
        {
            continue;
        }
        engines.push(EngineConfig {
            name,
            weight: 1.0,
            dims: None,
        });
    }

    KhiveConfig {
        engines,
        ..KhiveConfig::default()
    }
}

// ---- Tests ----

// Kept in-crate (not tests/): exercises private ConfigError variants not part
// of the public API.
#[cfg(test)]
#[path = "engine_config_tests.rs"]
mod tests;
