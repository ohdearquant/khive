//! khived daemon server — persistent warm runtime over a Unix socket.
//!
//! The daemon binds `~/.khive/khived.sock`, accepts length-prefixed request
//! frames, dispatches them through a [`DaemonDispatch`] implementor, and serves
//! results back. It is transport-agnostic: the MCP crate provides the dispatch
//! impl, but any future client (CLI, HTTP gateway) can reuse this server.
//!
//! The client side (forwarding, auto-spawn) lives in the transport crate
//! (e.g. `khive-mcp`), not here.

use std::sync::Arc;

mod store_guard;
mod store_identity;
#[cfg(unix)]
use store_guard::ensure_claimed_parent_identity;
#[cfg(unix)]
pub use store_guard::{acquire_daemon_store_guards, bind_daemon_store_files, claim_stores};
pub use store_guard::{assert_daemon_store_identities, DaemonStoreGuard};
#[cfg(unix)]
pub use store_identity::claimed_daemon_store_identity;
mod supervisor_marker;
#[cfg(unix)]
pub use supervisor_marker::supervisor_marker_path;

#[cfg(unix)]
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use libc;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use crate::pack::RequestIdentity;
#[cfg(unix)]
use khive_db::{run_checkpoint_task, CheckpointConfig, CheckpointLifecycleOwner, ConnectionPool};

mod load_limits;
#[cfg(unix)]
use load_limits::{admit_or_refuse_busy, ConnectionAdmission};
pub use load_limits::{
    recall_ledger_snapshot, track_recall_ledger_task, ConnectionCapSnapshot, RecallLedgerSnapshot,
};

/// Maximum frame size accepted in either direction.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Wire protocol version for the daemon IPC framing.
///
/// Increment this constant whenever the request or response frame shape
/// changes in a backward-incompatible way. The client sends its version
/// in every request; the daemon rejects mismatches with an explicit error
/// that names both sides so the operator knows exactly what to do
/// (`make local` rebuilds the client binary).
/// See `docs/api/daemon.md#protocol_version` for the version-by-version history.
pub const PROTOCOL_VERSION: u32 = 8;

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
struct DaemonLifecycle {
    options: DaemonOptions,
    state: std::sync::Mutex<DaemonLifecycleState>,
    /// Admission for new connections on the daemon socket.
    connections: ConnectionAdmission,
}

#[cfg(unix)]
struct DaemonLifecycleState {
    snapshot: DaemonLifecycleSnapshot,
    last_request_completion: Option<tokio::time::Instant>,
}

#[cfg(unix)]
impl DaemonLifecycle {
    fn new(options: DaemonOptions, report: DaemonStartupReport) -> Self {
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

    fn snapshot(&self) -> DaemonLifecycleSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    fn ready(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_request_completion = Some(tokio::time::Instant::now());
    }

    fn admit(self: &Arc<Self>) -> Option<OrdinaryRequestGuard> {
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
    fn try_idle(&self, blockers: impl FnOnce() -> Vec<String>) -> bool {
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

    fn draining(&self, reason: DaemonShutdownReason) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.phase != DaemonLifecyclePhase::Stopped {
            state.snapshot.phase = DaemonLifecyclePhase::Draining;
            state.snapshot.shutdown_reason = Some(reason);
        }
    }

    fn stopped(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .phase = DaemonLifecyclePhase::Stopped;
    }
}

#[cfg(unix)]
struct OrdinaryRequestGuard(Arc<DaemonLifecycle>);

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

/// Internal signal carried in a dispatch result until the daemon moves it to
/// response-frame metadata. It must never be sent in `result`: older v8
/// clients publish that string without inspecting its contents.
#[doc(hidden)]
pub const DAEMON_LEXICAL_TIMEOUT_MARKER: &str = "__khive_daemon_lexical_timeout";

const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 10;
/// An accepted local socket must finish its first frame within this window.
/// Dispatch deadlines start only after decoding, so they cannot reap peers
/// that connect and then stop sending request bytes.
#[cfg(unix)]
const INITIAL_FRAME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
fn next_accept_error_backoff(previous: Option<std::time::Duration>) -> std::time::Duration {
    previous
        .map(|delay| delay.saturating_mul(2))
        .unwrap_or_else(|| std::time::Duration::from_millis(10))
        .min(std::time::Duration::from_secs(1))
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
fn khive_dir() -> PathBuf {
    khive_root_from(
        std::env::var("HOME").ok(),
        std::env::var("USERPROFILE").ok(),
    )
}

/// Env-free core of [`khive_dir`], split out so the fallback chain is
/// testable without mutating process-global environment variables.
fn khive_root_from(home: Option<String>, userprofile: Option<String>) -> PathBuf {
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
const SOCKET_PATH_ENV: &str = "KHIVE_SOCKET";

/// Env var overriding the PID-file half of the daemon rendezvous.
#[cfg(unix)]
const PID_PATH_ENV: &str = "KHIVE_PID";

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
fn default_socket_path() -> PathBuf {
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
fn ensure_rendezvous_overrides_paired() -> anyhow::Result<()> {
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
fn read_supervisor_marker_claim() -> Option<(u32, String)> {
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
fn current_supervisor_claim() -> Option<String> {
    let claim = std::env::var(SUPERVISOR_CLAIM_ENV).ok()?;
    let (pid, published_claim) = read_supervisor_marker_claim()?;
    (pid == std::process::id() && claim == published_claim).then_some(claim)
}

#[cfg(unix)]
fn open_lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

#[cfg(unix)]
fn acquire_flock_blocking(path: &std::path::Path, label: &str) -> Option<std::fs::File> {
    let file = match open_lock_file(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = ?path, "cannot open {label} lock file");
            return None;
        }
    };
    // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        tracing::warn!("flock LOCK_EX failed on {label} lock");
        return None;
    }
    Some(file)
}

/// Acquire an exclusive advisory flock on the recovery/startup lock file.
///
/// The returned `File` holds the lock for its lifetime; dropping it releases
/// it.  Used by both the client (serializing kill+spawn) and the daemon server
/// (serializing cleanup+bind+pid-write) so the two critical sections are
/// mutually exclusive across processes.
#[cfg(unix)]
pub fn acquire_recovery_lock() -> Option<std::fs::File> {
    acquire_flock_blocking(&lock_path(), "recovery")
}

/// Attempt to acquire an exclusive advisory flock on `path`, retrying with a
/// non-blocking `flock(LOCK_NB)` until `deadline` elapses. Bounded alternative
/// to `acquire_recovery_lock`/`acquire_daemon_boot_guard`'s unbounded blocking
/// flock — see `docs/api/daemon.md#try_acquire_flock_until` for why a caller
/// merely detecting lock freedom needs a deadline instead.
///
/// - `Ok(Some(file))` — the lock was free within the deadline.
/// - `Ok(None)` — `deadline` elapsed while the lock stayed held; an explicit
///   "could not confirm" outcome, distinct from a hard I/O error.
/// - `Err(_)` — the lock file could not be opened, or `flock` failed for a
///   reason other than contention.
///
/// Blocking (paces retries with `std::thread::sleep`) — async callers must
/// run this via `spawn_blocking`.
#[cfg(unix)]
fn try_acquire_flock_until(
    path: &std::path::Path,
    deadline: std::time::Instant,
) -> std::io::Result<Option<std::fs::File>> {
    let file = open_lock_file(path)?;
    let poll_interval = std::time::Duration::from_millis(10);
    loop {
        // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(file));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(poll_interval.min(deadline - now));
    }
}

/// Bounded, deadline-aware variant of [`acquire_daemon_boot_guard`]: attempts
/// the SAME boot/recovery lock ([`lock_path`]) but gives up at `deadline`
/// instead of blocking forever. For callers that need to detect "is a boot in
/// progress right now" without risking an unbounded wait behind a wedged
/// holder: e.g. khive-mcp's `confirm_genuinely_dead` re-probing rounds,
/// where `DEAD_CONFIRM_ROUNDS` must bound elapsed time, not just probe count.
#[cfg(unix)]
pub fn try_acquire_daemon_boot_guard_until(
    deadline: std::time::Instant,
) -> std::io::Result<Option<DaemonBootGuard>> {
    try_acquire_flock_until(&lock_path(), deadline)
}

/// Bounded, deadline-aware acquisition of the recoverer-only lock
/// ([`recoverer_lock_path`]). See [`try_acquire_daemon_boot_guard_until`] for
/// the shared rationale — a second recoverer waiting for a peer's dead
/// confirmation/kill/spawn critical section must give up and report
/// "uncertain" rather than block forever if that peer is itself wedged.
#[cfg(unix)]
pub fn try_acquire_recoverer_lock_until(
    deadline: std::time::Instant,
) -> std::io::Result<Option<std::fs::File>> {
    try_acquire_flock_until(&recoverer_lock_path(), deadline)
}

/// Guard returned by [`acquire_daemon_boot_guard`], held across cold-boot
/// schema initialization (migrations + pack schema plans / FTS DDL) through
/// daemon bind + pid-write.
#[cfg(unix)]
pub type DaemonBootGuard = std::fs::File;

/// Acquire the recovery/boot lock, treating failure as fatal.
///
/// Unlike [`acquire_recovery_lock`] (best-effort, `None` on failure: used by
/// shutdown cleanup, where skipping unlink is safer than blocking forever),
/// daemon-mode boot must hold this lock across migrations/FTS DDL through
/// bind+pid-write. Silently continuing with no lock reopens the cold-boot FTS
/// race this guard exists to close, so callers that are about to run
/// daemon-mode boot (or wait for one to quiesce) must fail loudly instead of
/// proceeding unguarded.
#[cfg(unix)]
pub fn acquire_daemon_boot_guard() -> anyhow::Result<DaemonBootGuard> {
    acquire_recovery_lock()
        .ok_or_else(|| anyhow::anyhow!("failed to acquire daemon boot/recovery lock"))
}

/// Identity of a bound Unix socket path, used to tell "the socket I bound" apart
/// from "a same-path socket some other daemon bound after mine was removed".
///
/// A socket path can be recreated by a different process between the time
/// this daemon captures its identity and the time it later checks it, so `dev`
/// and `ino` (not the path) are what must match for cleanup to be safe.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn socket_identity(path: &std::path::Path) -> Option<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(SocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

// ── connection principal ──────────────────────────────────────────────────────

/// The uid on the other end of an accepted connection, read from the kernel.
///
/// This is the only identity on this socket the caller cannot choose. Every
/// identity field on the request frame — `namespace`, `actor_id`,
/// `visible_namespaces`, `config_id` — is supplied by the connecting process,
/// so none of them can answer "who is this". A check reading self-asserted
/// fields is not a weak gate, it is not a gate: anyone who wants to pass it
/// asserts the passing values.
///
/// `getpeereid(2)` on macOS/BSD, `SO_PEERCRED` on Linux. Both report the peer's
/// credentials as recorded by the kernel at connect time.
#[cfg(unix)]
pub(crate) fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();

    #[cfg(any(target_os = "macos", target_os = "ios", target_vendor = "apple"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is a live connected socket owned by `stream` for the
        // duration of this call; both out-params are valid initialized locals.
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid as u32)
    }

    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `fd` is a live connected socket owned by `stream`; `cred` is
        // an initialized local of exactly `len` bytes, which is what
        // SO_PEERCRED writes.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_vendor = "apple"
    )))]
    {
        let _ = fd;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "peer-credential capture is not implemented for this platform",
        ))
    }
}

/// Whether a connection from `uid` may be served by this daemon.
///
/// ADR-096 accepted per-request identity threading **for the single-principal
/// owner-only socket only**, and rested that on three things: the `0600`
/// socket, all connections being the same uid, and the database being already
/// same-uid-accessible. The socket mode is asserted at bind. This asserts the
/// second, which previously had no representation in the code at all — nothing
/// read peer identity, so nothing could notice when it stopped being true.
///
/// **Principal is not attribution.** Many `actor_id`s over one socket is
/// exactly what ADR-096 shipped and what every seat on a normal host does;
/// refusing a second distinct actor would break the accepted design. The
/// principal is the uid, and this refuses only a genuinely foreign one.
///
/// **There is deliberately no configuration escape hatch.** A flag permitting
/// other uids would not weaken this assertion, it would delete it, in the way
/// hardest to notice later: the check still exists, its tests still pass, and
/// the deployment that matters has it off. A deployment that genuinely needs
/// multiple uids needs a code change and a gated ADR — which is precisely the
/// decision that should be impossible to make by accident.
#[cfg(unix)]
pub(crate) fn uid_is_permitted(peer: u32, daemon_euid: u32) -> bool {
    peer == daemon_euid
}

// ── wire types ────────────────────────────────────────────────────────────────

mod config_id;
#[cfg(test)]
use config_id::parse_config_id;
pub use config_id::{
    config_id_extra_embedder_exclusions, config_ids_compatible, first_config_mismatch_field,
};

/// Request frame sent from a client to the daemon.
#[derive(Serialize, Deserialize, Default)]
pub struct DaemonRequestFrame {
    pub ops: String,
    /// Parse and inspect the catalog without dispatch, identity, or storage access.
    #[serde(default)]
    pub plan: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_per_op: Option<Vec<Option<String>>>,
    /// The client's resolved storage/gate default namespace for this request.
    ///
    /// As of protocol version 3 (ADR-096) the daemon serves the request under
    /// this namespace instead of rejecting on mismatch: a per-request
    /// identity input, not a same-process-identity assertion.
    pub namespace: String,
    /// The client's resolved write-stamp / gate actor identity (ADR-057),
    /// carried on the frame so the warm daemon stamps writes with the
    /// *caller's* actor instead of its own baked `actor_id` (ADR-096). `None`
    /// mints `ActorRef::anonymous()`, matching an unconfigured actor.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// Opaque process provenance resolved in the originating client process.
    /// It is carried per request because a shared warm daemon's environment
    /// does not identify the worker that submitted the operation. Protocol v4
    /// makes this field part of dispatch semantics: a v3 daemon must reject the
    /// request rather than execute it while silently discarding provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_ref: Option<String>,
    /// The client's resolved extra read-visibility namespaces (ADR-007 Rule
    /// 3b), carried on the frame so the warm daemon widens read scope to
    /// match the caller's own configuration rather than its own baked
    /// `visible_namespaces` (ADR-096). A non-`local` `actor_id` joins default
    /// reads where the registry mints the token (ADR-007 Rev 4 Rule 3b), so
    /// an empty list still includes that actor in default reads. Explicit
    /// `namespace=` operations remain scoped to exactly that namespace.
    #[serde(default)]
    pub visible_namespaces: Vec<String>,
    /// Fingerprint of the client's engine-coherence config: packs, db target,
    /// embedders, backend routing, and construction-baked outbound policy.
    /// Identity fields are carried separately in this frame. The daemon rejects
    /// requests whose configuration differs, except when its extra-embedder set
    /// is a superset of the client's and every other field matches. See
    /// ADR-027 / ADR-049 / ADR-096.
    #[serde(default)]
    pub config_id: String,
    /// IPC protocol version sent by the client. Pre-versioning clients omit
    /// this field (deserializes to 0). The daemon compares against
    /// [`PROTOCOL_VERSION`] and rejects mismatches with an explicit error.
    #[serde(default)]
    pub protocol_version: u32,
    /// When `true`, the daemon returns an identity frame (ok=true, result=None)
    /// immediately after identity validation — without calling the dispatcher.
    /// Used by the client's under-lock recovery probe to confirm a daemon is
    /// alive and identity-matching without dispatching any mutating verb.
    /// Pre-probe clients omit this field (deserializes to false → normal dispatch).
    #[serde(default)]
    pub probe_only: bool,
    /// When `true`, the daemon returns a point-in-time [`MetricsSnapshot`] of
    /// its server-side gauges (a read-only measurement surface for the
    /// load/perf harness) instead of dispatching any op. Handled before the
    /// `config_id` equality reject: a gauge read is process-global and
    /// namespace/config-agnostic, not a namespaced record operation.
    /// READ-ONLY — this field is the only input the frame accepts for a
    /// metrics request; there is no reset or mutation reachable over the
    /// wire. Pre-metrics clients omit this field (deserializes to `false` →
    /// normal dispatch, unaffected).
    #[serde(default)]
    pub metrics_only: bool,
    /// Output format for this request (ADR-078). Forwarded to the daemon's
    /// serialization seam. `None` means use the daemon's resolved default.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Per-operation output format overrides (ADR-078).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_per_op: Option<Vec<Option<String>>>,
    /// Whether this request originated from the agent-facing MCP `request`
    /// tool (the wire surface). When `true`, the daemon rejects
    /// `Visibility::Subhandler` verbs: agents must not invoke internal
    /// subhandlers. When `false` (the default, and the only value any
    /// operator path sends), subhandlers are allowed: `kkernel exec` and
    /// other in-process callers are trusted operator surfaces.
    ///
    /// This is the origin discriminator, not a daemon-vs-local one: operator
    /// requests flow through the daemon by default too, so the gate cannot
    /// key on transport.
    #[serde(default)]
    pub from_wire: bool,
    /// Request-group correlation id (khive#948), echoed back unchanged on
    /// [`DaemonResponseFrame::request_id`] and stamped into the dispatch's
    /// audit event (`resource.request_id`) so a benchmark harness can join
    /// its own pre-send sample to the server-side audit row for the same
    /// request. Agent-facing MCP requests always carry one: the bridge keeps a
    /// caller-supplied value or mints an opaque nonzero value when absent.
    /// Operator-built/probe frames may still use `None`. Purely additive —
    /// `#[serde(default)]` matches `metrics_only`/`format`/`format_per_op`
    /// precedent, with no `PROTOCOL_VERSION` bump.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
}

/// A dispatch failure whose domain outcome remains available to the transport.
#[derive(Debug, Clone)]
pub struct DaemonDispatchError {
    pub message: String,
    pub error_detail: serde_json::Value,
}

/// Per-field container limit, asserted equal to the request parser's bound by MCP.
pub const ERROR_DETAIL_NESTING_DEPTH_LIMIT: usize = 64;

fn error_detail_value_within_limit(value: &serde_json::Value) -> bool {
    let mut pending = vec![(value, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        match value {
            serde_json::Value::Array(items) if depth < ERROR_DETAIL_NESTING_DEPTH_LIMIT => {
                pending.extend(items.iter().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Object(fields) if depth < ERROR_DETAIL_NESTING_DEPTH_LIMIT => {
                pending.extend(fields.values().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => return false,
            _ => {}
        }
    }
    true
}

fn drop_error_detail_iteratively(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(fields) => pending.extend(fields.into_values()),
            _ => {}
        }
    }
}

impl DaemonDispatchError {
    /// Missing or unrecognized disposition from a legacy implementation is unknown.
    pub fn new(message: impl Into<String>, error_detail: Option<serde_json::Value>) -> Self {
        let message = message.into();
        let mut fields = match error_detail {
            Some(serde_json::Value::Object(fields)) => fields,
            Some(data) => serde_json::Map::from_iter([("data".to_string(), data)]),
            None => serde_json::Map::new(),
        };
        let disposition = match fields
            .get("domain_disposition")
            .and_then(serde_json::Value::as_str)
        {
            Some("committed") => crate::DomainDisposition::Committed,
            Some("not_committed") => crate::DomainDisposition::NotCommitted,
            _ => crate::DomainDisposition::Unknown,
        };
        if disposition != crate::DomainDisposition::Committed {
            if let Some(result) = fields.remove("domain_result") {
                drop_error_detail_iteratively(result);
            }
        }
        let rejected: Vec<String> = fields
            .iter()
            .filter(|(_, value)| !error_detail_value_within_limit(value))
            .map(|(name, _)| name.clone())
            .collect();
        let omitted_result = rejected.iter().any(|name| name == "domain_result");
        let omitted_detail = !rejected.is_empty();
        for name in rejected {
            if let Some(value) = fields.remove(&name) {
                drop_error_detail_iteratively(value);
            }
        }
        let mut error_detail = serde_json::Value::Object(fields);
        if error_detail["kind"].as_str().is_none() {
            error_detail["kind"] = serde_json::json!("internal");
        }
        if error_detail["message"].as_str().is_none() {
            error_detail["message"] = serde_json::json!(message);
        }
        error_detail["domain_disposition"] = serde_json::json!(disposition.as_str());
        if omitted_detail {
            error_detail["code"] = serde_json::json!(if omitted_result {
                "result_too_deep"
            } else {
                "error_detail_too_deep"
            });
        }
        Self {
            message,
            error_detail,
        }
    }
}

/// Response frame sent from the daemon back to a client.
#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonResponseFrame {
    pub ok: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Additive failure metadata; legacy protocol-v4 peers still read `error` as text.
    /// On a successful response, `{"lexical_timeout":true}` is a daemon-only
    /// diagnostic that old clients ignore and new clients log locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<serde_json::Value>,
    pub namespace_mismatch: bool,
    /// Set when the request's `config_id` does not match the daemon's. Like
    /// `namespace_mismatch`, this signals the client to fall back to local
    /// dispatch rather than execute under a different runtime/config.
    #[serde(default)]
    pub config_mismatch: bool,
    /// The `config_id` the daemon dispatched under, echoed back so the client
    /// can positively confirm the result came from a matching runtime. A
    /// pre-`config_id` daemon omits this field (deserializes to `None`), which
    /// the client treats as a mismatch and falls back to local dispatch — this
    /// closes the upgrade window where a new restricted client could otherwise
    /// trust a still-warm legacy daemon's broader registry.
    #[serde(default)]
    pub served_config_id: Option<String>,
    /// Set when the client's `protocol_version` does not match the daemon's
    /// [`PROTOCOL_VERSION`]. The client must treat this as a hard error and
    /// surface the human-readable `error` field rather than falling back to
    /// local dispatch (which would hide the version skew).
    #[serde(default)]
    pub version_mismatch: bool,
    /// The daemon's [`PROTOCOL_VERSION`], echoed in error responses so the
    /// client can include both sides in the diagnostic message. Pre-versioning
    /// daemons omit this field (deserializes to 0).
    #[serde(default)]
    pub daemon_protocol_version: u32,
    /// Populated when the request set `metrics_only: true`: a point-in-time
    /// snapshot of the daemon's server-side gauges. `None` on every other
    /// response, and on any response from a daemon that predates this field
    /// (client-side back-compat via `#[serde(default)]`, matching
    /// `served_config_id`'s upgrade-window handling above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsSnapshot>,
    /// Echo of the request's `request_id` (khive#948), present whenever the
    /// frame that produced this response carried one — including on every
    /// error/denied arm, not only success, so a client can join a failure
    /// the same way it joins a success. `#[serde(default)]` so an older
    /// daemon's response (predating this field) deserializes to `None`
    /// rather than a parse error.
    #[serde(default)]
    pub request_id: Option<u64>,
}

/// Move the private dispatch signal out of the result before any client can
/// observe it. Unmarked results retain their exact bytes. The marked result
/// was serialized from a JSON Value by the MCP server, so reserializing after
/// removal reproduces its public envelope. The marker's escaped frame cost
/// equals `error_detail:{"lexical_timeout":true}`, keeping the server's exact
/// frame-fit calculation valid after this move.
#[cfg(unix)]
fn take_daemon_lexical_timeout_marker(raw: String) -> (String, Option<serde_json::Value>) {
    if !raw.contains(DAEMON_LEXICAL_TIMEOUT_MARKER) {
        return (raw, None);
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (raw, None);
    };
    let Some(fields) = value.as_object_mut() else {
        return (raw, None);
    };
    if !fields
        .get("results")
        .is_some_and(serde_json::Value::is_array)
    {
        return (raw, None);
    }
    let Some(marker) = fields.remove(DAEMON_LEXICAL_TIMEOUT_MARKER) else {
        return (raw, None);
    };
    let detail =
        (marker.as_bool() == Some(true)).then(|| serde_json::json!({"lexical_timeout": true}));
    (
        serde_json::to_string(&value).expect("serde_json::Value is serializable"),
        detail,
    )
}

/// One checkpoint store in this daemon's fixed topology. IDs are process-local:
/// `main` or `secondary:<index>` in dispatcher order. The basename is display-only,
/// not an identity; no directory path is exposed. Restart/topology changes reset
/// the interpretation of interval deltas.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct CheckpointStoreMetrics {
    pub store_id: String,
    pub role: String,
    pub database: Option<String>,
    #[serde(flatten)]
    pub timing: khive_db::checkpoint::CheckpointTiming,
}

/// Point-in-time snapshot of the daemon's server-side gauges — the
/// load/perf harness read-surface (measurement substrate, not a product feature).
///
/// Every field here is a **server-side** gauge reachable from `handle_conn`
/// without any mutation: [`khive_storage::tx_registry`] (ADR-091 Plank 0,
/// process-global singleton), the main pool's backend-keyed routine WAL
/// sample and process TRUNCATE counters (`khive_db::checkpoint`), and the
/// ADR-067 Component A write queue depth/latest writer-stage sample. There is
/// no reset reachable through this type or through [`DaemonRequestFrame`] —
/// gauges out, nothing in.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    /// Launch mode and the current lifecycle of this daemon incarnation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<DaemonLifecycleSnapshot>,
    /// Last-observed WAL page count from the periodic checkpoint tick.
    /// `None` when the checkpoint task has never ticked in this process
    /// (for example, an in-memory dispatcher with no pool, or a daemon that
    /// just started and hasn't hit its first tick yet).
    pub wal_pages: Option<u64>,
    /// Logical frames present in the WAL at the main backend's most recent
    /// periodic PASSIVE checkpoint. Kept separate from physical allocation.
    #[serde(default)]
    pub wal_log_frames: Option<u64>,
    /// Frames backfilled by that same periodic PASSIVE pass.
    #[serde(default)]
    pub wal_checkpointed_frames: Option<u64>,
    /// Logical frames still pending after that pass (`log - checkpointed`).
    #[serde(default)]
    pub wal_pending_frames: Option<u64>,
    /// Physical `-wal` sidecar bytes captured at the same routine tick.
    #[serde(default)]
    pub wal_physical_bytes: Option<u64>,
    /// Wall-clock timestamp of the routine WAL sample, for staleness checks.
    #[serde(default)]
    pub wal_observed_at_unix_ms: Option<u64>,
    /// Cumulative actual routine PASSIVE calls for each store this daemon
    /// checkpoints. Microseconds preserve sub-millisecond costs; max is since
    /// process start, not an interval maximum. Counters saturate rather than wrap.
    #[serde(default)]
    pub wal_checkpoint_stores: Vec<CheckpointStoreMetrics>,
    /// Total WAL TRUNCATE escalation attempts (ADR-091 Plank 2) made in this
    /// process's lifetime, regardless of whether they succeeded in reclaiming
    /// pages.
    pub wal_truncate_attempts: u64,
    /// Current consecutive-failure count for TRUNCATE attempts that failed to
    /// bring the WAL back below `warn_pages`; resets to 0 the next time an
    /// attempt clears it.
    pub wal_truncate_consecutive_failures: u64,
    /// Total checkpoint ticks skipped because the dedicated checkpoint
    /// connection was unavailable (ADR-091 checkpoint-pressure telemetry),
    /// across this process's lifetime. `#[serde(default)]` so an older client
    /// decoding a newer daemon's snapshot (or vice versa) does not fail.
    #[serde(default)]
    pub wal_checkpoint_skipped_ticks: u64,
    /// Current consecutive-skip run length; 0 once the next tick is observed.
    #[serde(default)]
    pub wal_checkpoint_consecutive_skips: u64,
    /// WAL page count last known at the time of the most recent skip, if any
    /// skip has occurred yet in this process.
    #[serde(default)]
    pub wal_checkpoint_last_skip_wal_pages: Option<u64>,
    /// Age, in microseconds, of the oldest currently-open transaction
    /// registry entry (ADR-091 Plank 0). `None` when no transaction is
    /// currently open.
    pub oldest_pinned_tx_micros: Option<u64>,
    /// Diagnostic label of the oldest currently-open transaction registry
    /// entry, if any and if it was registered with one.
    pub oldest_pinned_tx_label: Option<String>,
    /// Number of currently open transaction registry entries.
    pub open_tx_count: usize,
    /// Current write-queue backlog depth (ADR-067 Component A): requests
    /// enqueued but not yet accepted by the `WriterTask` drain loop. `None`
    /// unless the write queue is enabled (`KHIVE_WRITE_QUEUE=1`) and a
    /// file-backed pool is available.
    pub write_queue_depth: Option<usize>,
    /// The write queue's configured bounded capacity
    /// (`PoolConfig::write_queue_capacity`), gated the same as
    /// `write_queue_depth`.
    pub write_queue_capacity: Option<usize>,
    /// Latest completed writer-task span: bounded-channel admission/backlog.
    #[serde(default)]
    pub write_last_queue_wait_micros: Option<u64>,
    /// Latest completed writer-task span: `BEGIN IMMEDIATE` acquisition.
    #[serde(default)]
    pub write_last_transaction_acquire_micros: Option<u64>,
    /// Latest completed writer-task span: application transaction body.
    #[serde(default)]
    pub write_last_body_micros: Option<u64>,
    /// Latest completed writer-task span: SQLite COMMIT/fsync phase.
    #[serde(default)]
    pub write_last_commit_micros: Option<u64>,
    /// Whole latest writer-task request span, retained for compatibility and
    /// comparison with the decomposed stages.
    #[serde(default)]
    pub write_last_total_micros: Option<u64>,
    /// Wall-clock timestamp of the writer-stage sample.
    #[serde(default)]
    pub write_last_observed_at_unix_ms: Option<u64>,
    /// Connection cap of the daemon socket this snapshot was served from.
    /// `None` from a daemon that predates the cap and when no listener owns
    /// the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<ConnectionCapSnapshot>,
    /// Bounds and counts of the best-effort recall serve-ledger tasks.
    /// `None` from a daemon that predates the bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_ledger: Option<RecallLedgerSnapshot>,
}

// ── framing ───────────────────────────────────────────────────────────────────

/// Read one length-prefixed frame (4-byte BE u32 length + JSON bytes).
#[cfg(unix)]
pub async fn read_frame<R>(stream: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("daemon frame of {len} bytes exceeds {MAX_FRAME_BYTES} cap"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(unix)]
fn initial_frame_timeout_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "daemon initial request frame read timed out",
    )
}

#[cfg(unix)]
async fn read_initial_frame<R>(
    stream: &mut R,
    deadline: tokio::time::Instant,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Tokio polls the inner future before checking its timer. A frame already
    // buffered when a delayed connection task first runs would otherwise pass
    // even though its acceptance-time deadline has expired.
    if tokio::time::Instant::now() >= deadline {
        return Err(initial_frame_timeout_error());
    }
    let raw = tokio::time::timeout_at(deadline, read_frame(stream))
        .await
        .map_err(|_| initial_frame_timeout_error())??;
    if tokio::time::Instant::now() >= deadline {
        return Err(initial_frame_timeout_error());
    }
    Ok(raw)
}

/// Write one length-prefixed frame.
#[cfg(unix)]
pub async fn write_frame<W>(stream: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon frame of {} bytes exceeds {MAX_FRAME_BYTES} cap",
                payload.len()
            ),
        ));
    }
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

// ── dispatch trait ────────────────────────────────────────────────────────────

/// Transport-agnostic dispatch interface for the daemon server.
///
/// The MCP crate implements this by dispatching through the shared request body
/// while honoring [`DaemonRequestFrame::from_wire`] (so subhandler visibility is
/// gated by request origin, not by transport); any future transport can do the
/// same.
#[cfg(unix)]
#[async_trait]
pub trait DaemonDispatch: Clone + Send + Sync + 'static {
    /// Named retained resources or unknown inventory that prevents idle exit.
    /// An implementor must explicitly account for its resources before retiring.
    fn idle_retirement_blockers(&self) -> Vec<String> {
        vec!["dispatcher_resource_inventory_unknown".to_owned()]
    }

    /// Describe syntax and loaded catalog membership without dispatching.
    fn plan(&self, ops: &str) -> String;

    /// Dispatch a verb-DSL request string and return the rendered result.
    ///
    /// `from_wire` carries the origin discriminator from
    /// [`DaemonRequestFrame::from_wire`]: when `true`, the implementor enforces
    /// verb visibility (rejects `Visibility::Subhandler` verbs); when `false`,
    /// the request is from a trusted operator surface and subhandlers pass.
    ///
    /// `identity` is the per-request identity context threaded from the frame
    /// (ADR-096): `Some(..)` when serving a request forwarded over the
    /// daemon socket (built from `frame.namespace` / `frame.actor_id` /
    /// `frame.visible_namespaces` by the connection handler), `None` for any
    /// other dispatch path. Implementors should mint the storage/gate token from
    /// `identity` when present and fall back to their own construction-baked
    /// identity when absent, so pure local (non-daemon) dispatch is unchanged.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, String>;

    /// Read-deadline ceiling for one request. The default is the operator
    /// ceiling; an implementor that understands the request may grant a
    /// longer bounded allowance (a long poll's declared wait plus a margin).
    fn request_read_timeout(&self, _ops: &str) -> std::time::Duration {
        khive_storage::request_read_timeout_from_env()
    }

    /// Preserve structured dispatch errors without breaking string-only implementors.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, DaemonDispatchError> {
        self.dispatch(
            ops,
            presentation,
            presentation_per_op,
            format,
            format_per_op,
            from_wire,
            identity,
        )
        .await
        .map_err(|message| DaemonDispatchError::new(message, None))
    }

    /// Warm every pack's in-memory state (ANN indexes, etc.).
    async fn warm_all(&self);

    /// The namespace this dispatcher was configured for.
    fn namespace(&self) -> &str;

    /// Fingerprint of this dispatcher's resolved runtime config (packs, db
    /// target, embedders). Used to reject forwarded requests from clients whose
    /// config differs, so a restricted client cannot dispatch through a broader
    /// daemon.
    fn config_id(&self) -> &str;

    /// Return the pool to use for background WAL checkpointing, if available.
    ///
    /// Implementors backed by a file-based SQLite database should return
    /// `Some(pool_arc)`. In-memory or test dispatchers that have no pool
    /// return `None` and the checkpoint task is not spawned.
    ///
    /// The default implementation returns `None`.
    fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
        None
    }

    /// File-backed backend pools beyond [`Self::pool_for_checkpoint`]'s pool
    /// (ADR-091 Amendment 3): one checkpoint task is spawned per entry here,
    /// in addition to the one spawned for the primary pool, so a
    /// multi-backend deployment gets PASSIVE/TRUNCATE checkpointing and
    /// sidecar enumeration on every file-backed backend it wired, not only
    /// the main one.
    ///
    /// The default implementation returns an empty `Vec` — an implementor
    /// with only one backend (or none) needs no override.
    fn secondary_pools_for_checkpoint(&self) -> Vec<Arc<ConnectionPool>> {
        Vec::new()
    }

    /// Return the audit `EventStore` the checkpoint task should append
    /// ADR-094 lifecycle events (`CheckpointOutcomeRecorded`) to, if any.
    ///
    /// Mirrors [`Self::pool_for_checkpoint`]'s default-`None` shape: an
    /// implementor with no configured event store (or no pool at all) simply
    /// gets a checkpoint task that never appends events — the checkpoint
    /// task itself remains fully functional either way.
    ///
    /// The default implementation returns `None`.
    fn event_store_for_checkpoint(&self) -> Option<Arc<dyn khive_storage::EventStore>> {
        None
    }
}

#[cfg(unix)]
struct CheckpointTaskSpec {
    pool: Arc<ConnectionPool>,
    lifecycle_owner: Option<CheckpointLifecycleOwner>,
    is_main: bool,
}

/// Build checkpoint-task fan-out and designate one lifecycle owner.
///
/// The main checkpoint task owns lifecycle emission when it exists. If the
/// main backend is in-memory and therefore has no checkpoint task, the first
/// file-backed secondary owns emission instead. All remaining tasks are
/// explicit non-owners.
#[cfg(unix)]
fn checkpoint_task_specs(
    main_pool: Option<Arc<ConnectionPool>>,
    secondary_pools: Vec<Arc<ConnectionPool>>,
    event_store: Option<Arc<dyn khive_storage::EventStore>>,
    namespace: String,
) -> Vec<CheckpointTaskSpec> {
    let mut tasks = Vec::with_capacity(usize::from(main_pool.is_some()) + secondary_pools.len());
    if let Some(pool) = main_pool {
        tasks.push(CheckpointTaskSpec {
            pool,
            lifecycle_owner: None,
            is_main: true,
        });
    }
    tasks.extend(secondary_pools.into_iter().map(|pool| CheckpointTaskSpec {
        pool,
        lifecycle_owner: None,
        is_main: false,
    }));

    if let (Some(task), Some(event_store)) = (tasks.first_mut(), event_store) {
        task.lifecycle_owner = Some(CheckpointLifecycleOwner::new(event_store, namespace));
    }
    tasks
}

// ── tracked background tasks ─────────────────────────────────────────────────
//
// Pack handlers (e.g. memory.recall's ADR-081 serve-ledger append) fire
// fire-and-forget `tokio::spawn`ed work off the response path so the caller
// never waits on a cross-pack dispatch or a SQL write. Left untracked, that
// work is invisible to `drain()`: a SIGTERM landing between the response
// returning and the spawned task completing can abort it mid-flight with no
// log and no row. `track_background_task` gives such spawns a process-wide
// presence that `drain()` waits on, exactly like the `active` counter does
// for in-flight connections: the caller still only pays for the spawn +
// counter increment, never the task's own work.
/// Set once by the boot path that takes the daemon role, and never cleared: a
/// process that is not the warm daemon has no path to becoming one except exec.
static WARM_INDEX_HOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declare this process the warm index host. Called by the serve path as soon as
/// the daemon role is decided, before any runtime is built, so nothing warms
/// under the wrong answer.
pub fn mark_warm_index_host() {
    WARM_INDEX_HOST.store(true, std::sync::atomic::Ordering::Release);
}

/// Whether this process is the warm index host.
///
/// Building an ANN index from the full corpus is minutes of CPU and hundreds of
/// megabytes of segment rewrite, and it pays for itself only across a process
/// that outlives the request. A short-lived client that does it pays the whole
/// cost, discards the result at exit, and publishes a checkpoint that every
/// other reader on the root must then re-read. Consumers use this to decide
/// whether to build or to serve degraded and let the daemon build.
pub fn is_warm_index_host() -> bool {
    WARM_INDEX_HOST.load(std::sync::atomic::Ordering::Acquire)
}

static BACKGROUND_TASKS: std::sync::OnceLock<Arc<std::sync::atomic::AtomicUsize>> =
    std::sync::OnceLock::new();

fn background_tasks() -> &'static Arc<std::sync::atomic::AtomicUsize> {
    BACKGROUND_TASKS.get_or_init(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
}

/// Decrements the shared background-task counter from `Drop`, so the count
/// comes back down whether the tracked future returns normally, panics, or
/// is cancelled — a plain post-`await` `fetch_sub` only covers the return
/// path and leaks the count forever on a panic, since unwinding skips every
/// statement after the panic point.
// ── outstanding background-task names ────────────────────────────────────────
//
// Names of the tasks the background counter is currently holding, so a drain
// timeout can say which ones held it open instead of printing a bare count.
// Registered and released at exactly the points the counter is incremented and
// decremented, and in the order that keeps the counter authoritative: the name
// goes in after the increment and comes out before the decrement, so a reported
// name always belongs to a task the counter already holds. The reverse ordering
// would let the warning name a task that had already finished, which is the one
// reading that would send someone looking in the wrong place.
static BACKGROUND_TASK_NAMES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
> = std::sync::OnceLock::new();

fn background_task_names_registry(
) -> &'static std::sync::Mutex<std::collections::HashMap<&'static str, usize>> {
    BACKGROUND_TASK_NAMES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Name recorded for tasks spawned through the unnamed entry points, which
/// stay on the public API of a published crate.
pub const UNNAMED_BACKGROUND_TASK: &str = "unnamed";

fn register_background_task_name(name: &'static str) {
    let mut names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *names.entry(name).or_insert(0) += 1;
}

fn release_background_task_name(name: &'static str) {
    let mut names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(count) = names.get_mut(name) {
        *count -= 1;
        if *count == 0 {
            names.remove(name);
        }
    }
}

/// Names of the in-flight tracked background tasks, sorted and deduplicated.
/// A diagnostic beside [`background_task_count`], never a substitute for it:
/// the count is what drain waits on.
pub fn background_task_names() -> Vec<String> {
    let names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out: Vec<String> = names.keys().map(|name| (*name).to_string()).collect();
    out.sort();
    out
}

#[cfg(unix)]
fn idle_retirement_blockers<D: DaemonDispatch>(dispatcher: &D) -> Vec<String> {
    let mut blockers = dispatcher.idle_retirement_blockers();
    if !khive_storage::tx_registry::snapshot().is_empty() {
        blockers.push("open_sql_transaction".to_owned());
    }
    blockers.extend(
        active_phase_names()
            .into_iter()
            .map(|name| format!("active_phase:{name}")),
    );
    let count = background_task_count();
    let names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if names.values().sum::<usize>() != count {
        blockers.push("tracked_worker_inventory_unsettled".to_owned());
    }
    // Only these inspected loops maintain replaceable caches/checkpoint state.
    // Their tracked lifetime still participates in the final drain.
    for name in names.keys() {
        if !matches!(
            *name,
            "wal_checkpoint" | "memory_ann_rotation_watch" | "knowledge_ann_rotation_watch"
        ) {
            blockers.push(format!("unsettled_worker:{name}"));
        }
    }
    blockers.sort();
    blockers.dedup();
    blockers
}

#[cfg(unix)]
async fn wait_for_idle<D: DaemonDispatch>(dispatcher: &D, lifecycle: &DaemonLifecycle) {
    if lifecycle.options.lifetime == DaemonLifetime::Persistent {
        std::future::pending::<()>().await;
    }
    loop {
        if lifecycle.try_idle(|| idle_retirement_blockers(dispatcher)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

struct BackgroundTaskGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
    name: &'static str,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        release_background_task_name(self.name);
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Spawn a task that daemon shutdown's `drain()` waits for and return its join
/// handle. Retaining the handle lets boot coordinators form an explicit barrier;
/// dropping it deliberately detaches the task while the background counter still
/// keeps daemon drain aware of its lifetime. The decrement happens via
/// `BackgroundTaskGuard`'s `Drop`, including panic and cancellation paths.
pub fn spawn_tracked_task<F, T>(fut: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    spawn_named_tracked_task(UNNAMED_BACKGROUND_TASK, fut)
}

/// [`spawn_tracked_task`] with a name that a drain timeout can print.
///
/// The name is a short static string describing the task, never a formatted or
/// caller-supplied value: it is read by an operator staring at a shutdown that
/// would not finish, so it is a label for a call site, not a record of one
/// occurrence.
pub fn spawn_named_tracked_task<F, T>(name: &'static str, fut: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    background_tasks().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    register_background_task_name(name);
    let guard = BackgroundTaskGuard {
        counter: background_tasks().clone(),
        name,
    };
    tokio::spawn(async move {
        let _guard = guard;
        fut.await
    })
}

/// Spawn a fire-and-forget task through [`spawn_tracked_task`].
///
/// Callers that need a boot or shutdown barrier should retain and await the
/// returned handle from [`spawn_tracked_task`] instead of detaching it here.
pub fn track_background_task<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    track_named_background_task(UNNAMED_BACKGROUND_TASK, fut);
}

/// [`track_background_task`] with a name that a drain timeout can print. See
/// [`spawn_named_tracked_task`] for what belongs in the name.
pub fn track_named_background_task<F>(name: &'static str, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    drop(spawn_named_tracked_task(name, fut));
}

/// Current count of in-flight tasks started via [`track_background_task`].
/// Exposed for tests; `drain()` reads the shared counter directly.
pub fn background_task_count() -> usize {
    background_tasks().load(std::sync::atomic::Ordering::SeqCst)
}

/// Process-wide daemon shutdown signal (ADR-119).
///
/// Cancelled exactly once, when the daemon's unified shutdown future resolves
/// — before `drain()` begins waiting on tracked tasks — so long-running
/// daemon components supervised outside this module observe shutdown through
/// the same path the daemon itself does, rather than inventing their own.
/// Clones share the underlying token; child tokens derived from it are
/// cancelled transitively.
///
/// In non-daemon processes the token simply never fires.
pub fn daemon_shutdown_token() -> tokio_util::sync::CancellationToken {
    static TOKEN: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
        std::sync::OnceLock::new();
    TOKEN
        .get_or_init(tokio_util::sync::CancellationToken::new)
        .clone()
}

// ── active background phase names (ADR-103) ──────────────────────────────────
//
// A lightweight, best-effort process-wide gauge of which named background
// phases (e.g. `ann_warm`) are in flight right now, read by `comm.health`'s
// resource self-report so a caller can see "what is the daemon doing" at a
// glance without correlating timestamps across the event log itself. Counted
// per name rather than boolean, since more than one occurrence of the same
// named phase can legitimately overlap (e.g. two embedding models warming
// concurrently) — the name only drops out of the reported set once every
// concurrent occurrence has ended.
static ACTIVE_PHASES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

fn active_phases() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    ACTIVE_PHASES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// RAII guard for one occurrence of a named background phase. Increments the
/// phase's count on creation (see [`register_active_phase`]); decrements on
/// `Drop`, so the count comes back down whether the guarded work returns
/// normally, panics, or is cancelled — the same rationale as
/// `BackgroundTaskGuard` above.
pub struct PhaseGuard {
    name: String,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let mut map = active_phases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.name) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.name);
            }
        }
    }
}

/// Register one occurrence of a named background phase as currently active.
/// Returns a guard: drop it (or let it fall out of scope) when the phase
/// ends. Best-effort process-wide gauge only, read by `comm.health` — never
/// load-bearing for correctness.
pub fn register_active_phase(name: &str) -> PhaseGuard {
    let mut map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *map.entry(name.to_string()).or_insert(0) += 1;
    PhaseGuard {
        name: name.to_string(),
    }
}

/// Currently active background-phase names, sorted for deterministic output.
/// Empty when no tracked phase is in flight.
pub fn active_phase_names() -> Vec<String> {
    let map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

// ── server ────────────────────────────────────────────────────────────────────

/// Build a point-in-time [`MetricsSnapshot`] of this process's server-side
/// gauges. Called only from `handle_conn`'s `metrics_only` arm — a
/// process-global, read-only assembly with no side effects of its own.
/// See `docs/api/daemon.md#build_metrics_snapshot` for where each gauge is sourced
/// from and why.
#[cfg(unix)]
fn build_metrics_snapshot<D: DaemonDispatch>(dispatcher: &D) -> MetricsSnapshot {
    let open_tx_count = khive_storage::tx_registry::snapshot().len();
    // ADR-091 Amendment 3: deliberately the process-wide aggregate, not a
    // backend-attributed view — this gauge reports "oldest pinned tx in this
    // process" across every wired backend, not an attribution claim about
    // which database it belongs to. Attribution consumers (the session sweep,
    // the per-backend checkpoint tasks) use the scoped `oldest_for` views in
    // khive-db instead.
    let (oldest_pinned_tx_micros, oldest_pinned_tx_label) =
        match khive_storage::tx_registry::oldest() {
            Some((_id, age, label)) => (Some(age.as_micros() as u64), label),
            None => (None, None),
        };

    let checkpoint_pool = dispatcher.pool_for_checkpoint();
    let mut secondary_index = 0;
    let wal_checkpoint_stores = checkpoint_task_specs(
        checkpoint_pool.clone(),
        dispatcher.secondary_pools_for_checkpoint(),
        None,
        String::new(),
    )
    .into_iter()
    .map(|task| {
        let (store_id, role) = if task.is_main {
            ("main".to_string(), "main".to_string())
        } else {
            let store_id = format!("secondary:{secondary_index}");
            secondary_index += 1;
            (store_id, "secondary".to_string())
        };
        CheckpointStoreMetrics {
            store_id,
            role,
            database: task
                .pool
                .canonical_path()
                .and_then(std::path::Path::file_name)
                .map(|name| name.to_string_lossy().into_owned()),
            timing: khive_db::checkpoint::checkpoint_timing(&task.pool),
        }
    })
    .collect();
    let routine_wal = checkpoint_pool
        .as_deref()
        .and_then(khive_db::checkpoint::routine_wal_observation);
    let writer_stages = checkpoint_pool
        .as_deref()
        .and_then(khive_db::writer_task::last_writer_stage_observation);
    let (write_queue_depth, write_queue_capacity) = checkpoint_pool
        .as_ref()
        .and_then(|pool| pool.writer_task_handle().ok().flatten())
        .map(|handle| (Some(handle.queue_depth()), Some(handle.capacity())))
        .unwrap_or((None, None));

    MetricsSnapshot {
        lifecycle: None,
        wal_pages: routine_wal.as_ref().map(|sample| sample.log_frames),
        wal_log_frames: routine_wal.as_ref().map(|sample| sample.log_frames),
        wal_checkpointed_frames: routine_wal
            .as_ref()
            .map(|sample| sample.checkpointed_frames),
        wal_pending_frames: routine_wal.as_ref().map(|sample| sample.pending_frames),
        wal_physical_bytes: routine_wal
            .as_ref()
            .and_then(|sample| sample.physical_wal_bytes),
        wal_observed_at_unix_ms: routine_wal
            .as_ref()
            .map(|sample| sample.observed_at_unix_ms),
        wal_checkpoint_stores,
        wal_truncate_attempts: khive_db::checkpoint::truncate_attempts(),
        wal_truncate_consecutive_failures: khive_db::checkpoint::truncate_consecutive_failures(),
        wal_checkpoint_skipped_ticks: khive_db::checkpoint::checkpoint_skipped_ticks(),
        wal_checkpoint_consecutive_skips: khive_db::checkpoint::checkpoint_consecutive_skips(),
        wal_checkpoint_last_skip_wal_pages: khive_db::checkpoint::checkpoint_last_skip_wal_pages(),
        oldest_pinned_tx_micros,
        oldest_pinned_tx_label,
        open_tx_count,
        write_queue_depth,
        write_queue_capacity,
        write_last_queue_wait_micros: writer_stages
            .as_ref()
            .map(|sample| sample.queue_wait_micros),
        write_last_transaction_acquire_micros: writer_stages
            .as_ref()
            .map(|sample| sample.transaction_acquire_micros),
        write_last_body_micros: writer_stages.as_ref().map(|sample| sample.body_micros),
        write_last_commit_micros: writer_stages.as_ref().map(|sample| sample.commit_micros),
        write_last_total_micros: writer_stages.as_ref().map(|sample| sample.total_micros),
        write_last_observed_at_unix_ms: writer_stages
            .as_ref()
            .map(|sample| sample.observed_at_unix_ms),
        connections: None,
        recall_ledger: Some(recall_ledger_snapshot()),
    }
}

#[cfg(unix)]
async fn write_response_frame<W>(stream: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(INITIAL_FRAME_READ_TIMEOUT, write_frame(stream, payload))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon response write timed out",
            )
        })?
}

#[cfg(unix)]
async fn wait_for_peer_disconnect(read: &mut tokio::net::unix::OwnedReadHalf) {
    let mut byte = [0u8; 1];
    // One request is admitted per connection. EOF, a read error, or any
    // subsequent byte all make this connection no longer a valid response
    // peer, so the first completed read is the entire observation.
    let _ = read.read(&mut byte).await;
}

#[cfg(all(unix, test))]
async fn handle_conn<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(all(unix, feature = "fault-injection"))]
#[doc(hidden)]
pub async fn handle_conn_for_test<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(unix)]
fn plan_frame_companion(raw: &[u8]) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    if value.get("plan").and_then(serde_json::Value::as_bool) != Some(true)
        || value
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(PROTOCOL_VERSION))
    {
        return None;
    }
    [
        "presentation",
        "presentation_per_op",
        "format",
        "format_per_op",
        "request_id",
    ]
    .into_iter()
    .find(|field| value.get(*field).is_some())
}

#[cfg(all(
    unix,
    any(test, feature = "fault-injection", feature = "test-internals")
))]
async fn handle_conn_with_shutdown<D: DaemonDispatch>(
    stream: UnixStream,
    dispatcher: D,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    initial_frame_deadline: tokio::time::Instant,
) {
    handle_conn_with_lifecycle(stream, dispatcher, shutdown, initial_frame_deadline, None).await;
}

#[cfg(unix)]
async fn handle_conn_with_lifecycle<D: DaemonDispatch>(
    mut stream: UnixStream,
    dispatcher: D,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    initial_frame_deadline: tokio::time::Instant,
    lifecycle: Option<Arc<DaemonLifecycle>>,
) {
    let mut ordinary_admission = None;
    let production_shutdown = shutdown.is_some();
    // A handover is delivered over the probed connection. The production
    // listener enforces same-uid admission, and direct handler tests cannot
    // self-signal because they do not supply the daemon shutdown receiver.
    let handover_peer_allowed = peer_uid(&stream)
        .ok()
        .is_some_and(|uid| uid == unsafe { libc::geteuid() } as u32);
    let (local_shutdown_tx, local_shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown = shutdown.unwrap_or(local_shutdown_rx);
    // Keeps the fallback receiver open in direct/test calls. Production owns
    // a sender at the daemon-run scope and passes its receiver above.
    let _local_shutdown_tx = local_shutdown_tx;
    let raw = match read_initial_frame(&mut stream, initial_frame_deadline).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "failed to read daemon request frame");
            return;
        }
    };
    #[derive(Deserialize)]
    struct SupervisorRequestEnvelope {
        #[serde(flatten)]
        frame: DaemonRequestFrame,
        #[serde(default)]
        supervisor_handover: bool,
    }
    let decoded: Result<SupervisorRequestEnvelope, _> = serde_json::from_slice(&raw);
    if decoded.as_ref().ok().is_none_or(|item| item.frame.plan) {
        if let Some(field) = plan_frame_companion(&raw) {
            let response = DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some(format!(
                    "invalid_params: plan=true cannot be combined with {field}"
                )),
                error_detail: Some(serde_json::json!({
                    "kind": "protocol",
                    "code": "invalid_params",
                    "message": format!("plan=true cannot be combined with {field}"),
                    "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                })),
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(dispatcher.config_id().to_string()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: None,
            };
            if let Ok(payload) = serde_json::to_vec(&response) {
                if let Err(error) = write_response_frame(&mut stream, &payload).await {
                    tracing::debug!(%error, "failed to write plan envelope refusal");
                }
            }
            return;
        }
    }
    let (frame, handover_requested) = match decoded {
        Ok(item) => (item.frame, item.supervisor_handover),
        Err(e) => {
            tracing::debug!(error = %e, "failed to decode daemon request frame");
            return;
        }
    };
    let supervisor_probe = frame.probe_only;
    let handover_accepted = handover_requested
        && supervisor_probe
        && production_shutdown
        && handover_peer_allowed
        && frame.protocol_version == PROTOCOL_VERSION
        && !frame.plan
        && !frame.metrics_only
        && frame.ops.is_empty()
        && read_supervisor_marker_claim().is_some_and(|(pid, _)| pid != std::process::id());
    let (mut peer_read, mut peer_write) = stream.into_split();

    let served_config_id = Some(dispatcher.config_id().to_string());
    let resp = if frame.protocol_version != PROTOCOL_VERSION {
        let msg = format!(
            "daemon protocol mismatch: client={} daemon={} — \
             rebuild/update the client binary (make local)",
            frame.protocol_version, PROTOCOL_VERSION,
        );
        tracing::warn!(
            client_version = frame.protocol_version,
            daemon_version = PROTOCOL_VERSION,
            "daemon protocol version mismatch"
        );
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some(msg.clone()),
            error_detail: Some(serde_json::json!({
                "kind": "protocol",
                "code": "version_mismatch",
                "message": msg,
                "domain_disposition": crate::DomainDisposition::Unknown.as_str(),
            })),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            // A client below this protocol is a bridge that predates the binary this
            // daemon was spawned from. Through protocol 5 the bridge treats an
            // explicit `version_mismatch` from a higher-numbered daemon as a terminal
            // error it repeats on every request, and re-execs itself onto the on-disk
            // binary only for the implicit shape: an unequal `daemon_protocol_version`
            // with the flag clear. Answering older clients in that shape, still
            // refused and still carrying the code in `error_detail`, lets every
            // pre-swap bridge replace itself on its first request instead of staying
            // refused until a person reconnects the session. A client above this
            // protocol keeps the explicit flag. Remove once no live bridge predates
            // the two-direction re-exec in khive-mcp (an inode census of `kkernel mcp`
            // processes before the swap): bridges built with it no longer read the
            // flag, but a bump while older bridges still run must keep this shape so
            // they replace themselves too.
            version_mismatch: frame.protocol_version > PROTOCOL_VERSION,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else if handover_requested {
        if handover_accepted {
            DaemonResponseFrame {
                ok: true,
                result: None,
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: frame.request_id,
            }
        } else {
            DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some("supervisor handover refused".to_string()),
                error_detail: Some(serde_json::json!({
                    "kind": "protocol",
                    "code": "supervisor_handover_refused",
                    "message": "supervisor handover refused",
                    "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                })),
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: frame.request_id,
            }
        }
    } else if frame.metrics_only && !frame.plan {
        // Process-global gauge read: namespace/config-agnostic, so this is
        // handled BEFORE the `config_id` equality reject below (unlike every
        // other arm) — a metrics probe must work regardless of which
        // client's config is asking, since it never touches the dispatcher's
        // packs/db/embed registry. READ-ONLY: builds a snapshot and returns
        // immediately, never reaching the ops-dispatch arm.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: Some({
                let mut metrics = build_metrics_snapshot(&dispatcher);
                metrics.lifecycle = lifecycle.as_ref().map(|state| {
                    let mut snapshot = state.snapshot();
                    snapshot.idle_blockers = idle_retirement_blockers(&dispatcher);
                    snapshot
                });
                metrics.connections = lifecycle.as_ref().map(|state| state.connections.snapshot());
                metrics
            }),
            request_id: frame.request_id,
        }
    // There is no `frame.namespace != dispatcher.namespace()` reject here.
    // The daemon accepts and serves the request under the frame's own
    // identity (namespace / actor / visible_namespaces, built into a
    // `RequestIdentity` below) over its one shared warm registry, rather
    // than rejecting a differently-attributed same-uid connection to a cold
    // local-dispatch fallback. `config_id`: which governs packs/db/embed
    // coherence for the shared warm engine: remains a hard reject for every
    // field other than a daemon-side superset of the client's extra embedders.
    } else if !config_ids_compatible(&frame.config_id, dispatcher.config_id()) {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            error_detail: Some(serde_json::json!({
                "kind": "protocol",
                "code": "config_mismatch",
                "message": "daemon configuration does not match the request",
                "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
            })),
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else if frame.plan {
        DaemonResponseFrame {
            ok: true,
            result: Some(dispatcher.plan(&frame.ops)),
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        }
    } else if frame.probe_only {
        // Probe-only request: identity checks passed; return immediately without
        // dispatching any verb. The client uses this to confirm the daemon is
        // alive and identity-matching without triggering any mutation.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else {
        if let Some(lifecycle) = &lifecycle {
            ordinary_admission = lifecycle.admit();
            if ordinary_admission.is_none() {
                let refusal = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some("daemon is draining; request was not admitted".to_owned()),
                    error_detail: Some(serde_json::json!({
                        "kind": "runtime", "code": "daemon_draining",
                        "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                    })),
                    request_id: frame.request_id,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: Some(dispatcher.config_id().to_owned()),
                    version_mismatch: false,
                    metrics: None,
                };
                if let Ok(payload) = serde_json::to_vec(&refusal) {
                    let _ = write_response_frame(&mut peer_write, &payload).await;
                }
                return;
            }
        }
        // Build the per-request identity context from the frame so the
        // implementor mints the storage/gate token from the CALLER's
        // identity, not the dispatcher's own construction-baked scalars.
        // This is always `Some` here: every frame that reaches this arm
        // carries a `namespace` (required on the wire) plus whatever
        // `actor_id`/`visible_namespaces` the client resolved (defaulting to
        // `None`/`vec![]` for an older, field-absent payload, which is
        // exactly the prior anonymous/no-extra-visibility behavior).
        // The caller's actor namespace joins default reads where the registry
        // mints the token (ADR-007 Rev 4 Rule 3b), the one seam every identity
        // path shares; the frame's list is forwarded as sent.
        let identity = RequestIdentity {
            namespace: frame.namespace.clone(),
            actor_id: frame.actor_id.clone(),
            visible_namespaces: frame.visible_namespaces.clone(),
            process_ref: frame.process_ref.clone(),
            request_id: frame.request_id,
        };
        tracing::debug!(
            request_id = frame.request_id,
            "daemon RequestIdentity constructed"
        );
        let (read_cancel_tx, read_cancel_rx) = tokio::sync::watch::channel(false);
        // The connection's own ceiling nests inside the dispatcher's, and a
        // nested scope keeps the earlier deadline, so the allowance must be
        // granted here or a long poll times out at the operator ceiling.
        let read_timeout = dispatcher.request_read_timeout(&frame.ops);
        let excluded_embedder_names =
            config_id_extra_embedder_exclusions(&frame.config_id, dispatcher.config_id())
                .expect("compatible configuration ids must expose their extra embedder sets");
        let dispatch = crate::runtime::scope_request_embedder_exclusions(
            excluded_embedder_names,
            khive_storage::scope_request_read_cancellation(
                shutdown,
                khive_storage::scope_request_read_cancellation(
                    read_cancel_rx,
                    khive_storage::scope_request_read_deadline(
                        read_timeout,
                        dispatcher.dispatch_with_error_detail(
                            frame.ops,
                            frame.presentation,
                            frame.presentation_per_op,
                            frame.format,
                            frame.format_per_op,
                            frame.from_wire,
                            Some(identity),
                        ),
                    ),
                ),
            ),
        );
        tokio::pin!(dispatch);
        let dispatch_result = tokio::select! {
            result = &mut dispatch => result,
            _ = wait_for_peer_disconnect(&mut peer_read) => {
                let _ = read_cancel_tx.send(true);
                dispatch.await
            }
        };
        match dispatch_result {
            Ok(result) => {
                let (result, detail) = take_daemon_lexical_timeout_marker(result);
                DaemonResponseFrame {
                    ok: true,
                    result: Some(result),
                    error: None,
                    error_detail: detail,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: frame.request_id,
                }
            }
            Err(error) => {
                let error = DaemonDispatchError::new(error.message, Some(error.error_detail));
                DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(error.message),
                    error_detail: Some(error.error_detail),
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: frame.request_id,
                }
            }
        }
    };

    let payload = if supervisor_probe {
        serde_json::to_value(&resp).and_then(|mut value| {
            if let Some(claim) = current_supervisor_claim() {
                value["supervisor_claim"] = serde_json::Value::String(claim);
            }
            if handover_accepted {
                value["supervisor_handover_accepted"] = serde_json::Value::Bool(true);
            }
            serde_json::to_vec(&value)
        })
    } else {
        serde_json::to_vec(&resp)
    };
    let mut handover_ack_written = false;
    match payload {
        Ok(payload) => {
            if payload.len() > MAX_FRAME_BYTES {
                // The serialized response exceeds the IPC frame cap.  Send a
                // small explicit error frame so the client can distinguish a
                // per-request payload-size failure from a daemon crash.  A
                // client that receives this error frame will NOT trigger
                // stale-daemon kill/respawn (ParseFailure requires a read_frame
                // error, not an ok=false result).
                tracing::warn!(
                    bytes = payload.len(),
                    limit = MAX_FRAME_BYTES,
                    "daemon response exceeds MAX_FRAME_BYTES; sending explicit error frame"
                );
                let message = format!(
                    "response too large: {} bytes exceeds {} byte IPC cap",
                    payload.len(),
                    MAX_FRAME_BYTES,
                );
                // One frame may aggregate successful, failed, and aborted operations.
                let err_resp = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(message.clone()),
                    error_detail: Some(serde_json::json!({
                        "kind": "transport",
                        "code": "response_frame_size_limit",
                        "message": message,
                        "domain_disposition": crate::DomainDisposition::Unknown.as_str(),
                    })),
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: resp.served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: resp.request_id,
                };
                if let Ok(err_payload) = serde_json::to_vec(&err_resp) {
                    if let Err(e) = write_response_frame(&mut peer_write, &err_payload).await {
                        tracing::debug!(error = %e, "failed to write oversized-response error frame");
                    }
                }
            } else {
                match write_response_frame(&mut peer_write, &payload).await {
                    Ok(()) => handover_ack_written = true,
                    Err(e) => tracing::debug!(error = %e, "failed to write daemon response frame"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialize daemon response frame"),
    }
    if handover_accepted && handover_ack_written {
        // Signal this process, not a PID observed earlier over a socket.
        // Production installed its SIGTERM handler before binding the socket.
        if unsafe { libc::raise(libc::SIGTERM) } != 0 {
            tracing::error!(error = %std::io::Error::last_os_error(), "self-directed handover signal failed");
        }
    }
    drop(ordinary_admission);
}

/// An accepted connection owns a drain slot from the accept loop until its
/// handler future is dropped. The claim happens before spawning so shutdown
/// cannot observe zero in the gap between `accept()` and the task's first poll.
#[cfg(unix)]
struct ActiveConnectionGuard {
    active: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(unix)]
impl ActiveConnectionGuard {
    fn claim(active: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { active }
    }
}

#[cfg(unix)]
impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn spawn_connection_task<F>(
    active: Arc<std::sync::atomic::AtomicUsize>,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let guard = ActiveConnectionGuard::claim(active);
    tokio::spawn(async move {
        let _guard = guard;
        future.await;
    })
}

/// Run the daemon: bind the socket, warm in the background, serve request
/// frames until SIGTERM/SIGINT.
///
/// Fatally acquires its own startup lock, which only protects
/// cleanup→pid-claim→bind — `dispatcher` has already run migrations and
/// applied pack schema plans while constructing itself, unguarded. Production
/// boot must go through [`run_daemon_with_boot_guard`] instead, which extends
/// the same lock back over construction. This entry point is for callers
/// (and tests) that build the dispatcher and start serving as one atomic
/// step with no separate boot-guard window to protect.
#[cfg(unix)]
pub async fn run_daemon<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let boot_guard = Some(acquire_daemon_boot_guard()?);
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        false,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

/// Run a real daemon server for an in-process multi-launch test.
///
/// Separate production daemon candidates have distinct PIDs, so the boot fence
/// recognizes a live incumbent and makes later candidates exit. Parallel
/// test launchers share one OS process and therefore one PID; this explicit
/// fault-injection entry point preserves the production fence semantics by
/// allowing a live same-PID incumbent to win. Ordinary daemon startup
/// continues to treat a same-PID rendezvous as stale, protecting PID-reuse
/// cleanup behavior.
///
/// A losing candidate still follows the ordinary daemon-exit path and cancels
/// the process-wide component shutdown token. Callers must therefore use a
/// component-free dispatcher; this seam validates socket/PID ownership, not
/// multi-candidate component lifecycle.
#[cfg(all(unix, any(test, feature = "fault-injection")))]
#[doc(hidden)]
pub async fn run_daemon_in_process_test<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let boot_guard = Some(acquire_daemon_boot_guard()?);
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        true,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RendezvousPathRole {
    Socket,
    PidFile,
}

#[cfg(unix)]
impl RendezvousPathRole {
    fn env_name(self) -> &'static str {
        match self {
            Self::Socket => SOCKET_PATH_ENV,
            Self::PidFile => PID_PATH_ENV,
        }
    }

    fn directory_name(self) -> &'static str {
        match self {
            Self::Socket => "socket directory",
            Self::PidFile => "PID-file directory",
        }
    }

    fn path_name(self) -> &'static str {
        match self {
            Self::Socket => "socket path",
            Self::PidFile => "PID-file path",
        }
    }

    fn path_component_name(self) -> &'static str {
        match self {
            Self::Socket => "socket-path",
            Self::PidFile => "PID-file-path",
        }
    }
}

/// Vet the socket's parent directory, re-permissioning it only when it is the
/// directory khive owns by convention.
///
/// `KHIVE_SOCKET` takes an arbitrary path, and the previous unconditional
/// chmod-0700 of its parent was wrong in both directions: pointed at a shared
/// parent like `/tmp` it either failed outright for an ordinary user, or —
/// worse — succeeded when privileged and stripped access for every other
/// process on the machine. A directory we did not create is never modified.
///
/// What the directory must actually prevent is a *takeover of the socket
/// path*: the connection gate is the 0600 socket plus the accept-time
/// peer-uid check, but both defend the daemon's own socket — neither helps
/// once another local user can put *their* listener at the path clients
/// resolve. There are two ways a shared directory allows that, and the
/// sticky bit closes neither: a writer can *pre-bind* the predictable path
/// before this daemon starts (the sticky bit restricts unlinking, not
/// creating), and the directory's *owner* can unlink and rebind even in a
/// 1777 directory (sticky exempts the directory owner). So a caller-chosen
/// directory is served only when it is trusted end to end: owned by this
/// daemon's euid or root, and not writable by group or other at all. The
/// umask-default 0755 stays acceptable; shared sticky directories like
/// `/tmp` do not.
#[cfg(unix)]
pub(crate) fn ensure_socket_dir_is_trusted(parent: &std::path::Path) -> anyhow::Result<()> {
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    ensure_rendezvous_dir_is_trusted(parent, RendezvousPathRole::Socket, daemon_euid, true)
}

/// Vet the parent directory of a PID file before reading, locking, or writing
/// it. The file's lock only protects the inode currently named by its path;
/// every directory component must therefore be as swap-resistant as the
/// socket rendezvous.
#[cfg(unix)]
pub fn ensure_pid_file_dir_is_trusted(pid_file: &std::path::Path) -> anyhow::Result<()> {
    let parent = pid_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    ensure_rendezvous_dir_is_trusted(parent, RendezvousPathRole::PidFile, daemon_euid, false)
}

#[cfg(unix)]
fn ensure_rendezvous_dir_is_trusted(
    parent: &std::path::Path,
    role: RendezvousPathRole,
    daemon_euid: u32,
    repair_owned_default: bool,
) -> anyhow::Result<()> {
    let env_name = role.env_name();
    let directory_name = role.directory_name();

    if repair_owned_default && parent == khive_dir() {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            anyhow::anyhow!(
                "refusing to start: cannot chmod 0700 {}: {e}. The khive directory must be \
                 owner-only as the {directory_name} for {env_name}; it is part of the \
                 same-uid guarantee this daemon enforces.",
                parent.display()
            )
        })?;
        return ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role);
    }

    // Fail closed on the stat itself: not being able to read the metadata is
    // not the same as the directory passing.
    let meta = std::fs::metadata(parent).map_err(|e| {
        anyhow::anyhow!(
            "refusing to start: cannot stat {directory_name} {} for {env_name}: {e}. \
             It gates rendezvous-path safety, and unreadable metadata is not a passing state.",
            parent.display()
        )
    })?;

    use std::os::unix::fs::MetadataExt;
    let owner = meta.uid();
    if owner != daemon_euid && owner != 0 {
        anyhow::bail!(
            "refusing to start: {directory_name} {} for {env_name} is owned by uid {owner}, \
             not this daemon's uid ({daemon_euid}) or root. A directory owner can replace \
             the rendezvous path regardless of mode bits. Point {env_name} at a directory \
             you own, or unset it for the default.",
            parent.display()
        );
    }

    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        anyhow::bail!(
            "refusing to start: {directory_name} {} for {env_name} is mode {:04o} — writable \
             by group or other, so another local user could replace the rendezvous path. \
             Use a directory only you can write, or unset {env_name} for the default. \
             This daemon is not changing the permissions of a directory it does not own.",
            parent.display(),
            mode & 0o7777
        );
    }

    ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role)
}

/// Socket-role form of [`ensure_rendezvous_path_is_swap_resistant`], used by the
/// socket-path tests.
#[cfg(all(unix, test))]
fn ensure_socket_path_is_swap_resistant(
    parent: &std::path::Path,
    daemon_euid: u32,
) -> anyhow::Result<()> {
    ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, RendezvousPathRole::Socket)
}

/// Walk the socket directory path exactly as the kernel will traverse it at
/// bind time — component by component, following symlinks — and refuse any
/// node another local user could swap after this validation.
///
/// Checking only the canonicalized result is not enough: bind and every
/// client traverse the *original* path, so a symlink component owned by
/// another user can resolve somewhere trusted while it is being validated
/// and be retargeted before the socket is bound. Validating the nodes the
/// traversal actually visits closes that gap: a symlink component is
/// acceptable only when its owner — the only party besides root who can
/// retarget it — is this daemon's euid or root, and every directory
/// component is held to the ancestor rule below.
///
/// The directory rule is deliberately weaker than the immediate parent's.
/// The parent hosts the socket file, where the threat is *creation* of the
/// predictable path — the sticky bit does not restrict creating, so no
/// sticky exception is sound there. An ancestor only threatens via
/// *rename/unlink of an existing entry we own*, which the sticky bit does
/// restrict: in a sticky directory, only the entry's owner, the directory's
/// owner, or root may rename it. So a root-owned `/tmp` (1777) is an
/// acceptable ancestor of a user-owned 0700 socket directory, while a
/// non-sticky group/other-writable ancestor, or one owned by a third uid
/// (who could rename the entry, or chmod the directory first), is refused.
/// Every stat failure fails closed.
#[cfg(unix)]
fn ensure_rendezvous_path_is_swap_resistant(
    parent: &std::path::Path,
    daemon_euid: u32,
    role: RendezvousPathRole,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let env_name = role.env_name();
    let directory_name = role.directory_name();
    let path_name = role.path_name();
    let component_name = role.path_component_name();

    let absolute = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| {
                anyhow::anyhow!(
                    "refusing to start: cannot resolve the working directory to absolutize \
                     {directory_name} {} for {env_name}: {e}.",
                    parent.display()
                )
            })?
            .join(parent)
    };

    fn push_components(stack: &mut Vec<std::ffi::OsString>, path: &std::path::Path) {
        let components: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        stack.extend(components.into_iter().rev());
    }

    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    push_components(&mut stack, &absolute);
    let mut resolved = std::path::PathBuf::new();
    let mut symlinks_followed = 0u32;

    while let Some(component) = stack.pop() {
        if component == "/" {
            resolved = std::path::PathBuf::from("/");
            continue;
        }
        if component == "." {
            continue;
        }
        if component == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&component);
        let meta = std::fs::symlink_metadata(&candidate).map_err(|e| {
            anyhow::anyhow!(
                "refusing to start: cannot stat {component_name} component {} for {env_name}: \
                 {e}. An unreadable component is not a passing one.",
                candidate.display()
            )
        })?;
        let owner = meta.uid();

        if meta.file_type().is_symlink() {
            symlinks_followed += 1;
            if symlinks_followed > 40 {
                anyhow::bail!(
                    "refusing to start: {path_name} for {env_name} resolves through more than \
                     40 symlinks at {} — treating this as a loop.",
                    candidate.display()
                );
            }
            if owner != daemon_euid && owner != 0 {
                anyhow::bail!(
                    "refusing to start: {component_name} symlink component {} for {env_name} \
                     is owned by uid {owner}, not this daemon's uid ({daemon_euid}) or root — \
                     its owner could retarget it after this check and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display()
                );
            }
            let target = std::fs::read_link(&candidate).map_err(|e| {
                anyhow::anyhow!(
                    "refusing to start: cannot read {component_name} symlink component {} \
                     for {env_name}: {e}.",
                    candidate.display()
                )
            })?;
            push_components(&mut stack, &target);
            continue;
        }

        if meta.is_dir() {
            let mode = meta.permissions().mode();
            let sticky = mode & 0o1000 != 0;
            if owner != daemon_euid && owner != 0 {
                anyhow::bail!(
                    "refusing to start: {component_name} ancestor {} for {env_name} is owned by \
                     uid {owner}, not this daemon's uid ({daemon_euid}) or root — its owner \
                     could rename the next path component and re-root the {path_name}. Point \
                     {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display()
                );
            }
            if mode & 0o022 != 0 && !sticky {
                anyhow::bail!(
                    "refusing to start: {component_name} ancestor {} for {env_name} is mode \
                     {:04o} — writable by group or other without the sticky bit, so another \
                     local user could rename the next path component and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display(),
                    mode & 0o7777
                );
            }
            resolved = candidate;
            continue;
        }

        anyhow::bail!(
            "refusing to start: {component_name} component {} for {env_name} is neither a \
             directory nor a symlink — the {path_name} cannot traverse it.",
            candidate.display()
        );
    }

    Ok(())
}

/// Run the daemon using a startup lock acquired by the caller *before*
/// building `dispatcher`, so a second process racing to boot (e.g. two
/// `kkernel mcp --daemon` spawns before either has bound its socket) cannot
/// run migrations/FTS DDL concurrently against the same database file.
/// `boot_guard` is only `None` on non-unix targets, where there is no
/// advisory boot lock to hold in the first place; every unix daemon-mode
/// caller passes `Some`.
///
/// The guard is held across cleanup → pid-claim → bind, then dropped. The
/// caller must not still be holding a *different* handle to the same lock
/// file when this function is entered — see the "Deadlock note" on the
/// `_startup_lock` binding below for why that would self-deadlock on `flock`.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard<D: DaemonDispatch>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
) -> anyhow::Result<()> {
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        false,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

/// Run the daemon and start host-owned background work only after the socket
/// is bound, permissions are restricted, and this process owns the PID file.
/// The callback runs once while the startup lock and teardown guard are held;
/// setup failures never invoke it. Work started by the callback must use the
/// daemon shutdown token and tracked-task drain contract.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) + Send,
{
    run_daemon_with_options_and_boot_guard_and_start(
        dispatcher,
        boot_guard,
        DaemonOptions::default(),
        |dispatcher| {
            start(dispatcher);
            DaemonStartupReport::default()
        },
    )
    .await
}

/// Start with an explicit launch mode and collect the host's startup inventory.
#[cfg(unix)]
pub async fn run_daemon_with_options_and_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    anyhow::ensure!(
        !options.idle_interval.is_zero(),
        "daemon idle interval must be positive"
    );
    run_daemon_with_boot_guard_inner(dispatcher, boot_guard, false, options, start).await
}

#[cfg(unix)]
async fn run_daemon_with_boot_guard_inner<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    allow_same_process_incumbent: bool,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    // Cancel on every exit, including setup failure and unwinding from the
    // post-ownership startup callback. The guard precedes all fallible work
    // so even a process that never becomes the daemon relinquishes its
    // process-lifetime shutdown token; restarting requires exec.
    struct ComponentTeardown;
    impl Drop for ComponentTeardown {
        fn drop(&mut self) {
            daemon_shutdown_token().cancel();
        }
    }
    let _component_teardown = ComponentTeardown;

    // Placed after the teardown guard so this refusal keeps the ADR-119
    // contract every other pre-bind error path has, and before the paths are
    // resolved so a split rendezvous never reaches cleanup/bind/pid-write.
    ensure_rendezvous_overrides_paired()?;

    let sock = socket_path();
    let pid_file = pid_path();
    let socket_parent = sock.parent();
    let pid_parent = pid_file.parent();

    if let Some(parent) = socket_parent {
        std::fs::create_dir_all(parent)?;
        ensure_socket_dir_is_trusted(parent)?;
    }
    // Identical parent paths traverse the same components, so the socket
    // check above also vets the PID-file parent. Aliased paths are checked
    // independently because each original path is traversed by file access.
    if pid_parent != socket_parent {
        ensure_pid_file_dir_is_trusted(&pid_file)?;
    }

    // Hold the startup lock across cleanup → pid-claim → bind so a concurrent
    // client's kill_and_respawn (which also holds this lock) cannot remove the
    // rendezvous paths during setup. The PID file's own lock is retained after
    // this shared startup lock is released, including while the listener drains
    // during shutdown.
    //
    // Deadlock note: the client holds this lock only during kill+spawn and
    // releases it before the spawned daemon process starts (the lock guard is
    // dropped when kill_and_respawn returns, before the readiness probe loop).
    // The daemon holds exactly one handle to this lock for its whole boot
    // sequence (received as `boot_guard`, extended from before `dispatcher`
    // was constructed) — never a second, independently-acquired handle in the
    // same process, which would self-deadlock on `flock`.
    let _startup_lock = boot_guard;

    // A second daemon must refuse loudly rather than exit successfully while
    // another daemon owns this rendezvous. Only `Stale` lets the caller proceed.
    match cleanup_stale_daemon(
        &sock,
        &pid_file,
        allow_same_process_incumbent,
        dispatcher.config_id(),
    )
    .await
    {
        Incumbent::Serving(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a khived instance is already serving this socket"
            );
            anyhow::bail!(
                "refusing to start: khived is already running as pid {incumbent_pid}, \
                 serving socket {}. Stop that instance first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Live(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a live process owns the PID file but no khived answered"
            );
            anyhow::bail!(
                "refusing to start: pid {incumbent_pid} owns the daemon PID file and is alive, \
                 but nothing answered the khived protocol on {}. It may be draining. Nothing \
                 was removed; stop that process first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Stale => {}
    }

    // Install signal streams before publishing either rendezvous file. A
    // supervisor may stop us as soon as connect/pid checks succeed, before
    // the accept loop or shutdown future has been polled. Keep these streams
    // alive so a signal during the rest of startup reaches normal cleanup.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let pid_file_guard = match write_pid_file_exclusive(&pid_file) {
        Ok(guard) => guard,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A PID file appeared between cleanup and our claim. Never touch
            // the winner's files; defer only if it already answers as khived.
            if pid_file_names_a_reachable_daemon(
                &pid_file,
                &sock,
                allow_same_process_incumbent,
                dispatcher.config_id(),
            )
            .await
            {
                tracing::info!(
                    "a replacement khived already claimed the pid/socket rendezvous; exiting"
                );
                return Ok(());
            }
            anyhow::bail!(
                "failed to claim daemon pid file at {pid_file:?}: it already exists \
                 and does not name a reachable daemon"
            );
        }
        Err(e) => return Err(e.into()),
    };

    let listener = match UnixListener::bind(&sock) {
        Ok(listener) => listener,
        Err(e) => {
            remove_pid_file_if_owned(&pid_file, &pid_file_guard);
            return Err(e.into());
        }
    };
    // Fail closed, same reason as the directory above. If this chmod fails the
    // socket is world-reachable in a way the accepted design never covered, so
    // the bound listener is dropped and the entry removed rather than served.
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        remove_pid_file_if_owned(&pid_file, &pid_file_guard);
        return Err(anyhow::anyhow!(
            "refusing to start: cannot chmod 0600 {}: {e}. The daemon socket must be owner-only \
             — it is half of the single-principal guarantee this daemon enforces.",
            sock.display()
        ));
    }
    // Captured while still holding the startup lock, immediately after
    // bind, so shutdown cleanup can later prove "this is still the same socket
    // I bound" rather than trusting the path alone.
    let bound_identity = socket_identity(&sock);

    let lifecycle = Arc::new(DaemonLifecycle::new(options, start(&dispatcher)));

    // Release the shared startup lock now that the listener is bound. The
    // locked PID file continues to identify this daemon through shutdown.
    drop(_startup_lock);
    tracing::info!(
        socket = ?sock,
        pid = std::process::id(),
        source_revision = crate::BUILD_INFO.source_revision,
        build_time = crate::BUILD_INFO.build_time,
        "khived listening"
    );

    {
        let warm = dispatcher.clone();
        track_named_background_task("daemon_warmup", async move {
            warm.warm_all().await;
        });
    }

    // The checkpoint task's own strong-count-based exit is unreachable
    // whenever `event_store_for_checkpoint()` returns `Some` (the ordinary
    // production shape), because the `SqlEventStore` it wraps retains its
    // own clone of the same pool. An explicit watch channel replaces that
    // mechanism: the sender is held for the remainder of this function's
    // scope and signalled as the first action once shutdown is observed,
    // below.
    let (checkpoint_shutdown_tx, checkpoint_shutdown_rx) = tokio::sync::watch::channel(());
    // ADR-091 Amendment 3: one checkpoint task per file-backed backend the
    // dispatcher wired — the primary pool plus every entry
    // `secondary_pools_for_checkpoint` returns — sharing this one shutdown
    // channel (the sender broadcasts to every receiver clone), so the single
    // send below stops every spawned task before `drain()`.
    let checkpoint_tasks = checkpoint_task_specs(
        dispatcher.pool_for_checkpoint(),
        dispatcher.secondary_pools_for_checkpoint(),
        dispatcher.event_store_for_checkpoint(),
        dispatcher.namespace().to_string(),
    );
    if !checkpoint_tasks.is_empty() {
        let cfg = CheckpointConfig::from_env();
        let checkpoint_task_count = checkpoint_tasks.len();
        for task in checkpoint_tasks {
            track_named_background_task(
                "wal_checkpoint",
                run_checkpoint_task(
                    task.pool,
                    cfg.clone(),
                    task.lifecycle_owner,
                    checkpoint_shutdown_rx.clone(),
                    task.is_main,
                ),
            );
        }
        tracing::info!(checkpoint_task_count, "WAL checkpoint task(s) started");
    }

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connection_tasks = Arc::new(std::sync::Mutex::new(
        Vec::<tokio::task::JoinHandle<()>>::new(),
    ));
    let (request_shutdown_tx, request_shutdown_rx) = tokio::sync::watch::channel(false);

    let shutdown = async {
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
        // Tokio retains its process-wide handlers after the streams are dropped.
        // This daemon cannot restart without exec; a repeat signal must terminate
        // even if shutdown is blocked in synchronous recovery-lock acquisition.
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: setting SIG_DFL for these valid signals needs no handler
            // pointer or shared Rust state and applies to the whole process.
            if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::pin!(shutdown);
    lifecycle.ready();

    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;

    let reason = tokio::select! {
        _ = async {
            let mut accept_error_backoff = None;
            let mut last_accept_error_log: Option<std::time::Instant> = None;
            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let initial_frame_deadline =
                            tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT;
                        accept_error_backoff = None;
                        last_accept_error_log = None;
                        // Refuse a foreign uid before any frame is read.
                        // Fails CLOSED: an error reading peer credentials is
                        // "cannot prove same-uid", which is the same answer as
                        // "is not same-uid" — never a pass.
                        //
                        // Scope: this is a same-UID check, which is strictly
                        // weaker than same-PRINCIPAL. Several distinct actors
                        // running under one uid all pass here, so this does not
                        // by itself establish that the daemon serves a single
                        // principal, and nothing downstream refuses or degrades
                        // on observing more than one. Do not describe it as a
                        // multi-principal guard — it bounds the process boundary,
                        // not the identity one.
                        match peer_uid(&stream) {
                            Ok(peer) if uid_is_permitted(peer, daemon_euid) => {}
                            Ok(peer) => {
                                tracing::error!(
                                    peer_uid = peer,
                                    daemon_euid,
                                    "refusing connection from a foreign uid: this daemon accepts \
                                     only peers running as its own uid"
                                );
                                drop(stream);
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "refusing connection: cannot read peer credentials, so \
                                     same-uid cannot be proven"
                                );
                                drop(stream);
                                continue;
                            }
                        }
                        // One permit per connection, taken before the task is
                        // spawned. Past the cap the peer is answered with a busy
                        // error and the stream is dropped; connections already
                        // admitted keep their permits and are not affected.
                        let Some(permit) = admit_or_refuse_busy(
                            &lifecycle.connections,
                            &mut stream,
                            dispatcher.config_id(),
                        )
                        .await
                        else {
                            continue;
                        };
                        // Keep the acceptance-time deadline across the
                        // credential check and connection-task scheduling.
                        let d = dispatcher.clone();
                        let shutdown = request_shutdown_rx.clone();
                        let lifecycle = Arc::clone(&lifecycle);
                        let handle = spawn_connection_task(Arc::clone(&active), async move {
                            // Released when the handler ends, whether it returns,
                            // panics or is aborted at shutdown.
                            let _permit = permit;
                            handle_conn_with_lifecycle(
                                stream,
                                d,
                                Some(shutdown),
                                initial_frame_deadline,
                                Some(lifecycle),
                            )
                            .await;
                        });
                        let mut tasks = connection_tasks
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tasks.retain(|task| !task.is_finished());
                        tasks.push(handle);
                    }
                    Err(e) => {
                        let delay = next_accept_error_backoff(accept_error_backoff);
                        accept_error_backoff = Some(delay);
                        let capacity_exhausted = matches!(
                            e.raw_os_error(),
                            Some(libc::EMFILE) | Some(libc::ENFILE)
                        );
                        if last_accept_error_log.is_none_or(|last| {
                            last.elapsed() >= std::time::Duration::from_secs(30)
                        }) {
                            tracing::error!(
                                error = %e,
                                capacity_exhausted,
                                retry_ms = delay.as_millis(),
                                "daemon accept failed; retrying with bounded backoff"
                            );
                            last_accept_error_log = Some(std::time::Instant::now());
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        } => DaemonShutdownReason::Signal,
        result = &mut shutdown => { result?; DaemonShutdownReason::Signal },
        _ = wait_for_idle(&dispatcher, &lifecycle) => DaemonShutdownReason::Idle,
    };

    lifecycle.draining(reason);

    // A listening backlog is not admitted work. Close it before draining so
    // new clients cannot finish writing to a socket nobody will accept.
    drop(listener);

    // Signal the checkpoint task to exit before draining, so `drain()`
    // actually waits on it via `track_background_task` rather than the
    // task outliving the drain window (or the process) unsignalled.
    let _ = checkpoint_shutdown_tx.send(());

    // Per-run signal: read scopes stop promptly, admitted writes ignore it and
    // retain the rest of the configured drain window to commit or roll back.
    if reason == DaemonShutdownReason::Signal {
        let _ = request_shutdown_tx.send(true);
    }

    // Same ordering contract for ADR-119 daemon components: cancel before
    // drain, so each component's supervisor (itself a tracked task) can run
    // its bounded shutdown inside the drain wait.
    daemon_shutdown_token().cancel();

    let drained = if reason == DaemonShutdownReason::Idle {
        tokio::select! {
            _ = drain_for_idle(&active, drain_timeout()) => true,
            result = &mut shutdown => {
                result?;
                lifecycle.draining(DaemonShutdownReason::Signal);
                let _ = request_shutdown_tx.send(true);
                drain(&active).await
            }
        }
    } else {
        drain(&active).await
    };
    let tasks = {
        let mut retained = connection_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *retained)
    };
    finish_connection_tasks(tasks, drained).await;

    // A concurrent client's `kill_and_respawn` may have already decided
    // this daemon looked stale, killed it, and spawned a replacement that
    // bound the same socket/PID paths while this daemon was draining above.
    // Reacquire the recovery lock (the same one that serializes startup) and
    // only unlink if the PID file still names this process AND the socket at
    // `sock` is still the exact one this daemon bound — otherwise a
    // replacement daemon owns those paths now and unlinking would delete its
    // live socket/PID out from under it.
    match acquire_recovery_lock() {
        Some(_shutdown_lock) => {
            shutdown_cleanup_if_owned(&sock, &pid_file, bound_identity);
        }
        None => {
            tracing::warn!(
                "could not acquire recovery lock for shutdown cleanup; \
                 skipping unlink to avoid deleting a replacement daemon's paths"
            );
        }
    }
    lifecycle.stopped();
    tracing::info!("khived stopped");
    Ok(())
}

/// Remove `sock`/`pid_file` only if they still belong to this process: the PID
/// file must name `std::process::id()` AND the socket currently at `sock` must
/// still be the exact one identified by `bound_identity` (dev/ino, not path).
///
/// Returns `true` if cleanup ran, `false` if it was skipped because a
/// replacement daemon already owns those paths. The caller must hold
/// the recovery lock across this call — the same lock daemon startup holds
/// across cleanup+bind+pid-write — so no replacement can bind between this
/// function's checks and its unlinks.
#[cfg(unix)]
fn shutdown_cleanup_if_owned(
    sock: &std::path::Path,
    pid_file: &std::path::Path,
    bound_identity: Option<SocketIdentity>,
) -> bool {
    let pid_is_ours = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    let socket_is_ours = bound_identity.is_some() && socket_identity(sock) == bound_identity;
    if pid_is_ours && socket_is_ours {
        let _ = std::fs::remove_file(sock);
        let _ = std::fs::remove_file(pid_file);
        true
    } else {
        tracing::warn!(
            socket = ?sock,
            pid_file = ?pid_file,
            "skipping shutdown cleanup — a replacement daemon already owns this socket/PID"
        );
        false
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Liveness verdict for a `kill(pid, 0)` probe.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidLiveness {
    /// errno 0 — signal delivery succeeded, the process exists and this
    /// caller may signal it.
    Alive,
    /// ESRCH (or any other non-EPERM errno) — no such process.
    Dead,
    /// EPERM — the process exists but this caller lacks permission to
    /// signal it. Unknown-safe: treated as running so stale-daemon cleanup
    /// never unlinks a live daemon's socket/PID file just because it is
    /// owned by a different user/uid.
    PermissionDenied,
}

#[cfg(unix)]
impl PidLiveness {
    fn is_running(self) -> bool {
        !matches!(self, PidLiveness::Dead)
    }
}

/// Maps a `kill(pid, 0)` outcome (return code + errno) to a [`PidLiveness`].
/// Pure and side-effect-free so the errno mapping can be unit tested without
/// a real process probe.
#[cfg(unix)]
fn classify_kill_result(rc: i32, errno: i32) -> PidLiveness {
    if rc == 0 {
        return PidLiveness::Alive;
    }
    match errno {
        libc::EPERM => PidLiveness::PermissionDenied,
        _ => PidLiveness::Dead,
    }
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    let rc = unsafe { libc::kill(pid, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    classify_kill_result(rc, errno).is_running()
}

/// Whether a PID may identify an incumbent from this candidate's point of view.
///
/// Production rejects the current PID so a stale rendezvous left by a prior
/// process whose PID was reused cannot protect an unrelated socket. The
/// in-process daemon harness opts in to the same-PID case because all of its
/// otherwise independent boot candidates necessarily share one OS process.
#[cfg(unix)]
fn pid_can_name_incumbent(pid: u32, current_pid: u32, allow_same_process_incumbent: bool) -> bool {
    allow_same_process_incumbent || pid != current_pid
}

/// Bounded timeout for the protocol-identity probe used by duplicate-daemon
/// detection. Short enough that a hung or foreign listener does not stall
/// startup; long enough for a live khived under normal load to answer a
/// `probe_only` frame.
#[cfg(unix)]
const DUPLICATE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether the listener at `sock` actually speaks the khived wire protocol
/// **as a khived this process can defer to** — identified by a configuration
/// compatible with `expected_config_id`.
///
/// A live PID plus an accepting Unix socket is not proof of khived: any
/// unrelated process that happens to have bound the same path also answers
/// `connect()`. Nor is any well-formed [`DaemonResponseFrame`] proof: a
/// `config_mismatch`/`version_mismatch` response, a `metrics_only` snapshot
/// response, or a legacy pre-probe daemon that falls through to normal
/// dispatch on the empty `ops` string, all deserialize cleanly without being
/// the unambiguous "yes, alive and identity-matching" answer this check
/// needs — the daemon's `metrics_only` arm in particular echoes the same
/// `ok=true, result=None, error=None`, all-mismatch-flags-false, matching
/// protocol version and `served_config_id` shape as the probe-ack arm, and
/// is distinguished only by carrying `metrics: Some(...)`. This sends a
/// bounded `probe_only` frame (the same identity probe the client-side
/// recovery path uses, `crates/khive-mcp/src/daemon.rs::probe_daemon_identity`)
/// carrying this process's own `config_id`, and requires a compatible
/// probe-branch shape back: `ok=true`, `result=None`, `error=None`,
/// `metrics=None`, `request_id=None` (this probe frame never sets one), no
/// mismatch flags, matching protocol version, and matching
/// `served_config_id` — mirroring the client probe's `is_probe_ack` check so
/// both sides of the protocol agree on what "alive" means. Connect, write,
/// and read are all inside the one bounded timeout: `UnixStream::connect`
/// itself awaits write readiness, so a listener with a saturated accept
/// backlog could otherwise hold this call open past the advertised bound.
/// A connect that succeeds but never answers, times out, or answers with
/// non-protocol bytes, a mismatched identity, a `metrics_only` snapshot, or
/// any other non-probe-shaped response is not treated as the same khived
/// and falls through to the stale-socket recovery path instead.
#[cfg(unix)]
async fn socket_speaks_khived_protocol(sock: &std::path::Path, expected_config_id: &str) -> bool {
    let probe = DaemonRequestFrame {
        probe_only: true,
        protocol_version: PROTOCOL_VERSION,
        config_id: expected_config_id.to_string(),
        ..Default::default()
    };
    let Ok(payload) = serde_json::to_vec(&probe) else {
        return false;
    };
    let response = tokio::time::timeout(DUPLICATE_PROBE_TIMEOUT, async {
        let mut stream = UnixStream::connect(sock).await.ok()?;
        write_frame(&mut stream, &payload).await.ok()?;
        let raw = read_frame(&mut stream).await.ok()?;
        serde_json::from_slice::<DaemonResponseFrame>(&raw).ok()
    })
    .await
    .ok()
    .flatten();

    let Some(resp) = response else {
        return false;
    };
    let is_probe_ack = resp.ok
        && resp.result.is_none()
        && resp.error.is_none()
        && resp.metrics.is_none()
        && resp.request_id.is_none();
    is_probe_ack
        && !resp.version_mismatch
        && !resp.namespace_mismatch
        && !resp.config_mismatch
        && resp.daemon_protocol_version == PROTOCOL_VERSION
        && resp
            .served_config_id
            .as_deref()
            .is_some_and(|served| config_ids_compatible(expected_config_id, served))
}

/// Whether connecting to an existing socket path is definitely unreachable.
/// Timeouts and other errors remain ambiguous so cleanup fails closed.
#[cfg(unix)]
async fn socket_is_unreachable(sock: &std::path::Path) -> bool {
    match tokio::time::timeout(DUPLICATE_PROBE_TIMEOUT, UnixStream::connect(sock)).await {
        Ok(Err(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        _ => false,
    }
}

/// What owns the daemon PID file, from the point of view of a process that wants
/// to start. A live owner is never cleaned up: a draining incumbent closes its
/// listener before it releases writers, so an unanswered socket is ambiguous and
/// deleting its PID file is how two daemons end up on one store.
#[cfg(unix)]
enum Incumbent {
    /// A live process that answered the khived protocol on the socket.
    Serving(u32),
    /// A live PID still has an active or ambiguous rendezvous. Nothing removed.
    Live(u32),
    /// Nothing live owns the store; the socket and PID file were removed.
    Stale,
}

/// Check whether `pid_file`/`sock` already name a live daemon and, if not,
/// remove the stale rendezvous files so the caller may bind fresh.
///
/// A live protocol responder, reachable socket, or live holder of the PID-file
/// lock means the caller must refuse to start. An unlocked PID with no listener
/// is a reused PID and may be reclaimed.
#[cfg(unix)]
async fn cleanup_stale_daemon(
    sock: &std::path::Path,
    pid_file: &std::path::Path,
    allow_same_process_incumbent: bool,
    expected_config_id: &str,
) -> Incumbent {
    let mut stale_pid_file_guard = None;
    if let Ok(pid_str) = std::fs::read_to_string(pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if pid_can_name_incumbent(pid, std::process::id(), allow_same_process_incumbent)
                && is_process_running(pid)
            {
                if sock.exists() && socket_speaks_khived_protocol(sock, expected_config_id).await {
                    return Incumbent::Serving(pid);
                }
                if sock.exists() && !socket_is_unreachable(sock).await {
                    return Incumbent::Live(pid);
                }
                match try_acquire_pid_file_lock(pid_file) {
                    Ok(Some(guard)) => stale_pid_file_guard = Some(guard),
                    Ok(None) => return Incumbent::Live(pid),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = ?pid_file,
                            "cannot check daemon PID-file lock"
                        );
                        return Incumbent::Live(pid);
                    }
                }
            }
        }
    }
    if sock.exists() {
        if let Err(e) = std::fs::remove_file(sock) {
            tracing::warn!(error = %e, path = ?sock, "failed to remove stale socket");
        }
    }
    if pid_file.exists() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            tracing::warn!(error = %e, path = ?pid_file, "failed to remove stale PID file");
        }
    }
    drop(stale_pid_file_guard);
    Incumbent::Stale
}

/// Create and lock `pid_file` exclusively (`O_EXCL`) and write this process's PID.
///
/// Uses `create_new(true)` rather than `create(true).truncate(true)` so
/// this can never silently overwrite a PID file another process created —
/// the held file lock also identifies a starting or draining daemon when its
/// socket is not yet reachable.
#[cfg(unix)]
fn write_pid_file_exclusive(pid_file: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    let mut f = opts.open(pid_file)?;
    // SAFETY: flock is a POSIX advisory lock with no memory side effects.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    f.write_all(std::process::id().to_string().as_bytes())?;
    Ok(f)
}

/// Try to lock an existing PID file without creating it. `Some(file)` means
/// there is no daemon lock holder; `None` means a daemon still owns the file.
#[cfg(unix)]
fn try_acquire_pid_file_lock(pid_file: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pid_file)?;
    // SAFETY: flock is a POSIX advisory lock with no memory side effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::EWOULDBLOCK)
    {
        Ok(None)
    } else {
        Err(error)
    }
}

/// Remove the PID file on setup failure only while its path still names the
/// file this start attempt created.
#[cfg(unix)]
fn remove_pid_file_if_owned(pid_file: &std::path::Path, guard: &std::fs::File) {
    let Ok(owned) = guard.metadata() else {
        return;
    };
    let Ok(current) = std::fs::metadata(pid_file) else {
        return;
    };
    if owned.dev() == current.dev() && owned.ino() == current.ino() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            tracing::warn!(error = %e, path = ?pid_file, "failed to remove unbound PID file");
        }
    }
}

/// Return `true` if `pid_file` currently names an eligible live process that
/// still answers on `sock` — i.e. a daemon already owns this rendezvous and it
/// is safe to defer to it rather than treat the `AlreadyExists` PID-file
/// collision as a boot failure. Eligibility requires a different PID in
/// production; the explicit in-process harness may allow the current PID.
#[cfg(unix)]
async fn pid_file_names_a_reachable_daemon(
    pid_file: &std::path::Path,
    sock: &std::path::Path,
    allow_same_process_incumbent: bool,
    expected_config_id: &str,
) -> bool {
    let Ok(pid_str) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    pid_can_name_incumbent(pid, std::process::id(), allow_same_process_incumbent)
        && is_process_running(pid)
        && sock.exists()
        && socket_speaks_khived_protocol(sock, expected_config_id).await
}

#[cfg(unix)]
async fn drain(active: &std::sync::atomic::AtomicUsize) -> bool {
    drain_with_timeout(active, drain_timeout()).await
}

/// Voluntary retirement keeps admitted workers and rendezvous ownership alive.
#[cfg(unix)]
async fn drain_for_idle(active: &std::sync::atomic::AtomicUsize, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut warned = false;
    while active.load(std::sync::atomic::Ordering::SeqCst) + background_task_count() != 0 {
        if !warned && tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "idle drain interval elapsed; retaining workers and rendezvous until settled"
            );
            warned = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
async fn drain_with_timeout(
    active: &std::sync::atomic::AtomicUsize,
    timeout: std::time::Duration,
) -> bool {
    use std::sync::atomic::Ordering;
    // One sequentially-consistent order spans connection handoff and tracked
    // task publication. Drain must not observe an ended connection and a
    // stale pre-publication background count as two simultaneous zeroes.
    let remaining = || active.load(Ordering::SeqCst) + background_task_count();
    if remaining() == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while remaining() > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining_connections = active.load(Ordering::SeqCst),
                remaining_background_tasks = background_task_count(),
                outstanding_background_tasks = %background_task_names().join(", "),
                "drain timeout reached; forcing shutdown"
            );
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
    true
}

#[cfg(unix)]
async fn finish_connection_tasks(tasks: Vec<tokio::task::JoinHandle<()>>, drained: bool) {
    if !drained {
        for task in &tasks {
            if !task.is_finished() {
                task.abort();
            }
        }
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// The bound `drain()` waits for tracked background tasks at daemon shutdown
/// (`KHIVE_DRAIN_TIMEOUT_SECS`, default 10s). Public so component supervision
/// can clamp per-component shutdown timeouts against it — a component timeout
/// longer than the drain bound could never complete its abort/state
/// transition before the daemon returns.
pub fn drain_timeout() -> std::time::Duration {
    let secs = std::env::var("KHIVE_DRAIN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Returns `true` for non-empty env values that are not `"0"` or `"false"`.
#[cfg(unix)]
pub fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

include!("daemon_khive_root_tests.rs");

/// Serve one already-admitted test connection through the production frame handler.
///
/// This seam owns no socket path, PID, boot guard, background components, or
/// process-wide shutdown state. The caller owns and joins the connection task.
/// It deliberately does not exercise listener admission or daemon lifecycle.
#[cfg(all(unix, any(test, feature = "test-internals")))]
#[doc(hidden)]
pub async fn serve_connection_for_test<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(all(test, unix))]
#[path = "daemon_tests.rs"]
mod tests;
