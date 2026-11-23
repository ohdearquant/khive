//! Seatbelt profile rendering and binary identity rules.
//!
//! The profile is deny-default. Reads are allowed under the run directory,
//! the operator's `[exec] read_roots`, and a fixed set of system paths that a
//! dynamically linked binary needs to start (measured on macOS 26 with the
//! Xcode Python framework: the dyld shared cache under `/System`, `/usr/lib`,
//! `/Library`, `/private/var/db`, `/private/etc`, `/dev`, and the root
//! directory itself, which dyld reads to resolve firmlinks). Writes are
//! allowed only under the run directory, the operator's `[exec] cache_roots`,
//! and to `/dev/null`. No network operation is allowed at all.
//!
//! A cache root is a directory every run may read, write and load code from,
//! and that outlives the run: a compiler's package cache and build output
//! live there. Nothing under it is captured into the run's output tree, and
//! one run can read and change what an earlier run left, so a run that had a
//! cache root is not a function of its input tree alone. The receipt names
//! the cache roots' digest for that reason.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use khive_runtime::engine_config::{ExecSectionConfig, DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S};

use crate::tree::digest_hex;

/// Directories every run may read: runtime necessities, never a home.
pub const SYSTEM_READ_ROOTS: &[&str] = &[
    "/System",
    "/usr",
    "/Library",
    "/private/var/db",
    "/private/etc",
    "/private/var/select",
    "/private/preboot",
    "/dev",
];

/// Registered binaries the run verb refuses by canonical file name regardless
/// of registry label (ADR-181 Amendment 1 item 8). The seatbelt also refuses
/// launches under these names; other names and paths are outside these matches.
pub const FORBIDDEN_BASENAMES: &[&str] = &["git", "gh"];

/// The sandbox launcher. Every run is a child of this binary, so its absence
/// is the whole reason the pack runs on macOS only.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Refuse before any work is done when the host has no sandbox backend.
///
/// The check reads the backend's own dependency rather than a compile-time
/// target, so a macOS host that is missing `sandbox-exec` refuses for the same
/// stated reason a Linux host does, and the reason names the platform instead
/// of arriving later as a failed spawn.
pub fn check_backend(path: &str) -> Result<(), String> {
    if Path::new(path).exists() {
        return Ok(());
    }
    Err(format!(
        "no sandbox backend on this host: every run is executed under the macOS \
         seatbelt sandbox, which needs {path}, and this host is {os}, where that \
         binary does not exist. Runs are macOS-only until a backend for this \
         platform ships.",
        os = std::env::consts::OS
    ))
}

fn quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Resolved exec configuration: every path canonicalized once at load.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub root: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub cache_roots: Vec<PathBuf>,
    pub env_keys: Vec<String>,
    pub never: Vec<PathBuf>,
    pub max_output_bytes: u64,
    pub timeout_default_s: f64,
    pub timeout_max_s: f64,
    pub binary_digest_timeout_s: u64,
    pub keep: bool,
    pub limits: Limits,
}

#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub cpu_seconds: Option<u64>,
    pub address_space: Option<u64>,
    pub file_size: Option<u64>,
    pub nproc: Option<u64>,
}

impl Limits {
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        if let Some(v) = self.cpu_seconds {
            m.insert("cpu_seconds".into(), json!(v));
        }
        if let Some(v) = self.address_space {
            m.insert("address_space".into(), json!(v));
        }
        if let Some(v) = self.file_size {
            m.insert("file_size".into(), json!(v));
        }
        if let Some(v) = self.nproc {
            m.insert("nproc".into(), json!(v));
        }
        Value::Object(m)
    }
}

pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_TIMEOUT_S: f64 = 30.0;
pub const DEFAULT_TIMEOUT_MAX_S: f64 = 600.0;

pub fn resolve(cfg: &ExecSectionConfig) -> Resolved {
    let root = cfg.root.as_deref().map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Path::new(&home).join(".khive").join("exec")
    });
    let mut read_roots: Vec<PathBuf> = cfg
        .read_roots
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)))
        .collect();
    read_roots.sort();
    read_roots.dedup();
    let mut cache_roots: Vec<PathBuf> = cfg
        .cache_roots
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)))
        .collect();
    cache_roots.sort();
    cache_roots.dedup();
    let mut never: Vec<PathBuf> = cfg
        .never
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)))
        .collect();
    never.sort();
    never.dedup();
    Resolved {
        root,
        read_roots,
        cache_roots,
        env_keys: cfg.env.clone(),
        never,
        max_output_bytes: cfg.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
        timeout_default_s: cfg.timeout_default_s.unwrap_or(DEFAULT_TIMEOUT_S),
        timeout_max_s: cfg.timeout_max_s.unwrap_or(DEFAULT_TIMEOUT_MAX_S),
        binary_digest_timeout_s: cfg
            .binary_digest_timeout_s
            .unwrap_or(DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S),
        keep: cfg.keep,
        limits: Limits {
            cpu_seconds: cfg.limits.cpu_seconds,
            address_space: cfg.limits.address_space,
            file_size: cfg.limits.file_size,
            nproc: cfg.limits.nproc,
        },
    }
}

/// Canonical serialization of the resolved read roots: a compact JSON array
/// of the sorted canonical paths. Its BLAKE3 hex is `read_roots_digest`.
pub fn read_roots_serialization(read_roots: &[PathBuf]) -> Vec<u8> {
    let roots: Vec<String> = read_roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    serde_json::to_vec(&roots).expect("roots serialize")
}

pub fn read_roots_digest(read_roots: &[PathBuf]) -> String {
    digest_hex(&read_roots_serialization(read_roots))
}

/// Why the configured cache roots cannot be used with the run root and the
/// read roots, if they cannot. A cache root that holds the run root, or sits
/// inside it, would let one run write into another run's directory. One that
/// equals, holds or sits inside a read root would make a directory the
/// configuration calls read-only writable. A relative one names a different
/// directory per working directory, and one that cannot be resolved cannot be
/// compared with either, so each is refused.
///
/// Entries are compared in canonical form, so a symbolic link or a `..`
/// component cannot hide an overlap. `read_roots` are the resolved read roots.
/// The check does not judge what else a cache root holds.
pub fn cache_root_conflict(
    root: &Path,
    read_roots: &[PathBuf],
    cache_roots: &[PathBuf],
) -> Option<String> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for entry in cache_roots {
        if !entry.is_absolute() {
            return Some(format!(
                "[exec] cache_roots entry {entry:?} is not an absolute path"
            ));
        }
        let cache = match std::fs::canonicalize(entry) {
            Ok(cache) => cache,
            Err(error) => {
                return Some(format!(
                    "[exec] cache_roots entry {entry:?} cannot be resolved ({error}); \
                     it must exist when a run starts"
                ));
            }
        };
        if root.starts_with(&cache) || cache.starts_with(&root) {
            return Some(format!(
                "[exec] cache_roots entry {entry:?} overlaps the run root {root:?}"
            ));
        }
        for read in read_roots {
            let read_canonical = std::fs::canonicalize(read).unwrap_or_else(|_| read.clone());
            if read_canonical.starts_with(&cache) || cache.starts_with(&read_canonical) {
                return Some(format!(
                    "[exec] cache_roots entry {entry:?} overlaps the read root {read:?}"
                ));
            }
        }
    }
    None
}

/// Digest of the resolved cache roots, in the read roots' serialization.
pub fn cache_roots_digest(cache_roots: &[PathBuf]) -> String {
    digest_hex(&read_roots_serialization(cache_roots))
}

/// Render the seatbelt profile for one run. `run_dir` must already be
/// canonical; both the canonical form and any `/tmp` alias resolve to it
/// because seatbelt matches canonical paths. With no cache roots the profile
/// is the one rendered before cache roots existed, byte for byte.
pub fn render_profile(
    run_dir: &Path,
    read_roots: &[PathBuf],
    cache_roots: &[PathBuf],
    never: &[PathBuf],
) -> String {
    let mut reads: Vec<String> = SYSTEM_READ_ROOTS
        .iter()
        .map(|p| format!("(subpath {})", quote(Path::new(p))))
        .collect();
    for root in read_roots.iter().chain(cache_roots) {
        reads.push(format!("(subpath {})", quote(root)));
    }
    reads.push(format!("(subpath {})", quote(run_dir)));
    // Metadata is a read too. Permit it for the same trees and their exact
    // ancestor directories, which dyld and pathname traversal must inspect.
    // A bare file-read-metadata allowance would expose every host path.
    let mut metadata = reads.clone();
    for root in SYSTEM_READ_ROOTS
        .iter()
        .map(|root| Path::new(*root))
        .chain(read_roots.iter().map(PathBuf::as_path))
        .chain(cache_roots.iter().map(PathBuf::as_path))
        .chain(std::iter::once(run_dir))
    {
        for ancestor in root.ancestors().skip(1) {
            metadata.push(format!("(literal {})", quote(ancestor)));
        }
    }
    metadata.sort();
    metadata.dedup();
    let mut maps: Vec<String> = ["/System", "/usr", "/Library"]
        .iter()
        .map(|p| format!("(subpath {})", quote(Path::new(p))))
        .collect();
    for root in read_roots.iter().chain(cache_roots) {
        maps.push(format!("(subpath {})", quote(root)));
    }
    let run = format!("(subpath {})", quote(run_dir));
    maps.push(run.clone());
    let mut writes = vec![run];
    for root in cache_roots {
        writes.push(format!("(subpath {})", quote(root)));
    }
    // ADR-181 Amendment 9: these name and path matches apply to each sandboxed
    // launch, not only to the registered binary.
    let mut denies: Vec<String> = vec![
        "(regex #\"(^|/)(git|gh)$\")".to_string(),
        "(regex #\"/git-[^/]+$\")".to_string(),
    ];
    for path in never {
        denies.push(format!("(literal {})", quote(path)));
    }
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec)\n\
         (deny process-exec {denies})\n\
         (allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow process-info* (target same-sandbox))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n\
         (allow file-ioctl (literal \"/dev/dtracehelper\"))\n\
         (allow file-read-metadata {metadata})\n\
         (allow file-read* file-test-existence (literal \"/\") {reads})\n\
         (allow file-map-executable {maps})\n\
         (allow file-write* {writes})\n\
         (allow file-write-data (literal \"/dev/null\"))\n",
        denies = denies.join(" "),
        reads = reads.join(" "),
        metadata = metadata.join(" "),
        maps = maps.join(" "),
        writes = writes.join(" "),
    )
}

/// Digest of the template with an empty run directory and no roots: names
/// the profile shape independently of any run.
pub fn template_digest() -> String {
    digest_hex(render_profile(Path::new("/"), &[], &[], &[]).as_bytes())
}

/// Why a registered binary may not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryRefusal {
    NotFound(String),
    NotAFile(String),
    Forbidden { canonical: String, basename: String },
    Never { canonical: String },
}

impl std::fmt::Display for BinaryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryRefusal::NotFound(p) => write!(f, "binary {p:?} does not exist"),
            BinaryRefusal::NotAFile(p) => write!(f, "binary {p:?} is not a regular file"),
            BinaryRefusal::Forbidden {
                canonical,
                basename,
            } => write!(
                f,
                "registered binary resolves to forbidden path {canonical:?} ({basename})"
            ),
            BinaryRefusal::Never { canonical } => {
                write!(
                    f,
                    "binary resolves to {canonical:?} which is in the [exec] never set"
                )
            }
        }
    }
}

/// Canonicalize the registered binary and apply the identity rules.
pub fn check_binary(registered: &str, never: &[PathBuf]) -> Result<PathBuf, BinaryRefusal> {
    let canonical = std::fs::canonicalize(registered)
        .map_err(|_| BinaryRefusal::NotFound(registered.to_string()))?;
    let meta = std::fs::metadata(&canonical)
        .map_err(|_| BinaryRefusal::NotFound(registered.to_string()))?;
    if !meta.is_file() {
        return Err(BinaryRefusal::NotAFile(canonical.to_string_lossy().into()));
    }
    let basename = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if FORBIDDEN_BASENAMES.contains(&basename.as_str()) {
        return Err(BinaryRefusal::Forbidden {
            canonical: canonical.to_string_lossy().into(),
            basename,
        });
    }
    if never.iter().any(|n| n == &canonical) {
        return Err(BinaryRefusal::Never {
            canonical: canonical.to_string_lossy().into(),
        });
    }
    Ok(canonical)
}

#[cfg(test)]
#[path = "sandbox/tests.rs"]
mod tests;
