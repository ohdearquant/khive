//! Seatbelt profile rendering and binary identity rules.
//!
//! The profile is deny-default. Reads are allowed under the run directory,
//! the operator's `[exec] read_roots`, and a fixed set of system paths that a
//! dynamically linked binary needs to start (measured on macOS 26 with the
//! Xcode Python framework: the dyld shared cache under `/System`, `/usr/lib`,
//! `/Library`, `/private/var/db`, `/private/etc`, `/dev`, and the root
//! directory itself, which dyld reads to resolve firmlinks). Writes are
//! allowed only under the run directory and to `/dev/null`. No network
//! operation is allowed at all.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use khive_runtime::engine_config::ExecSectionConfig;

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

/// Binaries the run verb refuses by canonical file name regardless of the
/// registry label (ADR-181 Amendment 1 item 8): version control never runs
/// inside a sandbox, it runs through the git verbs.
pub const FORBIDDEN_BASENAMES: &[&str] = &["git", "gh"];

fn quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Resolved exec configuration: every path canonicalized once at load.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub root: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub env_keys: Vec<String>,
    pub never: Vec<PathBuf>,
    pub max_output_bytes: u64,
    pub timeout_default_s: f64,
    pub timeout_max_s: f64,
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
        env_keys: cfg.env.clone(),
        never,
        max_output_bytes: cfg.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
        timeout_default_s: cfg.timeout_default_s.unwrap_or(DEFAULT_TIMEOUT_S),
        timeout_max_s: cfg.timeout_max_s.unwrap_or(DEFAULT_TIMEOUT_MAX_S),
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

/// Render the seatbelt profile for one run. `run_dir` must already be
/// canonical; both the canonical form and any `/tmp` alias resolve to it
/// because seatbelt matches canonical paths.
pub fn render_profile(run_dir: &Path, read_roots: &[PathBuf], never: &[PathBuf]) -> String {
    let mut reads: Vec<String> = SYSTEM_READ_ROOTS
        .iter()
        .map(|p| format!("(subpath {})", quote(Path::new(p))))
        .collect();
    for root in read_roots {
        reads.push(format!("(subpath {})", quote(root)));
    }
    reads.push(format!("(subpath {})", quote(run_dir)));
    let mut maps: Vec<String> = ["/System", "/usr", "/Library"]
        .iter()
        .map(|p| format!("(subpath {})", quote(Path::new(p))))
        .collect();
    for root in read_roots {
        maps.push(format!("(subpath {})", quote(root)));
    }
    let run = format!("(subpath {})", quote(run_dir));
    maps.push(run.clone());
    // ADR-181 Amendment 3 item 3: version control and the never set are
    // refused by the kernel, not only at the registered binary.
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
         (allow file-read-metadata)\n\
         (allow file-read* file-test-existence (literal \"/\") {reads})\n\
         (allow file-map-executable {maps})\n\
         (allow file-write* {run})\n\
         (allow file-write-data (literal \"/dev/null\"))\n",
        denies = denies.join(" "),
        reads = reads.join(" "),
        maps = maps.join(" "),
        run = run,
    )
}

/// Digest of the template with an empty run directory and no roots: names
/// the profile shape independently of any run.
pub fn template_digest() -> String {
    digest_hex(render_profile(Path::new("/"), &[], &[]).as_bytes())
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
                "binary resolves to {canonical:?} ({basename}); version control never runs inside exec.run"
            ),
            BinaryRefusal::Never { canonical } => {
                write!(f, "binary resolves to {canonical:?} which is in the [exec] never set")
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
mod tests {
    use super::*;

    #[test]
    fn profile_names_run_dir_and_roots_only() {
        let p = render_profile(
            Path::new("/private/tmp/run-1"),
            &[PathBuf::from("/opt/py")],
            &[PathBuf::from("/opt/never/tool")],
        );
        assert!(p.contains("(deny default)"));
        assert!(p.contains(
            "(deny process-exec (regex #\"(^|/)(git|gh)$\") (regex #\"/git-[^/]+$\") (literal \"/opt/never/tool\"))"
        ));
        assert!(p.contains("(subpath \"/private/tmp/run-1\")"));
        assert!(p.contains("(subpath \"/opt/py\")"));
        assert!(!p.contains("/Users"));
        assert!(!p.contains("network"));
    }

    #[test]
    fn roots_digest_changes_with_roots() {
        let a = read_roots_digest(&[PathBuf::from("/a")]);
        let b = read_roots_digest(&[PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_ne!(a, b);
        assert_eq!(a, digest_hex(br#"["/a"]"#));
    }

    #[test]
    fn forbidden_basenames_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        std::fs::write(&git, b"#!/bin/sh\n").unwrap();
        let alias = dir.path().join("innocent");
        std::os::unix::fs::symlink(&git, &alias).unwrap();
        assert!(matches!(
            check_binary(alias.to_str().unwrap(), &[]),
            Err(BinaryRefusal::Forbidden { .. })
        ));
        let other = dir.path().join("tool");
        std::fs::write(&other, b"x").unwrap();
        let canonical = std::fs::canonicalize(&other).unwrap();
        assert!(matches!(
            check_binary(other.to_str().unwrap(), std::slice::from_ref(&canonical)),
            Err(BinaryRefusal::Never { .. })
        ));
        assert_eq!(
            check_binary(other.to_str().unwrap(), &[]).unwrap(),
            canonical
        );
    }
}
