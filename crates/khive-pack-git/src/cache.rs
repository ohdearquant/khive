//! Scratch-clone cache for `git.digest`'s remote-URL mode (ADR-088
//! Amendment 1). Clones/fetches into
//! `~/.khive/scratch/git-digest/<cache_key>/`, keyed by canonical URL
//! (`crate::source::cache_key`). An LRU cap (env-var configured:
//! `KHIVE_GIT_DIGEST_CACHE_MAX_REPOS`, `KHIVE_GIT_DIGEST_CACHE_MAX_BYTES`,
//! `KHIVE_GIT_DIGEST_CLONE_MAX_BYTES`, `KHIVE_GIT_DIGEST_SCRATCH_ROOT`)
//! evicts least-recently-used clones once the cache exceeds its repo-count
//! or total-byte limit; a per-clone size cap rejects an oversized
//! clone/fetch before it enters the addressable cache slot. A per-`cache_key`
//! advisory `slot_lock` (issue #805) serializes each slot's check-and-mutate
//! span.
//!
//! Fresh clones stage under a private namespace this cache owns outright
//! (`<root>/.khive-git-staging/`), never directly in the (possibly shared,
//! possibly `KHIVE_GIT_DIGEST_SCRATCH_ROOT`-overridden) cache root -- a
//! staging entry's shape can never collide with unrelated operator data
//! there. Each staging entry holds an exclusive advisory lock
//! (`std::fs::File::try_lock`) on a file inside it for the whole span of the
//! clone; opening the cache root reclaims a staging entry only once that
//! lock is provably free (killed-clone residue no in-process handler could
//! remove), never merely because it looks old -- a legitimate clone still
//! running holds the lock regardless of how long it has been running. See
//! crates/khive-pack-git/docs/api/cache.md for the full design rationale
//! (ownership-proof eviction, staging-then-move installation, liveness-based
//! crash-residue reaping, fd-verified owned-slot deletion, per-clone cap
//! enforcement, slot serialization).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use khive_fs::fd_relative::{open_dir_at, stat_at, stat_fd};
use uuid::Uuid;

use crate::source::{cache_key, redact_repo_url};

#[cfg(windows)]
#[path = "cache_windows.rs"]
mod windows_slot;

pub const DEFAULT_MAX_REPOS: usize = 5;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_CLONE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

const MARKER_FILE: &str = ".khive-last-used";
/// Private subdirectory of the cache root this crate owns outright: fresh
/// clones stage here (never directly in `root`), and an owned cache slot's
/// deletion is routed through here too (unix). Never treated as a cache
/// slot itself -- its name is not `cache_key`-shaped.
const STAGING_NAMESPACE: &str = ".khive-git-staging";
/// Advisory-lock file inside one staging entry, held for the whole span of
/// the clone that owns it. See the module doc.
const STAGING_LOCK_FILE: &str = ".khive-staging.lock";
/// Marker file recording when the namespace was last swept, so
/// `prepare_cache_root` does a full scan+liveness pass at most once per
/// `REAP_THROTTLE_INTERVAL` instead of on every cache mutation.
const REAP_SWEEP_MARKER: &str = ".khive-last-swept";
const REAP_THROTTLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Floor for the clone-size poll interval (see `poll_sleep_duration`). The
/// walk that measures a clone's on-disk size costs O(entries in the partial
/// tree): on a small/cheap clone the loop sleeps this long between polls, and
/// on an entry-heavy or slow one it sleeps at least four times the previous
/// walk's own wall-clock cost, bounding the walk to at most a fifth of the
/// loop's total time no matter how large the tree grows.
const CLONE_SIZE_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Belt-and-suspenders fallback for a staging entry that crashed before it
/// could write its own lock file (the brief mkdir-then-open-lock gap at the
/// very start of `install_fresh_clone`) -- not the primary staleness
/// signal, which is the lock itself. This cannot false-positive against a
/// long-running live clone: a live clone writes its lock file within
/// microseconds of creating its staging directory, well before `git clone`
/// itself ever starts.
const STALE_STAGING_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub enum CacheError {
    Io(std::io::Error),
    Git(String),
    CloneTooLarge {
        bytes: u64,
        cap: u64,
    },
    /// A repair operation would have to touch a path that does not prove
    /// itself an owned cache slot. See
    /// crates/khive-pack-git/docs/api/cache.md#cacheerrorunsafetoreplace.
    UnsafeToReplace(PathBuf),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "scratch-cache I/O error: {e}"),
            CacheError::Git(msg) => write!(f, "{msg}"),
            CacheError::CloneTooLarge { bytes, cap } => write!(
                f,
                "clone exceeds the per-clone size cap ({bytes} bytes > {cap} bytes); \
                 the clone was removed. Raise KHIVE_GIT_DIGEST_CLONE_MAX_BYTES if this \
                 repository's history is legitimately this large."
            ),
            CacheError::UnsafeToReplace(path) => write!(
                f,
                "refusing to replace {} -- it does not prove itself an owned cache slot",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<std::io::Error> for CacheError {
    fn from(e: std::io::Error) -> Self {
        CacheError::Io(e)
    }
}

const MAX_GIT_DIAGNOSTIC_BYTES: usize = 16 * 1024;

pub(crate) fn sanitize_diagnostic(message: &str) -> String {
    message
        .lines()
        .map(|line| {
            let line: String = line
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            let sanitized = line
                .split_whitespace()
                .map(|word| {
                    // Remove whole URL/address tokens, including a URL cut off
                    // at the capture bound before its userinfo separator.
                    if word.contains("://") || word.contains('@') {
                        "[redacted remote]"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let lower = sanitized.to_ascii_lowercase();
            if [
                "authorization:",
                "cookie:",
                "password=",
                "password:",
                "token=",
                "token:",
                "secret=",
                "secret:",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                return "[redacted credential diagnostic]".to_string();
            }
            sanitized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn capture_diagnostic(mut stderr: impl Read, finished: &AtomicBool) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = match stderr.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if finished.load(Ordering::Acquire) {
                    return Ok(retained);
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Ok(retained);
        }
        let keep = count.min(MAX_GIT_DIAGNOSTIC_BYTES - retained.len());
        retained.extend_from_slice(&buffer[..keep]);
        if retained.len() == MAX_GIT_DIAGNOSTIC_BYTES && finished.load(Ordering::Acquire) {
            return Ok(retained);
        }
    }
}

struct DiagnosticPipe(std::process::ChildStderr);

impl DiagnosticPipe {
    fn new(stderr: std::process::ChildStderr) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = stderr.as_raw_fd();
            // The owned pipe has one reader; nonblocking mode lets that
            // reader stop when git exits even if a descendant retained it.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self(stderr))
    }
}

impl Read for DiagnosticPipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
            use windows_sys::Win32::System::Pipes::PeekNamedPipe;
            let mut available = 0;
            let ok = unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    Ok(0)
                } else {
                    Err(error)
                };
            }
            if available == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let count = buffer.len().min(available as usize);
            return self.0.read(&mut buffer[..count]);
        }
        #[cfg(not(windows))]
        self.0.read(buffer)
    }
}

#[cfg(any(unix, windows))]
fn with_git_diagnostics(
    command: &mut Command,
    operation: &str,
    run: impl FnOnce(&mut std::process::Child) -> Result<(), CacheError>,
) -> Result<(), CacheError> {
    command.stderr(Stdio::piped());
    let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
    .map_err(|error| CacheError::Git(format!("spawning {operation}: {error}")))?;
    let stderr = match DiagnosticPipe::new(child.stderr.take().expect("stderr configured as piped"))
    {
        Ok(stderr) => stderr,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CacheError::Git(format!(
                "preparing {operation} diagnostic capture failed"
            )));
        }
    };
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Drain concurrently even after the retained prefix fills, so git
        // cannot deadlock on a full pipe while the clone-size monitor runs.
        let captured = match std::thread::Builder::new()
            .spawn_scoped(scope, || capture_diagnostic(stderr, &finished))
        {
            Ok(captured) => captured,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CacheError::Git(format!(
                    "starting {operation} diagnostic reader failed"
                )));
            }
        };
        let result = run(&mut child);
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        finished.store(true, Ordering::Release);
        let diagnostic = captured
            .join()
            .ok()
            .and_then(Result::ok)
            .map(|bytes| sanitize_diagnostic(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();
        result.map_err(|error| match error {
            CacheError::Git(message) if !diagnostic.trim().is_empty() => {
                CacheError::Git(format!("{message}: {diagnostic}"))
            }
            other => other,
        })
    })
}

#[cfg(not(any(unix, windows)))]
fn with_git_diagnostics(
    command: &mut Command,
    operation: &str,
    run: impl FnOnce(&mut std::process::Child) -> Result<(), CacheError>,
) -> Result<(), CacheError> {
    command.stderr(Stdio::null());
    let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
    .map_err(|error| CacheError::Git(format!("spawning {operation}: {error}")))?;
    run(&mut child)
}

fn scratch_root() -> PathBuf {
    if let Ok(over) = std::env::var("KHIVE_GIT_DIGEST_SCRATCH_ROOT") {
        if !over.is_empty() {
            return PathBuf::from(over);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".khive")
        .join("scratch")
        .join("git-digest")
}

fn max_repos() -> usize {
    khive_runtime::env_parse_or("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", DEFAULT_MAX_REPOS)
}

fn max_total_bytes() -> u64 {
    khive_runtime::env_parse_or("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", DEFAULT_MAX_TOTAL_BYTES)
}

fn clone_max_bytes() -> u64 {
    khive_runtime::env_parse_or("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", DEFAULT_CLONE_MAX_BYTES)
}

/// Per-cache-slot advisory locks, keyed by `cache_key` (issue #805): each of
/// `ensure_clone`, `refetch_clone`, and `reclone` is a check-then-mutate
/// sequence (does `is_owned_entry`/existence hold, act on the result), and
/// nothing previously ordered two such sequences racing the *same* slot --
/// `refetch_clone`'s own doc comment used to admit this. Holding this slot's
/// lock for the full span of one of those functions serializes same-key
/// mutation while leaving distinct keys free to run concurrently: each
/// `cache_key` gets its own `Mutex` entry here, so locking one slot never
/// blocks a caller operating on a different slot. `SlotLock::drop` removes
/// an entry once the final live handle releases it, keeping the registry
/// bounded by active slot operations rather than process-lifetime history.
static SLOT_LOCKS: std::sync::LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

struct SlotLock {
    key: String,
    mutex: Arc<Mutex<()>>,
}

impl std::ops::Deref for SlotLock {
    type Target = Mutex<()>;

    fn deref(&self) -> &Self::Target {
        &self.mutex
    }
}

impl Drop for SlotLock {
    fn drop(&mut self) {
        let mut locks = SLOT_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The registry and this handle are the final two owners only when no
        // waiter or guard can still reference this mutex.
        let is_final_handle = Arc::strong_count(&self.mutex) == 2;
        let is_registered = locks
            .get(&self.key)
            .is_some_and(|mutex| Arc::ptr_eq(mutex, &self.mutex));
        if is_final_handle && is_registered {
            locks.remove(&self.key);
            let live_entries = locks.len();
            if locks.capacity() > live_entries.saturating_mul(4) {
                locks.shrink_to(live_entries);
            }
        }
    }
}

/// Eviction passes are serialized so the last overlapping slot mutation to
/// reach eviction observes every earlier successful operation that has
/// released its slot lock and can restore the configured caps. Callers
/// already hold their own slot lock; eviction only probes candidate locks
/// with `try_lock`, so this ordering cannot deadlock with another mutation
/// waiting here.
static EVICTION_LOCK: Mutex<()> = Mutex::new(());

/// Get-or-create the advisory lock for cache slot `key`. Callers hold the
/// returned lock for the entire check-and-mutate span of their operation on
/// that slot (see `SLOT_LOCKS`). The handle's drop check runs while holding
/// the registry mutex, so a concurrent lookup either increments the same
/// `Arc` first or observes the entry only after its final handle is gone.
fn slot_lock(key: &str) -> SlotLock {
    let mut locks = SLOT_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = key.to_string();
    let mutex = locks
        .entry(key.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    SlotLock { key, mutex }
}

/// Ensure a local clone of `canonical_url` exists and is up to date; returns
/// the repo's local path.
///
/// Fetches into the existing slot if one already proves itself owned
/// (`is_owned_entry`); otherwise clones fresh into a private staging
/// directory, enforces the per-clone size cap, and only then moves it into
/// the addressable cache slot. Returns `CacheError::UnsafeToReplace` if a
/// non-owned directory already occupies the cache-key path, and
/// `CacheError::CloneTooLarge` if the clone/fetch exceeds
/// `digest_cache_clone_max_bytes`. Runs LRU eviction over the rest of the
/// cache after a successful clone/fetch (this clone is exempt from its own
/// eviction pass). See crates/khive-pack-git/docs/api/cache.md#ensure_clone for
/// the staging-then-move and ownership-guard rationale.
pub fn ensure_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    let outcome = ensure_clone_locked(&root, canonical_url);
    finish_mutation(&root, &outcome);
    outcome
}

/// Bring the cache caps back within limits after a mutation whose slot lock
/// has just been released. A successful `ensure_clone`/`refetch_clone`/
/// `reclone` already ran `evict_lru` under its lock (protecting the slot it
/// returns), so nothing is needed on success. A FAILED mutation skipped that
/// pass, and a concurrent eviction may have deferred this slot while its lock
/// was held — leaving the caps exceeded with nothing scheduled to correct them
/// (#960). Enforce them now that the lock is free. Best-effort: the mutation's
/// own error is the one propagated, so a secondary eviction failure is logged,
/// not surfaced.
fn finish_mutation(root: &Path, outcome: &Result<PathBuf, CacheError>) {
    if outcome.is_ok() {
        return;
    }
    if let Err(evict_err) = enforce_caps(root) {
        tracing::warn!(
            error = %evict_err,
            "cap enforcement after a failed cache mutation did not complete"
        );
    }
}

fn ensure_clone_locked(root: &Path, canonical_url: &str) -> Result<PathBuf, CacheError> {
    prepare_cache_root(root)?;
    let key = cache_key(canonical_url);
    let lock = slot_lock(&key);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let repo_dir = root.join(&key);
    let cap = clone_max_bytes();

    // Slot state is decided exactly once, in the same breath as the
    // ownership check; the arms below key on this decision and never ask
    // the filesystem again. A second `.git` read would answer a question
    // about whatever occupies that pathname NOW — after our own removal,
    // or after a foreign process's write in a shared scratch root, that is
    // not necessarily anything this process owns, and the fetch arm would
    // then mutate an unowned repository (refs and ownership marker).
    // `slot_lock` is in-process only, so a foreign writer is not excluded.
    enum SlotState {
        /// `.git` present at the decision point, ownership verified,
        /// no legacy worktree.
        Owned,
        /// Legacy slot (pre-`--no-checkout` worktree) replaced whole by our
        /// own act; the pathname is vacant as far as this process is
        /// concerned. The removal re-derives ownership from a descriptor it
        /// opens itself, so migration adds no new destructive traversal.
        Replaced,
        /// No slot at the decision point.
        Absent,
    }
    let slot = if repo_dir.join(".git").exists() {
        if !is_owned_entry(&repo_dir) {
            return Err(CacheError::UnsafeToReplace(repo_dir));
        }
        if migrate_legacy_slot(root, &repo_dir)? {
            SlotState::Replaced
        } else {
            SlotState::Owned
        }
    } else {
        SlotState::Absent
    };

    match slot {
        SlotState::Owned => {
            // Ownership was proven at the decision point by pathname; prove it
            // again from a descriptor immediately before mutating. See
            // `revalidate_owned_slot` for the two-layer TOCTOU story.
            let validated = revalidate_owned_slot(&repo_dir)?;
            fetch(&repo_dir, &validated)?;
            advance_to_fetched_tip(&repo_dir, &validated)?;
            // `repo_dir` was just fetched into and its ownership already
            // confirmed above; it vanishing here is a real problem (`slot_lock`
            // excludes a concurrent `ensure_clone`/`refetch_clone`/`reclone` on
            // this same key, so nothing else in this crate should be touching
            // it), not a maybe-absent slot, so propagate rather than swallow.
            let size = dir_size(&repo_dir)?;
            if size > cap {
                // Windows pins prohibit renaming/removing the validated slot.
                // All git children have completed; release before owned cleanup.
                drop(validated);
                remove_owned_entry(root, &repo_dir)?;
                return Err(CacheError::CloneTooLarge { bytes: size, cap });
            }
            touch(&repo_dir)?;
        }
        // If a foreign directory appears at the pathname after this decision,
        // `install_fresh_clone` fails closed: it stages into a private
        // namespace and installs with a single `rename`, which refuses a
        // non-empty destination rather than overwriting it.
        SlotState::Replaced | SlotState::Absent => {
            install_fresh_clone(canonical_url, root, &repo_dir, cap)?;
        }
    }

    evict_lru(root, &repo_dir)?;
    Ok(repo_dir)
}

/// Re-fetch a corrupt-but-present cache slot with `git fetch --refetch`
/// (issue #765), re-checking ownership immediately before fetching. See
/// crates/khive-pack-git/docs/api/cache.md#refetch_clone.
pub(crate) fn refetch_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    let outcome = refetch_clone_locked(&root, canonical_url);
    finish_mutation(&root, &outcome);
    outcome
}

fn refetch_clone_locked(root: &Path, canonical_url: &str) -> Result<PathBuf, CacheError> {
    prepare_cache_root(root)?;
    let key = cache_key(canonical_url);
    let lock = slot_lock(&key);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let repo_dir = root.join(&key);
    if !repo_dir.join(".git").exists() {
        return Err(CacheError::Git(format!(
            "refetch requested for {:?} but no cache slot exists at {}",
            redact_repo_url(canonical_url),
            repo_dir.display()
        )));
    }
    // Re-check ownership immediately before mutating the slot (issue #765
    // follow-up PR #788) — see crates/khive-pack-git/docs/api/cache.md#refetch_clone.
    if !is_owned_entry(&repo_dir) {
        return Err(CacheError::UnsafeToReplace(repo_dir));
    }

    let cap = clone_max_bytes();

    // Same legacy-slot migration as the `ensure_clone_locked` path: a repair
    // pass reaches slots that predate `--no-checkout` too, and the invariant is
    // that no slot carries a worktree, not that no slot acquires one. A fresh
    // install is already the repaired state this path was trying to reach, so
    // there is nothing left to refetch afterwards.
    if migrate_legacy_slot(root, &repo_dir)? {
        install_fresh_clone(canonical_url, root, &repo_dir, cap)?;
        evict_lru(root, &repo_dir)?;
        return Ok(repo_dir);
    }

    // Same TOCTOU guard as the `ensure_clone_locked` Owned arm: the pathname
    // check above (and the migration between it and here) leave a window a
    // shared-root writer can use — re-prove ownership from a descriptor
    // immediately before the mutation.
    let validated = revalidate_owned_slot(&repo_dir)?;
    fetch_refetch(&repo_dir, &validated)?;
    advance_to_fetched_tip(&repo_dir, &validated)?;

    let size = dir_size(&repo_dir)?;
    if size > cap {
        // Ownership-guarded removal, not a raw `remove_dir_all` — see
        // crates/khive-pack-git/docs/api/cache.md#refetch_clone.
        drop(validated);
        remove_owned_entry(root, &repo_dir)?;
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
    }

    touch(&repo_dir)?;
    evict_lru(root, &repo_dir)?;
    Ok(repo_dir)
}

/// Evict an owned cache slot (if present) and install a fresh clone in its
/// place (issue #765's fallback when a refetch cannot repair the slot). See
/// crates/khive-pack-git/docs/api/cache.md#reclone.
pub(crate) fn reclone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    let outcome = reclone_locked(&root, canonical_url);
    finish_mutation(&root, &outcome);
    outcome
}

fn reclone_locked(root: &Path, canonical_url: &str) -> Result<PathBuf, CacheError> {
    prepare_cache_root(root)?;
    let key = cache_key(canonical_url);
    let lock = slot_lock(&key);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let repo_dir = root.join(&key);
    let cap = clone_max_bytes();

    remove_owned_entry(root, &repo_dir)?;
    install_fresh_clone(canonical_url, root, &repo_dir, cap)?;

    evict_lru(root, &repo_dir)?;
    Ok(repo_dir)
}

fn staging_namespace_path(root: &Path) -> PathBuf {
    root.join(STAGING_NAMESPACE)
}

/// Create (if needed) and return the private staging namespace: a
/// subdirectory of `root` this cache owns outright, never shared with
/// operator data even under a broad `KHIVE_GIT_DIGEST_SCRATCH_ROOT`
/// override. Fresh clones stage here; an owned slot's deletion is routed
/// through here too (unix).
fn ensure_staging_namespace(root: &Path) -> std::io::Result<PathBuf> {
    let path = staging_namespace_path(root);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Shared staging-clone-then-move path for both a first-time `ensure_clone`
/// and a `reclone` repair. See
/// crates/khive-pack-git/docs/api/cache.md#install_fresh_clone.
fn install_fresh_clone(
    canonical_url: &str,
    root: &Path,
    repo_dir: &Path,
    cap: u64,
) -> Result<(), CacheError> {
    let namespace_root = ensure_staging_namespace(root)
        .map_err(|e| io_err("install_fresh_clone: staging namespace", root, e))?;
    let wrapper = namespace_root.join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&wrapper)
        .map_err(|e| io_err("install_fresh_clone: create staging wrapper", &wrapper, e))?;

    let lock_path = wrapper.join(STAGING_LOCK_FILE);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&wrapper);
            io_err("install_fresh_clone: open staging lock", &lock_path, e)
        })?;
    // Held for this whole span: while this handle (or any dup of its
    // underlying fd) stays open, `reap_stale_staging`'s `try_lock` on this
    // same path observes contention and treats this wrapper as live
    // regardless of its age. A process kill (including SIGKILL) closes
    // every fd the kernel holds for it and releases the lock automatically
    // -- exactly the abandoned-staging signal the reaper needs, and never a
    // false positive against a clone that is merely slow.
    lock_file.try_lock().map_err(|e| {
        let _ = std::fs::remove_dir_all(&wrapper);
        io_err(
            "install_fresh_clone: lock staging wrapper",
            &lock_path,
            std::io::Error::from(e),
        )
    })?;

    let staging_dir = wrapper.join("repo");
    clone(canonical_url, &staging_dir, cap).inspect_err(|_| {
        // `git clone` can create and partially populate the destination
        // before failing (network drop, auth failure, bad ref) -- clean up
        // the whole wrapper so a run of failures doesn't leave staging
        // litter under the private namespace.
        let _ = std::fs::remove_dir_all(&wrapper);
    })?;
    let size = dir_size(&staging_dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&wrapper);
    })?;
    if size > cap {
        let _ = std::fs::remove_dir_all(&wrapper);
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
    }
    touch(&staging_dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&wrapper);
    })?;
    std::fs::rename(&staging_dir, repo_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&wrapper);
        CacheError::Io(e)
    })?;
    // The wrapper now contains only the lock file; drop the lock after
    // removal is queued so a concurrent reaper never observes a
    // still-locked-but-empty wrapper as a decision point (it simply won't
    // see it at all once removed).
    let _ = std::fs::remove_dir_all(&wrapper);
    drop(lock_file);
    Ok(())
}

/// Remove `repo_dir` only when it is a direct child of `root` AND passes
/// `is_owned_entry`. A slot that does not currently exist is not an error.
fn remove_owned_entry(root: &Path, repo_dir: &Path) -> Result<(), CacheError> {
    if !repo_dir.exists() {
        return Ok(());
    }
    if repo_dir.parent() != Some(root) || !is_owned_entry(repo_dir) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }
    delete_verified_owned_entry(root, repo_dir)
}

/// Race-resistant deletion of an owned cache slot living in the (possibly
/// shared, possibly `KHIVE_GIT_DIGEST_SCRATCH_ROOT`-overridden) cache root.
/// `remove_owned_entry` already checked ownership by pathname above; on
/// unix this re-verifies it against an `openat(O_NOFOLLOW)`-opened handle
/// bound to the inode at that name right now, then moves it into the
/// private staging namespace with a single fd-relative `renameat` call
/// before the (possibly slow, for a multi-GB clone) recursive delete runs.
/// This shrinks the pathname-TOCTOU window an external writer racing the
/// shared root could exploit from "however long the recursive delete
/// takes" down to the handful of syscalls between the `openat` and the
/// `renameat`; a final fd-vs-renamed-entry identity check (inode never
/// changes across a same-filesystem rename) confirms the move carried
/// exactly the directory that was validated. See
/// crates/khive-pack-git/docs/api/cache.md#delete_verified_owned_entry.
#[cfg(unix)]
fn delete_verified_owned_entry(root: &Path, repo_dir: &Path) -> Result<(), CacheError> {
    let name = repo_dir
        .file_name()
        .ok_or_else(|| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    let root_fd = unix_fd::open_dir_nofollow(root)
        .map_err(|e| io_err("delete_verified_owned_entry: open root", root, e))?;
    let target_fd = open_dir_at(&root_fd, name)
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    if !is_owned_entry_via_fd(&target_fd) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }
    let target_id = stat_fd(&target_fd)
        .map_err(|e| io_err("delete_verified_owned_entry: fstat target", repo_dir, e))?;

    let namespace_root = ensure_staging_namespace(root)
        .map_err(|e| io_err("delete_verified_owned_entry: staging namespace", root, e))?;
    let namespace_fd = unix_fd::open_dir_nofollow(&namespace_root).map_err(|e| {
        io_err(
            "delete_verified_owned_entry: open staging namespace",
            &namespace_root,
            e,
        )
    })?;
    let trash_name = format!("trash-{}", Uuid::new_v4());
    let trash_name_os = std::ffi::OsStr::new(&trash_name);
    unix_fd::renameat(&root_fd, name, &namespace_fd, trash_name_os).map_err(|e| {
        io_err(
            "delete_verified_owned_entry: renameat to private namespace",
            repo_dir,
            e,
        )
    })?;

    // `target_fd` still refers to the same inode after the rename (a file
    // descriptor tracks the open file, not its name). Compare it against
    // what `renameat` actually landed under `trash_name` to confirm the
    // move carried exactly the directory validated above, not something
    // that slipped into `name`'s place in the syscalls between the checks
    // above and the `renameat` call.
    let moved_path = namespace_root.join(&trash_name);
    let moved_id = stat_at(&namespace_fd, trash_name_os).map_err(|e| {
        io_err(
            "delete_verified_owned_entry: fstat moved entry",
            &moved_path,
            e,
        )
    })?;
    if (moved_id.st_dev, moved_id.st_ino) != (target_id.st_dev, target_id.st_ino) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }

    remove_dir_all_retrying(&moved_path).map_err(CacheError::Io)
}

#[cfg(not(unix))]
fn delete_verified_owned_entry(_root: &Path, repo_dir: &Path) -> Result<(), CacheError> {
    remove_dir_all_retrying(repo_dir).map_err(CacheError::Io)
}

/// A slot the caller just revalidated. On unix it carries a descriptor bound
/// to the slot's `.git` directory OBJECT, opened `O_NOFOLLOW` relative to the
/// validated parent, and every git command is bound to it (`fchdir` into the
/// descriptor before exec, `--git-dir .`), so git never re-resolves the name
/// `.git` and never resolves the slot pathname either. A swap after
/// validation — of the slot pathname OR of the `.git` child entry — including
/// a symlink pointed at an ancestor repository, which a re-resolved `--git-dir
/// .git` or an absolute `--git-dir` would happily follow, is never seen.
/// Windows retains no-delete-sharing handles for every component of the
/// resolved command path, including `.git`, through the synchronous wait.
/// Other non-Unix platforms retain the weaker pathname fallback.
struct ValidatedSlot {
    #[cfg(unix)]
    git_dir: std::fs::File,
    #[cfg(windows)]
    windows: windows_slot::PinnedSlot,
}

#[cfg(unix)]
impl ValidatedSlot {
    /// Test-only: bind WITHOUT the ownership check, so tests can prove what a
    /// bound command does against a hostile slot shape. Binds the slot's
    /// `.git` when present (an owned slot); otherwise binds the slot directory
    /// itself, so a command against a hostile empty slot fails loudly on "not
    /// a git repository" under an explicit `--git-dir .` rather than
    /// discovering upward into an ancestor.
    #[cfg(test)]
    fn for_test(dir: &Path) -> Self {
        let parent = unix_fd::open_dir_nofollow(dir).expect("open test slot dir");
        let git_dir = open_dir_at(&parent, std::ffi::OsStr::new(".git")).unwrap_or(parent);
        Self { git_dir }
    }
}

/// Start a `git` invocation addressed at the validated slot. Unix: the child
/// `fchdir`s into the validated `.git` descriptor before exec and uses
/// `--git-dir .`, so git operates on the descriptor-resolved `.git` object and
/// never re-resolves the name `.git` (which a relative `--git-dir .git` would,
/// following a `.git` symlink swapped in after validation). Windows: the
/// absolute path whose complete directory chain remains pinned against swaps.
/// Other non-Unix targets retain a pathname fallback. Commands are ref/git-dir-only
/// (fetch, remote set-head, symbolic-ref, update-ref); none needs a work tree,
/// so cwd being the git dir is correct.
/// Callers that only read the exit status must null stdout themselves — git
/// ref chatter ("origin/HEAD is unchanged...") otherwise interleaves with
/// the process's own stdout protocol stream. (Not nulled here: `.output()`
/// callers need stdout captured, and an explicit null would empty it.)
fn git_at_slot(repo: &Path, slot: &ValidatedSlot) -> Command {
    let mut cmd = Command::new("git");
    // Status-only helpers must never inherit the daemon log stream (#1854).
    // Fetch overrides this with bounded per-operation diagnostic capture.
    cmd.stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = slot.git_dir.as_raw_fd();
        // SAFETY: `fchdir` is async-signal-safe, and the descriptor outlives
        // the child's pre-exec window because every caller holds `slot`
        // across the synchronous wait on the command.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fchdir(fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        cmd.arg("--git-dir").arg(".");
        let _ = repo;
    }
    #[cfg(windows)]
    {
        cmd.arg("--git-dir").arg(slot.windows.git_dir());
        let _ = repo;
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Pathname-bound fallback: non-Unix targets cannot pass a directory
        // descriptor to git, so `.git` is re-resolved by name here. This
        // reopens the symlink-swap TOCTOU the Unix descriptor-pin closes — a
        // `.git` swapped for a symlink after validation redirects git to an
        // unowned repository. This fallback does not provide the Unix or
        // Windows slot-pinning guarantee.
        let _ = slot;
        cmd.arg("--git-dir").arg(repo.join(".git"));
    }
    cmd
}

/// TOCTOU guard for mutating an Owned slot: re-derive ownership from a
/// descriptor opened NOW, immediately before the mutation, rather than
/// trusting the pathname check made at the decision point. A shared-root
/// writer that swapped the slot (e.g. for an empty directory, or a symlink)
/// between the decision and the fetch fails this check (`O_NOFOLLOW` refuses
/// the symlink outright). The returned handle carries the validated
/// descriptor so the subsequent git commands stay bound to the same object —
/// see `ValidatedSlot` and `git_at_slot`.
#[cfg(unix)]
fn revalidate_owned_slot(repo_dir: &Path) -> Result<ValidatedSlot, CacheError> {
    let fd = unix_fd::open_dir_nofollow(repo_dir)
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    if !is_owned_entry_via_fd(&fd) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }
    // Bind the `.git` child directory itself, opened `O_NOFOLLOW` relative to
    // the pinned parent. This closes the child-entry race the parent FD alone
    // left open: after `is_owned_entry_via_fd` confirms `.git` is a directory,
    // a shared-root writer can still swap `.git` for a symlink at an ancestor
    // repo, which a later `--git-dir .git` (re-resolved by name) would follow.
    // `open_dir_at` refuses that symlink (`ELOOP`) and otherwise pins
    // the exact `.git` inode, so `git_at_slot`'s `--git-dir .` can never reach
    // outside the validated slot.
    let git_dir = open_dir_at(&fd, std::ffi::OsStr::new(".git"))
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    Ok(ValidatedSlot { git_dir })
}

#[cfg(windows)]
fn revalidate_owned_slot(repo_dir: &Path) -> Result<ValidatedSlot, CacheError> {
    windows_slot::PinnedSlot::open(repo_dir)
        .map(|windows| ValidatedSlot { windows })
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))
}

/// Other non-Unix fallback: pathname re-check at the same call site. Weaker than
/// the fd-bound form — the explicit `--git-dir` layer still prevents upward
/// discovery, though not a symlink swapped in after this check. That
/// pathname-bound TOCTOU remains on platforms other than Unix and Windows.
#[cfg(not(any(unix, windows)))]
fn revalidate_owned_slot(repo_dir: &Path) -> Result<ValidatedSlot, CacheError> {
    if !is_owned_entry(repo_dir) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }
    Ok(ValidatedSlot {})
}

/// fd-relative mirror of `is_owned_entry`: proves ownership against an
/// already-opened, `O_NOFOLLOW`-bound handle instead of re-resolving
/// `path.join(...)` by name. See crates/khive-pack-git/docs/api/cache.md#is_owned_entry.
#[cfg(unix)]
fn is_owned_entry_via_fd(target_fd: &std::fs::File) -> bool {
    let git_is_directory = stat_at(target_fd, std::ffi::OsStr::new(".git"))
        .is_ok_and(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFDIR);
    let marker_is_regular_file = stat_at(target_fd, std::ffi::OsStr::new(MARKER_FILE))
        .is_ok_and(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFREG);
    git_is_directory && marker_is_regular_file
}

/// The two primitives `khive_fs::fd_relative` has no equivalent for: the
/// path-based directory open and `renameat`. The descriptor-relative `openat`,
/// `fstatat` and `fstat` the cache needs (`open_dir_at`, `stat_at`, `stat_fd`)
/// come from that shared module: every operation after the initial
/// `open`/`openat` is relative to a handle the kernel resolved once, immune
/// to the original pathname being swapped out from under it afterward.
#[cfg(unix)]
mod unix_fd {
    use std::ffi::{CString, OsStr};
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::path::Path;

    use khive_fs::fd_relative;

    /// Open `path` as a directory, refusing to follow a symlink at the
    /// final component. The returned handle is bound to that exact inode:
    /// every later `*at()` call against it is immune to `path` being
    /// replaced out from under it afterward.
    pub(super) fn open_dir_nofollow(path: &Path) -> io::Result<fs::File> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: `c_path` is NUL-terminated and lives for the duration of
        // the call; `O_NOFOLLOW` refuses a symlink at the final component
        // and `O_DIRECTORY` refuses a non-directory.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by a successful `open` and is
        // owned here.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    /// `renameat(from, name, to, to_name)` — move `name` out of `from` and
    /// into `to` under `to_name`, both endpoints fd-relative so neither is
    /// re-resolved by pathname at the moment of the move.
    pub(super) fn renameat(
        from: &fs::File,
        name: &OsStr,
        to: &fs::File,
        to_name: &OsStr,
    ) -> io::Result<()> {
        let c_name = fd_relative::c_name(name)?;
        let c_to_name = fd_relative::c_name(to_name)?;
        // SAFETY: both fds are live directory descriptors; both C strings
        // are NUL-terminated for the duration of the call.
        let rc = unsafe {
            libc::renameat(
                from.as_raw_fd(),
                c_name.as_ptr(),
                to.as_raw_fd(),
                c_to_name.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A Win32 job object that kills every process it contains when the handle
/// closes, standing in for the Unix process-group kill (`terminate_clone`)
/// `Command` has no cross-platform equivalent for. `git clone` on Windows
/// can spawn a transport helper (`git-remote-https`) as a child of `git`;
/// `TerminateProcess` on the `git` PID alone leaves that helper running and
/// still writing into the destination past the cap. A job object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` reaches the whole tree in one call.
#[cfg(windows)]
mod windows_job {
    use super::CacheError;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// `dwCreationFlags` value for `CommandExt::creation_flags`: the child's
    /// primary thread starts suspended so `CloneJob::create_and_adopt` can
    /// assign the process to the job before any of the child's own code --
    /// including code that could spawn a transport grandchild -- runs.
    pub(super) const CREATE_SUSPENDED: u32 = 0x0000_0004;

    pub(super) struct CloneJob(HANDLE);

    // SAFETY: a job object HANDLE has no thread affinity; the kernel object
    // it names is safe to signal (`TerminateJobObject`) from any thread.
    unsafe impl Send for CloneJob {}

    impl Drop for CloneJob {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a live handle this struct owns exclusively.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    impl CloneJob {
        /// Create a kill-on-close job object, assign `child` (spawned with
        /// [`CREATE_SUSPENDED`]) to it, and resume its primary thread.
        ///
        /// Assignment must land before the process runs any code, or a
        /// fast-spawning transport child could start -- inheriting no job
        /// membership -- before this call takes effect. `std::process::Child`
        /// does not expose the primary-thread handle `CreateProcessW`
        /// returns (the standard library closes it, having no use for it
        /// itself), so resuming re-finds that thread by PID through a
        /// toolhelp snapshot instead of the handle Win32 handed back at
        /// creation. A suspended process has not executed any code yet --
        /// not even CRT startup -- so it owns exactly one thread; the
        /// snapshot for this PID cannot return more than one entry.
        pub(super) fn create_and_adopt(child: &std::process::Child) -> Result<Self, CacheError> {
            // SAFETY: both arguments are optional-by-null per the Win32
            // contract (no security attributes, anonymous job).
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(CacheError::Git(format!(
                    "creating clone job object: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let job = CloneJob(handle);

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is a fully initialized (zeroed then patched)
            // JOBOBJECT_EXTENDED_LIMIT_INFORMATION; its size matches the
            // struct `JobObjectExtendedLimitInformation` documents.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(CacheError::Git(format!(
                    "configuring clone job kill-on-close: {}",
                    std::io::Error::last_os_error()
                )));
            }

            let process_handle = child.as_raw_handle() as HANDLE;
            // SAFETY: `process_handle` is the live handle `std::process::Child`
            // holds for the child it just spawned suspended; it stays valid
            // for the lifetime of `child`, which outlives this call.
            let ok = unsafe { AssignProcessToJobObject(job.0, process_handle) };
            if ok == 0 {
                return Err(CacheError::Git(format!(
                    "assigning clone process to job: {}",
                    std::io::Error::last_os_error()
                )));
            }

            resume_suspended_primary_thread(child.id())?;
            Ok(job)
        }

        pub(super) fn terminate(&self) -> Result<(), CacheError> {
            // SAFETY: `self.0` is a live job handle owned by this struct.
            let ok = unsafe { TerminateJobObject(self.0, 1) };
            if ok == 0 {
                return Err(CacheError::Git(format!(
                    "terminating clone job: {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(())
        }
    }

    fn resume_suspended_primary_thread(pid: u32) -> Result<(), CacheError> {
        // SAFETY: `TH32CS_SNAPTHREAD` with a zero size snapshots every
        // thread on the system; the handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(CacheError::Git(format!(
                "snapshotting threads to resume clone process {pid}: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut thread_id = None;
        // SAFETY: `entry` is sized and zeroed per the Win32 contract for the
        // first `Thread32First` call; `snapshot` is the live handle above.
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while has_entry {
            if entry.th32OwnerProcessID == pid {
                thread_id = Some(entry.th32ThreadID);
                break;
            }
            // SAFETY: `entry`/`snapshot` as above.
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        // SAFETY: `snapshot` is a live handle owned by this function.
        unsafe {
            CloseHandle(snapshot);
        }

        let thread_id = thread_id.ok_or_else(|| {
            CacheError::Git(format!(
                "resuming clone process {pid}: its primary thread was not found in the snapshot"
            ))
        })?;

        // SAFETY: `thread_id` was just read from a live snapshot entry
        // owned by `pid`.
        let thread_handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread_handle.is_null() {
            return Err(CacheError::Git(format!(
                "opening clone process {pid} primary thread {thread_id}: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: `thread_handle` was just opened with THREAD_SUSPEND_RESUME.
        let result = unsafe { ResumeThread(thread_handle) };
        let last_error = std::io::Error::last_os_error();
        // SAFETY: closing the handle opened above.
        unsafe {
            CloseHandle(thread_handle);
        }
        if result == u32::MAX {
            return Err(CacheError::Git(format!(
                "resuming clone process {pid} primary thread {thread_id}: {last_error}"
            )));
        }
        Ok(())
    }
}

/// Retries `remove_dir_all` a few times before giving up — see
/// crates/khive-pack-git/docs/api/cache.md#remove_dir_all_retrying.
fn remove_dir_all_retrying(path: &Path) -> std::io::Result<()> {
    let mut last_err = None;
    for attempt in 0..5 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
    Err(last_err.expect("loop always sets last_err before exiting"))
}

/// How long the clone-size monitor sleeps between polls, given how long the
/// previous `dir_size` walk itself took. Never shorter than
/// `CLONE_SIZE_POLL_INTERVAL`, and long enough that the walk cannot occupy
/// more than a fifth of the loop's wall-clock time on a large or entry-heavy
/// clone tree.
fn poll_sleep_duration(walk_elapsed: Duration) -> Duration {
    CLONE_SIZE_POLL_INTERVAL.max(walk_elapsed.saturating_mul(4))
}

/// Cross-platform handle for whatever isolation mechanism keeps a clone's
/// transport/`index-pack` descendants reachable from a single stop signal.
/// Unix isolates via `process_group`, so `child.id()` alone is enough to
/// reach the whole tree (see `terminate_clone`) and this carries nothing.
/// Windows has no process-group equivalent for `Command`, so this carries
/// the job object every descendant is assigned into (`windows_job`).
#[cfg(windows)]
type CloneIsolation = windows_job::CloneJob;
#[cfg(not(windows))]
type CloneIsolation = ();

/// `-c maintenance.auto=false` on every clone/fetch into a cache slot: git
/// can otherwise spawn a detached background maintenance child that mutates
/// the slot's `.git` tree concurrently with a `dir_size`/`evict_lru` walk
/// (issue #842 flake family). See
/// crates/khive-pack-git/docs/api/cache.md#clone-git-subprocess-maintenanceautofalse.
///
/// `--no-checkout` is what makes `--filter=blob:none` actually hold. Without
/// it `git clone` checks out the default branch, and the checkout lazily
/// backfills every blob reachable at `HEAD` — so the filtered clone pays for
/// the blobs anyway and `dir_size` measures a filtered object store plus a
/// fully materialized tree. Nothing reads this slot's worktree: every
/// consumer command is `rev-parse`, `log`, or `remote`, all of which read
/// refs and the object database. Measured on this repository:
/// 61.7 MiB with the checkout, 5.6 MiB without, and all four reader commands
/// return identical output either way.
///
/// The flag is only half the fix — see [`advance_to_fetched_tip`], which had
/// to stop using `reset --hard` for the same reason.
fn clone(url: &str, dest: &Path, cap: u64) -> Result<(), CacheError> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("clone")
        .arg("--no-progress")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(url)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Isolate git and any transport/index-pack descendants so crossing
        // the cap can stop the entire transfer, not merely its direct shell.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Held suspended until `CloneIsolation::adopt_suspended` assigns the
        // process to a job object -- see that function's doc comment for why
        // a plain post-spawn assignment races a fast-spawning transport
        // child.
        command.creation_flags(windows_job::CREATE_SUSPENDED);
    }

    with_git_diagnostics(&mut command, "git clone", |child| {
        #[cfg(windows)]
        let isolation = match windows_job::CloneJob::create_and_adopt(child) {
            Ok(job) => job,
            Err(e) => {
                // The child is still suspended and has run no code; a plain kill
                // (no job needed, since nothing was assigned to one) is enough.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        #[cfg(not(windows))]
        let isolation: CloneIsolation = ();

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    terminate_clone(child, &isolation)?;
                    return Err(CacheError::Git(format!(
                        "waiting for git clone {:?}: {e}",
                        redact_repo_url(url)
                    )));
                }
            }

            let walk_start = std::time::Instant::now();
            let size = match dir_size(dest) {
                Ok(size) => size,
                // Git creates the destination itself; it is legitimately absent
                // for the first few polls after spawn.
                Err(CacheError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => 0,
                Err(e) => {
                    terminate_clone(child, &isolation)?;
                    return Err(e);
                }
            };
            let walk_elapsed = walk_start.elapsed();
            if size > cap {
                terminate_clone(child, &isolation)?;
                return Err(CacheError::CloneTooLarge { bytes: size, cap });
            }
            std::thread::sleep(poll_sleep_duration(walk_elapsed));
        };
        if !status.success() {
            return Err(CacheError::Git(format!(
                "git clone {:?} failed (exit {status})",
                redact_repo_url(url)
            )));
        }
        Ok(())
    })
}

/// Stop and reap an in-flight clone. On Unix the child is its own process
/// group, so the signal also reaches transport and index-pack descendants.
/// On Windows `isolation` is the job object every descendant was assigned
/// into at spawn time; closing it (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`)
/// reaches the same tree the process-group signal reaches on Unix.
fn terminate_clone(
    child: &mut std::process::Child,
    #[cfg_attr(not(windows), allow(unused_variables))] isolation: &CloneIsolation,
) -> Result<(), CacheError> {
    #[cfg(unix)]
    let kill_result = {
        // The child was spawned into its own process group, so its pid is the
        // group id.
        let pgid = child.id() as i32;
        match khive_runtime::process_group::signal_process_group(pgid, libc::SIGKILL) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(_) => child.kill(),
        }
    };
    #[cfg(windows)]
    let kill_result = isolation.terminate();
    #[cfg(not(any(unix, windows)))]
    let kill_result = child.kill();

    // Always attempt to reap, including when the process exited between the
    // preceding `try_wait` and the kill signal.
    let wait_result = child.wait();
    if let Err(e) = kill_result {
        return Err(CacheError::Git(format!(
            "stopping oversized git clone: {e}"
        )));
    }
    wait_result
        .map(|_| ())
        .map_err(|e| CacheError::Git(format!("reaping stopped git clone: {e}")))
}

/// Advance the cache clone's `HEAD` to the tip `fetch` just brought in.
/// `git fetch` updates remote-tracking refs only; without this step the
/// clone's HEAD stays wherever the original `git clone` (or the last
/// reclone) left it, and every walk of `HEAD` silently covers stale history
/// (issue #1644 — measured: a slot whose FETCH_HEAD was minutes old walked a
/// HEAD three weeks behind and reported a clean empty pass).
///
/// This moves a ref rather than resetting a working tree. `reset --hard`
/// populates the index and the worktree, which on a `--no-checkout` slot
/// materializes every blob reachable at the new tip and undoes the blob
/// filter on each pass — measured on this repository: a 5.6 MiB slot became
/// 62.5 MiB after one `reset --hard`. Nothing reads the worktree, so there
/// is nothing for the reset to produce except the bytes the filter exists to
/// avoid.
///
/// Failing to advance is a hard error, not a warning: proceeding would
/// reintroduce the stale-walk defect silently.
fn advance_to_fetched_tip(repo: &Path, slot: &ValidatedSlot) -> Result<(), CacheError> {
    // `origin/HEAD` is created by `git clone`; repair it first in case an
    // older slot predates it or the remote's default branch moved. Best
    // effort — the ref update below is the step that must succeed.
    // Descriptor-bound `--git-dir` throughout this function — see `fetch`:
    // these commands mutate refs and must never resolve to an ancestor
    // repository if the slot vanishes or is symlink-swapped mid-sequence.
    // (`set-head` also prints "origin/HEAD is unchanged..." to stdout, which
    // `git_at_slot` nulls so it cannot corrupt the caller's stdout stream.)
    let _ = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || {
            git_at_slot(repo, slot)
                .args(["remote", "set-head", "origin", "--auto"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdout(Stdio::null())
                .spawn()
        },
    )
    .and_then(|mut child| child.wait());

    // A fresh slot's HEAD is a symref to the default branch, so the branch is
    // what has to move for `rev-parse HEAD` and `log` to resolve at the tip.
    // A slot whose HEAD is already detached has no branch to move, and there
    // the HEAD file itself is the target. Both shapes occur: the first is
    // what `git clone` produces, the second is reachable through repair paths
    // and through slots older than this code.
    let symref = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || {
            git_at_slot(repo, slot)
                .args(["symbolic-ref", "-q", "HEAD"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
        },
    )
    .and_then(|child| child.wait_with_output())
    .map_err(|e| CacheError::Git(format!("spawning git symbolic-ref: {e}")))?;
    let branch = String::from_utf8_lossy(&symref.stdout).trim().to_string();

    let mut cmd = git_at_slot(repo, slot);
    cmd.arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("update-ref");
    if symref.status.success() && !branch.is_empty() {
        cmd.arg(&branch);
    } else {
        cmd.arg("--no-deref").arg("HEAD");
    }
    cmd.arg("refs/remotes/origin/HEAD").stdout(Stdio::null());
    let status = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || cmd.spawn(),
    )
    .and_then(|mut child| child.wait())
    .map_err(|e| CacheError::Git(format!("spawning git update-ref: {e}")))?;
    if !status.success() {
        return Err(CacheError::Git(format!(
            "advancing {} to the fetched tip failed (exit {status}); a stale \
             HEAD would walk stale history, so this pass refuses to proceed",
            repo.display()
        )));
    }
    Ok(())
}

/// Does this slot still carry a worktree left behind by a clone that predates
/// `--no-checkout`?
///
/// `--no-checkout` and the ref-only [`advance_to_fetched_tip`] together stop a
/// slot from *becoming* materialized, and neither one un-materializes a slot
/// that already is. An installation upgraded across this change keeps every
/// slot it already had: `ensure_clone_locked` sees a `.git` directory, takes
/// the existing-slot path, fetches, moves a ref, and touches the marker,
/// none of which removes a file outside `.git`. Without this step the saving
/// arrives only when a slot happens to be evicted or recloned, which is to say
/// on no schedule at all.
///
/// This is checked before the cap check on purpose. `dir_size` counts the
/// worktree, so a legacy slot can exceed the cap on exactly the bytes the
/// migration is about to reclaim, and the caller would otherwise evict a slot
/// for a size it is no longer going to have.
///
/// Replacing the slot rather than stripping it also disposes of the index for
/// free. A populated index left beside a removed worktree would make every
/// removed path read as a staged deletion to anything that later ran a
/// status-like command; a reinstalled slot is byte-for-byte what a fresh
/// `--no-checkout` clone produces, because it is one.
///
/// This is a read, and the migration it gates is a whole-slot replacement
/// through [`remove_owned_entry`] followed by [`install_fresh_clone`] — the
/// same pair [`reclone_locked`] uses.
///
/// Detecting and then deleting in place would be the obvious shape and is the
/// wrong one. Recursively removing children through the shared cache-key
/// pathname reintroduces exactly the race `remove_owned_entry` exists to close:
/// the slot lock is same-process only, so between an ownership check and a
/// pathname traversal an external writer can replace `<root>/<key>`, and the
/// traversal then deletes the replacement's children. `remove_owned_entry`
/// instead opens the slot with `O_DIRECTORY | O_NOFOLLOW`, re-checks ownership
/// against that descriptor, renames by descriptor into the private staging
/// namespace, and verifies the moved inode before removing anything. Routing
/// the migration through it means this change adds no new destructive traversal
/// at all.
///
/// Both error directions of this detector are safe, which is why it is allowed
/// to be a plain pathname read: a false positive costs one unnecessary
/// reinstall of a slot that is about to be fetched anyway, and a false negative
/// leaves the slot exactly as it was before this change. It never decides what
/// gets deleted; the removal re-derives that from a descriptor it opens itself.
#[cfg(unix)]
fn slot_carries_worktree(repo: &Path) -> Result<bool, CacheError> {
    let entries = std::fs::read_dir(repo).map_err(|e| {
        CacheError::Git(format!(
            "reading {} to detect a legacy worktree: {e}",
            repo.display()
        ))
    })?;
    for entry in entries {
        let entry = entry
            .map_err(|e| CacheError::Git(format!("reading an entry of {}: {e}", repo.display())))?;
        let name = entry.file_name();
        if name == ".git" || name == MARKER_FILE {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Replace a legacy worktree-carrying slot, reporting whether it did.
///
/// Returns `true` when the slot was removed, which means the caller must treat
/// the cache key as absent from that point on and must NOT re-derive that fact
/// by asking the filesystem again: between the removal and any such re-check,
/// an external writer can create a directory at the same pathname, and the
/// caller would then fetch into, ref-update, and marker-touch a repository it
/// never established ownership of. The per-key lock is same-process only, so it
/// does not exclude that writer. The boolean is the state; the pathname is not.
///
/// Migration is Unix-only, and deliberately so rather than incidentally.
/// Removal goes through `delete_verified_owned_entry`, whose non-Unix body is a
/// pathname-based recursive delete with no ownership check at all. That body is
/// pre-existing and reachable on Windows through the size-cap eviction path;
/// what this function declines to do is widen its reach by adding a second
/// caller. On a non-Unix target a legacy slot therefore keeps its worktree until
/// ordinary LRU or cap eviction retires it, which is exactly the behaviour that
/// target had before this change.
#[cfg(unix)]
fn migrate_legacy_slot(root: &Path, repo_dir: &Path) -> Result<bool, CacheError> {
    if slot_carries_worktree(repo_dir)? {
        remove_owned_entry(root, repo_dir)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(unix))]
fn migrate_legacy_slot(_root: &Path, _repo_dir: &Path) -> Result<bool, CacheError> {
    Ok(false)
}

fn fetch(repo: &Path, slot: &ValidatedSlot) -> Result<(), CacheError> {
    // Descriptor-bound `--git-dir` (never `-C`): `-C` runs repository
    // DISCOVERY, which walks upward when the slot has vanished and can land
    // on an ancestor repository, and an absolute `--git-dir` still follows a
    // symlink swapped in after revalidation. `git_at_slot` binds the command
    // to the validated directory object itself.
    let mut command = git_at_slot(repo, slot);
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("fetch")
        .arg("--no-progress")
        .arg("--prune")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null());
    with_git_diagnostics(&mut command, "git fetch", |child| {
        let status = child
            .wait()
            .map_err(|e| CacheError::Git(format!("waiting for git fetch: {e}")))?;
        if !status.success() {
            return Err(CacheError::Git(format!(
                "git fetch in {} failed (exit {status})",
                repo.display()
            )));
        }
        Ok(())
    })
}

/// Issue #765 repair primitive: `git fetch --refetch origin` obtains a
/// complete fresh filtered packfile instead of incrementally trusting the
/// existing object store.
fn fetch_refetch(repo: &Path, slot: &ValidatedSlot) -> Result<(), CacheError> {
    // Descriptor-bound — see `fetch` for the discovery and symlink hazards.
    let mut command = git_at_slot(repo, slot);
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("fetch")
        .arg("--no-progress")
        .arg("--refetch")
        .arg("origin")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null());
    with_git_diagnostics(&mut command, "git fetch --refetch", |child| {
        let status = child
            .wait()
            .map_err(|e| CacheError::Git(format!("waiting for git fetch --refetch: {e}")))?;
        if !status.success() {
            return Err(CacheError::Git(format!(
                "git fetch --refetch in {} failed (exit {status})",
                repo.display()
            )));
        }
        Ok(())
    })
}

/// Wraps an I/O error with the operation and path it happened on.
fn io_err(op: &str, path: &Path, e: std::io::Error) -> CacheError {
    CacheError::Io(std::io::Error::new(
        e.kind(),
        format!("{op} {}: {e}", path.display()),
    ))
}

/// Create/open the daemon-owned cache root and the private staging
/// namespace inside it. Reclaims abandoned staging directories a killed
/// clone could not clean up itself -- at most once per
/// `REAP_THROTTLE_INTERVAL`, since every public cache mutation runs this
/// before its own work and a full liveness pass over every staging entry on
/// every single mutation is unbounded latency for no benefit once the
/// namespace is already clean.
fn prepare_cache_root(root: &Path) -> Result<(), CacheError> {
    std::fs::create_dir_all(root)
        .map_err(|e| io_err("prepare_cache_root: create_dir_all", root, e))?;
    let namespace_root = ensure_staging_namespace(root)
        .map_err(|e| io_err("prepare_cache_root: create staging namespace", root, e))?;
    if !reap_due(&namespace_root)? {
        return Ok(());
    }
    let removed = reap_stale_staging(root, SystemTime::now(), STALE_STAGING_AGE)?;
    mark_reap_swept(&namespace_root);
    if removed > 0 {
        tracing::info!(
            removed,
            root = %root.display(),
            "reclaimed abandoned git-digest staging directories"
        );
    }
    Ok(())
}

/// Whether enough time has passed since the last sweep to run another one.
/// A missing marker (first call ever, or a namespace a previous sweep just
/// emptied without leaving the marker readable) always sweeps.
fn reap_due(namespace_root: &Path) -> Result<bool, CacheError> {
    let marker = namespace_root.join(REAP_SWEEP_MARKER);
    match std::fs::metadata(&marker).and_then(|m| m.modified()) {
        Ok(last) => {
            let elapsed = SystemTime::now()
                .duration_since(last)
                .unwrap_or(Duration::MAX);
            Ok(elapsed >= REAP_THROTTLE_INTERVAL)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(io_err("prepare_cache_root: read sweep marker", &marker, e)),
    }
}

/// Best-effort: a failure to record the sweep marker only costs an extra
/// sweep next time, never correctness.
fn mark_reap_swept(namespace_root: &Path) {
    let marker = namespace_root.join(REAP_SWEEP_MARKER);
    if let Err(e) = std::fs::write(&marker, b"") {
        tracing::warn!(
            error = %e,
            "failed to record git-digest staging sweep marker"
        );
    }
}

/// Exact ownership proof for a staging wrapper: a canonical lowercase
/// hyphenated UUID, a direct child of the private staging namespace.
fn is_staging_wrapper_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name)
}

/// Deletion residue owned by this cache: `delete_verified_owned_entry`
/// renames a doomed cache slot to `trash-<canonical UUID>` inside the
/// private namespace before recursively deleting it. A kill in that window
/// leaves the renamed directory behind, so the sweep must admit these names
/// too or the deletion path reintroduces the unreclaimable-residue class
/// this module exists to close. Trash entries never carry a staging lock
/// file, so `staging_liveness` judges them by the conservative age fence
/// alone; an entry whose recursive delete is still in flight is protected
/// by that fence, and a concurrent double-delete resolves benignly because
/// `remove_staging_wrapper` tolerates `NotFound`.
fn is_trash_residue_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.strip_prefix("trash-")
        .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok_and(|id| id.to_string() == suffix))
}

/// Liveness verdict for one staging wrapper.
enum StagingLiveness {
    /// Another cleanup already removed the wrapper before its mtime read.
    Gone,
    /// A live handle holds the wrapper's lock (or it has not existed long
    /// enough yet for a missing lock file to mean anything) -- must survive
    /// regardless of age.
    Live,
    /// No live handle holds the lock: either `try_lock` acquired it
    /// (nothing else has it open), or the lock file was never written and
    /// the wrapper is old enough that it cannot be a legitimate in-flight
    /// clone.
    Abandoned,
}

/// Liveness, not age, is the deletion criterion (see the module doc). A
/// wrapper whose lock file exists is judged purely by whether `try_lock`
/// can acquire it -- an active clone running past `max_age` still holds the
/// lock and survives; a killed clone's lock is released by the kernel the
/// instant the process dies and is reaped on the very next sweep,
/// regardless of how fresh its mtime looks. A missing lock file (the
/// narrow crash-before-lock-file window) falls back to the same
/// conservative age fence the old age-only check used.
fn staging_liveness(
    wrapper: &Path,
    now: SystemTime,
    max_age: Duration,
    wrapper_modified: &mut impl FnMut(&Path) -> std::io::Result<SystemTime>,
) -> Result<StagingLiveness, CacheError> {
    let lock_path = wrapper.join(STAGING_LOCK_FILE);
    match std::fs::OpenOptions::new().write(true).open(&lock_path) {
        Ok(lock_file) => match lock_file.try_lock() {
            Ok(()) => Ok(StagingLiveness::Abandoned),
            Err(std::fs::TryLockError::WouldBlock) => Ok(StagingLiveness::Live),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(io_err("reap_stale_staging: try_lock", &lock_path, e))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let modified = match wrapper_modified(wrapper) {
                Ok(modified) => modified,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(StagingLiveness::Gone);
                }
                Err(e) => return Err(io_err("reap_stale_staging: wrapper mtime", wrapper, e)),
            };
            match now.duration_since(modified) {
                Ok(age) if age > max_age => Ok(StagingLiveness::Abandoned),
                _ => Ok(StagingLiveness::Live),
            }
        }
        Err(e) => Err(io_err(
            "reap_stale_staging: open staging lock",
            &lock_path,
            e,
        )),
    }
}

/// Remove a staging wrapper found abandoned by `staging_liveness`. The
/// wrapper lives inside the private namespace this cache owns outright, so
/// unlike an owned cache slot in the shared root (`delete_verified_owned_entry`)
/// there is no external-writer exposure to harden against here -- nothing
/// but this cache ever creates entries under `STAGING_NAMESPACE`.
fn remove_staging_wrapper(namespace_root: &Path, name: &std::ffi::OsStr) -> Result<(), CacheError> {
    match remove_dir_all_retrying(&namespace_root.join(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CacheError::Io(e)),
    }
}

/// Reclaim abandoned staging wrappers and interrupted-deletion residue
/// under the private namespace. A deletion candidate must be a real
/// directory (never a symlink, file, or nested path) whose name is exactly
/// a canonical lowercase hyphenated UUID (clone wrapper) or
/// `trash-<canonical UUID>` (deletion residue), and must be judged
/// `Abandoned` by `staging_liveness`. `now`/`max_age` are explicit so the
/// fallback age boundary is deterministic in tests.
fn reap_stale_staging(
    root: &Path,
    now: SystemTime,
    max_age: Duration,
) -> Result<usize, CacheError> {
    reap_stale_staging_with(root, now, max_age, |path| {
        std::fs::symlink_metadata(path).and_then(|m| m.modified())
    })
}

// The second stat is injectable so tests can interpose after actual wrapper admission.
fn reap_stale_staging_with(
    root: &Path,
    now: SystemTime,
    max_age: Duration,
    mut wrapper_modified: impl FnMut(&Path) -> std::io::Result<SystemTime>,
) -> Result<usize, CacheError> {
    let namespace_root = staging_namespace_path(root);
    let read_dir = match std::fs::read_dir(&namespace_root) {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(io_err(
                "reap_stale_staging: read_dir namespace",
                &namespace_root,
                e,
            ));
        }
    };
    let mut removed = 0usize;

    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(io_err(
                    "reap_stale_staging: read_dir entry",
                    &namespace_root,
                    e,
                ));
            }
        };
        let name = entry.file_name();
        if !is_staging_wrapper_name(&name) && !is_trash_residue_name(&name) {
            continue;
        }
        let path = entry.path();
        if path.parent() != Some(namespace_root.as_path()) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("reap_stale_staging: stat", &path, e)),
        };
        if !metadata.file_type().is_dir() {
            continue;
        }

        match staging_liveness(&path, now, max_age, &mut wrapper_modified)? {
            StagingLiveness::Gone | StagingLiveness::Live => continue,
            StagingLiveness::Abandoned => {
                remove_staging_wrapper(&namespace_root, &name)?;
                removed += 1;
            }
        }
    }

    Ok(removed)
}

fn touch(repo_dir: &Path) -> Result<(), CacheError> {
    let marker = repo_dir.join(MARKER_FILE);
    std::fs::write(&marker, b"").map_err(|e| io_err("touch: write marker", &marker, e))?;
    Ok(())
}

/// Recursive directory size, following no symlinks. Tolerant of a
/// *descendant* disappearing mid-walk (contributes 0 bytes), or a Windows
/// delete-pending descendant whose metadata stays inaccessible after the
/// bounded recheck. The walk **root** itself vanishing or becoming
/// inaccessible is NOT tolerated. See
/// crates/khive-pack-git/docs/api/cache.md#dir_size.
fn dir_size(path: &Path) -> Result<u64, CacheError> {
    dir_size_with(
        path,
        cfg!(windows),
        |path| std::fs::symlink_metadata(path),
        || std::thread::sleep(DIR_SIZE_DENIED_WAIT),
    )
}

// A brief cleanup grace period before a denied descendant metadata probe is
// skipped. Windows delete-pending handles have no guaranteed release deadline.
// Four rechecks at 10 ms bound this grace at 40 ms.
const DIR_SIZE_DENIED_RETRIES: usize = 4;
const DIR_SIZE_DENIED_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

fn dir_size_io<T>(
    is_root: bool,
    retry_denied: bool,
    skip_denied: bool,
    mut operation: impl FnMut() -> std::io::Result<T>,
    wait: &mut impl FnMut(),
) -> std::io::Result<Option<T>> {
    let mut retries = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_root => {
                return Ok(None);
            }
            Err(error)
                if retry_denied
                    && !is_root
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && retries < DIR_SIZE_DENIED_RETRIES =>
            {
                retries += 1;
                wait();
            }
            Err(error)
                if retry_denied
                    && skip_denied
                    && !is_root
                    && error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    }
}

// The operation and wait seams keep portable fault tests deterministic while
// the production wrapper alone chooses the platform-specific retry policy.
fn dir_size_with(
    path: &Path,
    retry_denied: bool,
    mut stat: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    mut wait: impl FnMut(),
) -> Result<u64, CacheError> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let is_root = p == path;
        let Some(md) = dir_size_io(is_root, retry_denied, true, || stat(&p), &mut wait)
            .map_err(|error| io_err("dir_size: stat", &p, error))?
        else {
            continue;
        };
        if md.is_dir() {
            let Some(read_dir) = dir_size_io(
                is_root,
                retry_denied,
                false,
                || std::fs::read_dir(&p),
                &mut wait,
            )
            .map_err(|error| io_err("dir_size: read_dir", &p, error))?
            else {
                continue;
            };
            for entry in read_dir {
                match entry {
                    Ok(entry) => stack.push(entry.path()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_err("dir_size: read_dir entry", &p, error)),
                }
            }
        } else {
            total += md.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
#[path = "cache_dir_size_tests.rs"]
mod dir_size_tests;

fn is_cache_key_name(name: &str) -> bool {
    name.len() == 16
        && name
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Whether `path` is a directory `ensure_clone` could plausibly have
/// created: a 16-lowercase-hex `cache_key`-shaped real directory (not a
/// symlink) containing both a `.git` entry and the `.khive-last-used`
/// marker. See crates/khive-pack-git/docs/api/cache.md#is_owned_entry.
fn is_owned_entry(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if !is_cache_key_name(name) {
        return false;
    }
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => {}
        _ => return false,
    }
    path.join(".git").exists() && path.join(MARKER_FILE).exists()
}

/// Evict least-recently-used clones under `root` until both the
/// repo-count cap and the total-byte cap are satisfied. `keep` is never
/// evicted, and its own vanishing is NOT tolerated (propagates as an
/// error); a listed candidate entry vanishing mid-walk IS tolerated
/// (skipped). See crates/khive-pack-git/docs/api/cache.md#evict_lru.
fn evict_lru(root: &Path, keep: &Path) -> Result<(), CacheError> {
    evict_to_caps(root, Some(keep))
}

/// Enforce the cache caps with no protected slot: evict least-recently-used
/// owned clones until both caps hold, treating every owned slot as a
/// candidate. Run after a cache mutation releases its slot lock on a FAILURE
/// path (#960). A failed `ensure_clone`/`refetch_clone`/`reclone` skips the
/// success-path `evict_lru`, and a concurrent eviction may have deferred this
/// slot (its lock was held) — so without this pass the caps can stay exceeded
/// with nothing scheduled to correct them. See
/// crates/khive-pack-git/docs/api/cache.md#enforce_caps.
fn enforce_caps(root: &Path) -> Result<(), CacheError> {
    evict_to_caps(root, None)
}

/// Shared eviction core. `keep = Some(slot)` protects that slot from eviction
/// and requires it to still exist (its vanishing propagates as an error);
/// `keep = None` protects nothing. Holds `EVICTION_LOCK` for the whole pass
/// and takes each candidate's `slot_lock` with `try_lock`, deferring (skipping)
/// a candidate whose lock is currently held rather than blocking on it — the
/// deferred candidate's own mutation runs its own tail pass once it settles.
fn evict_to_caps(root: &Path, keep: Option<&Path>) -> Result<(), CacheError> {
    let _eviction_guard = EVICTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut entries: Vec<(PathBuf, String, SystemTime, u64)> = Vec::new();
    let read_dir =
        std::fs::read_dir(root).map_err(|e| io_err("evict_lru: read_dir root", root, e))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            // The directory listing raced a concurrent removal of one of its
            // own entries (e.g. another `evict_lru`/`ensure_clone` repairing
            // the same root) -- nothing to evict there anymore.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("evict_lru: read_dir entry", root, e)),
        };
        let p = entry.path();
        if keep == Some(p.as_path()) {
            continue;
        }
        let Some(key) = p.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_cache_key_name(key) || !is_owned_entry(&p) {
            continue;
        }
        let key = key.to_string();
        let lock = slot_lock(&key);
        let _candidate_guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => continue,
        };
        if !p.is_dir() || !is_owned_entry(&p) {
            continue;
        }
        let mtime = std::fs::metadata(p.join(MARKER_FILE))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let size = match dir_size(&p) {
            Ok(size) => size,
            // `p` was listed above but a concurrent repair on the same root
            // has since deleted it whole -- there is no slot left to weigh
            // in eviction accounting, not a size of `0` to record.
            Err(CacheError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        entries.push((p, key, mtime, size));
    }
    entries.sort_by_key(|(_, _, mtime, _)| *mtime);

    let (keep_size, keep_count) = match keep {
        Some(keep) => (dir_size(keep)?, 1),
        None => (0, 0),
    };
    let mut total: u64 = entries.iter().map(|(_, _, _, s)| s).sum::<u64>() + keep_size;
    let mut count = entries.len() + keep_count;
    let cap_repos = max_repos();
    let cap_bytes = max_total_bytes();

    for (path, key, _, measured_size) in entries {
        if count <= cap_repos && total <= cap_bytes {
            break;
        }
        let lock = slot_lock(&key);
        let _candidate_guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => continue,
        };
        if !is_owned_entry(&path) {
            count = count.saturating_sub(1);
            total = total.saturating_sub(measured_size);
            continue;
        }
        let current_size = dir_size(&path)?;
        total = total
            .saturating_sub(measured_size)
            .saturating_add(current_size);
        if count <= cap_repos && total <= cap_bytes {
            break;
        }
        remove_owned_entry(root, &path)?;
        count -= 1;
        total = total.saturating_sub(current_size);
    }
    Ok(())
}

/// Serializes tests that touch process-global env vars (`scratch_root()`
/// reads them). See crates/khive-pack-git/docs/api/cache.md#env_mutex.
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "cache_stderr_tests.rs"]
mod stderr_tests;

#[cfg(all(test, unix))]
#[path = "cache_fd_tests.rs"]
mod fd_tests;
