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
//!
//! A tool profile is a short list of additions the operator configures for
//! the runs of one registered tool: the Metal GPU, unix-domain sockets under
//! the directories the run may already write, exact extra files, and exact
//! directories it may list. The additions are appended to the profile above
//! and to nothing else, so a tool with no entry runs under the same bytes as
//! before. Every rule here was accepted by driving it, never by its text.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use khive_runtime::engine_config::{
    ExecSectionConfig, ExecToolProfileConfig, DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S,
};

use crate::tree::digest_hex;

/// Directories every run may read: runtime necessities, never a home.
///
/// `/bin` is here for name lookup. A run could always launch `/bin/mkdir` by
/// its path, but a shell searching `PATH` inspects each candidate before it
/// launches one, and that inspection was refused, so `mkdir` by name answered
/// "command not found" whatever `PATH` held. It holds the system's own tools
/// on the sealed volume, as `/usr` does.
pub const SYSTEM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/System",
    "/usr",
    "/Library",
    "/private/var/db",
    "/private/etc",
    "/private/var/select",
    "/private/preboot",
    "/dev",
];

/// Root links through which macOS names two of the roots above: `/etc` is a
/// link to `private/etc` and `/var` a link to `private/var`. A run may read
/// each link itself and nothing through it that the roots do not already
/// allow. Without this `/etc/hosts`, `/etc/ssl/cert.pem` and every file under
/// `/usr/share/zoneinfo` (a link into `/var/db/timezone`) are refused under
/// the names programs use, while the same files read under `/private`.
pub const SYSTEM_ROOT_LINKS: &[&str] = &["/etc", "/var"];

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
    pub tool_profiles: Vec<ToolProfile>,
}

/// One path an operator named in a tool profile: the spelling in the
/// configuration and the path it resolves to. Both are kept because a program
/// opens the spelling, and the sandbox matches the resolved path while still
/// checking each directory the spelling walks through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedPath {
    pub written: PathBuf,
    pub canonical: PathBuf,
}

/// The additions one registered tool's runs get, resolved once at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolProfile {
    pub tool: String,
    pub metal: bool,
    /// Metal's shader cache folder for the user the runs execute as. `None`
    /// when `metal` is off, or when the host could not name the folder, in
    /// which case a run is refused rather than run without the device.
    pub metal_cache: Option<PathBuf>,
    pub unix_sockets: bool,
    pub files: Vec<NamedPath>,
    pub list_dirs: Vec<NamedPath>,
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
        tool_profiles: cfg
            .tool_profiles
            .iter()
            .map(|profile| resolve_tool_profile(profile, user_cache_dir()))
            .collect(),
    }
}

/// The name of Metal's shader cache folder inside a user's cache folder.
pub const METAL_CACHE_DIR: &str = "com.apple.metal";

/// The calling user's cache folder, as the system names it, resolved.
#[cfg(target_os = "macos")]
pub fn user_cache_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the buffer is valid for its whole length and confstr writes at
    // most that many bytes, including the terminator.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if written == 0 || written > buffer.len() {
        return None;
    }
    buffer.truncate(written - 1);
    std::fs::canonicalize(PathBuf::from(std::ffi::OsString::from_vec(buffer))).ok()
}

/// No platform but macOS has the folder, and no run starts there either.
#[cfg(not(target_os = "macos"))]
pub fn user_cache_dir() -> Option<PathBuf> {
    None
}

/// Resolve a path that may not exist yet: its directory is resolved and its
/// last component kept, so a lock file a run creates still resolves.
fn resolve_named(written: &str) -> NamedPath {
    let written = PathBuf::from(written);
    let canonical = std::fs::canonicalize(&written).unwrap_or_else(|_| {
        match (written.parent(), written.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .unwrap_or_else(|_| written.clone()),
            _ => written.clone(),
        }
    });
    NamedPath { written, canonical }
}

/// Resolve one configured tool profile. `user_cache` is the cache folder of
/// the user the runs execute as; it is an argument so the result does not
/// depend on which host renders it.
pub fn resolve_tool_profile(
    cfg: &ExecToolProfileConfig,
    user_cache: Option<PathBuf>,
) -> ToolProfile {
    let named = |paths: &[String]| {
        let mut out: Vec<NamedPath> = paths.iter().map(|path| resolve_named(path)).collect();
        out.sort_by(|a, b| {
            a.canonical
                .cmp(&b.canonical)
                .then(a.written.cmp(&b.written))
        });
        out.dedup();
        out
    };
    ToolProfile {
        tool: cfg.tool.clone(),
        metal: cfg.metal,
        metal_cache: cfg
            .metal
            .then(|| user_cache.map(|cache| cache.join(METAL_CACHE_DIR)))
            .flatten(),
        unix_sockets: cfg.unix_sockets,
        files: named(&cfg.files),
        list_dirs: named(&cfg.list_dirs),
    }
}

/// Why the configured tool profiles cannot be used, if they cannot. A profile
/// with no tool name or a name used twice does not say which runs it is for,
/// a relative path names a different place per working directory, and a path
/// under the run root would reach into another run's directory.
pub fn tool_profile_conflict(root: &Path, profiles: &[ToolProfile]) -> Option<String> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut seen: Vec<&str> = Vec::new();
    for profile in profiles {
        if profile.tool.trim().is_empty() {
            return Some("[[exec.tool_profiles]] entry has an empty tool name".into());
        }
        if seen.contains(&profile.tool.as_str()) {
            return Some(format!(
                "[[exec.tool_profiles]] names tool {:?} more than once",
                profile.tool
            ));
        }
        seen.push(&profile.tool);
        for (key, paths) in [("files", &profile.files), ("list_dirs", &profile.list_dirs)] {
            for path in paths {
                if !path.written.is_absolute() {
                    return Some(format!(
                        "[[exec.tool_profiles]] {key} entry {:?} for tool {:?} is not an absolute path",
                        path.written, profile.tool
                    ));
                }
                if path.canonical.starts_with(&root) {
                    return Some(format!(
                        "[[exec.tool_profiles]] {key} entry {:?} for tool {:?} is under the run root {root:?}",
                        path.written, profile.tool
                    ));
                }
            }
        }
    }
    None
}

/// The profile configured for a registered tool's name, if there is one.
pub fn tool_profile_for<'a>(profiles: &'a [ToolProfile], tool: &str) -> Option<&'a ToolProfile> {
    profiles.iter().find(|profile| profile.tool == tool)
}

/// What a receipt and `exec.identity` say about a tool profile.
pub fn tool_profile_json(profile: &ToolProfile) -> Value {
    let paths = |paths: &[NamedPath]| -> Vec<String> {
        paths
            .iter()
            .map(|path| path.canonical.to_string_lossy().to_string())
            .collect()
    };
    json!({
        "tool": profile.tool,
        "metal": profile.metal,
        "metal_cache": profile.metal_cache.as_ref().map(|p| p.to_string_lossy().to_string()),
        "unix_sockets": profile.unix_sockets,
        "files": paths(&profile.files),
        "list_dirs": paths(&profile.list_dirs),
        "rules_digest": tool_rules_digest(profile),
    })
}

/// Digest of a tool profile's rules rendered for an empty run directory and
/// no cache roots: names the additions independently of any run.
pub fn tool_rules_digest(profile: &ToolProfile) -> String {
    digest_hex(render_tool_rules(Path::new("/"), &[], profile).as_bytes())
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

/// Why the configured cache roots cannot be used with the run root, if they
/// cannot. A cache root that holds the run root, or sits inside it, would let
/// one run write into another run's directory, and a relative one names a
/// different directory per working directory.
pub fn cache_root_conflict(root: &Path, cache_roots: &[PathBuf]) -> Option<String> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for cache in cache_roots {
        if !cache.is_absolute() {
            return Some(format!(
                "[exec] cache_roots entry {cache:?} is not an absolute path"
            ));
        }
        if root.starts_with(cache) || cache.starts_with(&root) {
            return Some(format!(
                "[exec] cache_roots entry {cache:?} overlaps the run root {root:?}"
            ));
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
    for link in SYSTEM_ROOT_LINKS {
        metadata.push(format!("(literal {})", quote(Path::new(link))));
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

/// Literal metadata rules for every directory above `path`, and for `path`
/// itself when `with_self` is set.
fn ancestor_literals(path: &Path, with_self: bool, out: &mut Vec<String>) {
    for ancestor in path.ancestors().skip(if with_self { 0 } else { 1 }) {
        out.push(format!("(literal {})", quote(ancestor)));
    }
}

/// The rules a tool profile appends to the template. Each block is the
/// smallest set that a driven run needed (macOS 27.2, Apple silicon):
///
/// - Metal: the GPU's user-client class, the system shader compiler, the
///   service that names the per-user cache folder, and Metal's own folder
///   inside it. Without the last two every pipeline is compiled again on
///   every run.
/// - Unix sockets: bind, accept and connect on socket files under the run
///   directory and the cache roots. No address family but unix, so nothing
///   reaches the network or a loopback port.
/// - Files: read and write of each exact path.
/// - Directories: opening and listing each exact path, nothing below it.
pub fn render_tool_rules(run_dir: &Path, cache_roots: &[PathBuf], profile: &ToolProfile) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut metadata: Vec<String> = Vec::new();
    if profile.metal {
        lines.push(
            "(allow iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))".to_string(),
        );
        lines.push(
            "(allow mach-lookup (global-name \"com.apple.MTLCompilerService\") \
             (global-name \"com.apple.bsd.dirhelper\"))"
                .to_string(),
        );
        if let Some(cache) = &profile.metal_cache {
            lines.push(format!(
                "(allow file-read* file-write* (subpath {}))",
                quote(cache)
            ));
            ancestor_literals(cache, false, &mut metadata);
            // The system names the folder through `/var`, a link.
            metadata.push("(literal \"/var\")".to_string());
        }
    }
    if profile.unix_sockets {
        for root in std::iter::once(run_dir).chain(cache_roots.iter().map(PathBuf::as_path)) {
            let root = quote(root);
            lines.push(format!(
                "(allow network-bind network-inbound (local unix-socket (subpath {root})))"
            ));
            lines.push(format!(
                "(allow network-outbound (remote unix-socket (subpath {root})))"
            ));
        }
    }
    for file in &profile.files {
        lines.push(format!(
            "(allow file-read* file-write* (literal {}))",
            quote(&file.canonical)
        ));
        ancestor_literals(&file.canonical, false, &mut metadata);
        ancestor_literals(&file.written, false, &mut metadata);
    }
    for dir in &profile.list_dirs {
        lines.push(format!(
            "(allow file-read-data (literal {}))",
            quote(&dir.canonical)
        ));
        ancestor_literals(&dir.canonical, true, &mut metadata);
        ancestor_literals(&dir.written, true, &mut metadata);
    }
    metadata.sort();
    metadata.dedup();
    if !metadata.is_empty() {
        lines.push(format!("(allow file-read-metadata {})", metadata.join(" ")));
    }
    let mut out = String::new();
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The profile for one run of a tool: the template, then the tool's
/// additions when the operator configured some. With no tool profile the
/// result is `render_profile`'s, byte for byte.
pub fn render_profile_for_tool(
    run_dir: &Path,
    read_roots: &[PathBuf],
    cache_roots: &[PathBuf],
    never: &[PathBuf],
    tool: Option<&ToolProfile>,
) -> String {
    let mut profile = render_profile(run_dir, read_roots, cache_roots, never);
    if let Some(tool) = tool {
        profile.push_str(&render_tool_rules(run_dir, cache_roots, tool));
    }
    profile
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
