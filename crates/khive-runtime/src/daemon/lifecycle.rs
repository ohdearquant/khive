use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[cfg(all(doc, unix))]
use super::{acquire_daemon_boot_guard, cleanup_stale_daemon};
#[cfg(unix)]
use super::{supervisor_marker_path, ConnectionAdmission};

/// ADR-049 Amendment 11's disclosed initial demand idle interval.
pub const DEFAULT_DEMAND_IDLE_SECS: u64 = 1_800;

/// A launch-time choice, never inferred from process ancestry or environment.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLifetime {
    Demand,
    #[default]
    Persistent,
}

/// Immutable daemon options. Existing entry points use persistent mode.
#[derive(Debug, Clone, Copy)]
pub struct DaemonOptions {
    pub lifetime: DaemonLifetime,
    pub idle_interval: std::time::Duration,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            lifetime: DaemonLifetime::Persistent,
            idle_interval: std::time::Duration::from_secs(DEFAULT_DEMAND_IDLE_SECS),
        }
    }
}

/// Host-owned startup decisions disclosed by lifecycle diagnostics.
#[derive(Debug, Clone, Default)]
pub struct DaemonStartupReport {
    pub skipped_components: Vec<String>,
    /// A named unknown inventory or service obligation prevents retirement.
    pub idle_ineligible_reasons: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLifecyclePhase {
    Serving,
    Draining,
    Stopped,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonShutdownReason {
    Idle,
    Signal,
}

/// Additive diagnostics for one daemon incarnation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DaemonLifecycleSnapshot {
    pub lifetime: DaemonLifetime,
    pub instance_generation: String,
    pub effective_idle_interval_ms: u64,
    pub phase: DaemonLifecyclePhase,
    pub shutdown_reason: Option<DaemonShutdownReason>,
    pub skipped_components: Vec<String>,
    pub idle_ineligible_reasons: Vec<String>,
    pub ordinary_requests: usize,
    pub idle_blockers: Vec<String>,
}

#[cfg(unix)]
pub(super) struct DaemonLifecycle {
    pub(super) options: DaemonOptions,
    state: std::sync::Mutex<DaemonLifecycleState>,
    /// Admission for new connections on the daemon socket.
    pub(super) connections: ConnectionAdmission,
}

#[cfg(unix)]
struct DaemonLifecycleState {
    snapshot: DaemonLifecycleSnapshot,
    last_request_completion: Option<tokio::time::Instant>,
}

#[cfg(unix)]
impl DaemonLifecycle {
    pub(super) fn new(options: DaemonOptions, report: DaemonStartupReport) -> Self {
        Self {
            options,
            state: std::sync::Mutex::new(DaemonLifecycleState {
                snapshot: DaemonLifecycleSnapshot {
                    lifetime: options.lifetime,
                    instance_generation: uuid::Uuid::new_v4().to_string(),
                    effective_idle_interval_ms: options
                        .idle_interval
                        .as_millis()
                        .min(u128::from(u64::MAX))
                        as u64,
                    phase: DaemonLifecyclePhase::Serving,
                    shutdown_reason: None,
                    skipped_components: report.skipped_components,
                    idle_ineligible_reasons: report.idle_ineligible_reasons,
                    ordinary_requests: 0,
                    idle_blockers: Vec::new(),
                },
                last_request_completion: None,
            }),
            connections: ConnectionAdmission::from_env(),
        }
    }

    pub(super) fn snapshot(&self) -> DaemonLifecycleSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    pub(super) fn ready(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_request_completion = Some(tokio::time::Instant::now());
    }

    pub(super) fn admit(self: &Arc<Self>) -> Option<OrdinaryRequestGuard> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.phase != DaemonLifecyclePhase::Serving {
            return None;
        }
        state.snapshot.ordinary_requests += 1;
        Some(OrdinaryRequestGuard(Arc::clone(self)))
    }

    /// The same mutex orders ordinary admission and the irreversible idle decision.
    pub(super) fn try_idle(&self, blockers: impl FnOnce() -> Vec<String>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.options.lifetime != DaemonLifetime::Demand
            || state.snapshot.phase != DaemonLifecyclePhase::Serving
            || state.snapshot.ordinary_requests != 0
            || !state.snapshot.idle_ineligible_reasons.is_empty()
            || state
                .last_request_completion
                .is_none_or(|last| last.elapsed() < self.options.idle_interval)
        {
            return false;
        }
        state.snapshot.idle_blockers = blockers();
        if !state.snapshot.idle_blockers.is_empty() {
            return false;
        }
        state.snapshot.phase = DaemonLifecyclePhase::Draining;
        state.snapshot.shutdown_reason = Some(DaemonShutdownReason::Idle);
        true
    }

    pub(super) fn draining(&self, reason: DaemonShutdownReason) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.phase != DaemonLifecyclePhase::Stopped {
            state.snapshot.phase = DaemonLifecyclePhase::Draining;
            state.snapshot.shutdown_reason = Some(reason);
        }
    }

    pub(super) fn stopped(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .phase = DaemonLifecyclePhase::Stopped;
    }
}

#[cfg(unix)]
pub(super) struct OrdinaryRequestGuard(Arc<DaemonLifecycle>);

#[cfg(unix)]
impl Drop for OrdinaryRequestGuard {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.snapshot.ordinary_requests -= 1;
        // The guard covers response transport and cleanup, not just dispatch.
        state.last_request_completion = Some(tokio::time::Instant::now());
    }
}

// ── paths ─────────────────────────────────────────────────────────────────────

/// Base `.khive` directory used to anchor every advisory lock/socket/pid
/// path below. Pure path computation — portable on every target, even
/// though most of its callers (socket/pid paths) are unix-only.
///
/// Resolution order: `HOME`, then `USERPROFILE` (the conventional Windows
/// home variable), then a platform-specific last resort. On non-unix targets
/// the last resort is the OS temp directory (per-user on Windows), so the
/// lock path can never be working-directory-relative there: two processes
/// opening the same database from different working directories must resolve
/// the same lock file. On unix the last resort stays the historical `"."` —
/// a shared world-writable anchor such as `/tmp` would be worse, since a
/// local attacker could pre-claim the directory and the socket/lock files
/// under it before the daemon's first run.
pub(super) fn khive_dir() -> PathBuf {
    khive_root_from(
        std::env::var("HOME").ok(),
        std::env::var("USERPROFILE").ok(),
    )
}

/// Env-free core of [`khive_dir`], split out so the fallback chain is
/// testable without mutating process-global environment variables.
pub(super) fn khive_root_from(home: Option<String>, userprofile: Option<String>) -> PathBuf {
    home.filter(|v| !v.trim().is_empty())
        .or_else(|| userprofile.filter(|v| !v.trim().is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(last_resort_root)
        .join(".khive")
}

/// The directory for the SQLite volume lock files, from the single rule in
/// [`khive_db::default_volume_lock_dir`]: `KHIVE_VOLUME_LOCK_DIR` when set,
/// else `<home>/.khive/sqlite-volume-locks`. Unlike `khive_dir` it has no
/// last-resort root: without a home directory the result is a configuration
/// error, because a working-directory-relative lock directory would give two
/// processes two different lock files.
pub fn volume_lock_dir() -> Result<PathBuf, khive_db::SqliteError> {
    khive_db::default_volume_lock_dir()
}

/// See [`khive_dir`] for why the two arms differ.
#[cfg(unix)]
fn last_resort_root() -> PathBuf {
    PathBuf::from(".")
}

#[cfg(not(unix))]
fn last_resort_root() -> PathBuf {
    std::env::temp_dir()
}

/// Env var overriding the socket half of the daemon rendezvous.
#[cfg(unix)]
pub(super) const SOCKET_PATH_ENV: &str = "KHIVE_SOCKET";

/// Env var overriding the PID-file half of the daemon rendezvous.
#[cfg(unix)]
pub(super) const PID_PATH_ENV: &str = "KHIVE_PID";

/// Read a path override, treating an empty value as unset.
///
/// One predicate for "the operator set this variable", shared by the path
/// resolvers and [`ensure_rendezvous_overrides_paired`]. A pairing check that
/// disagreed with the resolvers about what counts as set would either refuse
/// boots that resolve consistently, or admit the split rendezvous it exists
/// to stop.
#[cfg(unix)]
fn path_override(key: &str) -> Option<PathBuf> {
    match std::env::var(key) {
        Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    }
}

#[cfg(unix)]
pub(super) fn default_socket_path() -> PathBuf {
    khive_dir().join("khived.sock")
}

#[cfg(unix)]
fn default_pid_path() -> PathBuf {
    khive_dir().join("khived.pid")
}

/// Unix socket path the daemon binds and clients connect to.
///
/// Overridable via the `KHIVE_SOCKET` env var (for tests and ops), which must
/// be set together with `KHIVE_PID`: the daemon refuses to boot when exactly
/// one of the two is set.
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    path_override(SOCKET_PATH_ENV).unwrap_or_else(default_socket_path)
}

/// PID file path written by the daemon.
///
/// Overridable via the `KHIVE_PID` env var, which must be set together with
/// `KHIVE_SOCKET`: the daemon refuses to boot when exactly one of the two is
/// set.
#[cfg(unix)]
pub fn pid_path() -> PathBuf {
    path_override(PID_PATH_ENV).unwrap_or_else(default_pid_path)
}

/// Refuse to boot when exactly one of `KHIVE_SOCKET` / `KHIVE_PID` is set
/// (#2656).
///
/// The socket and the PID file are the two halves of one rendezvous, but they
/// resolve independently: with only `KHIVE_SOCKET` set a daemon binds a
/// private socket while still claiming the shared PID file, and with only
/// `KHIVE_PID` set it writes a private PID file while binding the shared
/// socket. Either way [`cleanup_stale_daemon`] reads an incumbent's pid out of
/// one instance's file and judges it by probing the other instance's socket,
/// so both of its branches are wrong: a live incumbent produces a refusal
/// naming a pid that has nothing to do with the socket being started, and a
/// pid that is no longer running makes this process delete a rendezvous file
/// another daemon's `shutdown_cleanup_if_owned` still expects to own.
///
/// Setting both variables (a fully private rendezvous) and setting neither
/// (the default rendezvous) are both unchanged.
#[cfg(unix)]
pub(super) fn ensure_rendezvous_overrides_paired() -> anyhow::Result<()> {
    match (path_override(SOCKET_PATH_ENV), path_override(PID_PATH_ENV)) {
        (Some(socket), None) => anyhow::bail!(
            "refusing to start: {SOCKET_PATH_ENV} is set to {} but {PID_PATH_ENV} is not set. \
             The socket and the PID file are two halves of one daemon rendezvous and must move \
             together: with only {SOCKET_PATH_ENV} set, this daemon would bind a private socket \
             while claiming the shared PID file at {}, which belongs to the default rendezvous \
             served on {}. Set {PID_PATH_ENV} to a private path beside the socket, or unset \
             {SOCKET_PATH_ENV} to share the default rendezvous.",
            socket.display(),
            default_pid_path().display(),
            default_socket_path().display(),
        ),
        (None, Some(pid)) => anyhow::bail!(
            "refusing to start: {PID_PATH_ENV} is set to {} but {SOCKET_PATH_ENV} is not set. \
             The socket and the PID file are two halves of one daemon rendezvous and must move \
             together: with only {PID_PATH_ENV} set, this daemon would write a private PID file \
             while binding the shared socket at {}, the default rendezvous whose owner is \
             recorded in {}. Set {SOCKET_PATH_ENV} to a private path beside the PID file, or \
             unset {PID_PATH_ENV} to share the default rendezvous.",
            pid.display(),
            default_socket_path().display(),
            default_pid_path().display(),
        ),
        _ => Ok(()),
    }
}

/// Advisory lock file used to serialize stale-daemon recovery across concurrent
/// clients (flock/`File::lock` on the file; released when the lock file
/// handle is dropped). Path computation is portable; `kkernel exec`'s
/// non-unix local-construction guard (`kkernel::exec::acquire_local_construction_guard`)
/// shares this exact path with the unix daemon-boot guard so the two stay
/// mutually exclusive.
///
/// Overridable via the `KHIVE_LOCK` env var (for tests).
pub fn lock_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_LOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.recovery.lock")
}

/// Advisory lock file used to serialize RECOVERY (kill+respawn) attempts
/// across concurrent clients only — the daemon's own boot sequence never
/// acquires this file ([`lock_path`] / [`acquire_daemon_boot_guard`] is the
/// boot-side lock). A recoverer holding this lock across dead-confirmation
/// → kill → spawn (khive-mcp's `kill_and_respawn`) therefore can never
/// deadlock against a peer daemon's boot, unlike holding the shared boot
/// lock for that whole span would.
///
/// Overridable via the `KHIVE_RECOVERER_LOCK` env var (for tests).
#[cfg(unix)]
pub fn recoverer_lock_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_RECOVERER_LOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.recoverer.lock")
}

#[cfg(unix)]
pub const SUPERVISOR_CLAIM_ENV: &str = "KHIVE_SUPERVISOR_CLAIM";

#[cfg(unix)]
pub(super) fn read_supervisor_marker_claim() -> Option<(u32, String)> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(supervisor_marker_path())
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut marker = String::new();
    file.take(4097).read_to_string(&mut marker).ok()?;
    if marker.len() > 4096 {
        return None;
    }
    let mut lines = marker.lines();
    if lines.next()?.is_empty() {
        return None;
    }
    let pid = lines.next()?.parse::<u32>().ok().filter(|pid| *pid > 0)?;
    lines
        .next()?
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)?;
    let claim = lines.next()?.to_string();
    if lines.next().is_some() {
        return None;
    }
    let parsed = uuid::Uuid::parse_str(&claim).ok()?;
    if parsed.get_version() != Some(uuid::Version::Random) || parsed.to_string() != claim {
        return None;
    }
    Some((pid, claim))
}

#[cfg(unix)]
pub(super) fn current_supervisor_claim() -> Option<String> {
    let claim = std::env::var(SUPERVISOR_CLAIM_ENV).ok()?;
    let (pid, published_claim) = read_supervisor_marker_claim()?;
    (pid == std::process::id() && claim == published_claim).then_some(claim)
}
